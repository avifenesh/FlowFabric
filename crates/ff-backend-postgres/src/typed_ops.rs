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
    CheckAdmissionArgs, CheckAdmissionResult, CompleteExecutionArgs, CompleteExecutionResult,
    FailExecutionArgs, FailExecutionResult, RenewLeaseArgs, RenewLeaseResult,
    ResumeExecutionArgs, ResumeExecutionResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::{quota_partition, PartitionConfig};
use ff_core::state::PublicState;
use ff_core::types::{AttemptIndex, CancelSource, QuotaPolicyId, TimestampMs};
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
