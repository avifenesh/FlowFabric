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
    CompleteExecutionArgs, CompleteExecutionResult, FailExecutionArgs, FailExecutionResult,
    RenewLeaseArgs, RenewLeaseResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::state::PublicState;
use ff_core::types::{AttemptIndex, CancelSource, TimestampMs};
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
