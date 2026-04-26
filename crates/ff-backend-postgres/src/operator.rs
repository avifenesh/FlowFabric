//! RFC-020 Wave 9 Spine-A pt.1 — operator-control mutating methods
//! (`cancel_execution` + `revoke_lease`).
//!
//! Both methods follow the §4.2 shared spine template:
//!
//!   1. `BEGIN ISOLATION LEVEL SERIALIZABLE`.
//!   2. `SELECT ... FOR UPDATE` on `ff_exec_core` (+ `ff_attempt`
//!      for the current-attempt row) — captures pre-state and
//!      acquires row locks against the `lease_expiry` /
//!      `attempt_timeout` reconcilers (§4.2.6).
//!   3. Validate against Valkey-canonical semantics (§5.2).
//!   4. Mutate with compare-and-set fencing (lease_epoch CAS on
//!      `revoke_lease`; lifecycle_phase check on `cancel_execution`).
//!   5. Emit the outbox row per §4.2.7 matrix (`ff_lease_event`).
//!   6. `COMMIT`. On `40001` / `40P01` (serialization / deadlock)
//!      retry up to `CANCEL_FLOW_MAX_ATTEMPTS = 3`; exhaustion
//!      surfaces `ContentionKind::RetryExhausted` to the caller.
//!
//! Per §4.2.6: existing reconcilers (`lease_expiry`, `attempt_timeout`)
//! filter on `lifecycle_phase = 'active'`, so a `'cancelled'` row
//! surfaced by `cancel_execution` is silently skipped — no new
//! reconciler modifications required.

use std::time::Duration;

use ff_core::contracts::{
    CancelExecutionArgs, CancelExecutionResult, CancelFlowArgs, CancelFlowHeader,
    ChangePriorityArgs, ChangePriorityResult, ReplayExecutionArgs, ReplayExecutionResult,
    RevokeLeaseArgs, RevokeLeaseResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::state::PublicState;
use ff_core::types::{CancelSource, ExecutionId, FlowId};
use serde_json::json;
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;
use crate::{lease_event, operator_event};

/// Max attempts on `40001` / `40P01`. Matches
/// [`crate::flow::CANCEL_FLOW_MAX_ATTEMPTS`] (RFC §4.2).
const MAX_ATTEMPTS: u32 = 3;

/// Extract the raw UUID suffix from an `ExecutionId`'s wire form
/// (`"{fp:N}:<uuid>"`). Mirrors [`crate::exec_core::eid_uuid`].
fn eid_uuid(eid: &ff_core::types::ExecutionId) -> Uuid {
    let s = eid.as_str();
    let suffix = s
        .split_once("}:")
        .map(|(_, u)| u)
        .expect("ExecutionId has `}:` separator (invariant)");
    Uuid::parse_str(suffix).expect("ExecutionId suffix is a valid UUID (invariant)")
}

fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after UNIX_EPOCH");
    (d.as_millis() as i64).max(0)
}

/// Map a `40001` / `40P01` sqlx conflict into the retry loop. Other
/// `Contention` variants propagate untouched (mirrors
/// `flow::is_serialization_conflict`).
fn is_serialization_conflict(err: &EngineError) -> bool {
    matches!(err, EngineError::Contention(ContentionKind::LeaseConflict))
}

async fn begin_serializable(pool: &PgPool) -> Result<sqlx::Transaction<'_, Postgres>, EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    Ok(tx)
}

/// Synthetic Postgres lease identity (the backend has no stable
/// `lease_id` column — §4.2.2 derives identity from
/// `(execution_id, attempt_index, lease_epoch)`). Surfaces on
/// [`RevokeLeaseResult::Revoked`] so callers have a stable string to
/// round-trip through logs / traces.
fn synthetic_lease_id(exec_uuid: Uuid, attempt_index: i32, lease_epoch: i64) -> String {
    format!("pg:{exec_uuid}:{attempt_index}:{lease_epoch}")
}

// ─── cancel_execution (§4.2.1) ────────────────────────────────────

/// Transactional body for [`cancel_execution_impl`].
///
/// Returns `Ok(result)` on success (one tx); `Err(e)` for
/// serialization retries (40001/40P01 → re-enters the outer loop) and
/// hard failures (NotFound, StaleLease, Validation on terminal
/// conflict).
async fn cancel_execution_once(
    pool: &PgPool,
    args: &CancelExecutionArgs,
) -> Result<CancelExecutionResult, EngineError> {
    let partition_key: i16 = args.execution_id.partition() as i16;
    let exec_uuid = eid_uuid(&args.execution_id);
    // Honour caller-supplied `args.now` for `terminal_at_ms` +
    // `raw_fields.last_mutation_at` + outbox `occurred_at_ms` so
    // retries + cross-backend comparisons don't drift on the DB host
    // clock.
    let now: i64 = args.now.0;

    let mut tx = begin_serializable(pool).await?;

    // Step 2 + 3 — pre-read under lock. Join on the current attempt
    // row to read `worker_instance_id` + `lease_epoch` (drives the
    // lease-active decision + CAS fencing).
    let row = sqlx::query(
        r#"
        SELECT ec.lifecycle_phase,
               ec.public_state,
               ec.attempt_index,
               a.worker_instance_id,
               a.lease_epoch
          FROM ff_exec_core ec
          LEFT JOIN ff_attempt a
            ON a.partition_key = ec.partition_key
           AND a.execution_id  = ec.execution_id
           AND a.attempt_index = ec.attempt_index
         WHERE ec.partition_key = $1 AND ec.execution_id = $2
         FOR NO KEY UPDATE OF ec
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        // Valkey path returns `execution_not_found` → NotFound
        // (`ScriptError::ExecutionNotFound`). Mirror that here.
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };

    let lifecycle_phase: String = row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let public_state: String = row.try_get("public_state").map_err(map_sqlx_error)?;
    let attempt_index: i32 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let worker_instance_id: Option<String> =
        row.try_get("worker_instance_id").map_err(map_sqlx_error)?;
    let lease_epoch: Option<i64> = row.try_get("lease_epoch").map_err(map_sqlx_error)?;

    // Terminal-state handling — idempotent replay if already
    // cancelled; hard conflict if terminal with a different outcome.
    // Mirrors `exec_core::cancel_impl` to keep operator-visible
    // semantics stable.
    if matches!(lifecycle_phase.as_str(), "terminal" | "cancelled") {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return if public_state == "cancelled" {
            Ok(CancelExecutionResult::Cancelled {
                execution_id: args.execution_id.clone(),
                public_state: PublicState::Cancelled,
            })
        } else {
            Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!(
                    "cancel_execution: execution_id={}: already terminal in state '{}'",
                    args.execution_id, public_state
                ),
            })
        };
    }

    // Lease fence validation (Valkey parity — only enforced when
    // `source != CancelSource::OperatorOverride`). Per
    // `CancelExecutionArgs` docstring, `lease_epoch` is REQUIRED
    // when source is not operator_override AND the execution is
    // active; missing input on that path surfaces a `Validation`
    // error rather than silently bypassing. The Valkey side keys
    // fence on `current_lease_id` + `current_lease_epoch`; Postgres
    // has no stable `lease_id` column, so we fence on `lease_epoch`.
    // `lease_id` in args is ignored on Postgres (documented parity
    // gap — matches the `lease_event::emit` docs for `lease_id = None`).
    let lease_active = worker_instance_id
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if !matches!(args.source, CancelSource::OperatorOverride) && lease_active {
        let Some(expected_epoch) = args.lease_epoch.as_ref() else {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!(
                    "cancel_execution: execution_id={}: lease_epoch required when source != operator_override and execution is active",
                    args.execution_id
                ),
            });
        };
        let expected: i64 = i64::try_from(expected_epoch.0).unwrap_or(i64::MAX);
        if lease_epoch.unwrap_or(0) != expected {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Err(EngineError::State(
                ff_core::engine_error::StateKind::StaleLease,
            ));
        }
    }

    // Step 4a — flip exec_core to cancelled. Matches the existing
    // `flow::cancel_flow` member-cancel shape (flow.rs:672) so the
    // terminal lifecycle literal stays `'cancelled'` (not `'terminal'`
    // + `terminal_outcome='cancelled'` — the Valkey encoding). Per
    // RFC §4.2.1 this is the Postgres-canonical encoding.
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase     = 'cancelled',
               ownership_state     = 'unowned',
               eligibility_state   = 'not_applicable',
               public_state        = 'cancelled',
               attempt_state       = 'cancelled',
               terminal_at_ms      = COALESCE(terminal_at_ms, $3),
               cancellation_reason = COALESCE(cancellation_reason, $4),
               cancelled_by        = COALESCE(cancelled_by, $5),
               raw_fields          = jsonb_set(raw_fields,
                                               '{last_mutation_at}',
                                               to_jsonb($3::text))
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(now)
    .bind(&args.reason)
    .bind(args.source.as_str())
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Step 4b — clear lease on the current attempt row if one was
    // active. Zeroes `worker_instance_id` + `lease_expires_at_ms` so
    // the `lease_expiry` reconciler + `claim_from_reclaim` path see a
    // released slot. `lease_epoch` bumps by 1 to fence any in-flight
    // RMW (same pattern as `revoke_lease`).
    if lease_active {
        sqlx::query(
            r#"
            UPDATE ff_attempt
               SET worker_instance_id   = NULL,
                   lease_expires_at_ms  = NULL,
                   lease_epoch          = lease_epoch + 1,
                   terminal_at_ms       = $4,
                   outcome              = 'cancelled'
             WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
            "#,
        )
        .bind(partition_key)
        .bind(exec_uuid)
        .bind(attempt_index)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Step 5 — outbox emit (§4.2.7). Only when a lease was
        // active; an already-unowned exec generates no lease event
        // (matches Valkey's `ff_cancel_execution` which XADDs a
        // `released` history event only on the active path).
        lease_event::emit(
            &mut tx,
            partition_key,
            exec_uuid,
            None, // Postgres has no stable lease_id — see lease_event::emit docs
            lease_event::EVENT_REVOKED,
            now,
        )
        .await?;
    }

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(CancelExecutionResult::Cancelled {
        execution_id: args.execution_id.clone(),
        public_state: PublicState::Cancelled,
    })
}

/// `EngineBackend::cancel_execution` impl — SERIALIZABLE + 3-attempt
/// retry loop on 40001 / 40P01.
pub(super) async fn cancel_execution_impl(
    pool: &PgPool,
    args: CancelExecutionArgs,
) -> Result<CancelExecutionResult, EngineError> {
    let mut last: Option<EngineError> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match cancel_execution_once(pool, &args).await {
            Ok(r) => return Ok(r),
            Err(err) => {
                if is_serialization_conflict(&err) {
                    if attempt + 1 < MAX_ATTEMPTS {
                        let ms = 5u64 * (1u64 << attempt);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    last = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    let _ = last;
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

// ─── revoke_lease (§4.2.2) ────────────────────────────────────────

async fn revoke_lease_once(
    pool: &PgPool,
    args: &RevokeLeaseArgs,
) -> Result<RevokeLeaseResult, EngineError> {
    let partition_key: i16 = args.execution_id.partition() as i16;
    let exec_uuid = eid_uuid(&args.execution_id);
    let now = now_ms();

    let mut tx = begin_serializable(pool).await?;

    // Pre-read: `ff_exec_core` under UPDATE lock to pin the current
    // attempt_index (defends against a concurrent `replay_execution`
    // that would bump it). `FOR NO KEY UPDATE` is unsupported on
    // nullable outer-join sides (pg 0A000), so we lock `ec` solo and
    // re-read the attempt row separately under its own `FOR UPDATE`.
    let ec_row = sqlx::query(
        r#"
        SELECT attempt_index
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR NO KEY UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(ec_row) = ec_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };
    let attempt_index: i32 = ec_row.try_get("attempt_index").map_err(map_sqlx_error)?;

    // Attempt row may not exist yet (pre-claim exec). Lock with
    // FOR UPDATE so the `lease_expiry` reconciler serializes with us.
    let att_row = sqlx::query(
        r#"
        SELECT worker_instance_id, lease_epoch
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
         FOR UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let (worker_instance_id, lease_epoch): (Option<String>, Option<i64>) = match att_row {
        Some(r) => (
            r.try_get("worker_instance_id").map_err(map_sqlx_error)?,
            r.try_get("lease_epoch").map_err(map_sqlx_error)?,
        ),
        None => (None, None),
    };

    let lease_active = worker_instance_id
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if !lease_active {
        // Valkey parity — `RevokeLeaseResult::AlreadySatisfied` when
        // the exec has no active lease (§4.2.2 +
        // `valkey/lib.rs:5881-5892`).
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(RevokeLeaseResult::AlreadySatisfied {
            reason: "no_active_lease".to_owned(),
        });
    }

    // Targeted-revoke fence: the caller identifies a specific owner
    // via `args.worker_instance_id`. If the locked attempt is held
    // by a different worker, surface `AlreadySatisfied` (idempotent
    // parity — the targeted lease is already gone, from this
    // caller's perspective). An empty caller-supplied wiid is a
    // wildcard (matches Valkey's `revoke_lease: HGET
    // current_worker_instance_id` fallback at `lib.rs:5869-5899`).
    let caller_wiid = args.worker_instance_id.as_str();
    if !caller_wiid.is_empty()
        && worker_instance_id.as_deref() != Some(caller_wiid)
    {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(RevokeLeaseResult::AlreadySatisfied {
            reason: "different_worker_instance_id".to_owned(),
        });
    }

    let prior_epoch = lease_epoch.unwrap_or(0);

    // Optional `expected_lease_id` fence. Valkey uses a stable
    // lease_id; Postgres synthesises one from
    // `(execution_id, attempt_index, lease_epoch)`. Empty string is
    // the documented "skip check" path on the args docstring.
    if let Some(expected) = args
        .expected_lease_id
        .as_ref()
        .filter(|s| !s.is_empty())
    {
        let current_id = synthetic_lease_id(exec_uuid, attempt_index, prior_epoch);
        if expected != &current_id {
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(RevokeLeaseResult::AlreadySatisfied {
                reason: "lease_id_mismatch".to_owned(),
            });
        }
    }

    // CAS on `lease_epoch`. Row-count = 0 → another writer bumped
    // the epoch (reconciler or concurrent revoke) → `AlreadySatisfied`
    // (§4.2.2 `reason: "epoch_moved"`).
    let affected = sqlx::query(
        r#"
        UPDATE ff_attempt
           SET worker_instance_id   = NULL,
               lease_expires_at_ms  = NULL,
               lease_epoch          = lease_epoch + 1
         WHERE partition_key = $1
           AND execution_id  = $2
           AND attempt_index = $3
           AND lease_epoch   = $4
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(attempt_index)
    .bind(prior_epoch)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?
    .rows_affected();

    if affected == 0 {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(RevokeLeaseResult::AlreadySatisfied {
            reason: "epoch_moved".to_owned(),
        });
    }

    // Flip `ff_exec_core` back to reclaimable-runnable so the normal
    // claim path picks it up — mirrors
    // `reconcilers::lease_expiry::release_one` (lease_expiry.rs:156-173).
    // Gated on `lifecycle_phase = 'active'` to avoid touching a row
    // that's been concurrently terminated (cancelled / completed).
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase   = 'runnable',
               ownership_state   = 'unowned',
               eligibility_state = 'eligible_now',
               attempt_state     = 'attempt_interrupted',
               raw_fields        = jsonb_set(raw_fields,
                                             '{last_mutation_at}',
                                             to_jsonb($3::text))
         WHERE partition_key = $1 AND execution_id = $2
           AND lifecycle_phase = 'active'
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Step 5 — outbox emit (§4.2.7 — `ff_lease_event event_type=revoked`).
    lease_event::emit(
        &mut tx,
        partition_key,
        exec_uuid,
        None,
        lease_event::EVENT_REVOKED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(RevokeLeaseResult::Revoked {
        lease_id: synthetic_lease_id(exec_uuid, attempt_index, prior_epoch),
        lease_epoch: (prior_epoch + 1).to_string(),
    })
}

/// `EngineBackend::revoke_lease` impl — SERIALIZABLE + 3-attempt
/// retry loop on 40001 / 40P01.
pub(super) async fn revoke_lease_impl(
    pool: &PgPool,
    args: RevokeLeaseArgs,
) -> Result<RevokeLeaseResult, EngineError> {
    let mut last: Option<EngineError> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match revoke_lease_once(pool, &args).await {
            Ok(r) => return Ok(r),
            Err(err) => {
                if is_serialization_conflict(&err) {
                    if attempt + 1 < MAX_ATTEMPTS {
                        let ms = 5u64 * (1u64 << attempt);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    last = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    let _ = last;
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

// ─── change_priority (§4.2.4, Rev 7 Fork 3) ────────────────────────

/// Shared retry wrapper — SERIALIZABLE + 3-attempt retry on 40001 /
/// 40P01 (§4.2 template).
async fn retry_serializable<F, Fut, T>(mut f: F) -> Result<T, EngineError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, EngineError>>,
{
    let mut last: Option<EngineError> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match f().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if is_serialization_conflict(&err) {
                    if attempt + 1 < MAX_ATTEMPTS {
                        let ms = 5u64 * (1u64 << attempt);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    last = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    let _ = last;
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

async fn change_priority_once(
    pool: &PgPool,
    args: &ChangePriorityArgs,
) -> Result<ChangePriorityResult, EngineError> {
    let partition_key: i16 = args.execution_id.partition() as i16;
    let exec_uuid = eid_uuid(&args.execution_id);
    let now: i64 = args.now.0;

    let mut tx = begin_serializable(pool).await?;

    // Pre-read under NO KEY UPDATE lock. Gate fields mirror the Valkey
    // Lua check (`flowfabric.lua:3683-3688`): lifecycle_phase = 'runnable'
    // AND eligibility_state = 'eligible_now' — any other state surfaces
    // `execution_not_eligible` (Rev 7 Fork 3, Option C).
    let row = sqlx::query(
        r#"
        SELECT lifecycle_phase, eligibility_state, priority
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR NO KEY UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };

    let lifecycle_phase: String = row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let eligibility_state: String = row.try_get("eligibility_state").map_err(map_sqlx_error)?;
    let old_priority: i32 = row.try_get("priority").map_err(map_sqlx_error)?;

    // Valkey-canonical gate (`flowfabric.lua:3683-3688`). Both fails-
    // closed with `execution_not_eligible`.
    if lifecycle_phase != "runnable" || eligibility_state != "eligible_now" {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::Contention(
            ContentionKind::ExecutionNotEligible,
        ));
    }

    // Clamp new_priority to [0, 9000] matching Valkey's
    // `ff_change_priority` (flowfabric.lua:3695-3696 — "same as
    // ff_create_execution").
    let new_priority = args.new_priority.clamp(0, 9000);

    // Mutate. Repeat the gate in the WHERE clause as belt-and-
    // suspenders; row-count = 0 on concurrent transition between
    // the pre-read and UPDATE surfaces the same `execution_not_eligible`
    // error.
    let affected = sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET priority   = $3,
               raw_fields = jsonb_set(raw_fields,
                                      '{last_mutation_at}',
                                      to_jsonb($4::text))
         WHERE partition_key = $1 AND execution_id = $2
           AND lifecycle_phase   = 'runnable'
           AND eligibility_state = 'eligible_now'
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(new_priority)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?
    .rows_affected();

    if affected == 0 {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::Contention(
            ContentionKind::ExecutionNotEligible,
        ));
    }

    // Outbox emit (§4.2.7 — ff_operator_event event_type='priority_changed').
    operator_event::emit(
        &mut tx,
        partition_key,
        exec_uuid,
        operator_event::EVENT_PRIORITY_CHANGED,
        json!({
            "old_priority": old_priority,
            "new_priority": new_priority,
        }),
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(ChangePriorityResult::Changed {
        execution_id: args.execution_id.clone(),
    })
}

pub(super) async fn change_priority_impl(
    pool: &PgPool,
    args: ChangePriorityArgs,
) -> Result<ChangePriorityResult, EngineError> {
    retry_serializable(|| change_priority_once(pool, &args)).await
}

// ─── replay_execution (§4.2.5, Rev 7 Forks 1 + 2) ──────────────────

async fn replay_execution_once(
    pool: &PgPool,
    args: &ReplayExecutionArgs,
) -> Result<ReplayExecutionResult, EngineError> {
    let partition_key: i16 = args.execution_id.partition() as i16;
    let exec_uuid = eid_uuid(&args.execution_id);
    let now: i64 = args.now.0;

    let mut tx = begin_serializable(pool).await?;

    // Pre-read under lock. Read the current attempt's `outcome` so we
    // can derive `terminal_outcome` per `exec_core::derive_terminal_outcome`.
    let ec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, flow_id, attempt_index, priority, raw_fields
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR NO KEY UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(ec_row) = ec_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };

    let lifecycle_phase: String = ec_row
        .try_get("lifecycle_phase")
        .map_err(map_sqlx_error)?;
    let flow_id: Option<Uuid> = ec_row.try_get("flow_id").map_err(map_sqlx_error)?;
    let attempt_index: i32 = ec_row.try_get("attempt_index").map_err(map_sqlx_error)?;

    // Valkey-canonical gate: `lifecycle_phase = 'terminal'`
    // (`flowfabric.lua:8535-8537` → `err("execution_not_terminal")`).
    if lifecycle_phase != "terminal" {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::State(StateKind::ExecutionNotTerminal));
    }

    // Read the current attempt's outcome for the skipped-flow-member
    // branch check.
    let att_row = sqlx::query(
        r#"
        SELECT outcome
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
         FOR UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let attempt_outcome: Option<String> = match att_row.as_ref() {
        Some(r) => r.try_get("outcome").map_err(map_sqlx_error)?,
        None => None,
    };

    // Branch selector matches Valkey (`flowfabric.lua:8555`).
    // Postgres `terminal_outcome` is derived, not stored; mirror
    // `exec_core::derive_terminal_outcome` at the SQL level.
    let is_skipped_flow_member =
        attempt_outcome.as_deref() == Some("skipped") && flow_id.is_some();

    // Skipped-flow-member branch (Rev 7 Fork 1 Option A).
    // Reset downstream edge-group counters: skip/fail/running → 0,
    // success preserved. Valkey ground-truth:
    // `flowfabric.lua:8580` comment "satisfied edges remain satisfied".
    let groups_reset: i64 = if is_skipped_flow_member {
        let count = sqlx::query(
            r#"
            UPDATE ff_edge_group
               SET skip_count    = 0,
                   fail_count    = 0,
                   running_count = 0
             WHERE (partition_key, flow_id, downstream_eid) IN (
               SELECT DISTINCT e.partition_key, e.flow_id, e.downstream_eid
                 FROM ff_edge e
                WHERE e.partition_key   = $1
                  AND e.downstream_eid = $2
             )
            "#,
        )
        .bind(partition_key)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        count as i64
    } else {
        0
    };

    // Both branches: in-place mutate `ff_exec_core` to
    // `lifecycle_phase='runnable'` — matches Valkey's base+skip
    // both writing `runnable` (flowfabric.lua:8591 + 8625).
    // `attempt_index` NOT bumped (Rev 7 Fork 2 Option A).
    //
    // Secondary state differs per branch per §4.2.5:
    //  - normal:  eligibility_state='eligible_now', public_state='waiting'
    //  - skipped: eligibility_state='blocked_by_dependencies',
    //             public_state='waiting_children'
    //
    // raw_fields.replay_count bumped (+1). Valkey bumps
    // exec_core.replay_count; Postgres stores it in raw_fields.
    let (eligibility_state, public_state) = if is_skipped_flow_member {
        ("blocked_by_dependencies", "waiting_children")
    } else {
        ("eligible_now", "waiting")
    };

    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase      = 'runnable',
               ownership_state      = 'unowned',
               eligibility_state    = $3,
               public_state         = $4,
               attempt_state        = 'pending_replay_attempt',
               terminal_at_ms       = NULL,
               result               = NULL,
               cancellation_reason  = NULL,
               cancelled_by         = NULL,
               raw_fields           = jsonb_set(
                 jsonb_set(raw_fields, '{last_mutation_at}', to_jsonb($5::text)),
                 '{replay_count}',
                 to_jsonb(COALESCE((raw_fields->>'replay_count')::int, 0) + 1)
               )
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(exec_uuid)
    .bind(eligibility_state)
    .bind(public_state)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // In-place mutate the current `ff_attempt` row (Rev 7 Fork 2
    // Option A). Reset outcome + lease state; bump lease_epoch to
    // fence any in-flight RMW. No new attempt row inserted; no
    // historical row retained. Only columns that exist on the real
    // schema are touched (`ff_attempt` has no `lease_id` / `result` /
    // `error_code` columns — see migrations/0001_initial.sql:160-176).
    if att_row.is_some() {
        sqlx::query(
            r#"
            UPDATE ff_attempt
               SET outcome              = NULL,
                   terminal_at_ms       = NULL,
                   worker_id            = NULL,
                   worker_instance_id   = NULL,
                   lease_expires_at_ms  = NULL,
                   lease_epoch          = lease_epoch + 1
             WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
            "#,
        )
        .bind(partition_key)
        .bind(exec_uuid)
        .bind(attempt_index)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    // Outbox emit (§4.2.7 — ff_operator_event event_type='replayed').
    let details = if is_skipped_flow_member {
        json!({
            "branch": "skipped_flow_member",
            "groups_reset": groups_reset,
        })
    } else {
        json!({
            "branch": "normal",
        })
    };
    operator_event::emit(
        &mut tx,
        partition_key,
        exec_uuid,
        operator_event::EVENT_REPLAYED,
        details,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let ps = if is_skipped_flow_member {
        PublicState::WaitingChildren
    } else {
        PublicState::Waiting
    };
    Ok(ReplayExecutionResult::Replayed { public_state: ps })
}

pub(super) async fn replay_execution_impl(
    pool: &PgPool,
    args: ReplayExecutionArgs,
) -> Result<ReplayExecutionResult, EngineError> {
    retry_serializable(|| replay_execution_once(pool, &args)).await
}

// ─── cancel_flow_header (§4.2.3) ───────────────────────────────────

/// Format a member execution UUID as the wire-form `ExecutionId`
/// string (`{fp:N}:<uuid>`) expected by downstream consumers.
fn member_wire_id(partition_key: i16, exec_uuid: Uuid) -> String {
    format!("{{fp:{partition_key}}}:{exec_uuid}")
}

async fn cancel_flow_header_once(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    args: &CancelFlowArgs,
) -> Result<CancelFlowHeader, EngineError> {
    let flow_uuid: Uuid = args.flow_id.0;
    // Flow + members share `partition_key` via the flow's partition
    // hash (RFC-011 co-location). Use the backend's configured
    // `partition_config` so non-default `num_flow_partitions`
    // deployments route correctly.
    let partition_key: i16 =
        ff_core::partition::flow_partition(&args.flow_id, partition_config).index as i16;
    let now: i64 = args.now.0;

    let mut tx = begin_serializable(pool).await?;

    // Pre-read the flow row under lock. Idempotent-replay trigger is
    // `public_flow_state` — flows already in a terminal state surface
    // `AlreadyTerminal` with stored policy/reason from `raw_fields` +
    // either the prior backlog-member enumeration (if we landed the
    // first flip) or live `ff_exec_core` membership (pre-E2-shape
    // flow cancelled by the legacy `flow::cancel_flow` path).
    let flow_row = sqlx::query(
        r#"
        SELECT public_flow_state, raw_fields
          FROM ff_flow_core
         WHERE partition_key = $1 AND flow_id = $2
         FOR NO KEY UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(flow_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(flow_row) = flow_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::NotFound { entity: "flow" });
    };

    let public_flow_state: String = flow_row
        .try_get("public_flow_state")
        .map_err(map_sqlx_error)?;
    let raw_fields: serde_json::Value = flow_row.try_get("raw_fields").map_err(map_sqlx_error)?;

    // Idempotent-replay path — flow already in a terminal state. Read
    // stored policy / reason (from raw_fields — `flow::cancel_flow`
    // writes `cancellation_policy` there) + the backlog-member rows
    // (our prior enumeration).
    if matches!(public_flow_state.as_str(), "cancelled" | "completed" | "failed") {
        let stored_cancellation_policy = raw_fields
            .get("cancellation_policy")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let stored_cancel_reason = raw_fields
            .get("cancel_reason")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        // Return the enumerated members from the backlog if we have
        // one; otherwise fall back to live `ff_exec_core` membership
        // (matches Valkey's SMEMBERS pattern on flow_members_set).
        let member_rows = sqlx::query(
            r#"
            SELECT execution_id
              FROM ff_cancel_backlog_member
             WHERE partition_key = $1 AND flow_id = $2
            "#,
        )
        .bind(partition_key)
        .bind(flow_uuid)
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let members: Vec<String> = if member_rows.is_empty() {
            // Pre-E2-shape flow: enumerate live members from exec_core.
            let live = sqlx::query(
                r#"
                SELECT execution_id
                  FROM ff_exec_core
                 WHERE partition_key = $1 AND flow_id = $2
                "#,
            )
            .bind(partition_key)
            .bind(flow_uuid)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
            live.iter()
                .map(|r| {
                    let u: Uuid = r.get("execution_id");
                    member_wire_id(partition_key, u)
                })
                .collect()
        } else {
            member_rows
                .iter()
                .map(|r| r.get::<String, _>("execution_id"))
                .collect()
        };

        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(CancelFlowHeader::AlreadyTerminal {
            stored_cancellation_policy,
            stored_cancel_reason,
            member_execution_ids: members,
        });
    }

    // Fresh cancel — flip flow_core + insert backlog header + enumerate
    // + bulk-insert backlog members + flip exec_core lifecycle_phase
    // per member (matches `flow::cancel_flow_once` pattern).

    sqlx::query(
        r#"
        UPDATE ff_flow_core
           SET public_flow_state = 'cancelled',
               terminal_at_ms    = COALESCE(terminal_at_ms, $3),
               raw_fields        = raw_fields
                                    || jsonb_build_object(
                                         'cancellation_policy', $4::text,
                                         'cancel_reason',       $5::text)
         WHERE partition_key = $1 AND flow_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(flow_uuid)
    .bind(now)
    .bind(&args.cancellation_policy)
    .bind(&args.reason)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        INSERT INTO ff_cancel_backlog
            (partition_key, flow_id, requested_at_ms, requester, reason,
             cancellation_policy, status)
        VALUES ($1, $2, $3, '', $4, $5, 'pending')
        ON CONFLICT (partition_key, flow_id) DO NOTHING
        "#,
    )
    .bind(partition_key)
    .bind(flow_uuid)
    .bind(now)
    .bind(&args.reason)
    .bind(&args.cancellation_policy)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Enumerate in-flight members.
    let member_rows = sqlx::query(
        r#"
        SELECT execution_id
          FROM ff_exec_core
         WHERE partition_key = $1 AND flow_id = $2
           AND lifecycle_phase NOT IN ('terminal','cancelled')
        "#,
    )
    .bind(partition_key)
    .bind(flow_uuid)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let member_uuids: Vec<Uuid> = member_rows.iter().map(|r| r.get("execution_id")).collect();
    let member_execution_ids: Vec<String> = member_uuids
        .iter()
        .map(|u| member_wire_id(partition_key, *u))
        .collect();

    if !member_uuids.is_empty() {
        // Bulk INSERT backlog members via UNNEST — one round-trip
        // regardless of membership cardinality (vs. N round-trips in
        // the prior per-member loop).
        sqlx::query(
            r#"
            INSERT INTO ff_cancel_backlog_member
                (partition_key, flow_id, execution_id)
            SELECT $1, $2, eid
              FROM UNNEST($3::text[]) AS eid
            ON CONFLICT (partition_key, flow_id, execution_id) DO NOTHING
            "#,
        )
        .bind(partition_key)
        .bind(flow_uuid)
        .bind(&member_execution_ids)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Bulk UPDATE each member's `ff_exec_core` lifecycle (same
        // shape as `flow::cancel_flow_once`). The Server's own
        // wait/async machinery drives per-member cancel events
        // downstream; the backlog rows live until `ack_cancel_member`
        // drains them.
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase     = 'cancelled',
                   eligibility_state   = 'cancelled',
                   public_state        = 'cancelled',
                   terminal_at_ms      = COALESCE(terminal_at_ms, $3),
                   cancellation_reason = COALESCE(cancellation_reason, $4),
                   cancelled_by        = COALESCE(cancelled_by, 'cancel_flow_header')
             WHERE partition_key = $1 AND execution_id = ANY($2::uuid[])
            "#,
        )
        .bind(partition_key)
        .bind(&member_uuids)
        .bind(now)
        .bind(&args.reason)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    // Outbox emit (§4.2.7 — flow-level `flow_cancel_requested`).
    operator_event::emit(
        &mut tx,
        partition_key,
        flow_uuid,
        operator_event::EVENT_FLOW_CANCEL_REQUESTED,
        json!({
            "flow_id": flow_uuid.to_string(),
            "cancellation_policy": &args.cancellation_policy,
            "reason": &args.reason,
            "member_count": member_execution_ids.len(),
        }),
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(CancelFlowHeader::Cancelled {
        cancellation_policy: args.cancellation_policy.clone(),
        member_execution_ids,
    })
}

pub(super) async fn cancel_flow_header_impl(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    args: CancelFlowArgs,
) -> Result<CancelFlowHeader, EngineError> {
    retry_serializable(|| cancel_flow_header_once(pool, partition_config, &args)).await
}

// ─── ack_cancel_member (§4.2.3) ────────────────────────────────────

async fn ack_cancel_member_once(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    flow_id: &FlowId,
    execution_id: &ExecutionId,
) -> Result<(), EngineError> {
    let flow_uuid: Uuid = flow_id.0;
    let partition_key: i16 =
        ff_core::partition::flow_partition(flow_id, partition_config).index as i16;
    let member_wire = execution_id.as_str();

    let mut tx = begin_serializable(pool).await?;

    // §4.2.3 — member-drain + conditional parent-DELETE. Under
    // SERIALIZABLE, concurrent ack × ack race: both TXs read identical
    // snapshots of `ff_cancel_backlog_member`; the losing TX gets
    // 40001 at COMMIT and is retried by `retry_serializable` — the
    // retry observes the winner's state and the member-DELETE becomes
    // a no-op (0 rows), the parent-DELETE predicate re-evaluates
    // against post-winner state.
    //
    // Implementation note: Postgres data-modifying CTEs share a
    // snapshot across all statements in the WITH clause — a CTE-form
    // `WITH deleted_member AS (DELETE ...) DELETE FROM parent WHERE
    // NOT EXISTS (...)` leaves the parent behind on the last-member
    // drain because the outer DELETE's `NOT EXISTS` still sees the
    // about-to-be-deleted row in the snapshot. Two statements in the
    // same tx observe the prior statement's writes — SERIALIZABLE
    // isolation still holds against concurrent acks.
    //
    // NO outbox emit (§4.2.7 — Valkey-parity quiet).
    sqlx::query(
        r#"
        DELETE FROM ff_cancel_backlog_member
         WHERE partition_key = $1
           AND flow_id       = $2
           AND execution_id  = $3
        "#,
    )
    .bind(partition_key)
    .bind(flow_uuid)
    .bind(member_wire)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        DELETE FROM ff_cancel_backlog
         WHERE partition_key = $1
           AND flow_id       = $2
           AND NOT EXISTS (
             SELECT 1 FROM ff_cancel_backlog_member
              WHERE partition_key = $1 AND flow_id = $2
           )
        "#,
    )
    .bind(partition_key)
    .bind(flow_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

pub(super) async fn ack_cancel_member_impl(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    flow_id: FlowId,
    execution_id: ExecutionId,
) -> Result<(), EngineError> {
    retry_serializable(|| {
        ack_cancel_member_once(pool, partition_config, &flow_id, &execution_id)
    })
    .await
}
