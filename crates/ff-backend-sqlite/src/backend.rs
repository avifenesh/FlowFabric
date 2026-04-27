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
#[cfg(feature = "streaming")]
use ff_core::backend::{SummaryDocument, TailVisibility};
use ff_core::capability::{BackendIdentity, Capabilities, Supports, Version};
use ff_core::caps::{CapabilityRequirement, matches as caps_matches};
use ff_core::contracts::{
    CancelFlowResult, ExecutionSnapshot, FlowSnapshot, ReportUsageResult,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllResult, SeedOutcome,
    SeedWaitpointHmacSecretArgs, SuspendArgs, SuspendOutcome,
};
#[cfg(feature = "core")]
use ff_core::contracts::{
    ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs, DeliverSignalResult,
    EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ListExecutionsPage, ListFlowsPage,
    ListLanesPage, ListSuspendedPage, SetEdgeGroupPolicyResult,
};
#[cfg(feature = "streaming")]
use ff_core::contracts::{STREAM_READ_HARD_CAP, StreamCursor, StreamFrame, StreamFrames};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{BackendError, ContentionKind, EngineError, ValidationKind};
use ff_core::handle_codec::HandlePayload;
use ff_core::types::{AttemptId, AttemptIndex, LeaseEpoch, LeaseFence, LeaseId};

use crate::errors::map_sqlx_error;
use crate::handle_codec::{decode_handle, encode_handle};
use crate::queries::{
    attempt as q_attempt, dispatch as q_dispatch, exec_core as q_exec, flow as q_flow,
    flow_staging as q_flow_staging, lease as q_lease, stream as q_stream,
};
use crate::retry::retry_serializable;
#[cfg(feature = "core")]
use ff_core::partition::PartitionKey;
#[cfg(feature = "core")]
use ff_core::types::EdgeId;
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};

use crate::pubsub::{OutboxEvent, PubSub};
use crate::registry;
#[cfg(feature = "core")]
use ff_core::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, ApplyDependencyToChildArgs,
    ApplyDependencyToChildResult, CancelExecutionArgs, CancelExecutionResult, CancelFlowArgs,
    CancelFlowHeader, ChangePriorityArgs, ChangePriorityResult, CreateExecutionArgs,
    CreateExecutionResult, CreateFlowArgs, CreateFlowResult, ExecutionInfo,
    ListPendingWaitpointsArgs, ListPendingWaitpointsResult, ReplayExecutionArgs,
    ReplayExecutionResult, RevokeLeaseArgs, RevokeLeaseResult, StageDependencyEdgeArgs,
    StageDependencyEdgeResult,
};
#[cfg(feature = "core")]
use ff_core::state::PublicState;
use tokio::sync::broadcast;

/// Phase-1a-wide `Unavailable` helper. Each stubbed method names
/// itself here so call-site errors carry a stable identifier.
#[inline]
fn unavailable<T>(op: &'static str) -> Result<T, EngineError> {
    Err(EngineError::Unavailable { op })
}

// ── Phase 2b.1: post-commit broadcast emit support ─────────────────────

/// Enum selector for the 5 broadcast channels. Inner transaction bodies
/// accumulate `(OutboxChannel, OutboxEvent)` pairs in a `Vec` and the
/// outer wrapper dispatches them AFTER `tx.commit()` succeeds. This
/// preserves the RFC-023 §4.2 ordering invariant: broadcast wakeup
/// fires only for events that genuinely committed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OutboxChannel {
    LeaseHistory,
    Completion,
    #[allow(dead_code)] // wired in Phase 2b.2 deliver_signal
    SignalDelivery,
    StreamFrame,
    #[allow(dead_code)] // wired in Phase 2b.2 operator ops
    OperatorEvent,
}

/// A pending post-commit broadcast emit. See [`OutboxChannel`].
pub(crate) type PendingEmit = (OutboxChannel, OutboxEvent);

/// Dispatch every pending emit via the appropriate broadcast channel.
/// Called AFTER `tx.commit()` returns OK so consumers only observe
/// wakeups for genuinely-committed events.
fn dispatch_pending_emits(pubsub: &PubSub, emits: &[PendingEmit]) {
    for (channel, ev) in emits {
        let sender: &broadcast::Sender<OutboxEvent> = match channel {
            OutboxChannel::LeaseHistory => &pubsub.lease_history,
            OutboxChannel::Completion => &pubsub.completion,
            OutboxChannel::SignalDelivery => &pubsub.signal_delivery,
            OutboxChannel::StreamFrame => &pubsub.stream_frame,
            OutboxChannel::OperatorEvent => &pubsub.operator_event,
        };
        PubSub::emit(sender, ev.clone());
    }
}

/// Read `last_insert_rowid()` inside the open txn and turn it into
/// an [`OutboxEvent`]. SQLite's AUTOINCREMENT outbox tables use the
/// rowid alias as the `event_id`, so this read is correct for every
/// outbox table defined under `migrations/000{1,6,7,10}_*.sql`.
async fn last_outbox_event(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    partition_key: i64,
) -> Result<OutboxEvent, EngineError> {
    let event_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(OutboxEvent {
        event_id,
        partition_key,
    })
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
    pubsub: &PubSub,
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
        Ok(Some((handle, emits))) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_pending_emits(pubsub, &emits);
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
) -> Result<Option<(Handle, Vec<PendingEmit>)>, EngineError> {
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
    // observes the acquisition. Post-commit broadcast emit wired in
    // Phase 2b.1 per RFC-023 §4.2.
    let mut emits: Vec<PendingEmit> = Vec::new();
    let ev = insert_lease_event(conn, part, exec_uuid, "acquired", now).await?;
    emits.push((OutboxChannel::LeaseHistory, ev));

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
    Ok(Some((encode_handle(&payload, HandleKind::Fresh), emits)))
}

async fn complete_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
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
        Ok(emits) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_pending_emits(pubsub, &emits);
            Ok(())
        }
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
) -> Result<Vec<PendingEmit>, EngineError> {
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

    let mut emits: Vec<PendingEmit> = Vec::new();
    let completion_ev = insert_completion_event_ev(conn, part, exec_uuid, "success", now).await?;
    emits.push((OutboxChannel::Completion, completion_ev));

    let lease_ev = insert_lease_event(conn, part, exec_uuid, "revoked", now).await?;
    emits.push((OutboxChannel::LeaseHistory, lease_ev));
    Ok(emits)
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
    pubsub: &PubSub,
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
        Ok((outcome, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_pending_emits(pubsub, &emits);
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
) -> Result<(FailOutcome, Vec<PendingEmit>), EngineError> {
    fence_check(conn, part, exec_uuid, attempt_index, expected_epoch).await?;
    let now = now_ms();
    let mut emits: Vec<PendingEmit> = Vec::new();

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

        let lease_ev = insert_lease_event(conn, part, exec_uuid, "revoked", now).await?;
        emits.push((OutboxChannel::LeaseHistory, lease_ev));
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
        Ok((
            FailOutcome::RetryScheduled {
                delay_until: ff_core::types::TimestampMs::from_millis(now),
            },
            emits,
        ))
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

        let completion_ev =
            insert_completion_event_ev(conn, part, exec_uuid, "failed", now).await?;
        emits.push((OutboxChannel::Completion, completion_ev));

        let lease_ev = insert_lease_event(conn, part, exec_uuid, "revoked", now).await?;
        emits.push((OutboxChannel::LeaseHistory, lease_ev));
        Ok((FailOutcome::TerminalFailed, emits))
    }
}

/// Emit one RFC-019 Stage B lease-lifecycle outbox row + return the
/// generated outbox `event_id` wrapped in an [`OutboxEvent`] for the
/// caller to queue as a post-commit broadcast.
///
/// Mirrors `ff-backend-postgres/src/lease_event.rs`. The PG
/// `pg_notify` trigger is dropped per RFC-023 §4.2 — broadcast moves
/// into the Rust post-commit dispatch landed in Phase 2b.1; durable
/// replay via `event_id > cursor` continues to ride against this
/// table.
async fn insert_lease_event(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    event_type: &str,
    now: i64,
) -> Result<OutboxEvent, EngineError> {
    sqlx::query(q_dispatch::INSERT_LEASE_EVENT_SQL)
        .bind(exec_uuid.to_string())
        .bind(event_type)
        .bind(now)
        .bind(part)
        // BLOB bind for the co-transactional exec_core lookup that
        // back-fills namespace + instance_tag (Phase 3.2 fix).
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    last_outbox_event(conn, part).await
}

/// Insert one completion outbox row (success / failed / cancelled /
/// retry) and return the `event_id` wrapped in an [`OutboxEvent`].
async fn insert_completion_event_ev(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    outcome: &str,
    now: i64,
) -> Result<OutboxEvent, EngineError> {
    sqlx::query(q_attempt::INSERT_COMPLETION_EVENT_SQL)
        .bind(outcome)
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    last_outbox_event(conn, part).await
}

// ── Phase 2a.3 hot-path bodies ────────────────────────────────────────

async fn renew_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    handle: &Handle,
) -> Result<LeaseRenewal, EngineError> {
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
        Ok((renewal, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_pending_emits(pubsub, &emits);
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
) -> Result<(LeaseRenewal, Vec<PendingEmit>), EngineError> {
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
    let ev = insert_lease_event(conn, part, exec_uuid, "renewed", now).await?;
    let emits = vec![(OutboxChannel::LeaseHistory, ev)];

    Ok((
        LeaseRenewal::new(u64::try_from(new_expires).unwrap_or(0), expected_epoch),
        emits,
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
    pubsub: &PubSub,
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
        Ok((outcome, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_pending_emits(pubsub, &emits);
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
) -> Result<(AppendFrameOutcome, Vec<PendingEmit>), EngineError> {
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

    // Post-commit broadcast on the stream_frame channel. `ff_stream_frame`
    // uses a composite primary key, not AUTOINCREMENT — `last_insert_rowid()`
    // still returns the rowid of the just-inserted row (SQLite assigns
    // one for every non-WITHOUT-ROWID table), so the outbox-event id is
    // unique per append within the table's rowid sequence.
    let stream_ev = last_outbox_event(conn, part).await?;
    let emits: Vec<PendingEmit> = vec![(OutboxChannel::StreamFrame, stream_ev)];

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
                        detail: format!("corrupt summary document in ff_stream_summary: {e}"),
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
    Ok((out, emits))
}

// ── Phase 2b.2.2 stream readers (Group C) ─────────────────────────────

/// Parse a [`StreamCursor`] into `(ts_ms, seq)`. Mirror of PG at
/// `ff-backend-postgres/src/stream.rs:365-395`. `Start` maps to the
/// smallest representable tuple (i64::MIN, i64::MIN) so the lower
/// bound on `read_stream` is inclusive-from-earliest; `End` maps to
/// (i64::MAX, i64::MAX) for the symmetric upper bound.
#[cfg(feature = "streaming")]
fn parse_cursor_bound(c: &StreamCursor) -> Result<(i64, i64), EngineError> {
    match c {
        StreamCursor::Start => Ok((i64::MIN, i64::MIN)),
        StreamCursor::End => Ok((i64::MAX, i64::MAX)),
        StreamCursor::At(s) => parse_concrete_cursor(s),
    }
}

#[cfg(feature = "streaming")]
fn parse_concrete_cursor(s: &str) -> Result<(i64, i64), EngineError> {
    let (ms, seq) = match s.split_once('-') {
        Some((a, b)) => (a, b),
        None => (s, "0"),
    };
    let ms: i64 = ms.parse().map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("bad stream cursor '{s}' (ms)"),
    })?;
    let sq: i64 = seq.parse().map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("bad stream cursor '{s}' (seq)"),
    })?;
    Ok((ms, sq))
}

#[cfg(feature = "streaming")]
fn row_to_frame(ts_ms: i64, seq: i64, fields_text: &str) -> StreamFrame {
    use std::collections::BTreeMap;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    if let Ok(serde_json::Value::Object(map)) =
        serde_json::from_str::<serde_json::Value>(fields_text)
    {
        for (k, v) in map {
            let s = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            out.insert(k, s);
        }
    }
    StreamFrame {
        id: format!("{ts_ms}-{seq}"),
        fields: out,
    }
}

#[cfg(feature = "streaming")]
async fn read_stream_impl(
    pool: &SqlitePool,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    from: StreamCursor,
    to: StreamCursor,
    count_limit: u64,
) -> Result<StreamFrames, EngineError> {
    let (part, exec_uuid) = split_exec_id(execution_id)?;
    let aidx: i64 = i64::from(attempt_index.0);
    let (from_ms, from_seq) = parse_cursor_bound(&from)?;
    let (to_ms, to_seq) = parse_cursor_bound(&to)?;
    let lim = i64::try_from(count_limit.min(STREAM_READ_HARD_CAP)).unwrap_or(i64::MAX);

    let rows = sqlx::query(q_stream::READ_STREAM_RANGE_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(aidx)
        .bind(from_ms)
        .bind(from_seq)
        .bind(to_ms)
        .bind(to_seq)
        .bind(lim)
        .fetch_all(pool)
        .await
        .map_err(map_sqlx_error)?;

    let mut frames = Vec::with_capacity(rows.len());
    for row in rows {
        let ts: i64 = row.try_get("ts_ms").map_err(map_sqlx_error)?;
        let seq: i64 = row.try_get("seq").map_err(map_sqlx_error)?;
        let fields_text: String = row.try_get("fields").map_err(map_sqlx_error)?;
        frames.push(row_to_frame(ts, seq, &fields_text));
    }
    Ok(StreamFrames {
        frames,
        closed_at: None,
        closed_reason: None,
    })
}

#[cfg(feature = "streaming")]
#[allow(clippy::too_many_arguments)] // mirrors the trait signature
async fn tail_stream_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    after: StreamCursor,
    block_ms: u64,
    count_limit: u64,
    visibility: TailVisibility,
) -> Result<StreamFrames, EngineError> {
    let (part, exec_uuid) = split_exec_id(execution_id)?;
    let aidx: i64 = i64::from(attempt_index.0);
    let (after_ms, after_seq) = match &after {
        StreamCursor::At(s) => parse_concrete_cursor(s)?,
        _ => {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "tail_stream requires concrete after cursor".into(),
            });
        }
    };
    let lim = i64::try_from(count_limit.min(STREAM_READ_HARD_CAP)).unwrap_or(i64::MAX);
    let sql = match visibility {
        TailVisibility::ExcludeBestEffort => q_stream::TAIL_STREAM_AFTER_EXCLUDE_BE_SQL,
        _ => q_stream::TAIL_STREAM_AFTER_SQL,
    };

    // Subscribe BEFORE the first SELECT so we never miss a broadcast
    // wake between SELECT and park. Matches PG's LISTEN-then-SELECT
    // handshake at `ff-backend-postgres/src/stream.rs:496-498`.
    let mut rx = pubsub.stream_frame.subscribe();

    let do_select = || async {
        sqlx::query(sql)
            .bind(part)
            .bind(exec_uuid)
            .bind(aidx)
            .bind(after_ms)
            .bind(after_seq)
            .bind(lim)
            .fetch_all(pool)
            .await
            .map_err(map_sqlx_error)
    };

    let rows = do_select().await?;
    if !rows.is_empty() || block_ms == 0 {
        return Ok(rows_to_frames(rows));
    }

    // Park on the broadcast receiver — NO SQLite connection held here.
    // Loop until timeout OR the re-SELECT returns a non-empty set:
    // a broadcast tick may correspond to a frame that failed the
    // visibility filter, in which case we re-park for the remainder.
    let start = std::time::Instant::now();
    let total = Duration::from_millis(block_ms);
    loop {
        let remaining = match total.checked_sub(start.elapsed()) {
            Some(r) if !r.is_zero() => r,
            _ => break,
        };
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(_)) => {}
            // Lagged → outbox is durable; just re-select.
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            // Producer closed → do one last re-select + return.
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Ok(rows_to_frames(do_select().await?));
            }
            // Timeout; fall through to break below.
            Err(_) => break,
        }
        let rows = do_select().await?;
        if !rows.is_empty() {
            return Ok(rows_to_frames(rows));
        }
        if start.elapsed() >= total {
            break;
        }
    }

    Ok(StreamFrames::empty_open())
}

#[cfg(feature = "streaming")]
fn rows_to_frames(rows: Vec<sqlx::sqlite::SqliteRow>) -> StreamFrames {
    let mut frames = Vec::with_capacity(rows.len());
    for row in rows {
        let ts: i64 = row.try_get("ts_ms").unwrap_or(0);
        let seq: i64 = row.try_get("seq").unwrap_or(0);
        let fields_text: String = row.try_get("fields").unwrap_or_default();
        frames.push(row_to_frame(ts, seq, &fields_text));
    }
    StreamFrames {
        frames,
        closed_at: None,
        closed_reason: None,
    }
}

#[cfg(feature = "streaming")]
async fn read_summary_impl(
    pool: &SqlitePool,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
) -> Result<Option<SummaryDocument>, EngineError> {
    let (part, exec_uuid) = split_exec_id(execution_id)?;
    let aidx: i64 = i64::from(attempt_index.0);

    let row = sqlx::query(q_stream::READ_SUMMARY_FULL_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(aidx)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;

    let Some(row) = row else { return Ok(None) };
    let doc_text: String = row.try_get("document_json").map_err(map_sqlx_error)?;
    let version: i64 = row.try_get("version").map_err(map_sqlx_error)?;
    let patch_kind_wire: Option<String> = row
        .try_get::<Option<String>, _>("patch_kind")
        .unwrap_or(None);
    let last_updated: i64 = row.try_get("last_updated_ms").map_err(map_sqlx_error)?;
    let first_applied: i64 = row.try_get("first_applied_ms").map_err(map_sqlx_error)?;

    // Re-serialize via serde_json to normalize whitespace so SQLite and
    // PG observers receive byte-identical documents for equivalent
    // stored state. A corrupt stored blob surfaces as Validation —
    // matches the Phase 2a.3 `append_frame` strict-parse posture.
    let parsed: serde_json::Value =
        serde_json::from_str(&doc_text).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("corrupt summary document in ff_stream_summary: {e}"),
        })?;
    let bytes = serde_json::to_vec(&parsed).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("summary document not serialisable: {e}"),
    })?;
    let patch_kind = match patch_kind_wire.as_deref() {
        Some("json-merge-patch") => PatchKind::JsonMergePatch,
        _ => PatchKind::JsonMergePatch,
    };
    Ok(Some(SummaryDocument::new(
        bytes,
        u64::try_from(version).unwrap_or(0),
        patch_kind,
        u64::try_from(last_updated).unwrap_or(0),
        u64::try_from(first_applied).unwrap_or(0),
    )))
}

// ── claim_from_reclaim ────────────────────────────────────────────────

async fn claim_from_reclaim_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    token: &ReclaimToken,
) -> Result<Option<Handle>, EngineError> {
    let eid = &token.grant.execution_id;
    let (part, exec_uuid) = split_exec_id(eid)?;

    let mut conn = begin_immediate(pool).await?;
    let result = claim_from_reclaim_inner(&mut conn, part, exec_uuid, token).await;
    match result {
        Ok(Some((handle, emits))) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_pending_emits(pubsub, &emits);
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
) -> Result<Option<(Handle, Vec<PendingEmit>)>, EngineError> {
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

    let ev = insert_lease_event(conn, part, exec_uuid, "reclaimed", now).await?;
    let emits = vec![(OutboxChannel::LeaseHistory, ev)];

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
    Ok(Some((encode_handle(&payload, HandleKind::Resumed), emits)))
}

// ── Phase 2b.1 producer-side bodies (Group A) ─────────────────────────

/// Serialize an optional [`ff_core::policy::ExecutionPolicy`] into the
/// TEXT JSON shape stored in `ff_exec_core.policy`. Mirrors PG at
/// `ff-backend-postgres/src/exec_core.rs:144-150` (the PG side stores
/// jsonb; SQLite stores the same JSON in a TEXT column).
#[cfg(feature = "core")]
fn encode_policy_json(
    policy: Option<&ff_core::policy::ExecutionPolicy>,
) -> Result<Option<String>, EngineError> {
    match policy {
        Some(p) => serde_json::to_string(p)
            .map(Some)
            .map_err(|e| EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("create_execution: policy: serialize failed: {e}"),
            }),
        None => Ok(None),
    }
}

/// Build `raw_fields` for a fresh `ff_exec_core` row. Mirror of PG's
/// `create_execution_impl` JSON shape so downstream read paths decode
/// identically. TEXT JSON in SQLite vs jsonb in PG is otherwise opaque.
#[cfg(feature = "core")]
fn build_create_execution_raw_fields(args: &CreateExecutionArgs) -> String {
    use serde_json::{Map, Value};
    let mut raw: Map<String, Value> = Map::new();
    raw.insert(
        "namespace".into(),
        Value::String(args.namespace.as_str().to_owned()),
    );
    raw.insert(
        "execution_kind".into(),
        Value::String(args.execution_kind.clone()),
    );
    raw.insert(
        "creator_identity".into(),
        Value::String(args.creator_identity.clone()),
    );
    if let Some(k) = &args.idempotency_key {
        raw.insert("idempotency_key".into(), Value::String(k.clone()));
    }
    if let Some(enc) = &args.payload_encoding {
        raw.insert("payload_encoding".into(), Value::String(enc.clone()));
    }
    raw.insert(
        "last_mutation_at".into(),
        Value::String(args.now.0.to_string()),
    );
    raw.insert("total_attempt_count".into(), Value::String("0".to_owned()));
    let tags_json: Map<String, Value> = args
        .tags
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    raw.insert("tags".into(), Value::Object(tags_json));
    Value::Object(raw).to_string()
}

#[cfg(feature = "core")]
async fn create_execution_impl(
    pool: &SqlitePool,
    args: &CreateExecutionArgs,
) -> Result<CreateExecutionResult, EngineError> {
    let part: i64 = i64::from(args.execution_id.partition());
    let exec_uuid = {
        let s = args.execution_id.as_str();
        let tail = s
            .split_once("}:")
            .map(|(_, t)| t)
            .ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("execution_id missing `}}:` separator: {s}"),
            })?;
        Uuid::parse_str(tail).map_err(|e| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution_id UUID invalid: {e}"),
        })?
    };
    let lane_id = args.lane_id.as_str().to_owned();
    let priority: i64 = i64::from(args.priority);
    let created_at_ms: i64 = args.now.0;
    let deadline_at_ms: Option<i64> = args.execution_deadline_at.map(|t| t.0);
    let raw_fields = build_create_execution_raw_fields(args);
    let policy_json = encode_policy_json(args.policy.as_ref())?;

    let mut conn = begin_immediate(pool).await?;

    let insert_result = sqlx::query(q_exec::INSERT_EXEC_CORE_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(&lane_id)
        .bind(priority)
        .bind(created_at_ms)
        .bind(deadline_at_ms)
        .bind(args.input_payload.as_slice())
        .bind(policy_json.as_deref())
        .bind(&raw_fields)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error);

    let result = async {
        let res = insert_result?;
        let inserted = res.rows_affected() > 0;

        if inserted {
            // Populate the capability junction — RFC-023 §4.1 A4.
            // Required caps live on
            // `ExecutionPolicy.routing_requirements.required_capabilities`;
            // if absent, no junction rows are written (matches PG's
            // empty `text[]` default — see PG reference at
            // `ff-backend-postgres/src/exec_core.rs:157-188` which also
            // stores an empty `text[]` array when the policy is None).
            let required: Vec<String> = args
                .policy
                .as_ref()
                .and_then(|p| p.routing_requirements.as_ref())
                .map(|r| r.required_capabilities.iter().cloned().collect())
                .unwrap_or_default();
            for cap in &required {
                sqlx::query(q_exec::INSERT_EXEC_CAPABILITY_SQL)
                    .bind(exec_uuid)
                    .bind(cap)
                    .execute(&mut *conn)
                    .await
                    .map_err(map_sqlx_error)?;
            }
        }

        // Lane-registry seed is idempotent and runs on every call so
        // a dynamic lane seen for the first time on a duplicate
        // create_execution still registers.
        sqlx::query(q_exec::INSERT_LANE_REGISTRY_SQL)
            .bind(&lane_id)
            .bind(created_at_ms)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        Ok::<bool, EngineError>(inserted)
    }
    .await;

    match result {
        Ok(inserted) => {
            commit_or_rollback(&mut conn).await?;
            if inserted {
                Ok(CreateExecutionResult::Created {
                    execution_id: args.execution_id.clone(),
                    public_state: PublicState::Waiting,
                })
            } else {
                Ok(CreateExecutionResult::Duplicate {
                    execution_id: args.execution_id.clone(),
                })
            }
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

#[cfg(feature = "core")]
async fn create_flow_impl(
    pool: &SqlitePool,
    args: &CreateFlowArgs,
) -> Result<CreateFlowResult, EngineError> {
    // Flow partition under single-writer SQLite is always 0 (§4.1 A3).
    let part: i64 = 0;
    let flow_uuid: Uuid = args.flow_id.0;
    let now_ms = args.now.0;

    let raw_fields = serde_json::json!({
        "flow_kind": args.flow_kind,
        "namespace": args.namespace.as_str(),
        "node_count": 0,
        "edge_count": 0,
        "last_mutation_at_ms": now_ms,
    })
    .to_string();

    let mut conn = begin_immediate(pool).await?;
    let ins = sqlx::query(q_flow::INSERT_FLOW_CORE_SQL)
        .bind(part)
        .bind(flow_uuid)
        .bind(now_ms)
        .bind(&raw_fields)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error);
    match ins {
        Ok(r) => {
            commit_or_rollback(&mut conn).await?;
            if r.rows_affected() > 0 {
                Ok(CreateFlowResult::Created {
                    flow_id: args.flow_id.clone(),
                })
            } else {
                Ok(CreateFlowResult::AlreadySatisfied {
                    flow_id: args.flow_id.clone(),
                })
            }
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

#[cfg(feature = "core")]
async fn add_execution_to_flow_impl(
    pool: &SqlitePool,
    args: &AddExecutionToFlowArgs,
) -> Result<AddExecutionToFlowResult, EngineError> {
    let part: i64 = 0;
    let flow_uuid: Uuid = args.flow_id.0;
    let (exec_part, exec_uuid) = split_exec_id(&args.execution_id)?;
    // Under single-writer SQLite every entity lives on partition 0
    // (§4.1 A3). The exec id MUST carry `{fp:0}` because any other
    // partition is unreachable.
    if exec_part != part {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution partition mismatch: expected 0, got {exec_part}"),
        });
    }
    let now_ms = args.now.0;

    let mut conn = begin_immediate(pool).await?;
    let work = async {
        // 1. Load flow_core.
        let flow_row = sqlx::query(q_flow_staging::SELECT_FLOW_CORE_FOR_STAGE_SQL)
            .bind(part)
            .bind(flow_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let Some(flow_row) = flow_row else {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "flow_not_found".into(),
            });
        };
        let public_flow_state: String = flow_row
            .try_get("public_flow_state")
            .map_err(map_sqlx_error)?;
        if matches!(
            public_flow_state.as_str(),
            "cancelled" | "completed" | "failed" | "terminal"
        ) {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "flow_already_terminal".into(),
            });
        }
        let raw_fields_text: String = flow_row.try_get("raw_fields").map_err(map_sqlx_error)?;

        // 2. Load exec_core back-pointer.
        let exec_row = sqlx::query(q_flow_staging::SELECT_EXEC_FLOW_ID_SQL)
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let Some(exec_row) = exec_row else {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "execution_not_found".into(),
            });
        };
        let existing_flow_id: Option<Uuid> = exec_row.try_get("flow_id").map_err(map_sqlx_error)?;

        // 3. Idempotent: already on this flow.
        if existing_flow_id == Some(flow_uuid) {
            // Read node_count from cached raw_fields (avoid a second SELECT).
            let raw_val: serde_json::Value = serde_json::from_str(&raw_fields_text)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            let nc = raw_val
                .get("node_count")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            return Ok(AddExecutionToFlowResult::AlreadyMember {
                execution_id: args.execution_id.clone(),
                node_count: nc,
            });
        }

        // 4. Cross-flow refusal.
        if let Some(other) = existing_flow_id
            && other != flow_uuid
        {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("already_member_of_different_flow:{other}"),
            });
        }

        // 5. Stamp exec.flow_id + bump flow counters.
        sqlx::query(q_flow_staging::UPDATE_EXEC_SET_FLOW_ID_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(flow_uuid)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query(q_flow_staging::BUMP_FLOW_NODE_COUNT_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(now_ms)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let new_nc: i64 = sqlx::query_scalar(q_flow_staging::SELECT_FLOW_NODE_COUNT_SQL)
            .bind(part)
            .bind(flow_uuid)
            .fetch_one(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        Ok(AddExecutionToFlowResult::Added {
            execution_id: args.execution_id.clone(),
            new_node_count: u32::try_from(new_nc.max(0)).unwrap_or(0),
        })
    }
    .await;

    match work {
        Ok(res) => {
            commit_or_rollback(&mut conn).await?;
            Ok(res)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

#[cfg(feature = "core")]
async fn stage_dependency_edge_impl(
    pool: &SqlitePool,
    args: &StageDependencyEdgeArgs,
) -> Result<StageDependencyEdgeResult, EngineError> {
    if args.upstream_execution_id == args.downstream_execution_id {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "self_referencing_edge".into(),
        });
    }

    let part: i64 = 0;
    let flow_uuid: Uuid = args.flow_id.0;
    let edge_uuid: Uuid = args.edge_id.0;
    let (up_part, upstream_uuid) = split_exec_id(&args.upstream_execution_id)?;
    let (down_part, downstream_uuid) = split_exec_id(&args.downstream_execution_id)?;
    if up_part != part || down_part != part {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "execution partition mismatch under single-writer SQLite".into(),
        });
    }
    let now_ms = args.now.0;
    let expected_rev = i64::try_from(args.expected_graph_revision).unwrap_or(i64::MAX);

    let mut conn = begin_immediate(pool).await?;
    let work = async {
        // 1. CAS bump flow_core. `changes()` after execute tells us
        //    whether the WHERE matched.
        let cas = sqlx::query(q_flow_staging::CAS_BUMP_FLOW_REV_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(expected_rev)
            .bind(now_ms)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        if cas.rows_affected() == 0 {
            // Distinguish flow-missing vs terminal vs stale-rev.
            let probe = sqlx::query(q_flow_staging::SELECT_FLOW_REV_AND_STATE_SQL)
                .bind(part)
                .bind(flow_uuid)
                .fetch_optional(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
            return match probe {
                None => Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: "flow_not_found".into(),
                }),
                Some(r) => {
                    let state: String = r.try_get("public_flow_state").map_err(map_sqlx_error)?;
                    if matches!(
                        state.as_str(),
                        "cancelled" | "completed" | "failed" | "terminal"
                    ) {
                        Err(EngineError::Validation {
                            kind: ValidationKind::InvalidInput,
                            detail: "flow_already_terminal".into(),
                        })
                    } else {
                        Err(EngineError::Contention(ContentionKind::StaleGraphRevision))
                    }
                }
            };
        }

        // 2. Membership check.
        let member_rows =
            sqlx::query_scalar::<_, Uuid>(q_flow_staging::SELECT_FLOW_MEMBERSHIP_PAIR_SQL)
                .bind(part)
                .bind(flow_uuid)
                .bind(upstream_uuid)
                .bind(downstream_uuid)
                .fetch_all(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
        if !member_rows.contains(&upstream_uuid) || !member_rows.contains(&downstream_uuid) {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "execution_not_in_flow".into(),
            });
        }

        // 3. Insert edge.
        let policy_json = serde_json::json!({
            "dependency_kind": args.dependency_kind,
            "satisfaction_condition": "all_required",
            "data_passing_ref": args.data_passing_ref.clone().unwrap_or_default(),
            "edge_state": "pending",
            "created_at_ms": now_ms,
            "created_by": "engine",
            "staged_at_ms": now_ms,
            "applied_at_ms": serde_json::Value::Null,
        })
        .to_string();
        let ins = sqlx::query(q_flow_staging::INSERT_EDGE_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(edge_uuid)
            .bind(upstream_uuid)
            .bind(downstream_uuid)
            .bind(&policy_json)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        if ins.rows_affected() == 0 {
            // Edge already exists — parity with the PG `Conflict(
            // DependencyAlreadyExists { existing })` path would require
            // rehydrating the existing `EdgeSnapshot`, but SQLite
            // currently has no `describe_edge` reader wired (Phase
            // 2b.2). Surface the conflict as a Validation error
            // naming the edge_id so callers see a stable signal;
            // when `describe_edge` lands this can tighten to
            // `Conflict(DependencyAlreadyExists {..})`.
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("dependency_already_exists:edge_id={edge_uuid}"),
            });
        }

        // 4. Read post-bump revision.
        let new_rev: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT graph_revision FROM ff_flow_core \
             WHERE partition_key = ?1 AND flow_id = ?2",
        )
        .bind(part)
        .bind(flow_uuid)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        Ok(StageDependencyEdgeResult::Staged {
            edge_id: args.edge_id.clone(),
            new_graph_revision: u64::try_from(new_rev).unwrap_or(0),
        })
    }
    .await;

    match work {
        Ok(res) => {
            commit_or_rollback(&mut conn).await?;
            Ok(res)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

#[cfg(feature = "core")]
async fn apply_dependency_to_child_impl(
    pool: &SqlitePool,
    args: &ApplyDependencyToChildArgs,
) -> Result<ApplyDependencyToChildResult, EngineError> {
    let part: i64 = 0;
    let flow_uuid: Uuid = args.flow_id.0;
    let edge_uuid: Uuid = args.edge_id.0;
    let (down_part, downstream_uuid) = split_exec_id(&args.downstream_execution_id)?;
    if down_part != part {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "execution partition mismatch under single-writer SQLite".into(),
        });
    }
    let now_ms = args.now.0;

    let mut conn = begin_immediate(pool).await?;
    let work = async {
        // 1. Load the edge row.
        let row = sqlx::query(q_flow_staging::SELECT_EDGE_POLICY_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(edge_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;
        let Some(row) = row else {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "edge_not_found".into(),
            });
        };
        let policy_text: String = row.try_get("policy").map_err(map_sqlx_error)?;
        let mut policy: serde_json::Value =
            serde_json::from_str(&policy_text).map_err(|e| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("ff_edge.policy: {e}"),
            })?;

        // 2. Idempotency.
        let already_applied = policy
            .get("applied_at_ms")
            .and_then(|v| v.as_i64())
            .is_some();
        if already_applied {
            return Ok(ApplyDependencyToChildResult::AlreadyApplied);
        }

        // 3. Mutate policy JSON.
        if let Some(obj) = policy.as_object_mut() {
            obj.insert("applied_at_ms".into(), serde_json::json!(now_ms));
            obj.insert("edge_state".into(), serde_json::json!("applied"));
        }
        let new_policy_text = policy.to_string();
        sqlx::query(q_flow_staging::UPDATE_EDGE_POLICY_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(edge_uuid)
            .bind(&new_policy_text)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        // 4. Upsert edge_group.
        let default_group_policy = serde_json::json!({ "kind": "all_of" }).to_string();
        sqlx::query(q_flow_staging::UPSERT_EDGE_GROUP_APPLY_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(downstream_uuid)
            .bind(&default_group_policy)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        // 5. Read post-upsert running_count.
        let unsatisfied: i64 =
            sqlx::query_scalar(q_flow_staging::SELECT_EDGE_GROUP_RUNNING_COUNT_SQL)
                .bind(part)
                .bind(flow_uuid)
                .bind(downstream_uuid)
                .fetch_one(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

        Ok(ApplyDependencyToChildResult::Applied {
            unsatisfied_count: u32::try_from(unsatisfied.max(0)).unwrap_or(0),
        })
    }
    .await;

    match work {
        Ok(res) => {
            commit_or_rollback(&mut conn).await?;
            Ok(res)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

fn cancel_policy_to_str(p: CancelFlowPolicy) -> &'static str {
    match p {
        CancelFlowPolicy::FlowOnly => "cancel_flow_only",
        CancelFlowPolicy::CancelAll => "cancel_all",
        CancelFlowPolicy::CancelPending => "cancel_pending",
        // Forward-compat for additive `CancelFlowPolicy` variants.
        // Per the project's non-exhaustive-enum rule (confirmed by
        // cursor-bugbot learned-rule #dc768b31; cf. Valkey backend fix
        // PR #114), destructive-action wildcards MUST default to the
        // LEAST-destructive variant — widening cancel scope
        // irreversibly destroys work, while narrowing is safely
        // retryable by the caller. The PG reference at
        // `ff-backend-postgres/src/flow.rs:525-534` takes the wider
        // default; SQLite intentionally diverges here to match
        // Valkey's correctness posture.
        _ => "cancel_flow_only",
    }
}

async fn cancel_flow_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    id: &FlowId,
    policy: CancelFlowPolicy,
) -> Result<CancelFlowResult, EngineError> {
    let part: i64 = 0;
    let flow_uuid: Uuid = id.0;
    let policy_str = cancel_policy_to_str(policy);
    let now_ms = now_ms();

    let mut conn = begin_immediate(pool).await?;
    let work: Result<(CancelFlowResult, Vec<PendingEmit>), EngineError> = async {
        // Step 1 — flip flow_core.
        let flip = sqlx::query(q_flow::UPDATE_FLOW_CORE_CANCEL_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(now_ms)
            .bind(policy_str)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        if flip.rows_affected() == 0 {
            // Flow not found — return idempotent empty-member success
            // matching PG at `ff-backend-postgres/src/flow.rs:635-641`.
            return Ok((
                CancelFlowResult::Cancelled {
                    cancellation_policy: policy_str.to_owned(),
                    member_execution_ids: Vec::new(),
                },
                Vec::new(),
            ));
        }

        // Step 2 — enumerate + cancel members.
        let member_rows: Vec<Uuid> = if matches!(policy, CancelFlowPolicy::FlowOnly) {
            Vec::new()
        } else {
            let sql = match policy {
                CancelFlowPolicy::CancelPending => q_flow::SELECT_FLOW_MEMBERS_CANCEL_PENDING_SQL,
                _ => q_flow::SELECT_FLOW_MEMBERS_CANCEL_ALL_SQL,
            };
            sqlx::query_scalar::<_, Uuid>(sql)
                .bind(part)
                .bind(flow_uuid)
                .fetch_all(&mut *conn)
                .await
                .map_err(map_sqlx_error)?
        };

        let mut member_execution_ids: Vec<String> = Vec::with_capacity(member_rows.len());
        let mut emits: Vec<PendingEmit> = Vec::new();
        for exec_uuid in &member_rows {
            sqlx::query(q_flow::UPDATE_EXEC_CORE_CANCEL_MEMBER_SQL)
                .bind(part)
                .bind(exec_uuid)
                .bind(now_ms)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

            // Completion outbox + lease-revoked outbox — mirror of PG
            // at `ff-backend-postgres/src/flow.rs:688-716`.
            let completion_ev =
                insert_completion_event_ev(&mut conn, part, *exec_uuid, "cancelled", now_ms)
                    .await?;
            emits.push((OutboxChannel::Completion, completion_ev));
            let lease_ev =
                insert_lease_event(&mut conn, part, *exec_uuid, "revoked", now_ms).await?;
            emits.push((OutboxChannel::LeaseHistory, lease_ev));

            member_execution_ids.push(format!("{{fp:{part}}}:{exec_uuid}"));
        }

        // Step 3 — CancelPending bookkeeping.
        if matches!(policy, CancelFlowPolicy::CancelPending) {
            sqlx::query(q_flow::INSERT_PENDING_CANCEL_GROUPS_SQL)
                .bind(part)
                .bind(flow_uuid)
                .bind(now_ms)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
        }

        Ok((
            CancelFlowResult::Cancelled {
                cancellation_policy: policy_str.to_owned(),
                member_execution_ids,
            },
            emits,
        ))
    }
    .await;

    match work {
        Ok((res, emits)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_pending_emits(pubsub, &emits);
            Ok(res)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
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
    /// Per-backend post-commit wakeup channels (Phase 2b.1 wiring).
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

    /// Test-only subscribe helper — returns a `Receiver` for the
    /// completion-outbox broadcast channel so integration tests can
    /// assert that a `complete()` / `fail()` / `cancel_flow()` call
    /// wakes subscribers post-commit. The production `subscribe_*`
    /// surface lands in Phase 2b.2; this accessor is narrow + hidden
    /// so it doesn't leak into the public API.
    #[doc(hidden)]
    pub fn subscribe_completion_for_test(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::pubsub::OutboxEvent> {
        self.inner.pubsub.completion.subscribe()
    }

    /// Test-only broadcast accessor for the `stream_frame` channel.
    /// Exposed so Phase 2b.2.2 `outbox_cursor::tests` can subscribe
    /// before driving `append_frame`.
    #[doc(hidden)]
    #[cfg(test)]
    pub(crate) fn stream_frame_receiver_for_test(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::pubsub::OutboxEvent> {
        self.inner.pubsub.stream_frame.subscribe()
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
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| claim_impl(pool, pubsub, lane, capabilities, &policy)).await
    }

    async fn renew(&self, handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| renew_impl(pool, pubsub, handle)).await
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
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| append_frame_impl(pool, pubsub, handle, frame.clone())).await
    }

    async fn complete(&self, handle: &Handle, payload: Option<Vec<u8>>) -> Result<(), EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| complete_impl(pool, pubsub, handle, payload.clone())).await
    }

    async fn fail(
        &self,
        handle: &Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| fail_impl(pool, pubsub, handle, reason.clone(), classification)).await
    }

    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        unavailable("sqlite.cancel")
    }

    async fn suspend(
        &self,
        handle: &Handle,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| crate::suspend_ops::suspend_impl(pool, pubsub, handle, args.clone()))
            .await
    }

    async fn suspend_by_triple(
        &self,
        exec_id: ExecutionId,
        triple: LeaseFence,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| {
            crate::suspend_ops::suspend_by_triple_impl(
                pool,
                pubsub,
                exec_id.clone(),
                triple.clone(),
                args.clone(),
            )
        })
        .await
    }

    async fn create_waitpoint(
        &self,
        handle: &Handle,
        waitpoint_key: &str,
        expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| {
            crate::suspend_ops::create_waitpoint_impl(pool, handle, waitpoint_key, expires_in)
        })
        .await
    }

    async fn observe_signals(&self, handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| crate::suspend_ops::observe_signals_impl(pool, handle)).await
    }

    async fn claim_from_reclaim(&self, token: ReclaimToken) -> Result<Option<Handle>, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| claim_from_reclaim_impl(pool, pubsub, &token)).await
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
        args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| crate::suspend_ops::deliver_signal_impl(pool, pubsub, args.clone()))
            .await
    }

    #[cfg(feature = "core")]
    async fn claim_resumed_execution(
        &self,
        args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| {
            crate::suspend_ops::claim_resumed_execution_impl(pool, pubsub, args.clone())
        })
        .await
    }

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        // RFC-023 Phase 2b.1 Group A — classic cancel_flow only. The
        // `wait` axis is a Valkey/PG async-dispatch concern (member
        // cancel fan-out); under single-writer SQLite every member
        // flip happens in the same transaction as the header flip, so
        // the result is always synchronous `Cancelled {..}`.
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        retry_serializable(|| cancel_flow_impl(pool, pubsub, id, policy)).await
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
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from: StreamCursor,
        to: StreamCursor,
        count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        let pool = &self.inner.pool;
        read_stream_impl(pool, execution_id, attempt_index, from, to, count_limit).await
    }

    #[cfg(feature = "streaming")]
    async fn tail_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        after: StreamCursor,
        block_ms: u64,
        count_limit: u64,
        visibility: TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        let pool = &self.inner.pool;
        let pubsub = &self.inner.pubsub;
        tail_stream_impl(
            pool,
            pubsub,
            execution_id,
            attempt_index,
            after,
            block_ms,
            count_limit,
            visibility,
        )
        .await
    }

    #[cfg(feature = "streaming")]
    async fn read_summary(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        let pool = &self.inner.pool;
        read_summary_impl(pool, execution_id, attempt_index).await
    }

    // ── RFC-017 Stage A — Ingress (create + flow staging) ──
    //
    // Phase 2b.1 Group A lands 5 of the 9 ingress methods. The
    // remaining 4 (cancel_execution / change_priority /
    // replay_execution / plus the operator-event reads) land in
    // Phase 2b.2 alongside the Group B/C/D.2 scope.

    #[cfg(feature = "core")]
    async fn create_execution(
        &self,
        args: CreateExecutionArgs,
    ) -> Result<CreateExecutionResult, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| create_execution_impl(pool, &args)).await
    }

    #[cfg(feature = "core")]
    async fn create_flow(&self, args: CreateFlowArgs) -> Result<CreateFlowResult, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| create_flow_impl(pool, &args)).await
    }

    #[cfg(feature = "core")]
    async fn add_execution_to_flow(
        &self,
        args: AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| add_execution_to_flow_impl(pool, &args)).await
    }

    #[cfg(feature = "core")]
    async fn stage_dependency_edge(
        &self,
        args: StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| stage_dependency_edge_impl(pool, &args)).await
    }

    #[cfg(feature = "core")]
    async fn apply_dependency_to_child(
        &self,
        args: ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| apply_dependency_to_child_impl(pool, &args)).await
    }

    // ── RFC-020 Wave 9 — Operator control (Phase 3.2) ────────────────
    //
    // Each body lives in `crate::operator` and follows the §4.2 shared
    // spine adapted for SQLite (BEGIN IMMEDIATE + WHERE-clause CAS +
    // post-commit broadcast). Outbox rows populate namespace +
    // instance_tag via co-transactional SELECT so tag-filtered
    // subscribers do not silently drop events.

    #[cfg(feature = "core")]
    async fn cancel_execution(
        &self,
        args: CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, EngineError> {
        crate::operator::cancel_execution_impl(&self.inner.pool, &self.inner.pubsub, args).await
    }

    #[cfg(feature = "core")]
    async fn revoke_lease(
        &self,
        args: RevokeLeaseArgs,
    ) -> Result<RevokeLeaseResult, EngineError> {
        crate::operator::revoke_lease_impl(&self.inner.pool, &self.inner.pubsub, args).await
    }

    #[cfg(feature = "core")]
    async fn change_priority(
        &self,
        args: ChangePriorityArgs,
    ) -> Result<ChangePriorityResult, EngineError> {
        crate::operator::change_priority_impl(&self.inner.pool, &self.inner.pubsub, args).await
    }

    #[cfg(feature = "core")]
    async fn replay_execution(
        &self,
        args: ReplayExecutionArgs,
    ) -> Result<ReplayExecutionResult, EngineError> {
        crate::operator::replay_execution_impl(&self.inner.pool, &self.inner.pubsub, args).await
    }

    // ── RFC-020 Wave 9 — Read model (Phase 3.3) ──────────────────────
    //
    // Three read-only methods paralleling PG §4.1. Normalisation
    // helpers collapse storage-tier lifecycle/state literals to the
    // `serde_snake_case` wire form; unknown tokens surface
    // `Corruption`. `get_execution_result` has current-attempt
    // semantics per RFC-020 Rev 7 Fork 3.

    #[cfg(feature = "core")]
    async fn read_execution_state(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<PublicState>, EngineError> {
        crate::reads::read_execution_state_impl(&self.inner.pool, id).await
    }

    #[cfg(feature = "core")]
    async fn read_execution_info(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionInfo>, EngineError> {
        crate::reads::read_execution_info_impl(&self.inner.pool, id).await
    }

    async fn get_execution_result(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        crate::reads::get_execution_result_impl(&self.inner.pool, id).await
    }

    // ── RFC-020 Wave 9 — Cancel-flow split (Phase 3.3) ───────────────
    //
    // `cancel_flow_header` is the atomic flow-state flip + member
    // enumeration; `ack_cancel_member` is the drain of one member +
    // parent-delete when empty. The Server composes these with its
    // wait/async machinery to build the wire-level
    // [`CancelFlowResult`]. `ack_cancel_member` is silent on the
    // outbox (RFC-020 §4.2.7 Valkey-parity).

    #[cfg(feature = "core")]
    async fn cancel_flow_header(
        &self,
        args: CancelFlowArgs,
    ) -> Result<CancelFlowHeader, EngineError> {
        crate::operator::cancel_flow_header_impl(&self.inner.pool, &self.inner.pubsub, args).await
    }

    #[cfg(feature = "core")]
    async fn ack_cancel_member(
        &self,
        flow_id: &FlowId,
        execution_id: &ExecutionId,
    ) -> Result<(), EngineError> {
        crate::operator::ack_cancel_member_impl(
            &self.inner.pool,
            flow_id.clone(),
            execution_id.clone(),
        )
        .await
    }

    // ── RFC-020 Wave 9 — list_pending_waitpoints (Phase 3.3) ─────────

    #[cfg(feature = "core")]
    async fn list_pending_waitpoints(
        &self,
        args: ListPendingWaitpointsArgs,
    ) -> Result<ListPendingWaitpointsResult, EngineError> {
        crate::suspend_ops::list_pending_waitpoints_impl(&self.inner.pool, args).await
    }

    // ── RFC-019 Stage B/C — subscribe_* (Phase 3.1) ──────────────────
    //
    // Each method wraps the Phase 2b.2.2
    // [`crate::outbox_cursor::OutboxCursorReader`] primitive against
    // the matching outbox table + broadcast channel. Cursor encoding,
    // `ScannerFilter` semantics, and event-type → typed-variant
    // mapping all mirror the Postgres reference in
    // `ff-backend-postgres/src/{lease,signal_delivery}_subscribe.rs`
    // so cross-backend consumers see identical shapes.
    //
    // `subscribe_instance_tags` stays on the trait default
    // (`Unavailable`) per RFC-020 §3.2 / the #311 deferral.

    async fn subscribe_completion(
        &self,
        cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::CompletionSubscription, EngineError> {
        let pool = self.inner.pool.clone();
        let wakeup = self.inner.pubsub.completion.subscribe();
        crate::completion_subscribe::subscribe(pool, wakeup, cursor, filter.clone()).await
    }

    async fn subscribe_lease_history(
        &self,
        cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::LeaseHistorySubscription, EngineError> {
        let pool = self.inner.pool.clone();
        let wakeup = self.inner.pubsub.lease_history.subscribe();
        crate::lease_event_subscribe::subscribe(pool, wakeup, cursor, filter.clone()).await
    }

    async fn subscribe_signal_delivery(
        &self,
        cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::SignalDeliverySubscription, EngineError> {
        let pool = self.inner.pool.clone();
        let wakeup = self.inner.pubsub.signal_delivery.subscribe();
        crate::signal_delivery_subscribe::subscribe(pool, wakeup, cursor, filter.clone()).await
    }

    // ── HMAC secret management (RFC-023 Phase 2b.2.1) ──

    async fn seed_waitpoint_hmac_secret(
        &self,
        args: SeedWaitpointHmacSecretArgs,
    ) -> Result<SeedOutcome, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| {
            crate::suspend_ops::seed_waitpoint_hmac_secret_impl(pool, args.clone())
        })
        .await
    }

    async fn rotate_waitpoint_hmac_secret_all(
        &self,
        args: RotateWaitpointHmacSecretAllArgs,
    ) -> Result<RotateWaitpointHmacSecretAllResult, EngineError> {
        let pool = &self.inner.pool;
        retry_serializable(|| {
            crate::suspend_ops::rotate_waitpoint_hmac_secret_all_impl(pool, args.clone())
        })
        .await
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
