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

use ff_core::contracts::{RenewLeaseArgs, RenewLeaseResult};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
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
