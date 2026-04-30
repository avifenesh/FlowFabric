//! PR-7b / #33: typed-FCALL EngineBackend bodies on SQLite.
//!
//! SQLite peers of the Postgres bodies at
//! `ff-backend-postgres/src/typed_ops.rs`. Each function mirrors the
//! PG shape modulo dialect: `FOR UPDATE` elided (implicit under
//! `BEGIN IMMEDIATE` RESERVED lock, RFC-023 §4.1 A3 single-writer),
//! `raw_fields->>'k'` → `json_extract(raw_fields, '$.k')`, PG
//! `jsonb || patch` → SQLite `json_set(raw_fields, '$.k', v)` chain,
//! `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` dropped
//! (`BEGIN IMMEDIATE` + single-writer is implicitly serialisable).
//!
//! # Transaction shape
//!
//! Each op:
//!
//! 1. Acquires a `BEGIN IMMEDIATE` connection via [`begin_immediate`]
//!    — writer lock, RESERVED level.
//! 2. Performs the op under that lock.
//! 3. Commits via [`commit_or_rollback`] (or [`rollback_quiet`] on
//!    error).
//! 4. The `EngineBackend` trait wrapper at [`SqliteBackend`] calls
//!    [`retry_serializable`](crate::retry::retry_serializable) around
//!    the impl fn to absorb `SQLITE_BUSY` under lock contention.
//!
//! # Error mapping (parity with PG)
//!
//! | Lua err code           | EngineError                                       |
//! |-----------------------|--------------------------------------------------|
//! | `execution_not_found`  | `NotFound { entity: "execution" }`               |
//! | `execution_not_active` | `Contention(ExecutionNotActive { … })`           |
//! | `stale_lease`          | `State(StaleLease)`                              |
//! | `lease_expired`        | `State(LeaseExpired)`                            |
//! | `lease_revoked`        | `State(LeaseRevoked)`                            |
//! | `fence_required`       | `Validation { InvalidInput, "fence_required" }`  |
//!
//! # Fencing asymmetry (same as PG)
//!
//! SQLite's `ff_attempt` schema persists only `lease_epoch`;
//! `lease_id` and `attempt_id` live on the worker-side `Handle` and
//! are never written to disk. `lease_epoch` is monotonically bumped
//! on every claim/reclaim, so a stale caller's epoch ≠ stored epoch
//! catches the same violations Valkey's full-triple check catches.

use ff_core::contracts::{
    CompleteExecutionArgs, CompleteExecutionResult, RenewLeaseArgs, RenewLeaseResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::state::PublicState;
use ff_core::types::{CancelSource, TimestampMs};
use sqlx::Row;
use sqlx::SqlitePool;

use crate::backend::{
    begin_immediate, commit_or_rollback, insert_completion_event_ev, insert_lease_event,
    rollback_quiet, split_exec_id, OutboxChannel, PendingEmit,
};
use crate::errors::map_sqlx_error;
use crate::pubsub::PubSub;
use crate::queries::{attempt as q_attempt, lease as q_lease};

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

/// Dispatch the accumulated pending emits after a successful commit.
/// Mirrors `backend::dispatch_pending_emits` but inlined here to keep
/// typed_ops decoupled from backend.rs's private helper.
fn dispatch_emits(pubsub: &PubSub, emits: Vec<PendingEmit>) {
    for (ch, ev) in emits {
        match ch {
            OutboxChannel::LeaseHistory => PubSub::emit(&pubsub.lease_history, ev),
            OutboxChannel::Completion => PubSub::emit(&pubsub.completion, ev),
            OutboxChannel::SignalDelivery => PubSub::emit(&pubsub.signal_delivery, ev),
            OutboxChannel::StreamFrame => PubSub::emit(&pubsub.stream_frame, ev),
            OutboxChannel::OperatorEvent => PubSub::emit(&pubsub.operator_event, ev),
        }
    }
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::renew_lease`].
///
/// Mirrors PG at `ff-backend-postgres/src/typed_ops.rs::renew_lease`:
///
/// 1. Fence required — `args.fence.is_none()` → `Validation{fence_required}`.
/// 2. `BEGIN IMMEDIATE` acquires the writer lock.
/// 3. Read attempt row; missing → `NotFound { entity: "attempt" }`.
/// 4. Epoch-only fence (see module-level asymmetry note); mismatch →
///    `State(StaleLease)`.
/// 5. Read exec_core; missing → `NotFound { entity: "execution" }`.
/// 6. `lifecycle_phase != 'active'` → `Contention(ExecutionNotActive{..})`.
/// 7. NULL or expired `lease_expires_at_ms` → `State(LeaseExpired)`.
/// 8. `UPDATE ff_attempt SET lease_expires_at_ms = now + ttl`.
/// 9. Emit `insert_lease_event(.., "renewed", now)` to the lease outbox.
/// 10. Return `RenewLeaseResult::Renewed { expires_at }`.
pub(crate) async fn renew_lease(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: RenewLeaseArgs,
) -> Result<RenewLeaseResult, EngineError> {
    let fence = args.fence.as_ref().ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: "fence_required".into(),
    })?;

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let attempt_index = i64::from(args.attempt_index.0);
    let expected_epoch = fence.lease_epoch.0;
    // Clamp u64 → i64 overflow to MAX rather than silently collapsing to 0.
    let lease_ttl_i64 = i64::try_from(args.lease_ttl_ms).unwrap_or(i64::MAX);

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        // Read attempt row.
        let row = sqlx::query(q_attempt::SELECT_ATTEMPT_EPOCH_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let Some(row) = row else {
            return Err(EngineError::NotFound { entity: "attempt" });
        };
        let observed_epoch_i: i64 = row.try_get("lease_epoch").map_err(map_sqlx_error)?;
        let observed_epoch = u64::try_from(observed_epoch_i).unwrap_or(0);
        if observed_epoch != expected_epoch {
            return Err(EngineError::State(StateKind::StaleLease));
        }

        // Read exec_core lifecycle + lease_expires_at (live-lease gate).
        let exec_row = sqlx::query(
            r#"
            SELECT lifecycle_phase,
                   ownership_state,
                   COALESCE(json_extract(raw_fields, '$.current_attempt_id'), '')
                       AS current_attempt_id,
                   COALESCE(json_extract(raw_fields, '$.terminal_outcome'), 'none')
                       AS terminal_outcome
              FROM ff_exec_core
             WHERE partition_key = ?1 AND execution_id = ?2
            "#,
        )
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let Some(exec_row) = exec_row else {
            return Err(EngineError::NotFound { entity: "execution" });
        };
        let ownership_state: String = exec_row
            .try_get("ownership_state")
            .map_err(map_sqlx_error)?;
        if ownership_state == "lease_revoked" {
            return Err(EngineError::State(StateKind::LeaseRevoked));
        }
        let lifecycle_phase: String = exec_row
            .try_get("lifecycle_phase")
            .map_err(map_sqlx_error)?;
        if lifecycle_phase != "active" {
            let terminal_outcome: String =
                exec_row.try_get("terminal_outcome").map_err(map_sqlx_error)?;
            let current_attempt_id: String = exec_row
                .try_get("current_attempt_id")
                .map_err(map_sqlx_error)?;
            return Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
                terminal_outcome,
                lease_epoch: observed_epoch.to_string(),
                lifecycle_phase,
                attempt_id: current_attempt_id,
            }));
        }

        let live_row = sqlx::query(
            "SELECT lease_expires_at_ms FROM ff_attempt \
             WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3",
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let observed_expires: Option<i64> = live_row
            .try_get("lease_expires_at_ms")
            .map_err(map_sqlx_error)?;

        let now = now_ms();
        match observed_expires {
            Some(exp) if exp > now => {}
            _ => return Err(EngineError::State(StateKind::LeaseExpired)),
        }

        // Update the lease.
        let new_expires = now.saturating_add(lease_ttl_i64);
        sqlx::query(q_lease::UPDATE_ATTEMPT_RENEW_SQL)
            .bind(new_expires)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        // Outbox: lease renewed.
        let ev = insert_lease_event(&mut conn, part, exec_uuid, "renewed", now).await?;
        let emits = vec![(OutboxChannel::LeaseHistory, ev)];

        Ok::<_, EngineError>((
            RenewLeaseResult::Renewed {
                expires_at: TimestampMs::from_millis(new_expires),
            },
            emits,
        ))
    }
    .await;

    match result {
        Ok((renewal, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_emits(pubsub, emits);
            Ok(renewal)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::complete_execution`].
///
/// Mirrors PG at `ff-backend-postgres/src/typed_ops.rs::complete_execution`:
///
/// 1. Fence-or-operator-override gate. `args.fence.is_none()` with
///    `source != OperatorOverride` → `Validation{fence_required}`.
/// 2. `BEGIN IMMEDIATE` writer lock.
/// 3. Read exec_core; missing → `NotFound { entity: "execution" }`.
///    `ownership_state == 'lease_revoked'` → `State(LeaseRevoked)`.
///    `lifecycle_phase != 'active'` → `Contention(ExecutionNotActive{..})`.
/// 4. Read attempt row; missing → `NotFound { entity: "attempt" }`.
/// 5. Epoch fence (when `args.fence` is `Some`); mismatch →
///    `State(StaleLease)`. Skipped on operator override.
/// 6. `lease_expires_at_ms` NULL or `<= now` → `State(LeaseExpired)`.
/// 7. Mark attempt terminal.
/// 8. Flip exec_core to terminal / completed; store result payload;
///    stamp `terminal_outcome='success'` in `raw_fields`.
/// 9. Emit completion outbox (carries namespace + instance_tag from
///    raw_fields so RFC-019 filtered subscriptions receive the event).
/// 10. Emit `lease_event::EVENT_REVOKED` on the lease outbox.
pub(crate) async fn complete_execution(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: CompleteExecutionArgs,
) -> Result<CompleteExecutionResult, EngineError> {
    let is_operator_override = matches!(args.source, CancelSource::OperatorOverride);
    if args.fence.is_none() && !is_operator_override {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "fence_required".into(),
        });
    }

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let attempt_index = i64::from(args.attempt_index.0);
    let now = args.now.0;
    let execution_id = args.execution_id.clone();

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        // Read exec_core first — lifecycle + ownership gates are
        // authoritative pre-conditions.
        let exec_row = sqlx::query(
            r#"
            SELECT lifecycle_phase, ownership_state,
                   COALESCE(json_extract(raw_fields, '$.current_attempt_id'), '')
                       AS current_attempt_id
              FROM ff_exec_core
             WHERE partition_key = ?1 AND execution_id = ?2
            "#,
        )
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let Some(exec_row) = exec_row else {
            return Err(EngineError::NotFound { entity: "execution" });
        };
        let ownership_state: String = exec_row
            .try_get("ownership_state")
            .map_err(map_sqlx_error)?;
        if ownership_state == "lease_revoked" {
            return Err(EngineError::State(StateKind::LeaseRevoked));
        }
        let lifecycle_phase: String = exec_row
            .try_get("lifecycle_phase")
            .map_err(map_sqlx_error)?;
        if lifecycle_phase != "active" {
            let current_attempt_id: String = exec_row
                .try_get("current_attempt_id")
                .map_err(map_sqlx_error)?;
            return Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
                terminal_outcome: String::new(),
                lease_epoch: String::new(),
                lifecycle_phase,
                attempt_id: current_attempt_id,
            }));
        }

        // Attempt fence + lease-live check.
        let attempt_row = sqlx::query(
            "SELECT lease_epoch, lease_expires_at_ms FROM ff_attempt \
             WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3",
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let Some(attempt_row) = attempt_row else {
            return Err(EngineError::NotFound { entity: "attempt" });
        };
        let observed_epoch_i: i64 = attempt_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
        let observed_epoch = u64::try_from(observed_epoch_i).unwrap_or(0);
        let observed_expires: Option<i64> = attempt_row
            .try_get("lease_expires_at_ms")
            .map_err(map_sqlx_error)?;

        if let Some(fence) = &args.fence
            && observed_epoch != fence.lease_epoch.0
        {
            return Err(EngineError::State(StateKind::StaleLease));
        }
        if let Some(exp) = observed_expires
            && exp <= now
        {
            return Err(EngineError::State(StateKind::LeaseExpired));
        }

        // Mark attempt terminal.
        sqlx::query(
            "UPDATE ff_attempt \
                SET terminal_at_ms = ?1, outcome = 'success', lease_expires_at_ms = NULL \
              WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4",
        )
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        // Flip exec_core to terminal/completed + stamp raw_fields.
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'terminal',
                   ownership_state = 'unowned',
                   eligibility_state = 'not_applicable',
                   attempt_state = 'attempt_terminal',
                   public_state = 'completed',
                   terminal_at_ms = ?1,
                   result = ?2,
                   raw_fields = json_set(raw_fields, '$.terminal_outcome', 'success')
             WHERE partition_key = ?3 AND execution_id = ?4
            "#,
        )
        .bind(now)
        .bind(args.result_payload.as_deref())
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        // Completion outbox.
        let completion_ev =
            insert_completion_event_ev(&mut conn, part, exec_uuid, "success", now).await?;
        let lease_ev = insert_lease_event(&mut conn, part, exec_uuid, "revoked", now).await?;
        let emits = vec![
            (OutboxChannel::Completion, completion_ev),
            (OutboxChannel::LeaseHistory, lease_ev),
        ];

        Ok::<_, EngineError>(emits)
    }
    .await;

    match result {
        Ok(emits) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_emits(pubsub, emits);
            Ok(CompleteExecutionResult::Completed {
                execution_id,
                public_state: PublicState::Completed,
            })
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}
