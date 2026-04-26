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
    CancelExecutionArgs, CancelExecutionResult, RevokeLeaseArgs, RevokeLeaseResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, ValidationKind};
use ff_core::state::PublicState;
use ff_core::types::CancelSource;
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;
use crate::lease_event;

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
/// Returns `Ok(Some(result))` on success, `Ok(None)` never (the
/// function always yields a concrete outcome), `Err(e)` for
/// serialization retries + hard failures.
async fn cancel_execution_once(
    pool: &PgPool,
    args: &CancelExecutionArgs,
) -> Result<CancelExecutionResult, EngineError> {
    let partition_key: i16 = args.execution_id.partition() as i16;
    let exec_uuid = eid_uuid(&args.execution_id);
    let now = now_ms();

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
    // source != operator_override). The Valkey side keys fence on
    // `current_lease_id` + `current_lease_epoch`; Postgres has no
    // stable `lease_id` column, so we fence on `lease_epoch` only
    // when the caller supplied one. `LeaseId` fencing is a no-op on
    // Postgres (documented in `operator.rs` + matches the
    // `lease_event::emit` docs for `lease_id = None`).
    let lease_active = worker_instance_id
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if !matches!(args.source, CancelSource::OperatorOverride)
        && lease_active
        && let Some(expected_epoch) = args.lease_epoch.as_ref()
    {
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
                   terminal_at_ms       = COALESCE(terminal_at_ms, $4),
                   outcome              = COALESCE(outcome, 'cancelled')
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

    let prior_epoch = lease_epoch.unwrap_or(0);

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
