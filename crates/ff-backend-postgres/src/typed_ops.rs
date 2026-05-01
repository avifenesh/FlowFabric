//! PR-7b / #453: typed-FCALL EngineBackend bodies on Postgres.
//!
//! Ports the Valkey `ff_*` Lua functions that landed with typed args
//! (`ClaimExecutionArgs`, `CompleteExecutionArgs`, …) to Postgres
//! SQL transactions. Each function here mirrors the Lua invariants in
//! `flowfabric.lua` precisely — the Lua is authoritative; the PG
//! transaction is a deterministic retelling in SQL.
//!
//! # Isolation
//!
//! Each op runs under the default `READ COMMITTED` isolation level
//! (some ops like `check_admission` upgrade to `SERIALIZABLE` per
//! their rustdoc) with explicit `FOR UPDATE` row locks on
//! `ff_exec_core` and/or `ff_attempt`. PG persists `lease_epoch` only;
//! it's monotonically bumped on each claim/reclaim and is sufficient
//! to detect the violations Valkey's full `(lease_id, lease_epoch,
//! attempt_id)` triple catches. See each op's rustdoc for the
//! specific fence shape.
//!
//! # Error mapping
//!
//! Lua `err("X")` codes map to `EngineError` per the established
//! `ff-script` → `ff-core::engine_error` conversion:
//!
//! | Lua                  | EngineError                                                       |
//! |----------------------|-------------------------------------------------------------------|
//! | `execution_not_found`| `NotFound { entity: "execution" }`                                |
//! | `execution_not_active`| `Contention(ExecutionNotActive { … })` (carries Lua-side detail) |
//! | `stale_lease`        | `State(StaleLease)`                                               |
//! | `lease_expired`      | `State(LeaseExpired)`                                             |
//! | `lease_revoked`      | `State(LeaseRevoked)`                                             |
//! | `fence_required`     | `Validation { InvalidInput, detail: "fence_required" }`           |

use ff_core::contracts::{
    CheckAdmissionArgs, CheckAdmissionResult, ClaimExecutionArgs, ClaimExecutionResult,
    ClaimGrantOutcome, ClaimedExecution, CompleteExecutionArgs, CompleteExecutionResult,
    EvaluateFlowEligibilityArgs, EvaluateFlowEligibilityResult, FailExecutionArgs,
    FailExecutionResult, IssueGrantAndClaimArgs, RenewLeaseArgs, RenewLeaseResult,
    ResumeExecutionArgs, ResumeExecutionResult,
};
use ff_core::state::AttemptType;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::{quota_partition, PartitionConfig};
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptId, AttemptIndex, CancelSource, LeaseEpoch, LeaseId, QuotaPolicyId, TimestampMs,
    WorkerId, WorkerInstanceId,
};
use sqlx::{PgPool, Row};

use crate::attempt::split_exec_id;
use crate::error::map_sqlx_error;
use crate::lease_event;

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

/// PG body for [`ff_core::engine_backend::EngineBackend::renew_lease`].
///
/// Mirrors `ff_renew_lease` in `flowfabric.lua` (Valkey authoritative):
///
/// 1. Fence required — `args.fence.is_none()` → `Validation{fence_required}`.
/// 2. Lock `ff_attempt (partition, exec_uuid, attempt_index) FOR UPDATE`.
/// 3. Compare stored `lease_epoch` to the fence triple's `lease_epoch`;
///    mismatch → `StaleLease`.
///
///    **Fencing asymmetry vs Valkey.** Valkey fences on the full
///    `(lease_id, lease_epoch, attempt_id)` triple because the Lua
///    stores all three on the `exec_core` hash. PG's `ff_attempt`
///    schema only persists `lease_epoch`; `lease_id` and `attempt_id`
///    live on the worker-side `Handle` (opaque blob, never written
///    to disk). `lease_epoch` is monotonically bumped by every
///    claim/reclaim, so a stale caller's epoch ≠ stored epoch is
///    sufficient to detect the same violations Valkey's triple
///    catches. Promoting to a full triple would require a
///    `lease_id`/`attempt_id` column migration; deferred per cairn
///    #453 §"Matrix asymmetry".
///
/// 4. Check lifecycle_phase on `ff_exec_core`; non-active →
///    `ExecutionNotActive`. Note: PG surface does not publish
///    `lease_revoked` as a separate state today (the revoke op would
///    null out `lease_expires_at_ms`), so revocation is detected via
///    the same NULL / expired check as expiry → `LeaseExpired`.
/// 5. Check existing `lease_expires_at_ms > now`; otherwise `LeaseExpired`.
/// 6. `UPDATE ff_attempt SET lease_expires_at_ms = now + ttl`.
/// 7. Emit `lease_event::EVENT_RENEWED` to the outbox.
/// 8. Return `RenewLeaseResult::Renewed { expires_at: new_expires }`.
pub(crate) async fn renew_lease(
    pool: &PgPool,
    args: RenewLeaseArgs,
) -> Result<RenewLeaseResult, EngineError> {
    // 1. Fence required.
    let fence = args.fence.as_ref().ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: "fence_required".into(),
    })?;

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    // `attempt_index` is `u32`; PG column is `integer` (i32). A caller
    // supplying an index beyond i32::MAX is a typed input error —
    // surface it rather than silently querying a truncated row.
    let attempt_index = i32::try_from(args.attempt_index.0).map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!(
            "attempt_index {} exceeds PG i32 column bound",
            args.attempt_index.0
        ),
    })?;
    let expected_epoch = fence.lease_epoch.0;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 2 + 3. Lock attempt row + validate fence (epoch only — see rustdoc
    // for the Valkey→PG fencing asymmetry).
    let attempt_row = sqlx::query(
        r#"
        SELECT lease_epoch, lease_expires_at_ms
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(attempt_row) = attempt_row else {
        return Err(EngineError::NotFound { entity: "attempt" });
    };

    let observed_epoch_i: i64 = attempt_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
    let observed_epoch = u64::try_from(observed_epoch_i).unwrap_or(0);
    let observed_expires_at: Option<i64> = attempt_row
        .try_get("lease_expires_at_ms")
        .map_err(map_sqlx_error)?;

    if observed_epoch != expected_epoch {
        return Err(EngineError::State(StateKind::StaleLease));
    }

    // 4. Lifecycle check.
    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase,
               attempt_state,
               COALESCE(raw_fields->>'current_attempt_id', '') AS current_attempt_id,
               COALESCE(raw_fields->>'terminal_outcome', 'none') AS terminal_outcome
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(exec_row) = exec_row else {
        return Err(EngineError::NotFound { entity: "execution" });
    };
    let lifecycle_phase: String = exec_row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
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

    let now = now_ms();
    // 5. Expiry / revocation check. A NULL `lease_expires_at_ms`
    // indicates the lease has been released (revoke/reclaim path);
    // treat as LeaseExpired for typed-surface consumers.
    match observed_expires_at {
        Some(exp) if exp > now => {}
        _ => return Err(EngineError::State(StateKind::LeaseExpired)),
    }

    // 6. Extend the lease. Clamp `lease_ttl_ms` overflow to `i64::MAX`
    // so a huge TTL extends the lease by as much as PG can represent
    // rather than silently collapsing to now (unwrap_or(0) bug).
    let new_expires = now.saturating_add(i64::try_from(args.lease_ttl_ms).unwrap_or(i64::MAX));
    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET lease_expires_at_ms = $1
         WHERE partition_key = $2 AND execution_id = $3 AND attempt_index = $4
        "#,
    )
    .bind(new_expires)
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 7. Outbox: lease renewed (mirrors RFC-019 Stage B event shape).
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        None,
        lease_event::EVENT_RENEWED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(RenewLeaseResult::Renewed {
        expires_at: ff_core::types::TimestampMs::from_millis(new_expires),
    })
}

/// PG body for [`ff_core::engine_backend::EngineBackend::complete_execution`].
///
/// Mirrors `ff_complete_execution` in `flowfabric.lua`:
///
/// 1. Fence-or-operator-override gate. `args.fence.is_none()` with
///    `source != OperatorOverride` → `Validation{fence_required}`.
/// 2. Lock `ff_attempt FOR UPDATE` + epoch fence (when present).
/// 3. Check `ff_exec_core.lifecycle_phase == 'active'`; otherwise
///    `ExecutionNotActive` (typed carries phase/epoch detail).
/// 4. Mark attempt terminal: `outcome='success'`, `terminal_at_ms=now`,
///    `lease_expires_at_ms=NULL`.
/// 5. Flip `ff_exec_core` to `lifecycle_phase='terminal'`,
///    `ownership_state='unowned'`, `eligibility_state='not_applicable'`,
///    `attempt_state='attempt_terminal'`, `public_state='completed'`,
///    `terminal_at_ms=now`, store `result` payload.
/// 6. Emit completion to `ff_completion_event` outbox (fires `pg_notify`
///    to subscribed listeners).
/// 7. Emit `lease_event::EVENT_REVOKED` to the lease outbox.
/// 8. Return `Completed { execution_id, public_state: Completed }`.
///
/// Operator-override path bypasses the epoch fence but still enforces
/// the lifecycle gate at step 3 — an operator cannot complete an
/// execution that's already terminal.
pub(crate) async fn complete_execution(
    pool: &PgPool,
    args: CompleteExecutionArgs,
) -> Result<CompleteExecutionResult, EngineError> {
    // 1. Fence-or-override gate.
    let is_operator_override = matches!(args.source, CancelSource::OperatorOverride);
    if args.fence.is_none() && !is_operator_override {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "fence_required".into(),
        });
    }

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let attempt_index =
        i32::try_from(args.attempt_index.0).map_err(|_| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "attempt_index {} exceeds PG i32 column bound",
                args.attempt_index.0
            ),
        })?;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 2. Lock exec_core FIRST (parity with Lua: lifecycle/ownership
    // gates are the authoritative pre-conditions, checked before the
    // attempt-row fence).
    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, ownership_state, flow_id,
               COALESCE(raw_fields->>'current_attempt_id', '') AS current_attempt_id
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let Some(exec_row) = exec_row else {
        return Err(EngineError::NotFound { entity: "execution" });
    };
    let lifecycle_phase: String = exec_row
        .try_get("lifecycle_phase")
        .map_err(map_sqlx_error)?;
    let ownership_state: String = exec_row
        .try_get("ownership_state")
        .map_err(map_sqlx_error)?;
    // Valkey's `validate_lease_and_mark_expired` rejects revoked
    // leases ahead of the lifecycle/attempt checks. PG signals
    // revocation via `ownership_state = 'lease_revoked'`.
    if ownership_state == "lease_revoked" {
        return Err(EngineError::State(StateKind::LeaseRevoked));
    }
    if lifecycle_phase != "active" {
        let current_attempt_id: String = exec_row
            .try_get("current_attempt_id")
            .map_err(map_sqlx_error)?;
        // Terminal outcome for PG is derived from `ff_attempt.outcome`
        // (schema of record — `raw_fields.terminal_outcome` is a
        // denormalisation legacy row that not every migration has).
        // Leave the wire-level detail blank when unknown rather than
        // fabricating a stale string from raw_fields.
        return Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
            terminal_outcome: String::new(),
            lease_epoch: String::new(),
            lifecycle_phase,
            attempt_id: current_attempt_id,
        }));
    }

    // 3. Lock attempt row + epoch fence (when caller supplied one) +
    // lease-live check. Lua's `validate_lease_and_mark_expired` rejects
    // expired leases with `lease_expired`; PG mirrors that by checking
    // `lease_expires_at_ms <= now` under the same lock.
    let attempt_row = sqlx::query(
        r#"
        SELECT lease_epoch, lease_expires_at_ms
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(attempt_row) = attempt_row else {
        return Err(EngineError::NotFound { entity: "attempt" });
    };

    let observed_epoch_i: i64 = attempt_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
    let observed_epoch = u64::try_from(observed_epoch_i).unwrap_or(0);
    let observed_expires_at: Option<i64> = attempt_row
        .try_get("lease_expires_at_ms")
        .map_err(map_sqlx_error)?;

    if let Some(fence) = &args.fence
        && observed_epoch != fence.lease_epoch.0
    {
        return Err(EngineError::State(StateKind::StaleLease));
    }

    // Prefer the caller-supplied `args.now` for scanner/cluster time
    // skew — PG-local `SystemTime::now()` is fine only if the caller
    // hasn't passed a timestamp. All typed-contract call sites DO
    // populate `args.now`, so this path is the norm.
    let now = args.now.0;
    if let Some(exp) = observed_expires_at
        && exp <= now
    {
        return Err(EngineError::State(StateKind::LeaseExpired));
    }

    // 4. Mark attempt terminal.
    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET terminal_at_ms = $1,
               outcome = 'success',
               lease_expires_at_ms = NULL
         WHERE partition_key = $2 AND execution_id = $3 AND attempt_index = $4
        "#,
    )
    .bind(now)
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 5. Flip exec_core to terminal + store result payload + stamp
    // `terminal_outcome=success` into `raw_fields` for backfills /
    // legacy readers that still look at the denormalised copy.
    let raw_patch = serde_json::json!({
        "terminal_outcome": "success",
    });
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'terminal',
               ownership_state = 'unowned',
               eligibility_state = 'not_applicable',
               attempt_state = 'attempt_terminal',
               public_state = 'completed',
               terminal_at_ms = $1,
               result = $2,
               raw_fields = raw_fields || $3::jsonb
         WHERE partition_key = $4 AND execution_id = $5
        "#,
    )
    .bind(now)
    .bind(args.result_payload.as_deref())
    .bind(raw_patch)
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 6. Completion outbox → pg_notify cascade for DAG dispatch.
    // Carry `namespace` + `instance_tag` forward from `ff_exec_core`
    // so downstream subscribers (RFC-019 filtered subscriptions)
    // receive the events instead of silently dropping on NULL.
    sqlx::query(
        r#"
        INSERT INTO ff_completion_event (
            partition_key, execution_id, flow_id, outcome,
            namespace, instance_tag, occurred_at_ms
        )
        SELECT partition_key, execution_id, flow_id, 'success',
               raw_fields->>'namespace',
               raw_fields->>'instance_tag',
               $3
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 7. RFC-019 Stage B lease outbox: revoked (terminal success).
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        None,
        lease_event::EVENT_REVOKED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(CompleteExecutionResult::Completed {
        execution_id: args.execution_id,
        public_state: PublicState::Completed,
    })
}


/// Parse the retry policy envelope:
/// `{ "max_retries": N, "backoff": { "type": "exponential"|"fixed", ... } }`.
/// Uses `serde_json::Value` (ff-backend-postgres doesn't depend on
/// `serde` directly; adding a dep for one struct isn't worth it).
/// Returns `(max_retries, compute_delay_fn)`.
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

/// Compute the next retry delay given a policy backoff JSON block +
/// the current retry counter. Mirrors the Lua branches in
/// `ff_fail_execution`. Default 1 s fixed when the backoff block
/// doesn't specify a recognised type.
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

/// PG body for [`ff_core::engine_backend::EngineBackend::fail_execution`].
///
/// Mirrors `ff_fail_execution` in `flowfabric.lua`. Two exit branches:
///
/// - **Retry path** (`retry_count < policy.max_retries`): flips
///   `ff_exec_core` to `runnable`/`delayed`, releases the lease
///   (`lease_expires_at_ms=NULL`), records pending retry lineage in
///   `raw_fields` (`pending_retry_reason`,
///   `pending_previous_attempt_index`, `retry_count` bump,
///   `delay_until`), increments `retry_count`. Returns
///   `RetryScheduled { delay_until, next_attempt_index }`.
/// - **Terminal path** (retries exhausted): flips `ff_exec_core` to
///   `terminal`/`failed`, stamps `terminal_at_ms`, emits completion
///   event (`outcome='failed'`) + lease-revoked outbox event.
///   Returns `TerminalFailed`.
pub(crate) async fn fail_execution(
    pool: &PgPool,
    args: FailExecutionArgs,
) -> Result<FailExecutionResult, EngineError> {
    // 1. Fence-or-operator-override gate.
    let is_operator_override = matches!(args.source, CancelSource::OperatorOverride);
    if args.fence.is_none() && !is_operator_override {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "fence_required".into(),
        });
    }

    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let attempt_index = i32::try_from(args.attempt_index.0).unwrap_or(i32::MAX);

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 2. Lock attempt row + fence.
    let attempt_row = sqlx::query(
        r#"
        SELECT lease_epoch
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(attempt_row) = attempt_row else {
        return Err(EngineError::NotFound { entity: "attempt" });
    };
    let observed_epoch_i: i64 = attempt_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
    let observed_epoch = u64::try_from(observed_epoch_i).unwrap_or(0);

    if let Some(fence) = &args.fence
        && observed_epoch != fence.lease_epoch.0
    {
        return Err(EngineError::State(StateKind::StaleLease));
    }

    // 3. Lock exec_core + lifecycle gate + read retry_count.
    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, flow_id,
               COALESCE((raw_fields->>'retry_count')::int, 0) AS retry_count,
               COALESCE(raw_fields->>'current_attempt_id', '') AS current_attempt_id,
               COALESCE(raw_fields->>'terminal_outcome', 'none') AS terminal_outcome
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(exec_row) = exec_row else {
        return Err(EngineError::NotFound { entity: "execution" });
    };
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
    let retry_count_i: i32 = exec_row.try_get("retry_count").map_err(map_sqlx_error)?;
    let retry_count = u32::try_from(retry_count_i.max(0)).unwrap_or(0);

    // 4. Parse retry policy + decide branch.
    let policy = parse_retry_policy(&args.retry_policy_json);
    let can_retry = retry_count < policy.max_retries;

    let now = now_ms();

    // 5. Mark attempt terminal (both branches).
    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET terminal_at_ms = $1,
               outcome = 'failure',
               lease_expires_at_ms = NULL
         WHERE partition_key = $2 AND execution_id = $3 AND attempt_index = $4
        "#,
    )
    .bind(now)
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if can_retry {
        // 6a. RETRY PATH
        let backoff_ms = compute_retry_delay_ms(&policy.backoff, retry_count);
        let delay_until = now.saturating_add(backoff_ms as i64);
        let next_attempt_index = attempt_index.saturating_add(1);

        let patch = serde_json::json!({
            "pending_retry_reason": args.failure_reason.clone(),
            "pending_previous_attempt_index": attempt_index.to_string(),
            "retry_count": (retry_count + 1).to_string(),
            "delay_until": delay_until.to_string(),
            "failure_reason": args.failure_reason.clone(),
        });

        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'runnable',
                   ownership_state = 'unowned',
                   eligibility_state = 'not_eligible_until_time',
                   attempt_state = 'pending_retry_attempt',
                   blocking_reason = 'waiting_for_retry_backoff',
                   public_state = 'delayed',
                   raw_fields = raw_fields || $1::jsonb
             WHERE partition_key = $2 AND execution_id = $3
            "#,
        )
        .bind(patch)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Lease outbox: released.
        lease_event::emit(
            &mut tx,
            part,
            exec_uuid,
            None,
            lease_event::EVENT_REVOKED,
            now,
        )
        .await?;

        tx.commit().await.map_err(map_sqlx_error)?;

        return Ok(FailExecutionResult::RetryScheduled {
            delay_until: TimestampMs::from_millis(delay_until),
            next_attempt_index: AttemptIndex::new(next_attempt_index as u32),
        });
    }

    // 6b. TERMINAL FAILED PATH
    let patch = serde_json::json!({
        "failure_reason": args.failure_reason.clone(),
        "terminal_outcome": "failed",
    });
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'terminal',
               ownership_state = 'unowned',
               eligibility_state = 'not_applicable',
               attempt_state = 'attempt_terminal',
               public_state = 'failed',
               terminal_at_ms = $1,
               raw_fields = raw_fields || $2::jsonb
         WHERE partition_key = $3 AND execution_id = $4
        "#,
    )
    .bind(now)
    .bind(patch)
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Completion outbox with outcome='failed' so downstream DAG can
    // cascade skip/fail.
    sqlx::query(
        r#"
        INSERT INTO ff_completion_event (
            partition_key, execution_id, flow_id, outcome,
            namespace, instance_tag, occurred_at_ms
        )
        SELECT partition_key, execution_id, flow_id, 'failed',
               NULL, NULL, $3
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        None,
        lease_event::EVENT_REVOKED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(FailExecutionResult::TerminalFailed)
}




/// PG body for [`ff_core::engine_backend::EngineBackend::resume_execution`].
///
/// Mirrors `ff_resume_execution` in `flowfabric.lua`:
///
/// 1. Lock `ff_exec_core FOR UPDATE` + verify `lifecycle_phase == 'suspended'`.
/// 2. Verify a matching `ff_suspension_current` row exists
///    (`ExecutionNotSuspended` otherwise).
/// 3. Flip `ff_exec_core` to `runnable`, compute eligibility based on
///    `resume_delay_ms`: > 0 → `not_eligible_until_time`/`delayed`,
///    else `eligible_now`/`waiting`. Clear `current_waitpoint_id` +
///    `current_suspension_id` from `raw_fields`.
/// 4. DELETE the `ff_suspension_current` row.
/// 5. Return `Resumed { public_state }`.
pub(crate) async fn resume_execution(
    pool: &PgPool,
    args: ResumeExecutionArgs,
) -> Result<ResumeExecutionResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 1. Lock exec_core + lifecycle gate.
    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
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

    // 2. Confirm a suspension row exists.
    let susp_row = sqlx::query(
        r#"
        SELECT 1 FROM ff_suspension_current
         WHERE partition_key = $1 AND execution_id = $2
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    if susp_row.is_none() {
        return Err(EngineError::State(StateKind::ExecutionNotSuspended));
    }

    let now = now_ms();

    // 3. Decide eligibility based on resume_delay_ms.
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
        // Unreachable — the branch above only sets Delayed or Waiting.
        _ => "waiting",
    };

    // 4. Flip exec_core.
    let patch = serde_json::json!({
        "current_suspension_id": "",
        "current_waitpoint_id": "",
    });
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'runnable',
               ownership_state = 'unowned',
               eligibility_state = $1,
               attempt_state = 'attempt_interrupted',
               blocking_reason = $2,
               public_state = $3,
               raw_fields = raw_fields || $4::jsonb
         WHERE partition_key = $5 AND execution_id = $6
        "#,
    )
    .bind(eligibility_state)
    .bind(blocking_reason)
    .bind(public_state_str)
    .bind(patch)
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 5. Drop the suspension_current row. `trigger_type` is advisory
    // and recorded for audit in the outbox only (PG shape doesn't
    // keep a separate "close_reason" on the row since we DELETE).
    let _ = &args.trigger_type;
    sqlx::query(
        r#"
        DELETE FROM ff_suspension_current
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let _ = now;
    Ok(ResumeExecutionResult::Resumed { public_state })
}

/// PG body for [`ff_core::engine_backend::EngineBackend::check_admission`].
///
/// Mirrors `ff_check_admission_and_record` in `flowfabric.lua`:
///
/// 1. Idempotency guard: `SELECT` on `ff_quota_admitted` for this
///    `(partition_key, quota_policy_id, execution_id)`; if present and
///    unexpired → `AlreadyAdmitted`.
/// 2. Sliding window sweep: `DELETE FROM ff_quota_window WHERE
///    admitted_at_ms < now - window_ms`.
/// 3. Rate check: `SELECT COUNT(*)` on remaining window; if
///    `count >= rate_limit` compute retry-after from the oldest row
///    → `RateExceeded { retry_after_ms }`.
/// 4. Concurrency check: `SELECT active_concurrency` on
///    `ff_quota_policy`; if `>= concurrency_cap` → `ConcurrencyExceeded`.
/// 5. Admit: INSERT into `ff_quota_window` + UPSERT
///    `ff_quota_admitted` (with TTL = window_ms) + increment
///    `ff_quota_policy.active_concurrency`.
/// 6. Return `Admitted`.
///
/// All reads + writes run under `SERIALIZABLE` isolation to match the
/// Lua's atomic semantics; the per-policy `partition_key` scope keeps
/// contention partition-local.
pub(crate) async fn check_admission(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    quota_policy_id: &QuotaPolicyId,
    args: CheckAdmissionArgs,
) -> Result<CheckAdmissionResult, EngineError> {
    let part = quota_partition(quota_policy_id, partition_config).index as i16;
    let policy_id_text = quota_policy_id.to_string();
    let now = args.now.0;
    let window_ms: i64 = i64::try_from(args.window_seconds.saturating_mul(1_000)).unwrap_or(i64::MAX);
    let exec_id_text = args.execution_id.to_string();

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    // SERIALIZABLE matches the Lua's single-slot atomicity guarantee.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // 1. Idempotency guard.
    let guard_row = sqlx::query(
        r#"
        SELECT expires_at_ms FROM ff_quota_admitted
         WHERE partition_key = $1 AND quota_policy_id = $2 AND execution_id = $3
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(&exec_id_text)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    if let Some(row) = guard_row {
        let expires_at_ms: i64 = row.try_get("expires_at_ms").map_err(map_sqlx_error)?;
        if expires_at_ms > now {
            tx.commit().await.map_err(map_sqlx_error)?;
            return Ok(CheckAdmissionResult::AlreadyAdmitted);
        }
    }

    // 2. Sliding window sweep for THIS policy (bounded delete — PK
    // prefix sarges the index; no dedicated sweep index needed).
    sqlx::query(
        r#"
        DELETE FROM ff_quota_window
         WHERE partition_key = $1
           AND quota_policy_id = $2
           AND admitted_at_ms < $3
        "#,
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(now.saturating_sub(window_ms))
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 3. Rate check.
    if args.rate_limit > 0 {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM ff_quota_window \
             WHERE partition_key = $1 AND quota_policy_id = $2",
        )
        .bind(part)
        .bind(&policy_id_text)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        if count as u64 >= args.rate_limit {
            let oldest: Option<(i64,)> = sqlx::query_as(
                "SELECT admitted_at_ms FROM ff_quota_window \
                 WHERE partition_key = $1 AND quota_policy_id = $2 \
                 ORDER BY admitted_at_ms ASC LIMIT 1",
            )
            .bind(part)
            .bind(&policy_id_text)
            .fetch_optional(&mut *tx)
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
            tx.commit().await.map_err(map_sqlx_error)?;
            return Ok(CheckAdmissionResult::RateExceeded { retry_after_ms });
        }
    }

    // 4. Concurrency check — UPSERT a zero-row then read under lock.
    let policy_row = sqlx::query(
        "SELECT active_concurrency FROM ff_quota_policy \
         WHERE partition_key = $1 AND quota_policy_id = $2 \
         FOR UPDATE",
    )
    .bind(part)
    .bind(&policy_id_text)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let active: i64 = policy_row
        .map(|r| r.try_get("active_concurrency").unwrap_or(0i64))
        .unwrap_or(0);
    if args.concurrency_cap > 0 && active as u64 >= args.concurrency_cap {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(CheckAdmissionResult::ConcurrencyExceeded);
    }

    // 5. Admit.
    sqlx::query(
        "INSERT INTO ff_quota_window \
            (partition_key, quota_policy_id, dimension, admitted_at_ms, execution_id) \
         VALUES ($1, $2, '', $3, $4)",
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(now)
    .bind(&exec_id_text)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        "INSERT INTO ff_quota_admitted \
            (partition_key, quota_policy_id, execution_id, admitted_at_ms, expires_at_ms) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (partition_key, quota_policy_id, execution_id) DO UPDATE \
            SET admitted_at_ms = EXCLUDED.admitted_at_ms, \
                expires_at_ms = EXCLUDED.expires_at_ms",
    )
    .bind(part)
    .bind(&policy_id_text)
    .bind(&exec_id_text)
    .bind(now)
    .bind(now.saturating_add(window_ms))
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if args.concurrency_cap > 0 {
        // Upsert the policy row's active_concurrency counter. If the
        // row doesn't exist yet, INSERT with count=1 + zero caps —
        // matches the Lua which operates on a raw INCR / SET even
        // before the policy was `create_quota_policy`'d.
        sqlx::query(
            "INSERT INTO ff_quota_policy \
                (partition_key, quota_policy_id, \
                 requests_per_window_seconds, max_requests_per_window, \
                 active_concurrency_cap, active_concurrency, \
                 created_at_ms, updated_at_ms) \
             VALUES ($1, $2, 0, 0, 0, 1, $3, $3) \
             ON CONFLICT (partition_key, quota_policy_id) DO UPDATE \
                SET active_concurrency = ff_quota_policy.active_concurrency + 1, \
                    updated_at_ms = EXCLUDED.updated_at_ms",
        )
        .bind(part)
        .bind(&policy_id_text)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(CheckAdmissionResult::Admitted)
}

/// PG body for [`ff_core::engine_backend::EngineBackend::evaluate_flow_eligibility`].
///
/// Mirrors `ff_evaluate_flow_eligibility` in `flowfabric.lua`. Read-only,
/// returns a textual status code:
///
/// - `not_found` — execution row absent
/// - `not_runnable` — `lifecycle_phase != 'runnable'`
/// - `owned` — `ownership_state != 'unowned'`
/// - `terminal` — `raw_fields.terminal_outcome != 'none'`
/// - `impossible` — at least one incoming edge's upstream is terminal
///   with `outcome='failed'` under an AllOf/required policy
/// - `blocked_by_dependencies` — at least one incoming edge's upstream
///   is not yet terminal under a required policy
/// - `eligible` — none of the above; execution can proceed
///
/// The dependency analysis walks `ff_edge` + joins upstream
/// `ff_exec_core` to derive `unsatisfied_required_count` /
/// `impossible_required_count` live instead of reading a persisted
/// counter (Valkey maintains `ff:deps:<eid>` counters; PG replicates
/// the logic via SQL). Matches cairn's pre-migration control-plane
/// path which already queries `ff_edge` for adjacency.
pub(crate) async fn evaluate_flow_eligibility(
    pool: &PgPool,
    args: EvaluateFlowEligibilityArgs,
) -> Result<EvaluateFlowEligibilityResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;

    // Step 1 — read exec_core state.
    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, ownership_state, flow_id,
               COALESCE(raw_fields->>'terminal_outcome', 'none') AS terminal_outcome
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
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
        // Standalone execution — no dep graph to evaluate.
        return Ok(EvaluateFlowEligibilityResult::Status {
            status: "eligible".into(),
        });
    };

    // Step 2 — walk incoming edges + classify upstreams.
    //
    // Required edges' upstream `ff_exec_core.raw_fields.terminal_outcome`:
    // - `failed` → impossible_required_count
    // - `success` → satisfied (doesn't count)
    // - anything else / missing → unsatisfied_required_count
    //
    // Policy is `jsonb`: we consider a policy "required" if
    // `policy->>'kind' IN ('required', 'AllOf', 'all_of')`. Optional
    // edges (Any-Of / Count policies) are not counted as required.
    let counts = sqlx::query(
        r#"
        SELECT
          SUM(CASE
                WHEN is_required
                  AND COALESCE(up.raw_fields->>'terminal_outcome', 'none') = 'failed'
                THEN 1 ELSE 0 END)::bigint AS impossible_count,
          SUM(CASE
                WHEN is_required
                  AND COALESCE(up.raw_fields->>'terminal_outcome', 'none') NOT IN ('failed', 'success')
                THEN 1 ELSE 0 END)::bigint AS unsatisfied_count
        FROM (
          SELECT e.upstream_eid,
                 (e.policy->>'kind') IN ('required', 'AllOf', 'all_of') AS is_required
            FROM ff_edge e
           WHERE e.partition_key = $1
             AND e.flow_id = $2
             AND e.downstream_eid = $3
        ) edges
        LEFT JOIN ff_exec_core up
          ON up.partition_key = $1 AND up.execution_id = edges.upstream_eid
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


/// PG body for [`ff_core::engine_backend::EngineBackend::claim_execution`].
///
/// Mirrors `ff_claim_execution` in `flowfabric.lua`. Consumes a
/// claim grant, creates a new attempt row, and transitions the
/// execution `runnable → active`.
///
/// Exit conditions (mirroring Lua `err(...)` codes):
///
/// - Execution row absent → `NotFound { entity: "execution" }`
/// - `lifecycle_phase != 'runnable'` → `Contention(ExecutionNotActive { .. })`
/// - `ownership_state != 'unowned'` → `Contention(LeaseConflict)`
/// - `eligibility_state != 'eligible_now'` → `Contention(ExecutionNotLeaseable)`
/// - Active attempt already exists (`attempt_state == 'running_attempt'`)
///   → `Contention(ConflictKind::ActiveAttemptExists)` — defense-in-depth
/// - Attempt-state says resume-required (`attempt_interrupted`) →
///   `Contention(ExecutionNotLeaseable)` (caller must use
///   `claim_resumed_execution`)
/// - No matching claim grant row → `Contention(InvalidClaimGrant)`
/// - Grant's `worker_id` doesn't match → `Contention(InvalidClaimGrant)`
/// - Grant's `expires_at_ms` ≤ now → `Contention(ClaimGrantExpired)`
///
/// On success: new `ff_attempt` row inserted with `lease_epoch =
/// prev + 1`, `ff_exec_core` flipped to `active`/`leased`/`running_attempt`
/// with `current_attempt_id` + `current_lease_id` + `current_worker_*`
/// stashed in `raw_fields`, claim grant row deleted, lease-acquired
/// outbox event emitted.
pub(crate) async fn claim_execution(
    pool: &PgPool,
    partition_config: &PartitionConfig,
    args: ClaimExecutionArgs,
) -> Result<ClaimExecutionResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let attempt_index_i32 =
        i32::try_from(args.expected_attempt_index.0).map_err(|_| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "expected_attempt_index {} exceeds PG i32 column bound",
                args.expected_attempt_index.0
            ),
        })?;
    let lease_ttl_i64 =
        i64::try_from(args.lease_ttl_ms).unwrap_or(i64::MAX);
    let now = args.now.0;
    let new_expires = now.saturating_add(lease_ttl_i64);

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 1. Lock exec_core + full state read.
    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, ownership_state, eligibility_state,
               attempt_state, flow_id,
               COALESCE(raw_fields->>'terminal_outcome', 'none') AS terminal_outcome,
               COALESCE(raw_fields->>'current_attempt_id', '') AS current_attempt_id
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(exec_row) = exec_row else {
        return Err(EngineError::NotFound { entity: "execution" });
    };
    let lifecycle_phase: String = exec_row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
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
        return Err(EngineError::Contention(ContentionKind::ExecutionNotLeaseable));
    }
    if attempt_state == "running_attempt" {
        // Defense-in-depth: invariant A3 from the Lua. Shouldn't be
        // reachable under normal flow (ownership_state would already
        // be `leased`) but we mirror the guard.
        return Err(EngineError::Conflict(
            ff_core::engine_error::ConflictKind::ActiveAttemptExists,
        ));
    }
    if attempt_state == "attempt_interrupted" {
        // Caller must route via claim_resumed_execution.
        return Err(EngineError::Contention(ContentionKind::ExecutionNotLeaseable));
    }

    // 2. Validate + consume claim grant.
    let grant_part = part; // grants live on the same exec partition
    let grant_row = sqlx::query(
        r#"
        SELECT worker_id, expires_at_ms
          FROM ff_claim_grant
         WHERE partition_key = $1 AND execution_id = $2 AND kind = 'claim'
         FOR UPDATE
        "#,
    )
    .bind(grant_part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let Some(grant_row) = grant_row else {
        return Err(EngineError::Contention(ContentionKind::InvalidClaimGrant));
    };
    let grant_worker_id: String = grant_row.try_get("worker_id").map_err(map_sqlx_error)?;
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
         WHERE partition_key = $1 AND execution_id = $2 AND kind = 'claim'",
    )
    .bind(grant_part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 3. Determine attempt type from existing attempt_state.
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

    // 4. Insert / upsert the attempt row at `expected_attempt_index`.
    // Valkey's Lua computes `next_att_idx = total_attempt_count` —
    // PG's callers are expected to pre-read the attempt counter and
    // pass it as `args.expected_attempt_index` (the field is
    // specifically named to reflect that contract). The PK is
    // (partition, execution_id, attempt_index) so each attempt is a
    // distinct row.
    sqlx::query(
        r#"
        INSERT INTO ff_attempt (
            partition_key, execution_id, attempt_index,
            worker_id, worker_instance_id,
            lease_epoch, lease_expires_at_ms, started_at_ms,
            policy, raw_fields
        ) VALUES (
            $1, $2, $3,
            $4, $5,
            1, $6, $7,
            NULLIF($8, '')::jsonb,
            jsonb_build_object(
              'attempt_type', $9::text,
              'attempt_id', $10::text,
              'lease_id', $11::text
            )
        )
        ON CONFLICT (partition_key, execution_id, attempt_index)
        DO UPDATE SET
            worker_id = EXCLUDED.worker_id,
            worker_instance_id = EXCLUDED.worker_instance_id,
            lease_epoch = ff_attempt.lease_epoch + 1,
            lease_expires_at_ms = EXCLUDED.lease_expires_at_ms,
            started_at_ms = COALESCE(ff_attempt.started_at_ms, EXCLUDED.started_at_ms),
            policy = EXCLUDED.policy,
            raw_fields = ff_attempt.raw_fields || EXCLUDED.raw_fields,
            terminal_at_ms = NULL,
            outcome = NULL
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index_i32)
    .bind(args.worker_id.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(new_expires)
    .bind(now)
    .bind(&args.attempt_policy_json)
    .bind(attempt_type_str)
    .bind(args.attempt_id.to_string())
    .bind(args.lease_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 5. Read back the new epoch (may be +1 over INSERT on the
    // conflict path).
    let epoch_row = sqlx::query(
        "SELECT lease_epoch FROM ff_attempt \
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index_i32)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let new_epoch_i: i64 = epoch_row.try_get("lease_epoch").map_err(map_sqlx_error)?;
    let new_epoch = u64::try_from(new_epoch_i).unwrap_or(0);

    // 6. Flip exec_core to active + stash lease identity in raw_fields.
    let raw_patch = serde_json::json!({
        "current_attempt_id": args.attempt_id.to_string(),
        "current_lease_id": args.lease_id.to_string(),
        "current_worker_id": args.worker_id.as_str(),
        "current_worker_instance_id": args.worker_instance_id.as_str(),
        "pending_retry_reason": "",
        "pending_replay_reason": "",
        "pending_replay_requested_by": "",
        "pending_previous_attempt_index": "",
    });
    let next_attempt_index_i = attempt_index_i32.saturating_add(1);
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'active',
               ownership_state = 'leased',
               eligibility_state = 'not_applicable',
               attempt_state = 'running_attempt',
               public_state = 'active',
               attempt_index = $1,
               deadline_at_ms = COALESCE($2, deadline_at_ms),
               raw_fields = raw_fields || $3::jsonb
         WHERE partition_key = $4 AND execution_id = $5
        "#,
    )
    .bind(next_attempt_index_i)
    .bind(args.execution_deadline_at)
    .bind(raw_patch)
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 7. Outbox: lease acquired.
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        None,
        lease_event::EVENT_ACQUIRED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let _ = partition_config; // reserved for a future partition cross-check
    let claimed = ClaimedExecution::new(
        args.execution_id,
        args.lease_id,
        ff_core::types::LeaseEpoch(new_epoch),
        args.expected_attempt_index,
        args.attempt_id,
        attempt_type,
        TimestampMs::from_millis(new_expires),
        ff_core::backend::stub_handle_fresh(),
    );
    Ok(ClaimExecutionResult::Claimed(claimed))
}

/// PG body for [`ff_core::engine_backend::EngineBackend::issue_grant_and_claim`].
///
/// Cairn #454 Phase 4c — backend-atomic composition of `issue_claim_grant`
/// + `claim_execution` in a single sqlx transaction. Mirrors Valkey's
/// `ff_issue_grant_and_claim` FCALL (Phase 3d, `c884bac`) with PG's
/// lease-epoch-only fence.
///
/// # Atomicity
///
/// `tx.begin()` … `tx.commit()`. sqlx `Transaction` auto-rolls back on
/// drop, so any `?`-bubbled error discards the inserted grant row +
/// attempt mutations — the PG equivalent of Valkey's Lua FCALL
/// single-serial guarantee.
///
/// # Dispatch
///
/// Branches on `ff_exec_core.attempt_state`:
///
/// - `attempt_interrupted` → resume-claim body (mirrors
///   [`crate::suspend_ops::claim_resumed_execution_impl`]): existing
///   attempt row is re-leased with bumped epoch; `attempt_index` is
///   reused.
/// - anything else → fresh-claim body: same gates as `claim_execution`
///   (`runnable` / `unowned` / `eligible_now`); mints a new attempt
///   row at `ff_exec_core.attempt_index` and bumps the pointer.
///
/// # Operator identity
///
/// The trait-level args are operator-facing (`execution_id`, `lane_id`,
/// `lease_duration_ms`); worker identity is synthesized as
/// `WorkerId("operator")` / `WorkerInstanceId("operator")` to keep
/// audit columns populated (matches Valkey Phase 3d decision).
///
/// # Grant row
///
/// Written with `grant_ttl_ms = lease_duration_ms`, then DELETE'd in
/// the same tx. The TTL is redundant on the happy path but audit-
/// consistent with Valkey's `PEXPIRE` on the Lua path.
pub(crate) async fn issue_grant_and_claim(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    args: IssueGrantAndClaimArgs,
) -> Result<ClaimGrantOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let lease_ttl_i64 = i64::try_from(args.lease_duration_ms).unwrap_or(i64::MAX);
    let now = now_ms();
    let new_expires = now.saturating_add(lease_ttl_i64);

    // Synthetic operator identity — mirrors Valkey Phase 3d.
    let worker_id = WorkerId::new("operator");
    let worker_instance_id = WorkerInstanceId::new("operator");
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 1. Lock exec_core + read state.
    let exec_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, ownership_state, eligibility_state,
               attempt_state, attempt_index,
               COALESCE(raw_fields->>'terminal_outcome', 'none') AS terminal_outcome,
               COALESCE(raw_fields->>'current_attempt_id', '') AS current_attempt_id
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(exec_row) = exec_row else {
        return Err(EngineError::NotFound { entity: "execution" });
    };
    let lifecycle_phase: String = exec_row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let ownership_state: String = exec_row.try_get("ownership_state").map_err(map_sqlx_error)?;
    let eligibility_state: String =
        exec_row.try_get("eligibility_state").map_err(map_sqlx_error)?;
    let attempt_state: String = exec_row.try_get("attempt_state").map_err(map_sqlx_error)?;
    let current_attempt_index: i32 =
        exec_row.try_get("attempt_index").map_err(map_sqlx_error)?;

    let is_resume = attempt_state == "attempt_interrupted";

    if !is_resume {
        // Fresh-claim gates — same as `claim_execution`.
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
            // Operator composition can run against both `eligible_now`
            // (no grant yet) and `pending_claim` (a scheduler-issued
            // grant already exists — our grant is additive and the
            // caller semantics are unchanged).
            return Err(EngineError::Contention(ContentionKind::ExecutionNotLeaseable));
        }
        if attempt_state == "running_attempt" {
            return Err(EngineError::Conflict(
                ff_core::engine_error::ConflictKind::ActiveAttemptExists,
            ));
        }
    }

    // 2. Insert grant row (audit trail). Random grant_id; upsert on
    //    conflict so a re-issue on the same partition is idempotent.
    let grant_id: Vec<u8> = uuid::Uuid::new_v4().as_bytes().to_vec();
    sqlx::query(
        r#"
        INSERT INTO ff_claim_grant (
            partition_key, grant_id, execution_id, kind,
            worker_id, worker_instance_id, lane_id,
            capability_hash, worker_capabilities,
            route_snapshot_json, admission_summary,
            grant_ttl_ms, issued_at_ms, expires_at_ms
        ) VALUES (
            $1, $2, $3, 'claim',
            $4, $5, $6,
            NULL, '[]'::jsonb,
            NULL, NULL,
            $7, $8, $9
        )
        ON CONFLICT (partition_key, grant_id) DO NOTHING
        "#,
    )
    .bind(part)
    .bind(&grant_id)
    .bind(exec_uuid)
    .bind(worker_id.as_str())
    .bind(worker_instance_id.as_str())
    .bind(args.lane_id.as_str())
    .bind(lease_ttl_i64)
    .bind(now)
    .bind(new_expires)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 3. Claim body — branch on attempt_state.
    let (used_attempt_index_i32, new_epoch_i): (i32, i64) = if is_resume {
        // Resume path: re-lease existing attempt row at the current
        // index (matches `claim_resumed_execution_impl`).
        sqlx::query(
            "UPDATE ff_attempt \
                SET worker_id = $1, worker_instance_id = $2, \
                    lease_epoch = lease_epoch + 1, \
                    lease_expires_at_ms = $3, started_at_ms = $4, outcome = NULL \
              WHERE partition_key = $5 AND execution_id = $6 AND attempt_index = $7",
        )
        .bind(worker_id.as_str())
        .bind(worker_instance_id.as_str())
        .bind(new_expires)
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .bind(current_attempt_index)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(
            "UPDATE ff_exec_core \
                SET lifecycle_phase = 'active', ownership_state = 'leased', \
                    eligibility_state = 'not_applicable', \
                    public_state = 'running', attempt_state = 'running_attempt', \
                    started_at_ms = COALESCE(started_at_ms, $3) \
              WHERE partition_key = $1 AND execution_id = $2",
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let (epoch,): (i64,) = sqlx::query_as(
            "SELECT lease_epoch FROM ff_attempt \
              WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3",
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(current_attempt_index)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        (current_attempt_index, epoch)
    } else {
        // Fresh path: mint a new attempt row at
        // `ff_exec_core.attempt_index` and bump the pointer.
        let attempt_index_i32 = current_attempt_index;
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
        sqlx::query(
            r#"
            INSERT INTO ff_attempt (
                partition_key, execution_id, attempt_index,
                worker_id, worker_instance_id,
                lease_epoch, lease_expires_at_ms, started_at_ms,
                policy, raw_fields
            ) VALUES (
                $1, $2, $3,
                $4, $5,
                1, $6, $7,
                NULL,
                jsonb_build_object(
                  'attempt_type', $8::text,
                  'attempt_id', $9::text,
                  'lease_id', $10::text
                )
            )
            ON CONFLICT (partition_key, execution_id, attempt_index)
            DO UPDATE SET
                worker_id = EXCLUDED.worker_id,
                worker_instance_id = EXCLUDED.worker_instance_id,
                lease_epoch = ff_attempt.lease_epoch + 1,
                lease_expires_at_ms = EXCLUDED.lease_expires_at_ms,
                started_at_ms = COALESCE(ff_attempt.started_at_ms, EXCLUDED.started_at_ms),
                raw_fields = ff_attempt.raw_fields || EXCLUDED.raw_fields,
                terminal_at_ms = NULL,
                outcome = NULL
            "#,
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index_i32)
        .bind(worker_id.as_str())
        .bind(worker_instance_id.as_str())
        .bind(new_expires)
        .bind(now)
        .bind(attempt_type_str)
        .bind(attempt_id.to_string())
        .bind(lease_id.to_string())
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let (epoch,): (i64,) = sqlx::query_as(
            "SELECT lease_epoch FROM ff_attempt \
              WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3",
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index_i32)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let raw_patch = serde_json::json!({
            "current_attempt_id": attempt_id.to_string(),
            "current_lease_id": lease_id.to_string(),
            "current_worker_id": worker_id.as_str(),
            "current_worker_instance_id": worker_instance_id.as_str(),
            "pending_retry_reason": "",
            "pending_replay_reason": "",
            "pending_replay_requested_by": "",
            "pending_previous_attempt_index": "",
        });
        let next_attempt_index_i = attempt_index_i32.saturating_add(1);
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'active',
                   ownership_state = 'leased',
                   eligibility_state = 'not_applicable',
                   attempt_state = 'running_attempt',
                   public_state = 'running',
                   attempt_index = $1,
                   raw_fields = raw_fields || $2::jsonb
             WHERE partition_key = $3 AND execution_id = $4
            "#,
        )
        .bind(next_attempt_index_i)
        .bind(raw_patch)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        (attempt_index_i32, epoch)
    };

    // 4. DELETE the grant row (same tx). The random grant_id from
    //    step 2 lives on only for audit purposes — composition is
    //    complete so the row is reaped.
    sqlx::query(
        "DELETE FROM ff_claim_grant \
         WHERE partition_key = $1 AND grant_id = $2",
    )
    .bind(part)
    .bind(&grant_id)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 5. Outbox: lease acquired.
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        Some(&lease_id.to_string()),
        lease_event::EVENT_ACQUIRED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let attempt_index =
        AttemptIndex::new(u32::try_from(used_attempt_index_i32.max(0)).unwrap_or(0));
    let new_epoch = u64::try_from(new_epoch_i).unwrap_or(0);
    Ok(ClaimGrantOutcome::new(
        lease_id,
        LeaseEpoch(new_epoch),
        attempt_index,
    ))
}

#[cfg(test)]
mod tests {
    // Integration tests live in `tests/typed_renew_lease.rs` +
    // sibling files per-method; they require a live Postgres
    // (`FF_PG_TEST_URL`). This module holds unit tests that don't
    // touch the DB.

    use super::*;
    use ff_core::types::{AttemptId, AttemptIndex, ExecutionId, LeaseEpoch, LeaseFence, LeaseId};

    // NOTE: the `renew_lease` fence-missing short-circuit is behavior-
    // covered by the ignore-gated integration test
    // `renew_lease_rejects_missing_fence` in
    // `tests/typed_renew_lease.rs`, which executes `renew_lease` end-to-
    // end with a live PG pool. A pool-free unit test of that gate can't
    // exercise the function body (we can't construct a `PgPool`), so
    // we don't inline a placeholder here.

    #[test]
    fn lease_fence_shape() {
        // Cheap compile-time check that the fence triple shape
        // matches what the PG body reads out of raw_fields.
        let f = LeaseFence {
            lease_id: LeaseId::new(),
            lease_epoch: LeaseEpoch(1),
            attempt_id: AttemptId::new(),
        };
        assert!(!f.lease_id.to_string().is_empty());
        assert!(!f.attempt_id.to_string().is_empty());
    }
}
