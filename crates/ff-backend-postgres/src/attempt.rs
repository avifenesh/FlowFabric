//! Attempt trait-method family — Postgres implementation.
//!
//! **RFC-v0.7 Wave 4b.** Bodies for `claim`, `claim_from_resume_grant`,
//! `renew`, `progress`, `complete`, `fail`, `delay`, `wait_children`.
//!
//! # Fence-triple invariants (RFC-003)
//!
//! Every RMW against `ff_attempt` is wrapped in a BEGIN/COMMIT + a
//! `SELECT ... FOR UPDATE` of the target attempt row. The claim cascade
//! uses `FOR UPDATE SKIP LOCKED` on the exec_core + attempt join so two
//! workers racing on the same lane cannot double-claim. Lease epoch is
//! the authoritative fencing token: every terminal/progress/renew op
//! first re-reads the attempt row under lock and compares
//! `lease_epoch` against the handle's embedded epoch. Mismatch →
//! `EngineError::Contention(LeaseConflict)`.
//!
//! # Isolation level (Q11)
//!
//! Default `READ COMMITTED`. The claim query uses `SKIP LOCKED` to
//! keep the scanner contention-free; terminal ops use row-level
//! `FOR UPDATE` on the already-identified attempt row. This is enough
//! for the fence-triple invariant because the primary key on
//! `ff_attempt` is `(partition_key, execution_id, attempt_index)` —
//! the `FOR UPDATE` is partition-local and deterministic.
//!
//! # Isolation note on capability matching
//!
//! Capability subset-match runs in Rust *after* the FOR UPDATE SKIP
//! LOCKED row is obtained. If the worker does not satisfy the row's
//! required_capabilities, we `ROLLBACK` (releasing the lock) and
//! return `Ok(None)` so another worker with the right caps can pick
//! the row up. We do NOT try to re-scan within the same claim call
//! — that would blow the blocking-wait budget in a pathological
//! mismatch loop; the caller's retry cadence is the right scope.

use ff_core::backend::{
    BackendTag, CapabilitySet, ClaimPolicy, FailOutcome, FailureClass, FailureReason, Handle,
    HandleKind, LeaseRenewal, ResumeToken,
};
use ff_core::caps::{matches as caps_matches, CapabilityRequirement};
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::handle_codec::{decode as decode_opaque, encode as encode_opaque, HandlePayload};
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, TimestampMs,
};
use sqlx::{PgPool, Row};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::error::map_sqlx_error;
use crate::lease_event;

// ── helpers ─────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

/// Extract `(partition_index, uuid)` from an `ExecutionId` formatted
/// `{fp:N}:<uuid>`.
fn split_exec_id(eid: &ExecutionId) -> Result<(i16, Uuid), EngineError> {
    let s = eid.as_str();
    let rest = s.strip_prefix("{fp:").ok_or_else(|| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: format!("execution_id missing `{{fp:` prefix: {s}"),
    })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: format!("execution_id missing `}}:`: {s}"),
    })?;
    let part: i16 = rest[..close]
        .parse()
        .map_err(|_| EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: format!("execution_id partition index not u16: {s}"),
        })?;
    let uuid = Uuid::parse_str(&rest[close + 2..]).map_err(|_| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: format!("execution_id UUID invalid: {s}"),
    })?;
    Ok((part, uuid))
}

fn decode_handle(handle: &Handle) -> Result<HandlePayload, EngineError> {
    if handle.backend != BackendTag::Postgres {
        return Err(EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::Corruption,
            detail: format!(
                "handle minted by {:?} passed to Postgres backend",
                handle.backend
            ),
        });
    }
    let decoded = decode_opaque(&handle.opaque)?;
    if decoded.tag != BackendTag::Postgres {
        return Err(EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::Corruption,
            detail: format!("inner handle tag mismatch: {:?}", decoded.tag),
        });
    }
    Ok(decoded.payload)
}

fn mint_handle(payload: HandlePayload, kind: HandleKind) -> Handle {
    let op = encode_opaque(BackendTag::Postgres, &payload);
    Handle::new(BackendTag::Postgres, kind, op)
}

// ── claim ───────────────────────────────────────────────────────────────

pub(crate) async fn claim(
    pool: &PgPool,
    lane: &LaneId,
    capabilities: &CapabilitySet,
    policy: &ClaimPolicy,
) -> Result<Option<Handle>, EngineError> {
    // We scan each partition in random order. For Wave 4b we iterate
    // partitions 0..256; a production path would use a sampled order +
    // per-lane cache. Keeping the happy-path simple: the test fixture
    // inserts into a known partition, so we scan all 256.
    let total_partitions: i16 = 256;
    for part in 0..total_partitions {
        match try_claim_in_partition(pool, part, lane, capabilities, policy).await? {
            Some(h) => return Ok(Some(h)),
            None => continue,
        }
    }
    Ok(None)
}

async fn try_claim_in_partition(
    pool: &PgPool,
    part: i16,
    lane: &LaneId,
    capabilities: &CapabilitySet,
    policy: &ClaimPolicy,
) -> Result<Option<Handle>, EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    // SELECT one eligible exec row in this partition/lane — FOR UPDATE
    // SKIP LOCKED keeps contending workers from pile-ups.
    let row = sqlx::query(
        r#"
        SELECT execution_id, required_capabilities, attempt_index
          FROM ff_exec_core
         WHERE partition_key = $1
           AND lane_id = $2
           AND lifecycle_phase = 'runnable'
           AND eligibility_state = 'eligible_now'
         ORDER BY priority DESC, created_at_ms ASC
         FOR UPDATE SKIP LOCKED
         LIMIT 1
        "#,
    )
    .bind(part)
    .bind(lane.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        // No candidate in this partition — release the tx.
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(None);
    };

    let exec_uuid: Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
    let required_caps: Vec<String> = row
        .try_get::<Vec<String>, _>("required_capabilities")
        .map_err(map_sqlx_error)?;
    let attempt_index_i: i32 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let req = CapabilityRequirement::new(required_caps);
    if !caps_matches(&req, capabilities) {
        // Release the exec row lock; skip.
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(None);
    }

    let attempt_index = AttemptIndex::new(u32::try_from(attempt_index_i.max(0)).unwrap_or(0));
    let now = now_ms();
    let lease_ttl_ms = i64::from(policy.lease_ttl_ms);
    let expires = now.saturating_add(lease_ttl_ms);

    // UPSERT the attempt row: fresh lease epoch 1 on first claim; on
    // a retry attempt the attempt_index is new so the PK doesn't
    // collide. We always INSERT ON CONFLICT DO UPDATE to be safe.
    sqlx::query(
        r#"
        INSERT INTO ff_attempt (
            partition_key, execution_id, attempt_index,
            worker_id, worker_instance_id,
            lease_epoch, lease_expires_at_ms, started_at_ms
        ) VALUES ($1, $2, $3, $4, $5, 1, $6, $7)
        ON CONFLICT (partition_key, execution_id, attempt_index)
        DO UPDATE SET
            worker_id = EXCLUDED.worker_id,
            worker_instance_id = EXCLUDED.worker_instance_id,
            lease_epoch = ff_attempt.lease_epoch + 1,
            lease_expires_at_ms = EXCLUDED.lease_expires_at_ms,
            started_at_ms = EXCLUDED.started_at_ms,
            outcome = NULL
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index_i)
    .bind(policy.worker_id.as_str())
    .bind(policy.worker_instance_id.as_str())
    .bind(expires)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Re-read the epoch we just wrote.
    let epoch_row = sqlx::query(
        r#"
        SELECT lease_epoch FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index_i)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let epoch_i: i64 = epoch_row.try_get("lease_epoch").map_err(map_sqlx_error)?;

    // Flip exec_core to active. #356: `started_at_ms` is set-once —
    // `COALESCE(started_at_ms, $3)` preserves the original first-claim
    // timestamp across reclaim + retry attempts, matching Valkey's
    // dedicated set-once `exec_core["started_at"]` field.
    //
    // `public_state = 'running'` mirrors the resume-claim write in
    // `suspend_ops.rs` and the SQLite first-claim write in
    // `ff-backend-sqlite/src/queries/exec_core.rs`. Without this field
    // the row stayed at its create-time `'waiting'` literal on PG
    // while Spine-B readers expected the claimed-execution literal.
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'active',
               ownership_state = 'leased',
               eligibility_state = 'not_applicable',
               public_state = 'running',
               attempt_state = 'running_attempt',
               started_at_ms = COALESCE(started_at_ms, $3)
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox: lease acquired.
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

    let exec_id = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).map_err(|e| {
        EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: format!("reassembling exec id: {e}"),
        }
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
    Ok(Some(mint_handle(payload, HandleKind::Fresh)))
}

// ── claim_from_resume_grant ──────────────────────────────────────────────────

pub(crate) async fn claim_from_resume_grant(
    pool: &PgPool,
    token: ResumeToken,
) -> Result<Option<Handle>, EngineError> {
    let eid = &token.grant.execution_id;
    let (part, exec_uuid) = split_exec_id(eid)?;
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    // Lock the attempt row.
    let row = sqlx::query(
        r#"
        SELECT attempt_index, lease_epoch, lease_expires_at_ms
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2
         ORDER BY attempt_index DESC
         FOR UPDATE
         LIMIT 1
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let Some(row) = row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::NotFound { entity: "attempt" });
    };
    let attempt_index_i: i32 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let current_epoch: i64 = row.try_get("lease_epoch").map_err(map_sqlx_error)?;
    let expires_at: Option<i64> = row
        .try_get::<Option<i64>, _>("lease_expires_at_ms")
        .map_err(map_sqlx_error)?;

    // Live-lease check. A valid reclaim requires the prior lease
    // to have expired (lease_expires_at_ms <= now) OR to be NULL
    // (released).
    let now = now_ms();
    let live = matches!(expires_at, Some(exp) if exp > now);
    if live {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(None); // caller sees `None` — documented below is LeaseConflict; but per trait shape Ok(None) means "grant no longer available". Use Contention when semantics demand hard signal.
    }

    // Bump epoch + install new worker + fresh expiry.
    let lease_ttl_ms = i64::from(token.lease_ttl_ms);
    let new_expires = now.saturating_add(lease_ttl_ms);
    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET worker_id = $1,
               worker_instance_id = $2,
               lease_epoch = lease_epoch + 1,
               lease_expires_at_ms = $3,
               started_at_ms = $4,
               outcome = NULL
         WHERE partition_key = $5 AND execution_id = $6 AND attempt_index = $7
        "#,
    )
    .bind(token.worker_id.as_str())
    .bind(token.worker_instance_id.as_str())
    .bind(new_expires)
    .bind(now)
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index_i)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'active',
               ownership_state = 'leased',
               eligibility_state = 'not_applicable',
               attempt_state = 'running_attempt'
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox: lease reclaimed.
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        None,
        lease_event::EVENT_RECLAIMED,
        now,
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let new_epoch = current_epoch.saturating_add(1);
    let payload = HandlePayload::new(
        eid.clone(),
        AttemptIndex::new(u32::try_from(attempt_index_i.max(0)).unwrap_or(0)),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(u64::try_from(new_epoch).unwrap_or(0)),
        u64::from(token.lease_ttl_ms),
        token.grant.lane_id.clone(),
        token.worker_instance_id.clone(),
    );
    Ok(Some(mint_handle(payload, HandleKind::Resumed)))
}

// ── fence check ─────────────────────────────────────────────────────────

/// Re-read the attempt row under FOR UPDATE + validate the handle's
/// `lease_epoch` matches. Returns the locked row's
/// `(attempt_index, lease_expires_at_ms)` for callers that need them
/// for post-update logic. Fence mismatch → `Contention(LeaseConflict)`.
async fn fence_check<'c>(
    tx: &mut sqlx::Transaction<'c, sqlx::Postgres>,
    part: i16,
    exec_uuid: Uuid,
    attempt_index: i32,
    expected_epoch: u64,
) -> Result<(), EngineError> {
    let row = sqlx::query(
        r#"
        SELECT lease_epoch FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
         FOR UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_optional(&mut **tx)
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

// ── renew ───────────────────────────────────────────────────────────────

pub(crate) async fn renew(
    pool: &PgPool,
    handle: &Handle,
) -> Result<LeaseRenewal, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i32::try_from(payload.attempt_index.0).unwrap_or(0);
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    fence_check(&mut tx, part, exec_uuid, attempt_index, payload.lease_epoch.0).await?;
    let now = now_ms();
    let new_expires = now.saturating_add(i64::try_from(payload.lease_ttl_ms).unwrap_or(0));
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
    // RFC-019 Stage B outbox: lease renewed.
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
    Ok(LeaseRenewal::new(
        u64::try_from(new_expires).unwrap_or(0),
        payload.lease_epoch.0,
    ))
}

// ── progress ────────────────────────────────────────────────────────────

pub(crate) async fn progress(
    pool: &PgPool,
    handle: &Handle,
    percent: Option<u8>,
    message: Option<String>,
) -> Result<(), EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i32::try_from(payload.attempt_index.0).unwrap_or(0);
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    fence_check(&mut tx, part, exec_uuid, attempt_index, payload.lease_epoch.0).await?;
    // Stash progress on the exec_core raw_fields jsonb. raw_fields is
    // the Wave-3 contract for "fields that don't yet have a typed
    // column" — progress_pct + progress_message live here until a
    // follow-up migration promotes them. This preserves the op's
    // observable side effect (caller can read it back) without
    // forking the schema.
    let mut patch = serde_json::Map::new();
    if let Some(pct) = percent {
        patch.insert("progress_pct".into(), serde_json::Value::from(pct));
    }
    if let Some(msg) = message {
        patch.insert("progress_message".into(), serde_json::Value::from(msg));
    }
    let patch_val = serde_json::Value::Object(patch);
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET raw_fields = raw_fields || $1::jsonb
         WHERE partition_key = $2 AND execution_id = $3
        "#,
    )
    .bind(patch_val)
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

// ── complete ────────────────────────────────────────────────────────────

pub(crate) async fn complete(
    pool: &PgPool,
    handle: &Handle,
    payload_bytes: Option<Vec<u8>>,
) -> Result<(), EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i32::try_from(payload.attempt_index.0).unwrap_or(0);
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    fence_check(&mut tx, part, exec_uuid, attempt_index, payload.lease_epoch.0).await?;
    let now = now_ms();

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

    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'terminal',
               ownership_state = 'unowned',
               eligibility_state = 'not_applicable',
               attempt_state = 'attempt_terminal',
               terminal_at_ms = $1,
               result = $2
         WHERE partition_key = $3 AND execution_id = $4
        "#,
    )
    .bind(now)
    .bind(payload_bytes.as_deref())
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Outbox: emit completion event → trigger fires pg_notify.
    sqlx::query(
        r#"
        INSERT INTO ff_completion_event (
            partition_key, execution_id, flow_id, outcome,
            namespace, instance_tag, occurred_at_ms
        )
        SELECT partition_key, execution_id, flow_id, 'success',
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

    // RFC-019 Stage B outbox: lease revoked (terminal success).
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
    Ok(())
}

// ── fail ────────────────────────────────────────────────────────────────

pub(crate) async fn fail(
    pool: &PgPool,
    handle: &Handle,
    reason: FailureReason,
    classification: FailureClass,
) -> Result<FailOutcome, EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i32::try_from(payload.attempt_index.0).unwrap_or(0);
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    fence_check(&mut tx, part, exec_uuid, attempt_index, payload.lease_epoch.0).await?;
    let now = now_ms();
    let retryable = matches!(
        classification,
        FailureClass::Transient | FailureClass::InfraCrash
    );

    if retryable {
        // Retry: re-enqueue the exec, release lease, bump attempt_index.
        sqlx::query(
            r#"
            UPDATE ff_attempt
               SET terminal_at_ms = $1,
                   outcome = 'retry',
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

        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'runnable',
                   ownership_state = 'unowned',
                   eligibility_state = 'eligible_now',
                   attempt_state = 'pending_retry_attempt',
                   attempt_index = attempt_index + 1,
                   raw_fields = raw_fields || jsonb_build_object('last_failure_message', $1::text)
             WHERE partition_key = $2 AND execution_id = $3
            "#,
        )
        .bind(&reason.message)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // RFC-019 Stage B outbox: lease revoked (retry scheduled).
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
        Ok(FailOutcome::RetryScheduled {
            delay_until: TimestampMs::from_millis(now),
        })
    } else {
        // Terminal-failed.
        sqlx::query(
            r#"
            UPDATE ff_attempt
               SET terminal_at_ms = $1,
                   outcome = 'failed',
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

        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase = 'terminal',
                   ownership_state = 'unowned',
                   eligibility_state = 'not_applicable',
                   attempt_state = 'attempt_terminal',
                   terminal_at_ms = $1,
                   raw_fields = raw_fields || jsonb_build_object('last_failure_message', $2::text)
             WHERE partition_key = $3 AND execution_id = $4
            "#,
        )
        .bind(now)
        .bind(&reason.message)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

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

        // RFC-019 Stage B outbox: lease revoked (terminal fail).
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
        Ok(FailOutcome::TerminalFailed)
    }
}

// ── delay ───────────────────────────────────────────────────────────────

pub(crate) async fn delay(
    pool: &PgPool,
    handle: &Handle,
    delay_until: TimestampMs,
) -> Result<(), EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i32::try_from(payload.attempt_index.0).unwrap_or(0);
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    fence_check(&mut tx, part, exec_uuid, attempt_index, payload.lease_epoch.0).await?;

    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET outcome = 'interrupted',
               lease_expires_at_ms = NULL
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'runnable',
               ownership_state = 'unowned',
               eligibility_state = 'not_eligible_until_time',
               attempt_state = 'attempt_interrupted',
               deadline_at_ms = $1
         WHERE partition_key = $2 AND execution_id = $3
        "#,
    )
    .bind(delay_until.0)
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox: lease revoked (delay).
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        None,
        lease_event::EVENT_REVOKED,
        now_ms(),
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

// ── wait_children ───────────────────────────────────────────────────────

pub(crate) async fn wait_children(
    pool: &PgPool,
    handle: &Handle,
) -> Result<(), EngineError> {
    let payload = decode_handle(handle)?;
    let (part, exec_uuid) = split_exec_id(&payload.execution_id)?;
    let attempt_index = i32::try_from(payload.attempt_index.0).unwrap_or(0);
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    fence_check(&mut tx, part, exec_uuid, attempt_index, payload.lease_epoch.0).await?;

    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET outcome = 'waiting_children',
               lease_expires_at_ms = NULL
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase = 'runnable',
               ownership_state = 'unowned',
               eligibility_state = 'blocked_by_dependencies',
               attempt_state = 'attempt_interrupted',
               blocking_reason = 'waiting_for_children'
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // RFC-019 Stage B outbox: lease revoked (wait_children).
    lease_event::emit(
        &mut tx,
        part,
        exec_uuid,
        None,
        lease_event::EVENT_REVOKED,
        now_ms(),
    )
    .await?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

