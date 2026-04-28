//! RFC-024 PR-D — `ff_claim_grant` table accessors + the two new
//! RFC-024 methods (`issue_reclaim_grant`, `reclaim_execution`).
//!
//! Replaces the pre-RFC JSON stash at
//! `ff_exec_core.raw_fields.claim_grant` (see `scheduler.rs:252` pre-
//! PR-D). The scheduler's write path now calls
//! [`write_claim_grant`] below; the scheduler's verify/read path
//! (`scheduler::verify_grant`) reads from the same table.
//!
//! # Isolation
//!
//! Both RFC-024 methods run under SERIALIZABLE + a 3-attempt retry
//! loop (mirrors `operator::cancel_execution_impl`).
//!
//! # Grant identity
//!
//! `grant_id` (BYTEA PK) is the sha256 of the Valkey/HMAC-signed
//! `grant_key` string. Deterministic, stable across retries, and
//! decoupled from the HMAC encoding (no need to re-sign on backfill).

use std::time::Duration;

use ff_core::backend::{BackendTag, Handle, HandleKind};
use ff_core::contracts::{
    IssueReclaimGrantArgs, IssueReclaimGrantOutcome, ReclaimExecutionArgs,
    ReclaimExecutionOutcome, ReclaimGrant,
};
use ff_core::engine_error::{ContentionKind, EngineError, ValidationKind};
use ff_core::handle_codec::{encode as encode_opaque, HandlePayload};
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::{AttemptIndex, ExecutionId, LeaseEpoch};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;
use crate::lease_event;
use crate::signal::{current_active_kid, hmac_sign};

const MAX_ATTEMPTS: u32 = 3;

/// Default per-Rust-surface ceiling per RFC-024 §4.6.
pub const DEFAULT_MAX_RECLAIM_COUNT: u32 = 1000;

/// sha256(grant_key) → 32-byte grant_id.
pub fn grant_id_from_key(grant_key: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(grant_key.as_bytes());
    h.finalize().to_vec()
}

fn capabilities_jsonb(caps: &std::collections::BTreeSet<String>) -> serde_json::Value {
    serde_json::Value::Array(
        caps.iter()
            .map(|c| serde_json::Value::String(c.clone()))
            .collect(),
    )
}

/// Insert a fresh claim grant row. Called by the scheduler's Stage 5.
#[allow(clippy::too_many_arguments)]
pub async fn write_claim_grant<'c>(
    tx: &mut sqlx::Transaction<'c, Postgres>,
    partition_key: i16,
    grant_key: &str,
    execution_id: Uuid,
    worker_id: &str,
    worker_instance_id: &str,
    grant_ttl_ms: u64,
    issued_at_ms: i64,
    expires_at_ms: i64,
) -> Result<(), EngineError> {
    let grant_id = grant_id_from_key(grant_key);
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
            $4, $5, NULL,
            NULL, '[]'::jsonb,
            NULL, NULL,
            $6, $7, $8
        )
        ON CONFLICT (partition_key, grant_id) DO UPDATE SET
            worker_id = EXCLUDED.worker_id,
            worker_instance_id = EXCLUDED.worker_instance_id,
            grant_ttl_ms = EXCLUDED.grant_ttl_ms,
            issued_at_ms = EXCLUDED.issued_at_ms,
            expires_at_ms = EXCLUDED.expires_at_ms
        "#,
    )
    .bind(partition_key)
    .bind(&grant_id)
    .bind(execution_id)
    .bind(worker_id)
    .bind(worker_instance_id)
    .bind(i64::try_from(grant_ttl_ms).unwrap_or(i64::MAX))
    .bind(issued_at_ms)
    .bind(expires_at_ms)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;

    // RFC-024 PR-D rollout dual-write — mirror the grant into the
    // legacy `ff_exec_core.raw_fields.claim_grant` JSON blob so an
    // older-version reader still in the rolling-deploy window can
    // verify grants issued by the new code. The new code reads from
    // `ff_claim_grant` exclusively; this write exists only for
    // mixed-version read compat and is dropped in a future
    // v0.13+ migration (0018) once the deploy window closes.
    //
    // Shape matches the pre-PR-D payload at `scheduler.rs:252`:
    //   { grant_key, worker_id, worker_instance_id,
    //     expires_at_ms, issued_at_ms, kid }
    // `kid` is not available here (sig lives inside `grant_key`);
    // legacy verify code only needs the 5 fields it actually reads.
    let grant_json = serde_json::json!({
        "grant_key": grant_key,
        "worker_id": worker_id,
        "worker_instance_id": worker_instance_id,
        "expires_at_ms": expires_at_ms,
        "issued_at_ms": issued_at_ms,
    });
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET raw_fields = jsonb_set(
                   COALESCE(raw_fields, '{}'::jsonb),
                   '{claim_grant}',
                   $3::jsonb,
                   true)
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .bind(grant_json)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;

    Ok(())
}

/// Read a claim-grant row (kind='claim') for verification /
/// diagnostics. Returns `Ok(None)` when absent.
pub async fn read_claim_grant_identity(
    pool: &PgPool,
    partition_key: i16,
    execution_id: Uuid,
) -> Result<Option<(String, String)>, EngineError> {
    let row = sqlx::query(
        r#"
        SELECT worker_id, worker_instance_id
          FROM ff_claim_grant
         WHERE partition_key = $1
           AND execution_id = $2
           AND kind = 'claim'
         ORDER BY issued_at_ms DESC
         LIMIT 1
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    Ok(row.map(|r| {
        (
            r.get::<String, _>("worker_id"),
            r.get::<String, _>("worker_instance_id"),
        )
    }))
}

// ─── retry helpers ──────────────────────────────────────────────────

fn is_retryable_serialization(err: &EngineError) -> bool {
    matches!(err, EngineError::Contention(ContentionKind::LeaseConflict))
}

async fn begin_serializable(pool: &PgPool) -> Result<sqlx::Transaction<'_, Postgres>, EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    Ok(tx)
}

fn split_exec_id(eid: &ExecutionId) -> Result<(i16, Uuid), EngineError> {
    let s = eid.as_str();
    let rest = s.strip_prefix("{fp:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `{{fp:` prefix: {s}"),
    })?;
    let close = rest.find("}:").ok_or_else(|| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id missing `}}:`: {s}"),
    })?;
    // Parse the wire-shape `u16` partition index and cast to the
    // schema's `smallint` column type. Matches the
    // `partition.index as i16` convention used in
    // `budget.rs:221`/`exec_core.rs:318`. Parsing directly as `i16`
    // would accept negatives and wrap under cast — wrong hash-tag
    // routing on negative input.
    let part_u: u16 = rest[..close].parse().map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id partition index not u16: {s}"),
    })?;
    let part = part_u as i16;
    let uuid = Uuid::parse_str(&rest[close + 2..]).map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id UUID invalid: {s}"),
    })?;
    Ok((part, uuid))
}

fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    (d.as_millis() as i64).max(0)
}

// ─── issue_reclaim_grant ─────────────────────────────────────────────

async fn issue_reclaim_grant_once(
    pool: &PgPool,
    args: &IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let (kid, secret) = current_active_kid(pool)
        .await?
        .ok_or(EngineError::Unavailable {
            op: "issue_reclaim_grant: ff_waitpoint_hmac keystore empty",
        })?;

    let mut tx = begin_serializable(pool).await?;

    // RFC §4.2 admission gate — lifecycle_phase='active' AND either
    // a reclaimable ownership_state literal OR the pre-RFC implicit
    // "lease expired / released" shape (lease_expires_at_ms <= now OR
    // worker_instance_id NULL).
    let row = sqlx::query(
        r#"
        SELECT ec.lifecycle_phase,
               ec.ownership_state,
               ec.lease_reclaim_count,
               a.lease_expires_at_ms,
               a.worker_instance_id
          FROM ff_exec_core ec
          LEFT JOIN ff_attempt a
            ON a.partition_key = ec.partition_key
           AND a.execution_id  = ec.execution_id
           AND a.attempt_index = ec.attempt_index
         WHERE ec.partition_key = $1 AND ec.execution_id = $2
         FOR NO KEY UPDATE OF ec
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };

    let lifecycle_phase: String = row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let ownership_state: String = row.try_get("ownership_state").map_err(map_sqlx_error)?;
    let reclaim_count: i32 = row.try_get("lease_reclaim_count").map_err(map_sqlx_error)?;
    let lease_expires_at: Option<i64> = row
        .try_get::<Option<i64>, _>("lease_expires_at_ms")
        .map_err(map_sqlx_error)?;
    let worker_instance_id: Option<String> = row
        .try_get::<Option<String>, _>("worker_instance_id")
        .map_err(map_sqlx_error)?;

    let reclaim_count_u = u32::try_from(reclaim_count.max(0)).unwrap_or(0);
    if reclaim_count_u >= DEFAULT_MAX_RECLAIM_COUNT {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(IssueReclaimGrantOutcome::ReclaimCapExceeded {
            execution_id: args.execution_id.clone(),
            reclaim_count: reclaim_count_u,
        });
    }

    let now = now_ms();
    let lease_expired = match lease_expires_at {
        Some(exp) => exp <= now,
        None => worker_instance_id.is_none() || worker_instance_id.as_deref() == Some(""),
    };
    let reclaimable_state = matches!(
        ownership_state.as_str(),
        "lease_expired_reclaimable" | "lease_revoked"
    );
    let phase_active = lifecycle_phase == "active";
    if !(phase_active && (reclaimable_state || lease_expired)) {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(IssueReclaimGrantOutcome::NotReclaimable {
            execution_id: args.execution_id.clone(),
            detail: format!(
                "execution not reclaimable: lifecycle_phase={lifecycle_phase}, ownership_state={ownership_state}"
            ),
        });
    }

    let partition = Partition {
        family: PartitionFamily::Execution,
        index: part as u16,
    };
    let hash_tag = partition.hash_tag();
    let expires_at_ms = now.saturating_add_unsigned(args.grant_ttl_ms.min(i64::MAX as u64));
    let message = format!(
        "{hash_tag}|{exec_uuid}|{wid}|{wiid}|{exp}|reclaim",
        wid = args.worker_id.as_str(),
        wiid = args.worker_instance_id.as_str(),
        exp = expires_at_ms,
    );
    let sig = hmac_sign(&secret, &kid, message.as_bytes());
    let grant_key = format!("pg:reclaim:{hash_tag}:{exec_uuid}:{expires_at_ms}:{sig}");
    let grant_id = grant_id_from_key(&grant_key);

    sqlx::query(
        r#"
        INSERT INTO ff_claim_grant (
            partition_key, grant_id, execution_id, kind,
            worker_id, worker_instance_id, lane_id,
            capability_hash, worker_capabilities,
            route_snapshot_json, admission_summary,
            grant_ttl_ms, issued_at_ms, expires_at_ms
        ) VALUES (
            $1, $2, $3, 'reclaim',
            $4, $5, $6,
            $7, $8,
            $9, $10,
            $11, $12, $13
        )
        "#,
    )
    .bind(part)
    .bind(&grant_id)
    .bind(exec_uuid)
    .bind(args.worker_id.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.lane_id.as_str())
    .bind(args.capability_hash.as_deref())
    .bind(capabilities_jsonb(&args.worker_capabilities))
    .bind(args.route_snapshot_json.as_deref())
    .bind(args.admission_summary.as_deref())
    .bind(i64::try_from(args.grant_ttl_ms).unwrap_or(i64::MAX))
    .bind(now)
    .bind(expires_at_ms)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(IssueReclaimGrantOutcome::Granted(ReclaimGrant::new(
        args.execution_id.clone(),
        PartitionKey::from(&partition),
        grant_key,
        expires_at_ms as u64,
        args.lane_id.clone(),
    )))
}

pub async fn issue_reclaim_grant_impl(
    pool: &PgPool,
    args: IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError> {
    let mut last: Option<EngineError> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match issue_reclaim_grant_once(pool, &args).await {
            Ok(r) => return Ok(r),
            Err(err) if is_retryable_serialization(&err) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    let ms = 5u64 * (1u64 << attempt);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                last = Some(err);
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    let _ = last;
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

// ─── reclaim_execution ─────────────────────────────────────────────

async fn reclaim_execution_once(
    pool: &PgPool,
    args: &ReclaimExecutionArgs,
) -> Result<ReclaimExecutionOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let max_reclaim_count = args.max_reclaim_count.unwrap_or(DEFAULT_MAX_RECLAIM_COUNT);

    let mut tx = begin_serializable(pool).await?;

    let grant_row = sqlx::query(
        r#"
        SELECT grant_id, worker_id, expires_at_ms, lane_id
          FROM ff_claim_grant
         WHERE partition_key = $1
           AND execution_id = $2
           AND kind = 'reclaim'
         ORDER BY issued_at_ms DESC
         FOR UPDATE
         LIMIT 1
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(grant_row) = grant_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(ReclaimExecutionOutcome::GrantNotFound {
            execution_id: args.execution_id.clone(),
        });
    };
    let grant_id: Vec<u8> = grant_row.try_get("grant_id").map_err(map_sqlx_error)?;
    let grant_worker_id: String = grant_row.try_get("worker_id").map_err(map_sqlx_error)?;
    let grant_expires_at_ms: i64 = grant_row
        .try_get("expires_at_ms")
        .map_err(map_sqlx_error)?;

    // RFC-024 §4.4 worker identity contract: worker_id must match;
    // worker_instance_id is informational only.
    if grant_worker_id != args.worker_id.as_str() {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "reclaim_execution: grant.worker_id={grant_worker_id} != args.worker_id={}",
                args.worker_id.as_str()
            ),
        });
    }
    let now = now_ms();
    if grant_expires_at_ms <= now {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(ReclaimExecutionOutcome::GrantNotFound {
            execution_id: args.execution_id.clone(),
        });
    }

    let core_row = sqlx::query(
        r#"
        SELECT lifecycle_phase, attempt_index, lease_reclaim_count
          FROM ff_exec_core
         WHERE partition_key = $1 AND execution_id = $2
         FOR NO KEY UPDATE
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let Some(core_row) = core_row else {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };
    let lifecycle_phase: String = core_row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let cur_attempt_index: i32 = core_row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let cur_reclaim_count: i32 = core_row
        .try_get("lease_reclaim_count")
        .map_err(map_sqlx_error)?;

    if lifecycle_phase == "terminal" || lifecycle_phase == "cancelled" {
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(ReclaimExecutionOutcome::NotReclaimable {
            execution_id: args.execution_id.clone(),
            detail: format!("execution terminal: lifecycle_phase={lifecycle_phase}"),
        });
    }

    // Column is `INTEGER NOT NULL DEFAULT 0` so the row value SHOULD
    // always be ≥ 0. Clamp defensively anyway: a negative value would
    // wrap under `as u32` and skip the cap check.
    let next_reclaim_count = (cur_reclaim_count.max(0) as u32).saturating_add(1);
    if next_reclaim_count > max_reclaim_count {
        sqlx::query(
            r#"
            UPDATE ff_exec_core
               SET lifecycle_phase    = 'terminal',
                   ownership_state    = 'unowned',
                   eligibility_state  = 'not_applicable',
                   public_state       = 'failed',
                   attempt_state      = 'terminal_failed',
                   terminal_at_ms     = COALESCE(terminal_at_ms, $3),
                   lease_reclaim_count = $4,
                   cancellation_reason = COALESCE(cancellation_reason, 'reclaim_cap_exceeded')
             WHERE partition_key = $1 AND execution_id = $2
            "#,
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(now)
        .bind(i32::try_from(next_reclaim_count).unwrap_or(i32::MAX))
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        sqlx::query("DELETE FROM ff_claim_grant WHERE partition_key = $1 AND grant_id = $2")
            .bind(part)
            .bind(&grant_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(ReclaimExecutionOutcome::ReclaimCapExceeded {
            execution_id: args.execution_id.clone(),
            reclaim_count: next_reclaim_count,
        });
    }

    // Mark prior attempt `interrupted_reclaimed`.
    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET outcome = 'interrupted_reclaimed',
               terminal_at_ms = COALESCE(terminal_at_ms, $4)
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(cur_attempt_index)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Insert NEW attempt row (attempt_index = cur+1, epoch = 1,
    // attempt_type = 'reclaim' in raw_fields).
    let new_attempt_index = cur_attempt_index + 1;
    let lease_ttl_ms = i64::try_from(args.lease_ttl_ms).unwrap_or(i64::MAX);
    let new_lease_expires = now.saturating_add(lease_ttl_ms);
    sqlx::query(
        r#"
        INSERT INTO ff_attempt (
            partition_key, execution_id, attempt_index,
            worker_id, worker_instance_id,
            lease_epoch, lease_expires_at_ms, started_at_ms,
            raw_fields
        ) VALUES (
            $1, $2, $3,
            $4, $5,
            1, $6, $7,
            jsonb_build_object('attempt_type', 'reclaim')
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(new_attempt_index)
    .bind(args.worker_id.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(new_lease_expires)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Flip exec_core to active on the new attempt; bump reclaim count.
    sqlx::query(
        r#"
        UPDATE ff_exec_core
           SET lifecycle_phase   = 'active',
               ownership_state   = 'leased',
               eligibility_state = 'not_applicable',
               public_state      = 'running',
               attempt_state     = 'running_attempt',
               attempt_index     = $3,
               lease_reclaim_count = $4,
               started_at_ms     = COALESCE(started_at_ms, $5)
         WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(new_attempt_index)
    .bind(i32::try_from(next_reclaim_count).unwrap_or(i32::MAX))
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Consume the grant.
    sqlx::query("DELETE FROM ff_claim_grant WHERE partition_key = $1 AND grant_id = $2")
        .bind(part)
        .bind(&grant_id)
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

    // RFC-024 §3.3 / §4.4: the HandlePayload carries the
    // caller-supplied `attempt_id` + `lease_id`. Valkey PR-F round-
    // trips these through `ff_reclaim_execution` ARGV[5] (lease_id) +
    // ARGV[7] (attempt_id) — the Lua echoes them back in the success
    // tuple, so the handle the Valkey surface mints embeds the
    // caller's identifiers. Postgres has no Lua round-trip; we use
    // the args values directly to preserve cross-backend parity +
    // idempotency + lease fencing.
    let payload = HandlePayload::new(
        args.execution_id.clone(),
        AttemptIndex::new(u32::try_from(new_attempt_index.max(0)).unwrap_or(0)),
        args.attempt_id.clone(),
        args.lease_id.clone(),
        LeaseEpoch(1),
        u32::try_from(args.lease_ttl_ms.min(u32::MAX as u64)).unwrap_or(u32::MAX) as u64,
        args.lane_id.clone(),
        args.worker_instance_id.clone(),
    );
    Ok(ReclaimExecutionOutcome::Claimed(mint_handle(
        payload,
        HandleKind::Reclaimed,
    )))
}

pub async fn reclaim_execution_impl(
    pool: &PgPool,
    args: ReclaimExecutionArgs,
) -> Result<ReclaimExecutionOutcome, EngineError> {
    let mut last: Option<EngineError> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match reclaim_execution_once(pool, &args).await {
            Ok(r) => return Ok(r),
            Err(err) if is_retryable_serialization(&err) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    let ms = 5u64 * (1u64 << attempt);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                last = Some(err);
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    let _ = last;
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

fn mint_handle(payload: HandlePayload, kind: HandleKind) -> Handle {
    let op = encode_opaque(BackendTag::Postgres, &payload);
    Handle::new(BackendTag::Postgres, kind, op)
}
