//! `SqliteBackend` — RFC-023 dev-only SQLite [`EngineBackend`] impl.
//!
//! Phase 1a lands the scaffolding: construction guard, registry
//! dedup, pool setup, WARN banner, and Unavailable stubs for every
//! required trait method. Phase 2+ progressively replaces the stubs
//! with real bodies paralleling `ff-backend-postgres`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use ff_core::backend::PrepareOutcome;
use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy, FailOutcome,
    FailureClass, FailureReason, Frame, FrameKind, Handle, HandleKind, LeaseRenewal, PatchKind,
    PendingWaitpoint, ReclaimToken, ResumeSignal, SUMMARY_NULL_SENTINEL, StreamMode,
    UsageDimensions,
};
use ff_core::capability::{BackendIdentity, Capabilities, Supports, Version};
use ff_core::caps::{CapabilityRequirement, matches as caps_matches};
use ff_core::contracts::{
    CancelFlowResult, ExecutionSnapshot, FlowSnapshot, ReportUsageResult, SuspendArgs,
    SuspendOutcome,
};
#[cfg(feature = "core")]
use ff_core::contracts::{
    ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs, DeliverSignalResult,
    EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ListExecutionsPage, ListFlowsPage,
    ListLanesPage, ListSuspendedPage, SetEdgeGroupPolicyResult,
};
#[cfg(feature = "streaming")]
use ff_core::contracts::{StreamCursor, StreamFrames};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{BackendError, ContentionKind, EngineError, ValidationKind};
use ff_core::handle_codec::HandlePayload;
use ff_core::types::{AttemptId, AttemptIndex, LeaseEpoch, LeaseId};

use crate::errors::map_sqlx_error;
use crate::handle_codec::{decode_handle, encode_handle};
use crate::queries::{
    attempt as q_attempt, exec_core as q_exec, lease as q_lease, stream as q_stream,
};
use crate::retry::retry_serializable;
#[cfg(feature = "core")]
use ff_core::partition::PartitionKey;
#[cfg(feature = "core")]
use ff_core::types::EdgeId;
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};

use crate::pubsub::PubSub;
use crate::registry;

/// Phase-1a-wide `Unavailable` helper. Each stubbed method names
/// itself here so call-site errors carry a stable identifier.
#[inline]
fn unavailable<T>(op: &'static str) -> Result<T, EngineError> {
    Err(EngineError::Unavailable { op })
}

// ── Phase 2a.2 helpers: hot-path shared logic ──────────────────────────

/// Unix-millis wall clock. Matches the PG reference shape
/// (`ff-backend-postgres/src/attempt.rs:55-63`); SQLite stores the
/// same `*_ms` fields so the value is directly comparable.
fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

/// Decompose an [`ff_core::types::ExecutionId`] formatted `{fp:N}:<uuid>`
/// into `(partition_index, uuid_bytes)` — SQLite stores the UUID as a
/// 16-byte `BLOB` (§4.1) so we bind via `uuid::Uuid`.
fn split_exec_id(eid: &ff_core::types::ExecutionId) -> Result<(i64, Uuid), EngineError> {
    let s = eid.as_str();
    let rest = s
        .strip_prefix("{fp:")
        .ok_or_else(|| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution_id missing `{{fp:` prefix: {s}"),
        })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `}}:`: {s}"),
    })?;
    let part: i64 = rest[..close].parse().map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id partition index not u16: {s}"),
    })?;
    let uuid = Uuid::parse_str(&rest[close + 2..]).map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id UUID invalid: {s}"),
    })?;
    Ok((part, uuid))
}

/// Acquire a pooled connection and issue `BEGIN IMMEDIATE`, escalating
/// the txn to RESERVED so §4.1 A3's single-writer invariant holds for
/// the full read-modify-write window.
///
/// The caller MUST arrange an explicit `commit()` on success and a
/// `rollback_quiet()` on every error path. Use
/// [`commit_or_rollback`] as the single tail-call so a `COMMIT`
/// failure deterministically rolls back — otherwise a half-open txn
/// could return to the pool and poison a later borrower.
///
/// sqlx's `Transaction` abstraction opens a plain `BEGIN` on SQLite
/// (no `IMMEDIATE` escalation on the public API today); we manage
/// the lock here manually and the per-op helpers in this file close
/// the rollback loop.
async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, EngineError> {
    let mut conn = pool.acquire().await.map_err(map_sqlx_error)?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(conn)
}

/// Commit the pending txn; on `COMMIT` failure issue a best-effort
/// `ROLLBACK` so the connection is returned to the pool in a clean
/// state (otherwise a pool-reuse borrower observes a half-open txn).
/// A secondary rollback error is swallowed — SQLite auto-rolls-back
/// on connection close, which happens when the pool drops an
/// unhealthy connection, so correctness is preserved.
async fn commit_or_rollback(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
) -> Result<(), EngineError> {
    if let Err(e) = sqlx::query("COMMIT")
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)
    {
        let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
        return Err(e);
    }
    Ok(())
}

/// Best-effort rollback on an error path. A failed rollback is
/// swallowed so the original error surfaces unchanged.
async fn rollback_quiet(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) {
    let _ = sqlx::query("ROLLBACK").execute(&mut **conn).await;
}

/// Fence check: under the `BEGIN IMMEDIATE` lock, read the attempt
/// row's `lease_epoch` and compare against the handle-embedded epoch.
/// Mismatch ⇒ [`ContentionKind::LeaseConflict`] (terminal for this
/// call; caller does not retry a fence mismatch).
async fn fence_check(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    attempt_index: i64,
    expected_epoch: u64,
) -> Result<(), EngineError> {
    let row = sqlx::query(q_attempt::SELECT_ATTEMPT_EPOCH_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let Some(row) = row else {
        return Err(EngineError::NotFound { entity: "attempt" });
    };
    let epoch_i: i64 = row.try_get("lease_epoch").map_err(map_sqlx_error)?;
    let observed = u64::try_from(epoch_i).unwrap_or(0);
    if observed != expected_epoch {
        return Err(EngineError::Contention(ContentionKind::LeaseConflict));
    }
    Ok(())
}

// ── Phase 2a.2 hot-path bodies ─────────────────────────────────────────

async fn claim_impl(
    pool: &SqlitePool,
    lane: &ff_core::types::LaneId,
    capabilities: &CapabilitySet,
    policy: &ClaimPolicy,
) -> Result<Option<Handle>, EngineError> {
    // RFC-023 §4.1 A3: SQLite is single-writer with
    // `num_flow_partitions = 1`, so we scan only partition 0 rather
    // than iterating 0..256 as the PG path does.
    let part: i64 = 0;

    let mut conn = begin_immediate(pool).await?;
    let result = claim_inner(&mut conn, part, lane, capabilities, policy).await;
    match result {
        Ok(Some(handle)) => {
            commit_or_rollback(&mut conn).await?;
            Ok(Some(handle))
        }
        Ok(None) => {
            rollback_quiet(&mut conn).await;
            Ok(None)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

/// Inside-txn body of [`claim_impl`] — any `?` short-circuit surfaces
/// to the caller which guarantees `rollback_quiet` via
/// [`claim_impl`]'s match arms.
async fn claim_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    lane: &ff_core::types::LaneId,
    capabilities: &CapabilitySet,
    policy: &ClaimPolicy,
) -> Result<Option<Handle>, EngineError> {
    // Scan up to CAP_SCAN_BATCH eligible rows in priority order and
    // walk until we find the first capability-satisfying one. Under
    // §4.1 A3 SQLite runs on a single partition, so a high-priority
    // row whose required caps the worker lacks would starve
    // downstream-priority matches if we only inspected the top
    // candidate (caught in PR-375 review). Bounded scan budget keeps
    // the lock window predictable.
    const CAP_SCAN_BATCH: i64 = 16;

    let candidate_rows = sqlx::query(q_attempt::SELECT_ELIGIBLE_EXEC_SQL)
        .bind(part)
        .bind(lane.as_str())
        .bind(CAP_SCAN_BATCH)
        .fetch_all(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    if candidate_rows.is_empty() {
        return Ok(None);
    }

    let mut claimable: Option<(Uuid, i64)> = None;
    for row in &candidate_rows {
        let exec_uuid: Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
        let attempt_index_i: i64 = row.try_get("attempt_index").map_err(map_sqlx_error)?;

        // Capability subset check (§4.1 A4): junction table read +
        // Rust-side `caps::matches`. Same post-lock Rust match as the
        // PG path at `ff-backend-postgres/src/attempt.rs:170-182`.
        let cap_rows = sqlx::query(q_attempt::SELECT_EXEC_CAPABILITIES_SQL)
            .bind(exec_uuid)
            .fetch_all(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        let tokens: Vec<String> = cap_rows
            .iter()
            .map(|r| r.try_get::<String, _>("capability"))
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlx_error)?;
        let req = CapabilityRequirement::new(tokens);
        if caps_matches(&req, capabilities) {
            claimable = Some((exec_uuid, attempt_index_i));
            break;
        }
    }

    let Some((exec_uuid, attempt_index_i)) = claimable else {
        // Every candidate in the batch required a capability the
        // worker lacks; surface `None` so the caller's retry cadence
        // re-enters later. A different-caps worker takes the batch
        // when it claims.
        return Ok(None);
    };

    let now = now_ms();
    let lease_ttl_ms = i64::from(policy.lease_ttl_ms);
    let expires = now.saturating_add(lease_ttl_ms);

    // UPSERT the attempt row. `RETURNING lease_epoch` round-trips
    // the post-UPSERT epoch in one statement (SQLite >= 3.35).
    let epoch_row = sqlx::query(q_attempt::UPSERT_ATTEMPT_ON_CLAIM_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index_i)
        .bind(policy.worker_id.as_str())
        .bind(policy.worker_instance_id.as_str())
        .bind(expires)
        .bind(now)
        .fetch_one(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let epoch_i: i64 = epoch_row.try_get("lease_epoch").map_err(map_sqlx_error)?;

    sqlx::query(q_exec::UPDATE_EXEC_CORE_CLAIM_SQL)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox parity (PG reference at
    // `ff-backend-postgres/src/lease_event.rs`): record a lease
    // lifecycle event so a later `subscribe_lease_history` reader
    // observes the acquisition. The PG `pg_notify` trigger is dropped
    // per §4.2; in-Rust broadcast wiring lands in a later phase.
    insert_lease_event(conn, part, exec_uuid, "acquired", now).await?;

    let attempt_index = AttemptIndex::new(u32::try_from(attempt_index_i.max(0)).unwrap_or(0));
    let exec_id = ff_core::types::ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}"))
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("reassembling exec id: {e}"),
        })?;
    let payload = HandlePayload::new(
        exec_id,
        attempt_index,
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(u64::try_from(epoch_i).unwrap_or(1)),
        u64::from(policy.lease_ttl_ms),
        lane.clone(),
        policy.worker_instance_id.clone(),
    );
    Ok(Some(encode_handle(&payload, HandleKind::Fresh)))
}

async fn complete_impl(
    pool: &SqlitePool,
    handle: &Handle,
    payload_bytes: Option<Vec<u8>>,
) -> Result<(), EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i64::from(payload.attempt_index.0);
    let expected_epoch = payload.lease_epoch.0;

    let mut conn = begin_immediate(pool).await?;
    let result = complete_inner(
        &mut conn,
        part,
        exec_uuid,
        attempt_index,
        expected_epoch,
        payload_bytes,
    )
    .await;
    match result {
        Ok(()) => commit_or_rollback(&mut conn).await,
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

async fn complete_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    attempt_index: i64,
    expected_epoch: u64,
    payload_bytes: Option<Vec<u8>>,
) -> Result<(), EngineError> {
    fence_check(conn, part, exec_uuid, attempt_index, expected_epoch).await?;
    let now = now_ms();

    sqlx::query(q_attempt::UPDATE_ATTEMPT_COMPLETE_SQL)
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    sqlx::query(q_exec::UPDATE_EXEC_CORE_COMPLETE_SQL)
        .bind(now)
        .bind(payload_bytes.as_deref())
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    sqlx::query(q_attempt::INSERT_COMPLETION_EVENT_SQL)
        .bind("success")
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    insert_lease_event(conn, part, exec_uuid, "revoked", now).await?;
    Ok(())
}

/// Classify whether a `fail()` call reschedules a retry or transitions
/// to terminal. Mirrors the PG reference behaviour
/// (`ff-backend-postgres/src/attempt.rs:622-626` — Transient /
/// InfraCrash → retry; Permanent / Timeout / Cancelled → terminal),
/// and handles the `#[non_exhaustive]` catch-all by defaulting future
/// variants to the **least-destructive** retry path per the project's
/// non-exhaustive-enum rule: terminal-failed is irreversible, so an
/// unknown classification MUST NOT silently burn the attempt.
fn classify_retryable(classification: FailureClass) -> bool {
    match classification {
        FailureClass::Transient | FailureClass::InfraCrash => true,
        FailureClass::Permanent | FailureClass::Timeout | FailureClass::Cancelled => false,
        // #[non_exhaustive]: unknown future variant → retry (least
        // destructive). A deliberate terminal variant is fine to add
        // here alongside Permanent in a follow-up PR; defaulting
        // unknowns to terminal would regress outcomes on backend
        // upgrades where a new variant lands before this classifier
        // is taught about it.
        _ => true,
    }
}

async fn fail_impl(
    pool: &SqlitePool,
    handle: &Handle,
    reason: FailureReason,
    classification: FailureClass,
) -> Result<FailOutcome, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i64::from(payload.attempt_index.0);
    let expected_epoch = payload.lease_epoch.0;
    let retryable = classify_retryable(classification);

    let mut conn = begin_immediate(pool).await?;
    let result = fail_inner(
        &mut conn,
        part,
        exec_uuid,
        attempt_index,
        expected_epoch,
        retryable,
        &reason,
        classification,
    )
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

#[allow(clippy::too_many_arguments)] // every arg is load-bearing attempt state
async fn fail_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    attempt_index: i64,
    expected_epoch: u64,
    retryable: bool,
    reason: &FailureReason,
    classification: FailureClass,
) -> Result<FailOutcome, EngineError> {
    fence_check(conn, part, exec_uuid, attempt_index, expected_epoch).await?;
    let now = now_ms();

    if retryable {
        sqlx::query(q_attempt::UPDATE_ATTEMPT_FAIL_RETRY_SQL)
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(q_exec::UPDATE_EXEC_CORE_FAIL_RETRY_SQL)
            .bind(&reason.message)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        insert_lease_event(conn, part, exec_uuid, "revoked", now).await?;
        // Log the transient failure so operators tracing a retry loop
        // can correlate cause without re-reading the attempt row
        // themselves (Gemini review #1).
        tracing::warn!(
            error.message = %reason.message,
            classification = ?classification,
            execution_id = %exec_uuid,
            attempt_index = attempt_index,
            "sqlite.fail: scheduling retry"
        );
        Ok(FailOutcome::RetryScheduled {
            delay_until: ff_core::types::TimestampMs::from_millis(now),
        })
    } else {
        sqlx::query(q_attempt::UPDATE_ATTEMPT_FAIL_TERMINAL_SQL)
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(q_exec::UPDATE_EXEC_CORE_FAIL_TERMINAL_SQL)
            .bind(now)
            .bind(&reason.message)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(q_attempt::INSERT_COMPLETION_EVENT_SQL)
            .bind("failed")
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        insert_lease_event(conn, part, exec_uuid, "revoked", now).await?;
        Ok(FailOutcome::TerminalFailed)
    }
}

/// Emit one RFC-019 Stage B lease-lifecycle outbox row.
///
/// Mirrors `ff-backend-postgres/src/lease_event.rs`. The PG
/// `pg_notify` trigger is dropped per RFC-023 §4.2 (broadcast moves
/// into a Rust post-commit channel in a later phase); durable replay
/// via `event_id > cursor` continues to ride against this table.
async fn insert_lease_event(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    event_type: &str,
    now: i64,
) -> Result<(), EngineError> {
    sqlx::query(
        r#"
        INSERT INTO ff_lease_event (
            execution_id, lease_id, event_type, occurred_at_ms, partition_key
        ) VALUES (?1, NULL, ?2, ?3, ?4)
        "#,
    )
    .bind(exec_uuid.to_string())
    .bind(event_type)
    .bind(now)
    .bind(part)
    .execute(&mut **conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

// ── Phase 2a.3 hot-path bodies ────────────────────────────────────────

async fn renew_impl(pool: &SqlitePool, handle: &Handle) -> Result<LeaseRenewal, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i64::from(payload.attempt_index.0);
    let expected_epoch = payload.lease_epoch.0;
    let lease_ttl_ms = i64::try_from(payload.lease_ttl_ms).unwrap_or(0);

    let mut conn = begin_immediate(pool).await?;
    let result = renew_inner(
        &mut conn,
        part,
        exec_uuid,
        attempt_index,
        expected_epoch,
        lease_ttl_ms,
    )
    .await;
    match result {
        Ok(renewal) => {
            commit_or_rollback(&mut conn).await?;
            Ok(renewal)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

async fn renew_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    attempt_index: i64,
    expected_epoch: u64,
    lease_ttl_ms: i64,
) -> Result<LeaseRenewal, EngineError> {
    fence_check(conn, part, exec_uuid, attempt_index, expected_epoch).await?;
    let now = now_ms();
    let new_expires = now.saturating_add(lease_ttl_ms);

    sqlx::query(q_lease::UPDATE_ATTEMPT_RENEW_SQL)
        .bind(new_expires)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox parity: lease renewed event.
    insert_lease_event(conn, part, exec_uuid, "renewed", now).await?;

    Ok(LeaseRenewal::new(
        u64::try_from(new_expires).unwrap_or(0),
        expected_epoch,
    ))
}

async fn progress_impl(
    pool: &SqlitePool,
    handle: &Handle,
    percent: Option<u8>,
    message: Option<String>,
) -> Result<(), EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i64::from(payload.attempt_index.0);
    let expected_epoch = payload.lease_epoch.0;

    let mut conn = begin_immediate(pool).await?;
    let result = progress_inner(
        &mut conn,
        part,
        exec_uuid,
        attempt_index,
        expected_epoch,
        percent,
        message,
    )
    .await;
    match result {
        Ok(()) => commit_or_rollback(&mut conn).await,
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

async fn progress_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    attempt_index: i64,
    expected_epoch: u64,
    percent: Option<u8>,
    message: Option<String>,
) -> Result<(), EngineError> {
    fence_check(conn, part, exec_uuid, attempt_index, expected_epoch).await?;

    // `UPDATE_EXEC_CORE_PROGRESS_SQL` is self-correct for any NULL/
    // non-NULL combination of the two binds (PR #376 Copilot review) —
    // its nested `CASE WHEN ? IS NULL` shape treats each field as
    // independent and leaves the corresponding JSON path absent when
    // the caller passed None. No Rust-side short-circuit needed.
    sqlx::query(q_exec::UPDATE_EXEC_CORE_PROGRESS_SQL)
        .bind(percent.map(i64::from))
        .bind(message)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

// ── append_frame (RFC-015 write surface) ──────────────────────────────

/// Apply one RFC 7396 JSON Merge Patch in-place. Mirrors the PG helper
/// at `ff-backend-postgres/src/stream.rs::apply_json_merge_patch`;
/// both implementations must honour the [`SUMMARY_NULL_SENTINEL`]
/// rewrite (leaf `"__ff_null__"` → JSON `null`) so the round-trip
/// invariant holds across backends.
fn apply_json_merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    use serde_json::Value;
    if let Value::Object(patch_map) = patch {
        if !target.is_object() {
            *target = Value::Object(serde_json::Map::new());
        }
        let target_map = target.as_object_mut().expect("just ensured object");
        for (k, v) in patch_map {
            match v {
                Value::Null => {
                    target_map.remove(k);
                }
                Value::String(s) if s == SUMMARY_NULL_SENTINEL => {
                    target_map.insert(k.clone(), Value::Null);
                }
                Value::Object(_) => {
                    let entry = target_map.entry(k.clone()).or_insert(Value::Null);
                    apply_json_merge_patch(entry, v);
                }
                other => {
                    target_map.insert(k.clone(), other.clone());
                }
            }
        }
    } else {
        *target = patch.clone();
    }
}

/// Build the `fields` JSON TEXT blob for a frame — mirrors the PG
/// helper at `ff-backend-postgres/src/stream.rs::build_fields_json`
/// so downstream readers observe the same shape on both backends.
fn build_fields_json(frame: &Frame) -> String {
    use serde_json::{Map, Value};
    let payload_str = String::from_utf8_lossy(&frame.bytes).into_owned();
    let mut map = Map::new();
    let frame_type = if frame.frame_type.is_empty() {
        match frame.kind {
            FrameKind::Stdout => "stdout",
            FrameKind::Stderr => "stderr",
            FrameKind::Event => "event",
            FrameKind::Blob => "blob",
            _ => "event",
        }
        .to_owned()
    } else {
        frame.frame_type.clone()
    };
    map.insert("frame_type".into(), Value::String(frame_type));
    map.insert("payload".into(), Value::String(payload_str));
    map.insert("encoding".into(), Value::String("utf8".into()));
    map.insert("source".into(), Value::String("worker".into()));
    if let Some(corr) = &frame.correlation_id {
        map.insert("correlation_id".into(), Value::String(corr.clone()));
    }
    Value::Object(map).to_string()
}

async fn append_frame_impl(
    pool: &SqlitePool,
    handle: &Handle,
    frame: Frame,
) -> Result<AppendFrameOutcome, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i64::from(payload.attempt_index.0);
    let expected_epoch = payload.lease_epoch.0;

    let mut conn = begin_immediate(pool).await?;
    let result = append_frame_inner(
        &mut conn,
        part,
        exec_uuid,
        attempt_index,
        expected_epoch,
        frame,
    )
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

async fn append_frame_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    attempt_index: i64,
    expected_epoch: u64,
    frame: Frame,
) -> Result<AppendFrameOutcome, EngineError> {
    fence_check(conn, part, exec_uuid, attempt_index, expected_epoch).await?;

    let ts_ms = now_ms();
    let mode_wire = frame.mode.wire_str();
    let fields_text = build_fields_json(&frame);

    // Mint `seq` as MAX(seq) + 1 under the txn lock. `BEGIN IMMEDIATE`
    // serializes writers so there is no need for an additional advisory
    // lock (the PG path uses `pg_advisory_xact_lock` because READ
    // COMMITTED isolates less strictly).
    let max_seq: Option<i64> = sqlx::query_scalar(q_stream::SELECT_MAX_SEQ_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .bind(ts_ms)
        .fetch_one(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let next_seq: i64 = max_seq.map(|s| s + 1).unwrap_or(0);

    sqlx::query(q_stream::INSERT_STREAM_FRAME_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .bind(ts_ms)
        .bind(next_seq)
        .bind(&fields_text)
        .bind(mode_wire)
        .bind(ts_ms)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    let mut summary_version: Option<u64> = None;

    // DurableSummary: JSON Merge Patch applied in Rust, TEXT in/out.
    if let StreamMode::DurableSummary { patch_kind } = &frame.mode {
        let patch: serde_json::Value =
            serde_json::from_slice(&frame.bytes).map_err(|e| EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("summary patch not valid JSON: {e}"),
            })?;

        let existing: Option<(String, i64)> = sqlx::query_as(q_stream::SELECT_STREAM_SUMMARY_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .fetch_optional(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        let (mut doc, prev_version): (serde_json::Value, i64) = match existing {
            Some((text, v)) => {
                // Strict-parse posture (PR #376 gemini review): a stored
                // `document_json` that no longer round-trips indicates
                // DB corruption. Surface loudly via `Corruption` rather
                // than silently overwriting with an empty object.
                let parsed: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| EngineError::Validation {
                        kind: ValidationKind::Corruption,
                        detail: format!(
                            "corrupt summary document in ff_stream_summary: {e}"
                        ),
                    })?;
                (parsed, v)
            }
            None => (serde_json::Value::Object(serde_json::Map::new()), 0),
        };

        match patch_kind {
            PatchKind::JsonMergePatch => apply_json_merge_patch(&mut doc, &patch),
            _ => apply_json_merge_patch(&mut doc, &patch),
        }

        let new_version = prev_version + 1;
        let patch_kind_wire = "json-merge-patch";
        let doc_text = doc.to_string();
        if prev_version == 0 {
            sqlx::query(q_stream::INSERT_STREAM_SUMMARY_SQL)
                .bind(part)
                .bind(exec_uuid)
                .bind(attempt_index)
                .bind(&doc_text)
                .bind(new_version)
                .bind(patch_kind_wire)
                .bind(ts_ms)
                .bind(ts_ms)
                .execute(&mut **conn)
                .await
                .map_err(map_sqlx_error)?;
        } else {
            sqlx::query(q_stream::UPDATE_STREAM_SUMMARY_SQL)
                .bind(part)
                .bind(exec_uuid)
                .bind(attempt_index)
                .bind(&doc_text)
                .bind(new_version)
                .bind(patch_kind_wire)
                .bind(ts_ms)
                .execute(&mut **conn)
                .await
                .map_err(map_sqlx_error)?;
        }
        summary_version = Some(u64::try_from(new_version).unwrap_or(0));
    }

    // BestEffortLive: EMA + trim. Computation ports from the PG helper
    // at `ff-backend-postgres/src/stream.rs:272-339`.
    if let StreamMode::BestEffortLive { config } = &frame.mode {
        let meta: Option<(f64, i64)> = sqlx::query_as(q_stream::SELECT_STREAM_META_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .fetch_optional(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        let (ema_prev, last_ts) = meta.unwrap_or((0.0, 0));
        let inst_rate: f64 = if last_ts > 0 && ts_ms > last_ts {
            1000.0 / ((ts_ms - last_ts) as f64)
        } else {
            0.0
        };
        let alpha = config.ema_alpha;
        let ema_new = alpha * inst_rate + (1.0 - alpha) * ema_prev;
        let k_raw = (ema_new * (f64::from(config.ttl_ms)) / 1000.0).ceil() as i64 * 2;
        let k = k_raw
            .max(i64::from(config.maxlen_floor))
            .min(i64::from(config.maxlen_ceiling));

        sqlx::query(q_stream::UPSERT_STREAM_META_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .bind(ema_new)
            .bind(ts_ms)
            .bind(k)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(q_stream::TRIM_STREAM_FRAMES_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .bind(k)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
    }

    let frame_count: i64 = sqlx::query_scalar(q_stream::COUNT_STREAM_FRAMES_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index)
        .fetch_one(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    let stream_id = format!("{ts_ms}-{next_seq}");
    let mut out = AppendFrameOutcome::new(stream_id, u64::try_from(frame_count).unwrap_or(0));
    if let Some(v) = summary_version {
        out = out.with_summary_version(v);
    }
    Ok(out)
}

// ── claim_from_reclaim ────────────────────────────────────────────────

async fn claim_from_reclaim_impl(
    pool: &SqlitePool,
    token: &ReclaimToken,
) -> Result<Option<Handle>, EngineError> {
    let eid = &token.grant.execution_id;
    let (part, exec_uuid) = split_exec_id(eid)?;

    let mut conn = begin_immediate(pool).await?;
    let result = claim_from_reclaim_inner(&mut conn, part, exec_uuid, token).await;
    match result {
        Ok(Some(handle)) => {
            commit_or_rollback(&mut conn).await?;
            Ok(Some(handle))
        }
        Ok(None) => {
            rollback_quiet(&mut conn).await;
            Ok(None)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

async fn claim_from_reclaim_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    token: &ReclaimToken,
) -> Result<Option<Handle>, EngineError> {
    // Latest attempt under the partition/exec. Mirror of PG at
    // `ff-backend-postgres/src/attempt.rs:294-308`.
    let row = sqlx::query(q_lease::SELECT_LATEST_ATTEMPT_FOR_RECLAIM_SQL)
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let Some(row) = row else {
        return Err(EngineError::NotFound { entity: "attempt" });
    };
    let attempt_index_i: i64 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let current_epoch: i64 = row.try_get("lease_epoch").map_err(map_sqlx_error)?;
    let expires_at: Option<i64> = row
        .try_get::<Option<i64>, _>("lease_expires_at_ms")
        .map_err(map_sqlx_error)?;

    let now = now_ms();
    // Live-lease → grant no longer honour-able.
    let live = matches!(expires_at, Some(exp) if exp > now);
    if live {
        return Ok(None);
    }

    let lease_ttl_ms = i64::from(token.lease_ttl_ms);
    let new_expires = now.saturating_add(lease_ttl_ms);

    sqlx::query(q_lease::UPDATE_ATTEMPT_RECLAIM_SQL)
        .bind(token.worker_id.as_str())
        .bind(token.worker_instance_id.as_str())
        .bind(new_expires)
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .bind(attempt_index_i)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    sqlx::query(q_lease::UPDATE_EXEC_CORE_RECLAIM_SQL)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    insert_lease_event(conn, part, exec_uuid, "reclaimed", now).await?;

    let new_epoch = current_epoch.saturating_add(1);
    let payload = HandlePayload::new(
        token.grant.execution_id.clone(),
        AttemptIndex::new(u32::try_from(attempt_index_i.max(0)).unwrap_or(0)),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(u64::try_from(new_epoch).unwrap_or(0)),
        u64::from(token.lease_ttl_ms),
        token.grant.lane_id.clone(),
        token.worker_instance_id.clone(),
    );
    Ok(Some(encode_handle(&payload, HandleKind::Resumed)))
}

/// Internal shared state. `Arc<SqliteBackendInner>` is what the
/// registry stores weak references to and what `SqliteBackend`
/// wraps.
pub(crate) struct SqliteBackendInner {
    /// Connection pool. Held live even when the trait-object surface
    /// isn't exercising it so Phase 2+ can migrate bodies without
    /// re-plumbing construction.
    #[allow(dead_code)]
    pub(crate) pool: SqlitePool,
    /// Per-backend wakeup channels (Phase 3 wiring).
    #[allow(dead_code)]
    pub(crate) pubsub: PubSub,
    /// Registry key (canonical path or verbatim `:memory:` URI).
    /// Held for Drop-time cleanup if we need it in a future phase;
    /// today the `Weak` entries decay naturally.
    #[allow(dead_code)]
    pub(crate) key: PathBuf,
    /// Sentinel connection for shared-cache `:memory:` databases.
    /// SQLite drops a shared-cache in-memory DB the moment the last
    /// connection referencing it closes; the pool recycles idle
    /// connections, so without a pinned sentinel the schema + data
    /// would silently reset between pool checkouts. `None` for
    /// filesystem-backed databases where the file itself is the
    /// durable backing store.
    ///
    /// Held in a `Mutex` so `Drop` can take ownership — sqlx's
    /// `SqliteConnection::close` is async + consumes `self`, but we
    /// don't need graceful close here: process exit drops the
    /// in-memory DB regardless. Presence alone is what keeps the
    /// shared cache alive.
    #[allow(dead_code)]
    pub(crate) memory_sentinel: Option<std::sync::Mutex<Option<sqlx::SqliteConnection>>>,
}

/// RFC-023 SQLite dev-only backend.
///
/// Construction demands `FF_DEV_MODE=1` (§4.5). Identical paths
/// within a process return the same handle via the §4.2 B6
/// registry.
#[derive(Clone)]
pub struct SqliteBackend {
    inner: Arc<SqliteBackendInner>,
}

impl std::fmt::Debug for SqliteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteBackend")
            .field("key", &self.inner.key)
            .finish()
    }
}

impl SqliteBackend {
    /// RFC-023 Phase 1a entry point. `path` accepts a filesystem
    /// path, `:memory:`, or a `file:...?mode=memory&cache=shared`
    /// URI.
    ///
    /// Uses the [`SqliteServerConfig`] defaults (pool size 4, WAL on
    /// for file paths). For operator-tuned pool/WAL settings, call
    /// [`SqliteBackend::new_with_config`].
    ///
    /// [`SqliteServerConfig`]: ff_server::config::SqliteServerConfig
    ///
    /// # Errors
    ///
    /// * [`BackendError::RequiresDevMode`] when `FF_DEV_MODE` is
    ///   unset or not `"1"`.
    /// * [`BackendError::Valkey`] (historical name — the classifier
    ///   is backend-agnostic despite the variant name) when the
    ///   pool cannot be constructed.
    pub async fn new(path: &str) -> Result<Arc<Self>, BackendError> {
        Self::new_with_tuning(path, 4, true).await
    }

    /// Operator-tuned entry point. `pool_size` sets the pool's max
    /// connections; `wal_mode` enables `PRAGMA journal_mode=WAL` for
    /// filesystem-backed databases (ignored for `:memory:` variants
    /// per RFC-023 §4.6).
    pub async fn new_with_tuning(
        path: &str,
        pool_size: u32,
        wal_mode: bool,
    ) -> Result<Arc<Self>, BackendError> {
        // §4.5 production guard — TYPE-level emission point (§3.3 A3).
        if std::env::var("FF_DEV_MODE").as_deref() != Ok("1") {
            return Err(BackendError::RequiresDevMode);
        }

        // §4.2 B6: canonicalize the key. `:memory:` and
        // `file::memory:...` pass through verbatim (distinct per-URI
        // entries via embedded UUIDs). Filesystem paths resolve via
        // `fs::canonicalize` when the file exists; absent files fall
        // back to the raw path so two concurrent constructions before
        // file creation still dedup.
        //
        // F1: bare `:memory:` is rewritten to
        // `file::memory:?cache=shared&uri=true` so a multi-connection
        // pool shares ONE in-memory database. Without this rewrite,
        // each pool connection opens its own private DB and tests see
        // schema mismatches silently.
        let is_memory = path == ":memory:" || path.starts_with("file::memory:");
        let effective_path: std::borrow::Cow<'_, str> = if path == ":memory:" {
            std::borrow::Cow::Borrowed("file::memory:?cache=shared")
        } else {
            std::borrow::Cow::Borrowed(path)
        };

        let key = if is_memory {
            PathBuf::from(effective_path.as_ref())
        } else {
            std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
        };

        if let Some(existing) = registry::lookup(&key) {
            // F6: emit WARN only on first-time construction. Registry
            // hits are dedup clones; operators already saw the banner
            // when the original handle was built.
            return Ok(Arc::new(Self { inner: existing }));
        }

        // Build the pool. sqlx's SqliteConnectOptions parses the full
        // URI form as well as plain paths. `create_if_missing` is
        // what embedded-test consumers expect.
        let opts: SqliteConnectOptions = effective_path
            .parse::<SqliteConnectOptions>()
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Protocol,
                message: format!("sqlite connect-opts parse for {path:?}: {e}"),
            })?
            .create_if_missing(true);

        // F2: apply WAL for filesystem-backed DBs only. SQLite's WAL
        // is a no-op (and warns) for `:memory:` variants per RFC §4.6.
        let opts = if wal_mode && !is_memory {
            opts.journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        } else {
            opts
        };

        // F2: pool size from config, default 4. Minimum 1 — sqlx
        // rejects 0 at pool-build time anyway.
        let pool_max = pool_size.max(1);
        let pool = SqlitePoolOptions::new()
            .max_connections(pool_max)
            .connect_with(opts.clone())
            .await
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Transport,
                message: format!("sqlite pool connect for {path:?}: {e}"),
            })?;

        // F1: for shared-cache `:memory:` DBs, open a standalone
        // sentinel connection and hold it for the `Arc`'s lifetime.
        // The shared cache is torn down the moment the last connection
        // closes; without the sentinel, a pool-idle cycle (all 4
        // connections temporarily returned) would drop the DB between
        // test assertions.
        let memory_sentinel = if is_memory {
            use sqlx::ConnectOptions;
            let conn = opts.connect().await.map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Transport,
                message: format!("sqlite sentinel connect for {path:?}: {e}"),
            })?;
            Some(std::sync::Mutex::new(Some(conn)))
        } else {
            None
        };

        // F6: §3.3 WARN banner — now emitted AFTER registry-miss is
        // confirmed so dedup clones don't spam the log.
        tracing::warn!(
            "FlowFabric SQLite backend active (FF_DEV_MODE=1). \
             This backend is dev-only; single-writer, single-process, \
             not supported in production. See RFC-023."
        );

        // RFC-023 Phase 1b: apply the 14 hand-ported SQLite-dialect
        // migrations against the freshly-constructed pool. `sqlx::migrate!`
        // embeds the files at compile time and records applied versions
        // in `_sqlx_migrations` so reruns are idempotent.
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Protocol,
                message: format!("sqlite migrate for {path:?}: {e}"),
            })?;

        let inner = Arc::new(SqliteBackendInner {
            pool,
            pubsub: PubSub::new(),
            key: key.clone(),
            memory_sentinel,
        });
        let inner = registry::insert(key, inner);
        Ok(Arc::new(Self { inner }))
    }

    /// Accessor for Phase 2+ code that needs direct pool access
    /// without re-routing through the trait surface.
    #[allow(dead_code)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.inner.pool
    }

    /// Test-only pool accessor. Hidden from rustdoc; not a stable
    /// API. Exists so the in-crate integration tests can verify
    /// pool-level behaviour (F1 shared-cache sentinel) without
    /// waiting for Phase 2 data-plane methods to land.
    #[doc(hidden)]
    pub fn pool_for_test(&self) -> &SqlitePool {
        &self.inner.pool
    }
}

#[async_trait]
impl EngineBackend for SqliteBackend {
    // ── Claim + lifecycle ──

    async fn claim(
        &self,
        lane: &LaneId,
        capabilities: &CapabilitySet,
        policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| claim_impl(pool, lane, capabilities, &policy)).await
    }

    async fn renew(&self, handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| renew_impl(pool, handle)).await
    }

    async fn progress(
        &self,
        handle: &Handle,
        percent: Option<u8>,
        message: Option<String>,
    ) -> Result<(), EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| progress_impl(pool, handle, percent, message.clone())).await
    }

    async fn append_frame(
        &self,
        handle: &Handle,
        frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| append_frame_impl(pool, handle, frame.clone())).await
    }

    async fn complete(&self, handle: &Handle, payload: Option<Vec<u8>>) -> Result<(), EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| complete_impl(pool, handle, payload.clone())).await
    }

    async fn fail(
        &self,
        handle: &Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| fail_impl(pool, handle, reason.clone(), classification)).await
    }

    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        unavailable("sqlite.cancel")
    }

    async fn suspend(
        &self,
        _handle: &Handle,
        _args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        unavailable("sqlite.suspend")
    }

    async fn create_waitpoint(
        &self,
        _handle: &Handle,
        _waitpoint_key: &str,
        _expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        unavailable("sqlite.create_waitpoint")
    }

    async fn observe_signals(&self, _handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError> {
        unavailable("sqlite.observe_signals")
    }

    async fn claim_from_reclaim(&self, token: ReclaimToken) -> Result<Option<Handle>, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| claim_from_reclaim_impl(pool, &token)).await
    }

    async fn delay(&self, _handle: &Handle, _delay_until: TimestampMs) -> Result<(), EngineError> {
        unavailable("sqlite.delay")
    }

    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        unavailable("sqlite.wait_children")
    }

    // ── Read / admin ──

    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        unavailable("sqlite.describe_execution")
    }

    async fn describe_flow(&self, _id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError> {
        unavailable("sqlite.describe_flow")
    }

    #[cfg(feature = "core")]
    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        unavailable("sqlite.list_edges")
    }

    #[cfg(feature = "core")]
    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        unavailable("sqlite.describe_edge")
    }

    #[cfg(feature = "core")]
    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        unavailable("sqlite.resolve_execution_flow_id")
    }

    #[cfg(feature = "core")]
    async fn list_flows(
        &self,
        _partition: PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        unavailable("sqlite.list_flows")
    }

    #[cfg(feature = "core")]
    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        unavailable("sqlite.list_lanes")
    }

    #[cfg(feature = "core")]
    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        unavailable("sqlite.list_suspended")
    }

    #[cfg(feature = "core")]
    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        unavailable("sqlite.list_executions")
    }

    #[cfg(feature = "core")]
    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        unavailable("sqlite.deliver_signal")
    }

    #[cfg(feature = "core")]
    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        unavailable("sqlite.claim_resumed_execution")
    }

    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        unavailable("sqlite.cancel_flow")
    }

    #[cfg(feature = "core")]
    async fn set_edge_group_policy(
        &self,
        _flow_id: &FlowId,
        _downstream_execution_id: &ExecutionId,
        _policy: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        unavailable("sqlite.set_edge_group_policy")
    }

    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        unavailable("sqlite.report_usage")
    }

    #[cfg(feature = "streaming")]
    async fn read_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("sqlite.read_stream")
    }

    #[cfg(feature = "streaming")]
    async fn tail_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _after: StreamCursor,
        _block_ms: u64,
        _count_limit: u64,
        _visibility: ff_core::backend::TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("sqlite.tail_stream")
    }

    #[cfg(feature = "streaming")]
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<ff_core::backend::SummaryDocument>, EngineError> {
        unavailable("sqlite.read_summary")
    }

    // ── RFC-018 capability discovery ──

    fn backend_label(&self) -> &'static str {
        "sqlite"
    }

    fn capabilities(&self) -> Capabilities {
        // RFC-023 §4.3: Phase 1a exposes only the identity tuple; real
        // `Supports::*` flags flip at the Phase 4 release PR when trait
        // bodies ship. Consumers seeing `supports.*` all-false under
        // "sqlite" know to expect `Unavailable` on every data-plane
        // call until Phase 2+ lands.
        Capabilities::new(
            BackendIdentity::new(
                "sqlite",
                Version::new(
                    env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
                    env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
                    env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
                ),
                "Phase-1a",
            ),
            Supports::none(),
        )
    }

    async fn prepare(&self) -> Result<PrepareOutcome, EngineError> {
        // Phase 1a: no boot-time prep (no migrations yet). Phase 1b
        // applies the migrations inside `SqliteBackend::new` itself
        // rather than here, matching the PG posture.
        Ok(PrepareOutcome::NoOp)
    }
}
