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
    CheckAdmissionArgs, CheckAdmissionResult, ClaimGrantOutcome, CompleteExecutionArgs,
    CompleteExecutionResult, DeliverApprovalSignalArgs, DeliverSignalArgs, DeliverSignalResult,
    FailExecutionArgs, FailExecutionResult, IssueGrantAndClaimArgs, RecordSpendArgs,
    ReleaseBudgetArgs, RenewLeaseArgs, RenewLeaseResult, ReportUsageResult, ResumeExecutionArgs,
    ResumeExecutionResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::PartitionConfig;
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptId, AttemptIndex, CancelSource, ExecutionId, LaneId, LeaseEpoch, LeaseId, QuotaPolicyId,
    SignalId, TimestampMs, WaitpointToken, WorkerId, WorkerInstanceId,
};
use serde_json::{json, Value as JsonValue};
use sqlx::Row;
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use uuid::Uuid;

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
                SET terminal_at_ms = ?1, outcome = 'failed', lease_expires_at_ms = NULL \
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

/// Internal outcome sum for `check_admission` — funnels the four
/// decision-tree endpoints through a single commit/rollback site.
enum AdmitOutcome {
    Admitted,
    AlreadyAdmitted,
    RateExceeded { retry_after_ms: u64 },
    ConcurrencyExceeded,
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
    _partition_config: &PartitionConfig,
    quota_policy_id: &QuotaPolicyId,
    args: CheckAdmissionArgs,
) -> Result<CheckAdmissionResult, EngineError> {
    // RFC-023 §4.1 A3 single-writer: SQLite doesn't HASH-partition the
    // quota tables; `create_quota_policy_impl` writes with
    // `partition_key = 0` (see `crate::budget::PART`). Reads MUST use
    // the same literal or the idempotency guard + rate check silently
    // miss the policy's rows. `_partition_config` is ignored on SQLite;
    // accepted in the signature for cross-backend call-site parity.
    let part: i64 = 0;
    let policy_id_text = quota_policy_id.to_string();
    let now = args.now.0;
    let window_ms: i64 =
        i64::try_from(args.window_seconds.saturating_mul(1_000)).unwrap_or(i64::MAX);
    let exec_id_text = args.execution_id.to_string();

    let mut conn = begin_immediate(pool).await?;

    // Wrap the full decision tree in an async block so early-return
    // error paths funnel through `rollback_quiet` instead of dropping
    // the connection mid-transaction (Copilot PR #462 finding).
    let result: Result<AdmitOutcome, EngineError> = async {
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
                return Ok(AdmitOutcome::AlreadyAdmitted);
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
                return Ok(AdmitOutcome::RateExceeded { retry_after_ms });
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
            return Ok(AdmitOutcome::ConcurrencyExceeded);
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

        Ok(AdmitOutcome::Admitted)
    }
    .await;

    match result {
        Ok(outcome) => {
            commit_or_rollback(&mut conn).await?;
            Ok(match outcome {
                AdmitOutcome::Admitted => CheckAdmissionResult::Admitted,
                AdmitOutcome::AlreadyAdmitted => CheckAdmissionResult::AlreadyAdmitted,
                AdmitOutcome::RateExceeded { retry_after_ms } => {
                    CheckAdmissionResult::RateExceeded { retry_after_ms }
                }
                AdmitOutcome::ConcurrencyExceeded => CheckAdmissionResult::ConcurrencyExceeded,
            })
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::evaluate_flow_eligibility`].
///
/// Read-only; no tx needed. Mirrors PG at
/// `ff-backend-postgres/src/typed_ops.rs::evaluate_flow_eligibility`.
/// Returns one of: `not_found`, `not_runnable`, `owned`, `terminal`,
/// `impossible`, `blocked_by_dependencies`, `eligible`.
///
/// Required-edge counts derived live via JOIN on `ff_edge` + upstream
/// `ff_exec_core.raw_fields.terminal_outcome`. SQLite swaps:
///
/// - `policy->>'kind'` → `json_extract(e.policy, '$.kind')`
/// - `raw_fields->>'terminal_outcome'` → `json_extract(up.raw_fields,
///   '$.terminal_outcome')`
/// - `flow_id` is BLOB on SQLite (uuid-bytes stored raw)
pub(crate) async fn evaluate_flow_eligibility(
    pool: &SqlitePool,
    args: ff_core::contracts::EvaluateFlowEligibilityArgs,
) -> Result<ff_core::contracts::EvaluateFlowEligibilityResult, EngineError> {
    use ff_core::contracts::EvaluateFlowEligibilityResult;

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;

    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, ownership_state, flow_id,
               COALESCE(json_extract(raw_fields, '$.terminal_outcome'), 'none')
                   AS terminal_outcome
          FROM ff_exec_core
         WHERE partition_key = ?1 AND execution_id = ?2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(exec_row) = exec_row else {
        return Ok(EvaluateFlowEligibilityResult::Status {
            status: "not_found".into(),
        });
    };
    let lifecycle_phase: String = exec_row
        .try_get("lifecycle_phase")
        .map_err(map_sqlx_error)?;
    if lifecycle_phase != "runnable" {
        return Ok(EvaluateFlowEligibilityResult::Status {
            status: "not_runnable".into(),
        });
    }
    let ownership_state: String = exec_row
        .try_get("ownership_state")
        .map_err(map_sqlx_error)?;
    if ownership_state != "unowned" {
        return Ok(EvaluateFlowEligibilityResult::Status {
            status: "owned".into(),
        });
    }
    let terminal_outcome: String = exec_row
        .try_get("terminal_outcome")
        .map_err(map_sqlx_error)?;
    if terminal_outcome != "none" {
        return Ok(EvaluateFlowEligibilityResult::Status {
            status: "terminal".into(),
        });
    }
    let flow_id_opt: Option<uuid::Uuid> = exec_row.try_get("flow_id").map_err(map_sqlx_error)?;
    let Some(flow_id) = flow_id_opt else {
        return Ok(EvaluateFlowEligibilityResult::Status {
            status: "eligible".into(),
        });
    };

    // Walk incoming edges + classify upstreams.
    let counts = sqlx::query(
        r#"
        SELECT
          SUM(CASE
                WHEN is_required = 1
                  AND COALESCE(json_extract(up.raw_fields, '$.terminal_outcome'), 'none') = 'failed'
                THEN 1 ELSE 0 END) AS impossible_count,
          SUM(CASE
                WHEN is_required = 1
                  AND COALESCE(json_extract(up.raw_fields, '$.terminal_outcome'), 'none') NOT IN ('failed', 'success')
                THEN 1 ELSE 0 END) AS unsatisfied_count
        FROM (
          SELECT e.upstream_eid,
                 CASE WHEN json_extract(e.policy, '$.kind') IN ('required', 'AllOf', 'all_of')
                      THEN 1 ELSE 0 END AS is_required
            FROM ff_edge e
           WHERE e.partition_key = ?1
             AND e.flow_id = ?2
             AND e.downstream_eid = ?3
        ) edges
        LEFT JOIN ff_exec_core up
          ON up.partition_key = ?1 AND up.execution_id = edges.upstream_eid
        "#,
    )
    .bind(part)
    .bind(flow_id)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx_error)?;

    let impossible: Option<i64> = counts.try_get("impossible_count").map_err(map_sqlx_error)?;
    let unsatisfied: Option<i64> =
        counts.try_get("unsatisfied_count").map_err(map_sqlx_error)?;
    let status = if impossible.unwrap_or(0) > 0 {
        "impossible"
    } else if unsatisfied.unwrap_or(0) > 0 {
        "blocked_by_dependencies"
    } else {
        "eligible"
    };
    Ok(EvaluateFlowEligibilityResult::Status {
        status: status.into(),
    })
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::claim_execution`].
///
/// Mirrors PG at `ff-backend-postgres/src/typed_ops.rs::claim_execution`.
/// Consumes a claim grant, creates a new attempt row, transitions
/// `runnable → active`.
///
/// Validates in order:
/// 1. `lifecycle_phase == 'runnable'` → else `ExecutionNotActive`.
/// 2. `ownership_state == 'unowned'` → else `LeaseConflict`.
/// 3. `eligibility_state == 'eligible_now'` → else `ExecutionNotLeaseable`.
/// 4. `attempt_state != 'running_attempt'` (defense-in-depth) →
///    else `Conflict(ActiveAttemptExists)`.
/// 5. `attempt_state != 'attempt_interrupted'` → else
///    `ExecutionNotLeaseable` (caller must use
///    `claim_resumed_execution`).
/// 6. `ff_claim_grant` row exists for this (partition, exec_uuid,
///    kind='claim') → else `InvalidClaimGrant`.
/// 7. Grant's `worker_id` matches → else `InvalidClaimGrant`.
/// 8. Grant's `expires_at_ms > now` (when set) → else
///    `ClaimGrantExpired`.
///
/// On success: DELETE grant, INSERT/UPSERT ff_attempt with lease
/// identity in raw_fields, flip exec_core to active/leased/running,
/// emit lease-acquired outbox.
pub(crate) async fn claim_execution(
    pool: &SqlitePool,
    _partition_config: &PartitionConfig,
    pubsub: &PubSub,
    args: ff_core::contracts::ClaimExecutionArgs,
) -> Result<ff_core::contracts::ClaimExecutionResult, EngineError> {
    use ff_core::contracts::{ClaimExecutionResult, ClaimedExecution};
    use ff_core::state::AttemptType;
    use ff_core::types::LeaseEpoch;

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let attempt_index = i64::from(args.expected_attempt_index.0);
    let lease_ttl_i64 = i64::try_from(args.lease_ttl_ms).unwrap_or(i64::MAX);
    let now = args.now.0;
    let new_expires = now.saturating_add(lease_ttl_i64);

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let exec_row = sqlx::query(
            r#"
            SELECT lifecycle_phase, ownership_state, eligibility_state,
                   attempt_state,
                   COALESCE(json_extract(raw_fields, '$.terminal_outcome'), 'none')
                       AS terminal_outcome,
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
        let lifecycle_phase: String =
            exec_row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
        let ownership_state: String = exec_row
            .try_get("ownership_state")
            .map_err(map_sqlx_error)?;
        let eligibility_state: String = exec_row
            .try_get("eligibility_state")
            .map_err(map_sqlx_error)?;
        let attempt_state: String =
            exec_row.try_get("attempt_state").map_err(map_sqlx_error)?;

        if lifecycle_phase != "runnable" {
            let terminal_outcome: String =
                exec_row.try_get("terminal_outcome").map_err(map_sqlx_error)?;
            let current_attempt_id: String = exec_row
                .try_get("current_attempt_id")
                .map_err(map_sqlx_error)?;
            return Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
                terminal_outcome,
                lease_epoch: String::new(),
                lifecycle_phase,
                attempt_id: current_attempt_id,
            }));
        }
        if ownership_state != "unowned" {
            return Err(EngineError::Contention(ContentionKind::LeaseConflict));
        }
        if eligibility_state != "eligible_now" {
            return Err(EngineError::Contention(
                ContentionKind::ExecutionNotLeaseable,
            ));
        }
        if attempt_state == "running_attempt" {
            return Err(EngineError::Conflict(
                ff_core::engine_error::ConflictKind::ActiveAttemptExists,
            ));
        }
        if attempt_state == "attempt_interrupted" {
            return Err(EngineError::Contention(
                ContentionKind::ExecutionNotLeaseable,
            ));
        }

        // Validate + consume claim grant.
        let grant_row = sqlx::query(
            "SELECT worker_id, expires_at_ms FROM ff_claim_grant \
             WHERE partition_key = ?1 AND execution_id = ?2 AND kind = 'claim'",
        )
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let Some(grant_row) = grant_row else {
            return Err(EngineError::Contention(ContentionKind::InvalidClaimGrant));
        };
        let grant_worker_id: String =
            grant_row.try_get("worker_id").map_err(map_sqlx_error)?;
        if grant_worker_id != args.worker_id.as_str() {
            return Err(EngineError::Contention(ContentionKind::InvalidClaimGrant));
        }
        let grant_expires_at: i64 = grant_row
            .try_get("expires_at_ms")
            .map_err(map_sqlx_error)?;
        if grant_expires_at > 0 && grant_expires_at < now {
            return Err(EngineError::Contention(ContentionKind::ClaimGrantExpired));
        }
        sqlx::query(
            "DELETE FROM ff_claim_grant \
             WHERE partition_key = ?1 AND execution_id = ?2 AND kind = 'claim'",
        )
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        // Attempt type from existing attempt_state.
        let attempt_type = if attempt_state == "pending_retry_attempt" {
            AttemptType::Retry
        } else if attempt_state == "pending_replay_attempt" {
            AttemptType::Replay
        } else {
            AttemptType::Initial
        };
        let attempt_type_str = match attempt_type {
            AttemptType::Initial => "initial",
            AttemptType::Retry => "retry",
            AttemptType::Reclaim => "reclaim",
            AttemptType::Replay => "replay",
            AttemptType::Fallback => "fallback",
        };

        // Insert / upsert the attempt row. SQLite `json_set(?, '$.k', v)`
        // for multi-key patch; NULLIF for empty-policy.
        sqlx::query(
            r#"
            INSERT INTO ff_attempt (
                partition_key, execution_id, attempt_index,
                worker_id, worker_instance_id,
                lease_epoch, lease_expires_at_ms, started_at_ms,
                policy, raw_fields
            ) VALUES (
                ?1, ?2, ?3,
                ?4, ?5,
                1, ?6, ?7,
                NULLIF(?8, ''),
                json_set('{}', '$.attempt_type', ?9, '$.attempt_id', ?10, '$.lease_id', ?11)
            )
            ON CONFLICT (partition_key, execution_id, attempt_index)
            DO UPDATE SET
                worker_id = excluded.worker_id,
                worker_instance_id = excluded.worker_instance_id,
                lease_epoch = ff_attempt.lease_epoch + 1,
                lease_expires_at_ms = excluded.lease_expires_at_ms,
                started_at_ms = COALESCE(ff_attempt.started_at_ms, excluded.started_at_ms),
                policy = excluded.policy,
                raw_fields = json_patch(ff_attempt.raw_fields, excluded.raw_fields),
                terminal_at_ms = NULL,
                outcome = NULL
            "#,
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .bind(args.worker_id.as_str())
        .bind(args.worker_instance_id.as_str())
        .bind(new_expires)
        .bind(now)
        .bind(&args.attempt_policy_json)
        .bind(attempt_type_str)
        .bind(args.attempt_id.to_string())
        .bind(args.lease_id.to_string())
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        // Read back the epoch (may be +1 on the conflict path).
        let epoch_row = sqlx::query(
            "SELECT lease_epoch FROM ff_attempt \
             WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3",
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        let new_epoch_i: i64 = epoch_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
        let new_epoch = u64::try_from(new_epoch_i).unwrap_or(0);

        // Flip exec_core to active + stash lease identity in raw_fields.
        let next_attempt_index = attempt_index.saturating_add(1);
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'active',
                   ownership_state = 'leased',
                   eligibility_state = 'not_applicable',
                   attempt_state = 'running_attempt',
                   public_state = 'active',
                   attempt_index = ?1,
                   deadline_at_ms = COALESCE(?2, deadline_at_ms),
                   raw_fields = json_set(
                       raw_fields,
                       '$.current_attempt_id', ?3,
                       '$.current_lease_id', ?4,
                       '$.current_worker_id', ?5,
                       '$.current_worker_instance_id', ?6,
                       '$.pending_retry_reason', '',
                       '$.pending_replay_reason', '',
                       '$.pending_replay_requested_by', '',
                       '$.pending_previous_attempt_index', ''
                   )
             WHERE partition_key = ?7 AND execution_id = ?8
            "#,
        )
        .bind(next_attempt_index)
        .bind(args.execution_deadline_at)
        .bind(args.attempt_id.to_string())
        .bind(args.lease_id.to_string())
        .bind(args.worker_id.as_str())
        .bind(args.worker_instance_id.as_str())
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        // Outbox: lease acquired.
        let ev = insert_lease_event(&mut conn, part, exec_uuid, "acquired", now).await?;
        let emits = vec![(OutboxChannel::LeaseHistory, ev)];

        Ok::<_, EngineError>((new_epoch, attempt_type, emits))
    }
    .await;

    match result {
        Ok((new_epoch, attempt_type, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_emits(pubsub, emits);
            let claimed = ClaimedExecution::new(
                args.execution_id,
                args.lease_id,
                LeaseEpoch(new_epoch),
                args.expected_attempt_index,
                args.attempt_id,
                attempt_type,
                TimestampMs::from_millis(new_expires),
                ff_core::backend::stub_handle_fresh(),
            );
            Ok(ClaimExecutionResult::Claimed(claimed))
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// cairn #454 Phase 5 — SQLite bodies for the four typed-FCALL methods
// (record_spend / release_budget / deliver_approval_signal /
// issue_grant_and_claim). Mirrors the Postgres references in
// `ff-backend-postgres/src/budget.rs`, `…/suspend_ops.rs`, and
// `…/typed_ops.rs`. SQLite single-partition (partition_key = 0, per
// `crate::budget::PART`) + single-writer (BEGIN IMMEDIATE + §4.1 A3)
// replaces PG's partition-hash math, `FOR NO KEY UPDATE`, and
// `SET TRANSACTION ISOLATION LEVEL SERIALIZABLE`.
// ───────────────────────────────────────────────────────────────────

const BUDGET_PART: i64 = 0;
/// Dedup-expiry window. Same 24h constant as PG + SQLite budget.rs.
const DEDUP_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

fn dim_row_key(name: &str) -> String {
    name.to_string()
}

fn limits_from_policy(policy: &JsonValue, key: &str) -> BTreeMap<String, u64> {
    policy
        .get(key)
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                .collect()
        })
        .unwrap_or_default()
}

fn outcome_to_json_text(r: &ReportUsageResult) -> String {
    match r {
        ReportUsageResult::Ok => json!({"kind": "Ok"}),
        ReportUsageResult::AlreadyApplied => json!({"kind": "AlreadyApplied"}),
        ReportUsageResult::SoftBreach {
            dimension,
            current_usage,
            soft_limit,
        } => json!({
            "kind": "SoftBreach",
            "dimension": dimension,
            "current_usage": current_usage,
            "soft_limit": soft_limit,
        }),
        ReportUsageResult::HardBreach {
            dimension,
            current_usage,
            hard_limit,
        } => json!({
            "kind": "HardBreach",
            "dimension": dimension,
            "current_usage": current_usage,
            "hard_limit": hard_limit,
        }),
        _ => json!({"kind": "Ok"}),
    }
    .to_string()
}

/// Extract the bare UUID-as-text for the `ff_budget_usage_by_exec.execution_id`
/// TEXT column (migration 0020).
fn exec_uuid_text(eid: &ExecutionId) -> Result<String, EngineError> {
    let (_, uuid) = split_exec_id(eid)?;
    Ok(uuid.to_string())
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::record_spend`].
pub(crate) async fn record_spend(
    pool: &SqlitePool,
    args: RecordSpendArgs,
) -> Result<ReportUsageResult, EngineError> {
    let budget_id_str = args.budget_id.to_string();
    let exec_uuid_str = exec_uuid_text(&args.execution_id)?;
    let now = now_ms();

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        // ── Dedup reservation ──
        let dedup_owned: Option<String> = if args.idempotency_key.is_empty() {
            None
        } else {
            let inserted = sqlx::query(
                "INSERT INTO ff_budget_usage_dedup \
                     (partition_key, dedup_key, outcome_json, applied_at_ms, expires_at_ms) \
                 VALUES (?1, ?2, '{}', ?3, ?4) \
                 ON CONFLICT (partition_key, dedup_key) DO NOTHING \
                 RETURNING applied_at_ms",
            )
            .bind(BUDGET_PART)
            .bind(&args.idempotency_key)
            .bind(now)
            .bind(now + DEDUP_TTL_MS)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
            if inserted.is_none() {
                return Ok(ReportUsageResult::AlreadyApplied);
            }
            Some(args.idempotency_key.clone())
        };

        let policy_row = sqlx::query(
            "SELECT policy_json FROM ff_budget_policy \
             WHERE partition_key = ?1 AND budget_id = ?2",
        )
        .bind(BUDGET_PART)
        .bind(&budget_id_str)
        .fetch_optional(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        let policy: JsonValue = match policy_row {
            Some(r) => {
                let text: String = r.try_get("policy_json").map_err(map_sqlx_error)?;
                serde_json::from_str(&text).unwrap_or_else(|_| JsonValue::Object(Default::default()))
            }
            None => JsonValue::Object(Default::default()),
        };
        let hard_limits = limits_from_policy(&policy, "hard_limits");
        let soft_limits = limits_from_policy(&policy, "soft_limits");

        let mut per_dim_current: BTreeMap<String, u64> = BTreeMap::new();
        for (dim, delta) in args.deltas.iter() {
            let dim_key = dim_row_key(dim);
            sqlx::query(
                "INSERT INTO ff_budget_usage \
                     (partition_key, budget_id, dimensions_key, current_value, updated_at_ms) \
                 VALUES (?1, ?2, ?3, 0, ?4) \
                 ON CONFLICT (partition_key, budget_id, dimensions_key) DO NOTHING",
            )
            .bind(BUDGET_PART)
            .bind(&budget_id_str)
            .bind(&dim_key)
            .bind(now)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            let row = sqlx::query(
                "SELECT current_value FROM ff_budget_usage \
                 WHERE partition_key = ?1 AND budget_id = ?2 AND dimensions_key = ?3",
            )
            .bind(BUDGET_PART)
            .bind(&budget_id_str)
            .bind(&dim_key)
            .fetch_one(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
            let cur: i64 = row.try_get("current_value").map_err(map_sqlx_error)?;
            let new_val = (cur as u64).saturating_add(*delta);

            if let Some(hard) = hard_limits.get(dim)
                && *hard > 0
                && new_val > *hard
            {
                sqlx::query(
                    "UPDATE ff_budget_policy \
                     SET breach_count = breach_count + 1, \
                         last_breach_at_ms = ?1, \
                         last_breach_dim = ?2, \
                         updated_at_ms = ?1 \
                     WHERE partition_key = ?3 AND budget_id = ?4",
                )
                .bind(now)
                .bind(dim)
                .bind(BUDGET_PART)
                .bind(&budget_id_str)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

                let outcome = ReportUsageResult::HardBreach {
                    dimension: dim.clone(),
                    current_usage: cur as u64,
                    hard_limit: *hard,
                };
                if let Some(dk) = dedup_owned.as_deref() {
                    finalize_dedup_spend(&mut conn, dk, &outcome).await?;
                }
                return Ok(outcome);
            }
            per_dim_current.insert(dim.clone(), new_val);
        }

        let mut soft_breach: Option<ReportUsageResult> = None;
        for (dim, delta) in args.deltas.iter() {
            let dim_key = dim_row_key(dim);
            let new_val = per_dim_current[dim];
            sqlx::query(
                "UPDATE ff_budget_usage \
                 SET current_value = current_value + ?1, updated_at_ms = ?2 \
                 WHERE partition_key = ?3 AND budget_id = ?4 AND dimensions_key = ?5",
            )
            .bind(*delta as i64)
            .bind(now)
            .bind(BUDGET_PART)
            .bind(&budget_id_str)
            .bind(&dim_key)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            sqlx::query(
                "INSERT INTO ff_budget_usage_by_exec \
                     (partition_key, budget_id, execution_id, dimensions_key, \
                      delta_total, updated_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (partition_key, budget_id, execution_id, dimensions_key) \
                 DO UPDATE SET delta_total = delta_total + excluded.delta_total, \
                               updated_at_ms = excluded.updated_at_ms",
            )
            .bind(BUDGET_PART)
            .bind(&budget_id_str)
            .bind(&exec_uuid_str)
            .bind(&dim_key)
            .bind(*delta as i64)
            .bind(now)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            if soft_breach.is_none()
                && let Some(soft) = soft_limits.get(dim)
                && *soft > 0
                && new_val > *soft
            {
                soft_breach = Some(ReportUsageResult::SoftBreach {
                    dimension: dim.clone(),
                    current_usage: new_val,
                    soft_limit: *soft,
                });
            }
        }

        if soft_breach.is_some() {
            sqlx::query(
                "UPDATE ff_budget_policy \
                 SET soft_breach_count = soft_breach_count + 1, \
                     updated_at_ms = ?1 \
                 WHERE partition_key = ?2 AND budget_id = ?3",
            )
            .bind(now)
            .bind(BUDGET_PART)
            .bind(&budget_id_str)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }

        let outcome = soft_breach.unwrap_or(ReportUsageResult::Ok);
        if let Some(dk) = dedup_owned.as_deref() {
            finalize_dedup_spend(&mut conn, dk, &outcome).await?;
        }
        Ok::<_, EngineError>(outcome)
    }
    .await;

    match result {
        Ok(outcome) => {
            commit_or_rollback(&mut conn).await?;
            Ok(outcome)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

async fn finalize_dedup_spend(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    dedup_key: &str,
    outcome: &ReportUsageResult,
) -> Result<(), EngineError> {
    let text = outcome_to_json_text(outcome);
    sqlx::query(
        "UPDATE ff_budget_usage_dedup SET outcome_json = ?1 \
         WHERE partition_key = ?2 AND dedup_key = ?3",
    )
    .bind(text)
    .bind(BUDGET_PART)
    .bind(dedup_key)
    .execute(&mut **conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

/// SQLite body for [`ff_core::engine_backend::EngineBackend::release_budget`].
pub(crate) async fn release_budget(
    pool: &SqlitePool,
    args: ReleaseBudgetArgs,
) -> Result<(), EngineError> {
    let budget_id_str = args.budget_id.to_string();
    let exec_uuid_str = exec_uuid_text(&args.execution_id)?;
    let now = now_ms();

    let mut conn = begin_immediate(pool).await?;

    let result: Result<(), EngineError> = async {
        let rows = sqlx::query(
            "SELECT dimensions_key, delta_total FROM ff_budget_usage_by_exec \
             WHERE partition_key = ?1 AND budget_id = ?2 AND execution_id = ?3",
        )
        .bind(BUDGET_PART)
        .bind(&budget_id_str)
        .bind(&exec_uuid_str)
        .fetch_all(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        if rows.is_empty() {
            return Ok(());
        }

        for row in &rows {
            let dim: String = row.try_get("dimensions_key").map_err(map_sqlx_error)?;
            let delta: i64 = row.try_get("delta_total").map_err(map_sqlx_error)?;
            sqlx::query(
                "UPDATE ff_budget_usage \
                 SET current_value = MAX(0, current_value - ?1), \
                     updated_at_ms = ?2 \
                 WHERE partition_key = ?3 AND budget_id = ?4 AND dimensions_key = ?5",
            )
            .bind(delta)
            .bind(now)
            .bind(BUDGET_PART)
            .bind(&budget_id_str)
            .bind(&dim)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        }

        sqlx::query(
            "DELETE FROM ff_budget_usage_by_exec \
             WHERE partition_key = ?1 AND budget_id = ?2 AND execution_id = ?3",
        )
        .bind(BUDGET_PART)
        .bind(&budget_id_str)
        .bind(&exec_uuid_str)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            commit_or_rollback(&mut conn).await?;
            Ok(())
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

/// SQLite body for
/// [`ff_core::engine_backend::EngineBackend::deliver_approval_signal`].
pub(crate) async fn deliver_approval_signal(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: DeliverApprovalSignalArgs,
) -> Result<DeliverSignalResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;

    let lane_row: Option<(String,)> = sqlx::query_as(
        "SELECT lane_id FROM ff_exec_core \
         WHERE partition_key = ?1 AND execution_id = ?2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    let authoritative_lane = match lane_row {
        Some((s,)) => LaneId::new(s),
        None => {
            return Err(EngineError::NotFound {
                entity: "execution",
            });
        }
    };
    if authoritative_lane.as_str() != args.lane_id.as_str() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "lane_mismatch: args.lane_id={} exec_core.lane_id={}",
                args.lane_id.as_str(),
                authoritative_lane.as_str()
            ),
        });
    }

    let partition = ff_core::partition::Partition {
        family: ff_core::partition::PartitionFamily::Execution,
        index: part as u16,
    };
    let pkey = ff_core::partition::PartitionKey::from(&partition);
    let token_str = crate::reads::read_waitpoint_token_impl(pool, &pkey, &args.waitpoint_id)
        .await?
        .ok_or(EngineError::NotFound {
            entity: "waitpoint",
        })?;

    let now = TimestampMs::now();
    let idempotency_key = format!("approval:{}:{}", args.signal_name, args.idempotency_suffix);
    let ds = DeliverSignalArgs {
        execution_id: args.execution_id.clone(),
        waitpoint_id: args.waitpoint_id.clone(),
        signal_id: SignalId::new(),
        signal_name: args.signal_name.clone(),
        signal_category: "approval".to_owned(),
        source_type: "operator".to_owned(),
        source_identity: String::new(),
        payload: None,
        payload_encoding: None,
        correlation_id: None,
        idempotency_key: Some(idempotency_key),
        target_scope: "execution".to_owned(),
        created_at: Some(now),
        dedup_ttl_ms: Some(args.signal_dedup_ttl_ms),
        resume_delay_ms: None,
        max_signals_per_execution: args.max_signals_per_execution,
        signal_maxlen: args.maxlen,
        waitpoint_token: WaitpointToken::new(token_str),
        now,
    };

    crate::suspend_ops::deliver_signal_impl(pool, pubsub, ds).await
}

/// SQLite body for
/// [`ff_core::engine_backend::EngineBackend::issue_grant_and_claim`].
pub(crate) async fn issue_grant_and_claim(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: IssueGrantAndClaimArgs,
) -> Result<ClaimGrantOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let lease_ttl_i64 = i64::try_from(args.lease_duration_ms).unwrap_or(i64::MAX);
    let now = now_ms();
    let new_expires = now.saturating_add(lease_ttl_i64);

    let worker_id = WorkerId::new("operator");
    let worker_instance_id = WorkerInstanceId::new("operator");
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let exec_row = sqlx::query(
            r#"
            SELECT lifecycle_phase, ownership_state, eligibility_state,
                   attempt_state, attempt_index,
                   COALESCE(json_extract(raw_fields, '$.terminal_outcome'), 'none')
                       AS terminal_outcome,
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
        let lifecycle_phase: String =
            exec_row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
        let ownership_state: String =
            exec_row.try_get("ownership_state").map_err(map_sqlx_error)?;
        let eligibility_state: String =
            exec_row.try_get("eligibility_state").map_err(map_sqlx_error)?;
        let attempt_state: String =
            exec_row.try_get("attempt_state").map_err(map_sqlx_error)?;
        let current_attempt_index_i: i64 =
            exec_row.try_get("attempt_index").map_err(map_sqlx_error)?;

        let is_resume = attempt_state == "attempt_interrupted";

        if !is_resume {
            if lifecycle_phase != "runnable" {
                let terminal_outcome: String =
                    exec_row.try_get("terminal_outcome").map_err(map_sqlx_error)?;
                let current_attempt_id: String = exec_row
                    .try_get("current_attempt_id")
                    .map_err(map_sqlx_error)?;
                return Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
                    terminal_outcome,
                    lease_epoch: String::new(),
                    lifecycle_phase,
                    attempt_id: current_attempt_id,
                }));
            }
            if ownership_state != "unowned" {
                return Err(EngineError::Contention(ContentionKind::LeaseConflict));
            }
            if eligibility_state != "eligible_now" && eligibility_state != "pending_claim" {
                return Err(EngineError::Contention(
                    ContentionKind::ExecutionNotLeaseable,
                ));
            }
            if attempt_state == "running_attempt" {
                return Err(EngineError::Conflict(
                    ff_core::engine_error::ConflictKind::ActiveAttemptExists,
                ));
            }
        }

        let grant_id_bytes = Uuid::new_v4().as_bytes().to_vec();
        sqlx::query(
            r#"
            INSERT INTO ff_claim_grant (
                partition_key, grant_id, execution_id, kind,
                worker_id, worker_instance_id, lane_id,
                capability_hash, worker_capabilities,
                route_snapshot_json, admission_summary,
                grant_ttl_ms, issued_at_ms, expires_at_ms
            ) VALUES (
                ?1, ?2, ?3, 'claim',
                ?4, ?5, ?6,
                NULL, '[]',
                NULL, NULL,
                ?7, ?8, ?9
            )
            ON CONFLICT (partition_key, grant_id) DO NOTHING
            "#,
        )
        .bind(part)
        .bind(&grant_id_bytes)
        .bind(exec_uuid)
        .bind(worker_id.as_str())
        .bind(worker_instance_id.as_str())
        .bind(args.lane_id.as_str())
        .bind(lease_ttl_i64)
        .bind(now)
        .bind(new_expires)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        let (used_attempt_index, new_epoch_i): (i64, i64) = if is_resume {
            sqlx::query(
                "UPDATE ff_attempt \
                    SET worker_id = ?1, worker_instance_id = ?2, \
                        lease_epoch = lease_epoch + 1, \
                        lease_expires_at_ms = ?3, started_at_ms = ?4, outcome = NULL \
                  WHERE partition_key = ?5 AND execution_id = ?6 AND attempt_index = ?7",
            )
            .bind(worker_id.as_str())
            .bind(worker_instance_id.as_str())
            .bind(new_expires)
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .bind(current_attempt_index_i)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            sqlx::query(
                "UPDATE ff_exec_core \
                    SET lifecycle_phase = 'active', ownership_state = 'leased', \
                        eligibility_state = 'not_applicable', \
                        public_state = 'running', attempt_state = 'running_attempt', \
                        started_at_ms = COALESCE(started_at_ms, ?3) \
                  WHERE partition_key = ?1 AND execution_id = ?2",
            )
            .bind(part)
            .bind(exec_uuid)
            .bind(now)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            let epoch_row = sqlx::query(
                "SELECT lease_epoch FROM ff_attempt \
                 WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3",
            )
            .bind(part)
            .bind(exec_uuid)
            .bind(current_attempt_index_i)
            .fetch_one(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
            let epoch: i64 = epoch_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
            (current_attempt_index_i, epoch)
        } else {
            let attempt_index_i = current_attempt_index_i;
            let attempt_type_str: &'static str = match attempt_state.as_str() {
                "pending_retry_attempt" => "retry",
                "pending_replay_attempt" => "replay",
                "pending_first_attempt" | "initial" => "initial",
                other => {
                    return Err(EngineError::Validation {
                        kind: ValidationKind::Corruption,
                        detail: format!(
                            "issue_grant_and_claim: unrecognized attempt_state={other}"
                        ),
                    });
                }
            };
            sqlx::query(
                r#"
                INSERT INTO ff_attempt (
                    partition_key, execution_id, attempt_index,
                    worker_id, worker_instance_id,
                    lease_epoch, lease_expires_at_ms, started_at_ms,
                    policy, raw_fields
                ) VALUES (
                    ?1, ?2, ?3,
                    ?4, ?5,
                    1, ?6, ?7,
                    NULL,
                    json_set('{}', '$.attempt_type', ?8, '$.attempt_id', ?9, '$.lease_id', ?10)
                )
                ON CONFLICT (partition_key, execution_id, attempt_index)
                DO UPDATE SET
                    worker_id = excluded.worker_id,
                    worker_instance_id = excluded.worker_instance_id,
                    lease_epoch = ff_attempt.lease_epoch + 1,
                    lease_expires_at_ms = excluded.lease_expires_at_ms,
                    started_at_ms = COALESCE(ff_attempt.started_at_ms, excluded.started_at_ms),
                    raw_fields = json_patch(ff_attempt.raw_fields, excluded.raw_fields),
                    terminal_at_ms = NULL,
                    outcome = NULL
                "#,
            )
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index_i)
            .bind(worker_id.as_str())
            .bind(worker_instance_id.as_str())
            .bind(new_expires)
            .bind(now)
            .bind(attempt_type_str)
            .bind(attempt_id.to_string())
            .bind(lease_id.to_string())
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            let epoch_row = sqlx::query(
                "SELECT lease_epoch FROM ff_attempt \
                 WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3",
            )
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index_i)
            .fetch_one(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
            let epoch: i64 = epoch_row.try_get("lease_epoch").map_err(map_sqlx_error)?;

            let next_attempt_index = attempt_index_i.saturating_add(1);
            sqlx::query(
                r#"
                UPDATE ff_exec_core
                   SET lifecycle_phase = 'active',
                       ownership_state = 'leased',
                       eligibility_state = 'not_applicable',
                       attempt_state = 'running_attempt',
                       public_state = 'running',
                       attempt_index = ?1,
                       raw_fields = json_set(
                           raw_fields,
                           '$.current_attempt_id', ?2,
                           '$.current_lease_id', ?3,
                           '$.current_worker_id', ?4,
                           '$.current_worker_instance_id', ?5,
                           '$.pending_retry_reason', '',
                           '$.pending_replay_reason', '',
                           '$.pending_replay_requested_by', '',
                           '$.pending_previous_attempt_index', ''
                       )
                 WHERE partition_key = ?6 AND execution_id = ?7
                "#,
            )
            .bind(next_attempt_index)
            .bind(attempt_id.to_string())
            .bind(lease_id.to_string())
            .bind(worker_id.as_str())
            .bind(worker_instance_id.as_str())
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

            (attempt_index_i, epoch)
        };

        sqlx::query(
            "DELETE FROM ff_claim_grant \
             WHERE partition_key = ?1 AND grant_id = ?2",
        )
        .bind(part)
        .bind(&grant_id_bytes)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        let ev = insert_lease_event(&mut conn, part, exec_uuid, "acquired", now).await?;
        let emits = vec![(OutboxChannel::LeaseHistory, ev)];

        Ok::<_, EngineError>((used_attempt_index, new_epoch_i, emits))
    }
    .await;

    match result {
        Ok((used_attempt_index, new_epoch_i, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_emits(pubsub, emits);
            let attempt_index =
                AttemptIndex::new(u32::try_from(used_attempt_index.max(0)).unwrap_or(0));
            let new_epoch = u64::try_from(new_epoch_i).unwrap_or(0);
            Ok(ClaimGrantOutcome::new(
                lease_id,
                LeaseEpoch(new_epoch),
                attempt_index,
            ))
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}
