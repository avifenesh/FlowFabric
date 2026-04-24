//! Post-completion dependency-resolution cascade (Wave 5a).
//!
//! When an execution reaches a terminal outcome the Wave 4b writer
//! (`attempt::complete` / `attempt::fail`) / Wave 4c (`cancel_flow`)
//! emits an `ff_completion_event` row. The engine's dispatch loop
//! polls the outbox by `event_id` and, for each event, calls
//! [`dispatch_completion`]. This module is the Postgres twin of the
//! Valkey `ff_resolve_dependency` FCALL cascade in
//! `ff-engine::partition_router::dispatch_dependency_resolution`.
//!
//! # Per-hop-tx (K-2 adjudication)
//!
//! The round-2 RFC debate locked the cascade to per-hop transactions
//! — NOT one mega-transaction spanning transitive descendants. A
//! fanout of N downstream edges that each fan out to M further
//! edges would otherwise hold `ff_edge_group` row locks across the
//! entire subgraph for the full cascade duration, which is
//! user-visible on large flows.
//!
//! The layout here:
//!
//! 1. Outer function [`dispatch_completion`] runs a short claim-tx
//!    that atomically flips `ff_completion_event.dispatched_at_ms`
//!    from NULL to `now_ms()` via `UPDATE ... RETURNING` — a
//!    concurrent dispatcher (retry, reconciler) observes the
//!    already-claimed row and short-circuits with
//!    [`DispatchOutcome::NoOp`].
//! 2. For each downstream edge, [`advance_edge_group`] runs its own
//!    read-committed tx with `SELECT ... FOR UPDATE` on the
//!    `ff_edge_group` row. Counter bump + policy eval + downstream
//!    state flip + sibling-cancel bookkeeping all commit together
//!    at hop boundary, then release the row lock.
//! 3. If a hop's tx exhausts its serialization retries we return
//!    [`EngineError::Contention(RetryExhausted)`]; the
//!    Wave 6 reconciler picks up the uncleared `dispatched_at_ms`
//!    via the partial index added in migration 0003.
//!
//! Failures mid-cascade leave the outbox row marked dispatched
//! (short-circuited) but partial downstream state; the reconciler
//! catches orphaned work on its next scan.

use std::time::Duration;

use ff_core::contracts::{EdgeDependencyPolicy, OnSatisfied};
use ff_core::engine_error::{ContentionKind, EngineError};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Max serialization-retry attempts per hop before we declare
/// contention and hand the event back to the reconciler.
const ADVANCE_MAX_ATTEMPTS: u32 = 3;

/// Outcome of a single dispatch invocation. Surfaces enough to let
/// callers (the dispatcher loop, tests) distinguish claim-races from
/// real work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Event was already claimed by a concurrent dispatcher or by a
    /// previous invocation; no work performed. Idempotent replay.
    NoOp,
    /// Dispatch fired; inner count is the number of downstream edges
    /// whose groups were advanced. `0` is legal (terminal exec with
    /// no outgoing edges, i.e. a leaf flow node).
    Advanced(usize),
}

/// Public entry point — poll one outbox event and cascade.
///
/// The caller owns polling cadence + ordering. Typical usage is a
/// tokio task that subscribes to
/// [`super::completion::subscribe`] and invokes this function for
/// each payload's `event_id`.
#[tracing::instrument(name = "pg.dispatch_completion", skip(pool))]
pub async fn dispatch_completion(
    pool: &PgPool,
    event_id: i64,
) -> Result<DispatchOutcome, EngineError> {
    // Claim step — atomic flip of `dispatched_at_ms` from NULL.
    // Returns the event row when we won the race; `None` otherwise.
    let now = now_ms();
    let row = sqlx::query(
        r#"
        UPDATE ff_completion_event
           SET dispatched_at_ms = $2
         WHERE event_id = $1
           AND dispatched_at_ms IS NULL
         RETURNING partition_key, execution_id, flow_id, outcome
        "#,
    )
    .bind(event_id)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Ok(DispatchOutcome::NoOp);
    };

    let partition_key: i16 = row.get("partition_key");
    let execution_id: Uuid = row.get("execution_id");
    let flow_id: Option<Uuid> = row.get("flow_id");
    let outcome: String = row.get("outcome");

    let Some(flow_id) = flow_id else {
        // Standalone exec: no edges to cascade. Claim stays set so a
        // replay short-circuits.
        return Ok(DispatchOutcome::Advanced(0));
    };

    // Outgoing edges: every edge whose upstream is this exec, under
    // this flow. RFC-011 co-locates flow + exec under the same
    // partition_key, so the query is partition-local.
    let edges = sqlx::query(
        r#"
        SELECT edge_id, downstream_eid
          FROM ff_edge
         WHERE partition_key = $1 AND flow_id = $2 AND upstream_eid = $3
        "#,
    )
    .bind(partition_key)
    .bind(flow_id)
    .bind(execution_id)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    if edges.is_empty() {
        return Ok(DispatchOutcome::Advanced(0));
    }

    let outcome_kind = OutcomeKind::from_str(&outcome);

    // Each edge advances in its own tx (K-2 per-hop-tx rule).
    let mut advanced: usize = 0;
    for edge in &edges {
        let downstream_eid: Uuid = edge.get("downstream_eid");
        advance_edge_group_with_retry(
            pool,
            partition_key,
            flow_id,
            execution_id,
            downstream_eid,
            outcome_kind,
        )
        .await?;
        advanced += 1;
    }

    Ok(DispatchOutcome::Advanced(advanced))
}

/// Counter bucket derived from `ff_completion_event.outcome`.
///
/// The outbox stores free-form outcome strings; the engine collapses
/// them into the three counters tracked on `ff_edge_group`. `skipped`
/// outcomes are treated as skip; anything terminal that isn't
/// explicitly "success" is a fail from the dependency resolver's
/// point of view (parity with the Valkey `ff_resolve_dependency`
/// Lua which checks `upstream_outcome == "success"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeKind {
    Success,
    Fail,
    Skip,
}

impl OutcomeKind {
    fn from_str(s: &str) -> Self {
        match s {
            "success" => Self::Success,
            "skipped" => Self::Skip,
            _ => Self::Fail,
        }
    }
}

/// Per-hop tx wrapper with SERIALIZABLE retry handling. Exhaustion
/// returns `Contention(RetryExhausted)` per Q11.
async fn advance_edge_group_with_retry(
    pool: &PgPool,
    partition_key: i16,
    flow_id: Uuid,
    upstream_eid: Uuid,
    downstream_eid: Uuid,
    outcome: OutcomeKind,
) -> Result<(), EngineError> {
    for attempt in 0..ADVANCE_MAX_ATTEMPTS {
        match advance_edge_group(
            pool,
            partition_key,
            flow_id,
            upstream_eid,
            downstream_eid,
            outcome,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) if is_serialization_conflict(&err) => {
                if attempt + 1 < ADVANCE_MAX_ATTEMPTS {
                    let ms = 5u64 * (1u64 << attempt);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                return Err(EngineError::Contention(ContentionKind::RetryExhausted));
            }
            Err(err) => return Err(err),
        }
    }
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

fn is_serialization_conflict(err: &EngineError) -> bool {
    // 40001 / 40P01 are mapped to Contention(LeaseConflict) up in
    // `error::map_sqlx_error`. Lock-timeout (55P03) is a legitimate
    // contention signal on `FOR UPDATE` + `lock_timeout`; the shared
    // error mapper routes it through `Transport`, so we probe the
    // SQLSTATE off the boxed sqlx error here before treating it as
    // a retryable contention fault.
    if matches!(err, EngineError::Contention(ContentionKind::LeaseConflict)) {
        return true;
    }
    if let EngineError::Transport { source, .. } = err
        && let Some(sqlx_err) = source.downcast_ref::<sqlx::Error>()
        && let Some(db) = sqlx_err.as_database_error()
        && let Some(code) = db.code()
        && code.as_ref() == "55P03"
    {
        // 55P03 = lock_not_available (lock_timeout variant hit
        // while waiting on a row lock).
        return true;
    }
    false
}

/// Per-hop transaction: bump counters on one `ff_edge_group` row,
/// evaluate the policy, apply the decision to the downstream exec,
/// optionally enqueue sibling-cancel bookkeeping.
#[tracing::instrument(
    name = "pg.advance_edge_group",
    skip(pool),
    fields(
        part = partition_key,
        flow = %flow_id,
        downstream = %downstream_eid,
    )
)]
async fn advance_edge_group(
    pool: &PgPool,
    partition_key: i16,
    flow_id: Uuid,
    upstream_eid: Uuid,
    downstream_eid: Uuid,
    outcome: OutcomeKind,
) -> Result<(), EngineError> {
    let _ = upstream_eid; // retained for tracing; no per-hop predicate yet.

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // FOR UPDATE pins the group row against concurrent advances.
    let row = sqlx::query(
        r#"
        SELECT policy, success_count, fail_count, skip_count, running_count,
               cancel_siblings_pending_flag
          FROM ff_edge_group
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3
         FOR UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(flow_id)
    .bind(downstream_eid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        // No group row for this downstream — nothing to advance.
        // Legal when a flow has edges without a staged group (should
        // not happen post-Wave 4c, but we treat as a no-op for
        // forward-compat instead of failing the cascade.)
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(());
    };

    let policy_raw: JsonValue = row.get("policy");
    let mut success: i32 = row.get("success_count");
    let mut fail: i32 = row.get("fail_count");
    let mut skip: i32 = row.get("skip_count");
    let mut running: i32 = row.get("running_count");
    let already_flagged: bool = row.get("cancel_siblings_pending_flag");

    // Any terminal outcome migrates one upstream out of the running
    // bucket — keeping `total = success + fail + skip + running`
    // invariant so the impossibility check works on the remaining
    // headroom.
    if running > 0 {
        running -= 1;
    }
    match outcome {
        OutcomeKind::Success => success += 1,
        OutcomeKind::Fail => fail += 1,
        OutcomeKind::Skip => skip += 1,
    }

    let policy = decode_policy(&policy_raw);
    let total = success + fail + skip + running.max(0);
    let decision = evaluate(&policy, success, fail, skip, total);

    // Writeback the counter state first. (If the decision flips
    // downstream or enqueues sibling cancels, those writes ride the
    // same tx below.)
    sqlx::query(
        r#"
        UPDATE ff_edge_group
           SET success_count = $4,
               fail_count    = $5,
               skip_count    = $6,
               running_count = $7
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3
        "#,
    )
    .bind(partition_key)
    .bind(flow_id)
    .bind(downstream_eid)
    .bind(success)
    .bind(fail)
    .bind(skip)
    .bind(running.max(0))
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let now = now_ms();

    match decision {
        Decision::Pending => { /* counters already persisted */ }
        Decision::Satisfied { cancel_siblings } => {
            // Mark downstream eligible (the scheduler claims it next).
            sqlx::query(
                r#"
                UPDATE ff_exec_core
                   SET eligibility_state = 'eligible_now',
                       lifecycle_phase   = CASE
                           WHEN lifecycle_phase = 'blocked' THEN 'runnable'
                           ELSE lifecycle_phase
                       END
                 WHERE partition_key = $1 AND execution_id = $2
                   AND lifecycle_phase NOT IN ('terminal','cancelled')
                "#,
            )
            .bind(partition_key)
            .bind(downstream_eid)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

            if cancel_siblings && !already_flagged {
                // Stage-C bookkeeping: add one row to
                // `ff_pending_cancel_groups` per still-running sibling
                // group and flip the flag so a replay doesn't
                // double-enqueue.
                sqlx::query(
                    r#"
                    INSERT INTO ff_pending_cancel_groups
                        (partition_key, flow_id, downstream_eid, enqueued_at_ms)
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT DO NOTHING
                    "#,
                )
                .bind(partition_key)
                .bind(flow_id)
                .bind(downstream_eid)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx_error)?;

                sqlx::query(
                    r#"
                    UPDATE ff_edge_group
                       SET cancel_siblings_pending_flag = TRUE
                     WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3
                    "#,
                )
                .bind(partition_key)
                .bind(flow_id)
                .bind(downstream_eid)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx_error)?;
            }
        }
        Decision::Impossible => {
            // Downstream can never satisfy → mark it skipped +
            // cascade by emitting its own completion event. The
            // Wave-5 dispatcher loop picks the new event up on its
            // next poll.
            let updated = sqlx::query(
                r#"
                UPDATE ff_exec_core
                   SET lifecycle_phase   = 'terminal',
                       eligibility_state = 'not_applicable',
                       public_state      = 'skipped',
                       attempt_state     = 'attempt_terminal',
                       terminal_at_ms    = COALESCE(terminal_at_ms, $3)
                 WHERE partition_key = $1 AND execution_id = $2
                   AND lifecycle_phase NOT IN ('terminal','cancelled')
                 RETURNING execution_id
                "#,
            )
            .bind(partition_key)
            .bind(downstream_eid)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;

            if updated.is_some() {
                sqlx::query(
                    r#"
                    INSERT INTO ff_completion_event
                        (partition_key, execution_id, flow_id, outcome, occurred_at_ms)
                    VALUES ($1, $2, $3, 'skipped', $4)
                    "#,
                )
                .bind(partition_key)
                .bind(downstream_eid)
                .bind(flow_id)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(map_sqlx_error)?;
            }
        }
    }

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

/// Decision surface returned by [`evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Policy not yet satisfied and not yet impossible.
    Pending,
    /// Downstream should transition to eligible. `cancel_siblings`
    /// fires the Stage-C bookkeeping write.
    Satisfied { cancel_siblings: bool },
    /// Policy can never be satisfied — skip the downstream.
    Impossible,
}

fn evaluate(
    policy: &EdgeDependencyPolicy,
    success: i32,
    fail: i32,
    skip: i32,
    total: i32,
) -> Decision {
    let nonsuccess = fail + skip;
    match policy {
        EdgeDependencyPolicy::AllOf => {
            if success == total && total > 0 {
                Decision::Satisfied { cancel_siblings: false }
            } else if nonsuccess > 0 {
                // Any upstream non-success under all-of → impossible.
                Decision::Impossible
            } else {
                Decision::Pending
            }
        }
        EdgeDependencyPolicy::AnyOf { on_satisfied } => {
            if success >= 1 {
                Decision::Satisfied {
                    cancel_siblings: matches!(on_satisfied, OnSatisfied::CancelRemaining),
                }
            } else if nonsuccess >= total && total > 0 {
                Decision::Impossible
            } else {
                Decision::Pending
            }
        }
        EdgeDependencyPolicy::Quorum { k, on_satisfied } => {
            let k = *k as i32;
            if success >= k {
                Decision::Satisfied {
                    cancel_siblings: matches!(on_satisfied, OnSatisfied::CancelRemaining),
                }
            } else if total - nonsuccess < k {
                // Remaining upstream headroom cannot reach k.
                Decision::Impossible
            } else {
                Decision::Pending
            }
        }
        // `#[non_exhaustive]` forward-compat: unknown policy variants
        // stay pending (dependency_reconciler eventually unjams).
        _ => Decision::Pending,
    }
}

fn decode_policy(v: &JsonValue) -> EdgeDependencyPolicy {
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("all_of");
    match kind {
        "any_of" => EdgeDependencyPolicy::AnyOf {
            on_satisfied: parse_on_satisfied(v),
        },
        "quorum" => {
            let k = v
                .get("k")
                .and_then(|x| x.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(1);
            EdgeDependencyPolicy::Quorum {
                k,
                on_satisfied: parse_on_satisfied(v),
            }
        }
        _ => EdgeDependencyPolicy::AllOf,
    }
}

fn parse_on_satisfied(v: &JsonValue) -> OnSatisfied {
    match v.get("on_satisfied").and_then(|x| x.as_str()) {
        Some("let_run") => OnSatisfied::LetRun,
        _ => OnSatisfied::CancelRemaining,
    }
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

// ── unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_all_of_satisfied() {
        let d = evaluate(&EdgeDependencyPolicy::AllOf, 3, 0, 0, 3);
        assert_eq!(d, Decision::Satisfied { cancel_siblings: false });
    }

    #[test]
    fn evaluate_all_of_impossible_on_fail() {
        let d = evaluate(&EdgeDependencyPolicy::AllOf, 2, 1, 0, 3);
        assert_eq!(d, Decision::Impossible);
    }

    #[test]
    fn evaluate_all_of_pending() {
        let d = evaluate(&EdgeDependencyPolicy::AllOf, 1, 0, 0, 3);
        assert_eq!(d, Decision::Pending);
    }

    #[test]
    fn evaluate_any_of_cancels_siblings() {
        let d = evaluate(
            &EdgeDependencyPolicy::AnyOf {
                on_satisfied: OnSatisfied::CancelRemaining,
            },
            1, 0, 0, 3,
        );
        assert_eq!(d, Decision::Satisfied { cancel_siblings: true });
    }

    #[test]
    fn evaluate_any_of_let_run() {
        let d = evaluate(
            &EdgeDependencyPolicy::AnyOf {
                on_satisfied: OnSatisfied::LetRun,
            },
            1, 0, 0, 3,
        );
        assert_eq!(d, Decision::Satisfied { cancel_siblings: false });
    }

    #[test]
    fn evaluate_any_of_impossible_when_all_fail() {
        let d = evaluate(
            &EdgeDependencyPolicy::AnyOf {
                on_satisfied: OnSatisfied::CancelRemaining,
            },
            0, 3, 0, 3,
        );
        assert_eq!(d, Decision::Impossible);
    }

    #[test]
    fn evaluate_quorum_satisfied_at_k() {
        let d = evaluate(
            &EdgeDependencyPolicy::Quorum {
                k: 2,
                on_satisfied: OnSatisfied::LetRun,
            },
            2, 0, 1, 3,
        );
        assert_eq!(d, Decision::Satisfied { cancel_siblings: false });
    }

    #[test]
    fn evaluate_quorum_impossible_when_headroom_exhausted() {
        // 5 upstream, k=3, 3 failed → only 2 possibly-success left.
        let d = evaluate(
            &EdgeDependencyPolicy::Quorum {
                k: 3,
                on_satisfied: OnSatisfied::CancelRemaining,
            },
            0, 3, 0, 5,
        );
        assert_eq!(d, Decision::Impossible);
    }

    #[test]
    fn outcome_kind_mapping() {
        assert_eq!(OutcomeKind::from_str("success"), OutcomeKind::Success);
        assert_eq!(OutcomeKind::from_str("failed"), OutcomeKind::Fail);
        assert_eq!(OutcomeKind::from_str("skipped"), OutcomeKind::Skip);
        assert_eq!(OutcomeKind::from_str("cancelled"), OutcomeKind::Fail);
    }
}
