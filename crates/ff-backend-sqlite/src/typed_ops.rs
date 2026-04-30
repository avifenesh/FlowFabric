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
    CheckAdmissionArgs, CheckAdmissionResult, CompleteExecutionArgs, CompleteExecutionResult,
    FailExecutionArgs, FailExecutionResult, RenewLeaseArgs, RenewLeaseResult,
    ResumeExecutionArgs, ResumeExecutionResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::{quota_partition, PartitionConfig};
use ff_core::state::PublicState;
use ff_core::types::{AttemptIndex, CancelSource, QuotaPolicyId, TimestampMs};
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

/// Parsed retry policy envelope (mirror of PG helper at
/// `ff-backend-postgres/src/typed_ops.rs::parse_retry_policy`).
struct ParsedRetryPolicy {
    max_retries: u32,
    backoff: serde_json::Value,
}

fn parse_retry_policy(s: &str) -> ParsedRetryPolicy {
    if s.is_empty() {
        return ParsedRetryPolicy {
            max_retries: 0,
            backoff: serde_json::Value::Null,
        };
    }
    let v: serde_json::Value = serde_json::from_str(s).unwrap_or(serde_json::Value::Null);
    let max_retries = v
        .get("max_retries")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let backoff = v.get("backoff").cloned().unwrap_or(serde_json::Value::Null);
    ParsedRetryPolicy {
        max_retries,
        backoff,
    }
}

fn compute_retry_delay_ms(backoff: &serde_json::Value, retry_count: u32) -> u64 {
    let kind = backoff.get("type").and_then(|x| x.as_str()).unwrap_or("");
    match kind {
        "exponential" => {
            let initial = backoff
                .get("initial_delay_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(1_000);
            let max_d = backoff
                .get("max_delay_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(60_000);
            let mult = backoff
                .get("multiplier")
                .and_then(|x| x.as_f64())
                .unwrap_or(2.0);
            let exp = mult.powi(retry_count as i32);
            let delay = (initial as f64) * exp;
            (delay.min(max_d as f64)) as u64
        }
        "fixed" => backoff
            .get("delay_ms")
            .and_then(|x| x.as_u64())
            .unwrap_or(1_000),
        _ => 1_000,
    }
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::fail_execution`].
///
/// Mirrors PG at `ff-backend-postgres/src/typed_ops.rs::fail_execution`
/// with the retry/terminal branch split:
///
/// - Retry path (`retry_count < policy.max_retries`): flip exec_core
///   to `runnable` / `delayed`; stash `pending_retry_reason`,
///   `pending_previous_attempt_index`, bumped `retry_count`,
///   `delay_until`, `failure_reason` in `raw_fields`. Return
///   `RetryScheduled { delay_until, next_attempt_index }`.
/// - Terminal path: flip exec_core to `terminal` / `failed`; emit
///   completion outbox with `outcome='failed'` + lease-revoked outbox.
///   Return `TerminalFailed`.
///
/// Fence-or-operator-override gate matches `complete_execution`.
pub(crate) async fn fail_execution(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: FailExecutionArgs,
) -> Result<FailExecutionResult, EngineError> {
    let is_operator_override = matches!(args.source, CancelSource::OperatorOverride);
    if args.fence.is_none() && !is_operator_override {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "fence_required".into(),
        });
    }

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let attempt_index = i64::from(args.attempt_index.0);

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        // Read attempt row for fence.
        let attempt_row = sqlx::query(q_attempt::SELECT_ATTEMPT_EPOCH_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let Some(attempt_row) = attempt_row else {
            return Err(EngineError::NotFound { entity: "attempt" });
        };
        let observed_epoch_i: i64 =
            attempt_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
        let observed_epoch = u64::try_from(observed_epoch_i).unwrap_or(0);

        if let Some(fence) = &args.fence
            && observed_epoch != fence.lease_epoch.0
        {
            return Err(EngineError::State(StateKind::StaleLease));
        }

        // Read exec_core for lifecycle + retry_count.
        let exec_row = sqlx::query(
            r#"
            SELECT lifecycle_phase,
                   ownership_state,
                   COALESCE(CAST(json_extract(raw_fields, '$.retry_count') AS INTEGER), 0)
                       AS retry_count,
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
        let retry_count_i: i64 = exec_row.try_get("retry_count").map_err(map_sqlx_error)?;
        let retry_count = u32::try_from(retry_count_i.max(0)).unwrap_or(0);

        let policy = parse_retry_policy(&args.retry_policy_json);
        let can_retry = retry_count < policy.max_retries;

        let now = now_ms();

        // Mark attempt terminal (both branches).
        sqlx::query(
            "UPDATE ff_attempt \
                SET terminal_at_ms = ?1, outcome = 'failure', lease_expires_at_ms = NULL \
              WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4",
        )
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        if can_retry {
            let backoff_ms = compute_retry_delay_ms(&policy.backoff, retry_count);
            let delay_until = now.saturating_add(backoff_ms as i64);
            let next_attempt_index = attempt_index.saturating_add(1) as u32;

            // Retry path: flip exec_core to runnable/delayed + stash
            // pending retry lineage. SQLite `json_set` accepts multiple
            // key/value pairs in one call — mirrors PG's
            // `raw_fields || patch` semantic observably.
            sqlx::query(
                r#"
                UPDATE ff_exec_core
                   SET lifecycle_phase = 'runnable',
                       ownership_state = 'unowned',
                       eligibility_state = 'not_eligible_until_time',
                       attempt_state = 'pending_retry_attempt',
                       blocking_reason = 'waiting_for_retry_backoff',
                       public_state = 'delayed',
                       raw_fields = json_set(
                           raw_fields,
                           '$.pending_retry_reason', ?1,
                           '$.pending_previous_attempt_index', ?2,
                           '$.retry_count', ?3,
                           '$.delay_until', ?4,
                           '$.failure_reason', ?1
                       )
                 WHERE partition_key = ?5 AND execution_id = ?6
                "#,
            )
            .bind(args.failure_reason.clone())
            .bind(attempt_index.to_string())
            .bind((retry_count + 1).to_string())
            .bind(delay_until.to_string())
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            let lease_ev = insert_lease_event(&mut conn, part, exec_uuid, "revoked", now).await?;
            let emits = vec![(OutboxChannel::LeaseHistory, lease_ev)];

            return Ok::<_, EngineError>((
                FailExecutionResult::RetryScheduled {
                    delay_until: TimestampMs::from_millis(delay_until),
                    next_attempt_index: AttemptIndex::new(next_attempt_index),
                },
                emits,
            ));
        }

        // Terminal path.
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'terminal',
                   ownership_state = 'unowned',
                   eligibility_state = 'not_applicable',
                   attempt_state = 'attempt_terminal',
                   public_state = 'failed',
                   terminal_at_ms = ?1,
                   raw_fields = json_set(
                       raw_fields,
                       '$.failure_reason', ?2,
                       '$.terminal_outcome', 'failed'
                   )
             WHERE partition_key = ?3 AND execution_id = ?4
            "#,
        )
        .bind(now)
        .bind(args.failure_reason.clone())
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        let completion_ev =
            insert_completion_event_ev(&mut conn, part, exec_uuid, "failed", now).await?;
        let lease_ev = insert_lease_event(&mut conn, part, exec_uuid, "revoked", now).await?;
        let emits = vec![
            (OutboxChannel::Completion, completion_ev),
            (OutboxChannel::LeaseHistory, lease_ev),
        ];

        Ok((FailExecutionResult::TerminalFailed, emits))
    }
    .await;

    match result {
        Ok((outcome, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_emits(pubsub, emits);
            Ok(outcome)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::resume_execution`].
///
/// Mirrors PG at `ff-backend-postgres/src/typed_ops.rs::resume_execution`.
/// No fence (caller is operator or signal delivery). Validates
/// suspended-state + suspension_current row presence, flips to
/// runnable, clears waitpoint/suspension refs, DELETEs suspension row.
pub(crate) async fn resume_execution(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: ResumeExecutionArgs,
) -> Result<ResumeExecutionResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let exec_row = sqlx::query(
            "SELECT lifecycle_phase FROM ff_exec_core \
             WHERE partition_key = ?1 AND execution_id = ?2",
        )
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let Some(exec_row) = exec_row else {
            return Err(EngineError::NotFound { entity: "execution" });
        };
        let lifecycle_phase: String = exec_row
            .try_get("lifecycle_phase")
            .map_err(map_sqlx_error)?;
        if lifecycle_phase != "suspended" {
            return Err(EngineError::State(StateKind::ExecutionNotSuspended));
        }

        let susp_row = sqlx::query(
            "SELECT 1 FROM ff_suspension_current \
             WHERE partition_key = ?1 AND execution_id = ?2",
        )
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        if susp_row.is_none() {
            return Err(EngineError::State(StateKind::ExecutionNotSuspended));
        }

        let (eligibility_state, blocking_reason, public_state) = if args.resume_delay_ms > 0 {
            (
                "not_eligible_until_time",
                "waiting_for_resume_delay",
                PublicState::Delayed,
            )
        } else {
            (
                "eligible_now",
                "waiting_for_worker",
                PublicState::Waiting,
            )
        };
        let public_state_str = match public_state {
            PublicState::Delayed => "delayed",
            PublicState::Waiting => "waiting",
            _ => "waiting",
        };

        // `trigger_type` is advisory; recorded nowhere on the PG path
        // and we match that here. Suppress unused-field lint:
        let _ = &args.trigger_type;

        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'runnable',
                   ownership_state = 'unowned',
                   eligibility_state = ?1,
                   attempt_state = 'attempt_interrupted',
                   blocking_reason = ?2,
                   public_state = ?3,
                   raw_fields = json_set(
                       raw_fields,
                       '$.current_suspension_id', '',
                       '$.current_waitpoint_id', ''
                   )
             WHERE partition_key = ?4 AND execution_id = ?5
            "#,
        )
        .bind(eligibility_state)
        .bind(blocking_reason)
        .bind(public_state_str)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(
            "DELETE FROM ff_suspension_current \
             WHERE partition_key = ?1 AND execution_id = ?2",
        )
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        Ok::<_, EngineError>(public_state)
    }
    .await;

    match result {
        Ok(public_state) => {
            commit_or_rollback(&mut conn).await?;
            let _ = pubsub;
            Ok(ResumeExecutionResult::Resumed { public_state })
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::check_admission`].
///
/// Mirrors PG at `ff-backend-postgres/src/typed_ops.rs::check_admission`.
/// The PG `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` is omitted —
/// `BEGIN IMMEDIATE` + SQLite's single-writer invariant is implicitly
/// serialisable for writers; the writer lock held across this tx gives
/// the same atomicity guarantee the PG path needs to prevent concurrent
/// admits from racing.
///
/// Four-exit decision tree: `AlreadyAdmitted`, `RateExceeded {
/// retry_after_ms }`, `ConcurrencyExceeded`, `Admitted`.
pub(crate) async fn check_admission(
    pool: &SqlitePool,
    partition_config: &PartitionConfig,
    quota_policy_id: &QuotaPolicyId,
    args: CheckAdmissionArgs,
) -> Result<CheckAdmissionResult, EngineError> {
    let part = quota_partition(quota_policy_id, partition_config).index as i64;
    let policy_id_text = quota_policy_id.to_string();
    let now = args.now.0;
    let window_ms: i64 =
        i64::try_from(args.window_seconds.saturating_mul(1_000)).unwrap_or(i64::MAX);
    let exec_id_text = args.execution_id.to_string();

    let mut conn = begin_immediate(pool).await?;

    // 1. Idempotency guard.
    let guard_row = sqlx::query(
        "SELECT expires_at_ms FROM ff_quota_admitted \
         WHERE partition_key = ?1 AND quota_policy_id = ?2 AND execution_id = ?3",
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(&exec_id_text)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;
    if let Some(row) = guard_row {
        let expires_at_ms: i64 = row.try_get("expires_at_ms").map_err(map_sqlx_error)?;
        if expires_at_ms > now {
            commit_or_rollback(&mut conn).await?;
            return Ok(CheckAdmissionResult::AlreadyAdmitted);
        }
    }

    // 2. Sliding window sweep for THIS policy.
    sqlx::query(
        "DELETE FROM ff_quota_window \
         WHERE partition_key = ?1 AND quota_policy_id = ?2 AND admitted_at_ms < ?3",
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(now.saturating_sub(window_ms))
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    // 3. Rate check.
    if args.rate_limit > 0 {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM ff_quota_window \
             WHERE partition_key = ?1 AND quota_policy_id = ?2",
        )
        .bind(part)
        .bind(&policy_id_text)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        if count as u64 >= args.rate_limit {
            let oldest: Option<(i64,)> = sqlx::query_as(
                "SELECT admitted_at_ms FROM ff_quota_window \
                 WHERE partition_key = ?1 AND quota_policy_id = ?2 \
                 ORDER BY admitted_at_ms ASC LIMIT 1",
            )
            .bind(part)
            .bind(&policy_id_text)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
            let retry = oldest
                .map(|(oldest_ms,)| {
                    let delta = oldest_ms.saturating_add(window_ms).saturating_sub(now);
                    u64::try_from(delta.max(0)).unwrap_or(0)
                })
                .unwrap_or(0);
            let jitter = args.jitter_ms.unwrap_or(0);
            let retry_after_ms = retry.saturating_add(jitter);
            commit_or_rollback(&mut conn).await?;
            return Ok(CheckAdmissionResult::RateExceeded { retry_after_ms });
        }
    }

    // 4. Concurrency check.
    let policy_row = sqlx::query(
        "SELECT active_concurrency FROM ff_quota_policy \
         WHERE partition_key = ?1 AND quota_policy_id = ?2",
    )
    .bind(part)
    .bind(&policy_id_text)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;
    let active: i64 = policy_row
        .map(|r| r.try_get("active_concurrency").unwrap_or(0i64))
        .unwrap_or(0);
    if args.concurrency_cap > 0 && active as u64 >= args.concurrency_cap {
        commit_or_rollback(&mut conn).await?;
        return Ok(CheckAdmissionResult::ConcurrencyExceeded);
    }

    // 5. Admit.
    sqlx::query(
        "INSERT INTO ff_quota_window \
            (partition_key, quota_policy_id, dimension, admitted_at_ms, execution_id) \
         VALUES (?1, ?2, '', ?3, ?4)",
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(now)
    .bind(&exec_id_text)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        "INSERT INTO ff_quota_admitted \
            (partition_key, quota_policy_id, execution_id, admitted_at_ms, expires_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (partition_key, quota_policy_id, execution_id) DO UPDATE \
            SET admitted_at_ms = excluded.admitted_at_ms, \
                expires_at_ms = excluded.expires_at_ms",
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(&exec_id_text)
    .bind(now)
    .bind(now.saturating_add(window_ms))
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    if args.concurrency_cap > 0 {
        sqlx::query(
            "INSERT INTO ff_quota_policy \
                (partition_key, quota_policy_id, \
                 requests_per_window_seconds, max_requests_per_window, \
                 active_concurrency_cap, active_concurrency, \
                 created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, 0, 0, 0, 1, ?3, ?3) \
             ON CONFLICT (partition_key, quota_policy_id) DO UPDATE \
                SET active_concurrency = active_concurrency + 1, \
                    updated_at_ms = excluded.updated_at_ms",
        )
        .bind(part)
        .bind(&policy_id_text)
        .bind(now)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    }

    commit_or_rollback(&mut conn).await?;
    Ok(CheckAdmissionResult::Admitted)
}
