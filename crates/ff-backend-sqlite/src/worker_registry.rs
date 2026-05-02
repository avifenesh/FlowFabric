//! RFC-025 Phase 4 — SQLite worker-registry bodies + TTL-sweep.
//!
//! Backs the five `EngineBackend` worker-registry methods
//! (`register_worker`, `heartbeat_worker`, `mark_worker_dead`,
//! `list_expired_leases`, `list_workers`) on SQLite, plus the
//! `worker_registry_ttl_sweep` reconciler that prunes rows whose
//! `last_heartbeat_ms + liveness_ttl_ms` has elapsed.
//!
//! Schema lives in `migrations/0021_worker_registry.sql`. Unlike
//! Postgres, SQLite is single-writer flat (RFC-023 §4.1 A3): no
//! `partition_key`, no HASH partitioning. `lanes` is stored as a
//! sorted-joined CSV since SQLite lacks `text[]`.
//!
//! Implementation notes mirror `ff-backend-postgres::worker_registry`
//! for behavioural parity; divergences are callouts on the call site.

use std::collections::BTreeSet;

use sqlx::{Row, SqlitePool};
#[cfg(feature = "suspension")]
use uuid::Uuid;

#[cfg(feature = "suspension")]
use ff_core::contracts::{
    ExpiredLeaseInfo, ExpiredLeasesCursor, LIST_EXPIRED_LEASES_DEFAULT_LIMIT,
    LIST_EXPIRED_LEASES_MAX_LIMIT, ListExpiredLeasesArgs, ListExpiredLeasesResult,
};
use ff_core::contracts::{
    HeartbeatWorkerArgs, HeartbeatWorkerOutcome, ListWorkersArgs, ListWorkersResult,
    MARK_WORKER_DEAD_REASON_MAX_BYTES, MarkWorkerDeadArgs, MarkWorkerDeadOutcome,
    RegisterWorkerArgs, RegisterWorkerOutcome, WorkerInfo,
};
use ff_core::engine_error::{EngineError, ValidationKind};
#[cfg(feature = "suspension")]
use ff_core::types::{AttemptIndex, ExecutionId, LeaseEpoch, LeaseId};
use ff_core::types::{LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId};

use crate::errors::map_sqlx_error;
use crate::reconcilers::ScanReport;

/// SQLite has no stable `lease_id` column on `ff_attempt` (identity
/// is `(execution_id, attempt_index, lease_epoch)`). For
/// `list_expired_leases` we surface a `LeaseId` derived from that
/// triple so two callers (or cross-backend fixtures) observe the
/// same id. Must match the PG sibling byte-for-byte.
#[cfg(feature = "suspension")]
fn synthetic_lease_uuid(exec_uuid: Uuid, attempt_index: i32, lease_epoch: i64) -> Uuid {
    let mut bytes = *exec_uuid.as_bytes();
    let ai = attempt_index.to_be_bytes();
    let le = lease_epoch.to_be_bytes();
    for i in 0..4 {
        bytes[8 + i] ^= ai[i];
    }
    for i in 0..8 {
        bytes[i] ^= le[i];
    }
    Uuid::from_bytes(bytes)
}

// ── register_worker ──────────────────────────────────────────────

pub async fn register_worker(
    pool: &SqlitePool,
    args: RegisterWorkerArgs,
) -> Result<RegisterWorkerOutcome, EngineError> {
    let lanes_csv: String = args
        .lanes
        .iter()
        .map(|l| l.0.as_str())
        .collect::<Vec<&str>>()
        .join(",");
    let caps_csv: String = args
        .capabilities
        .iter()
        .cloned()
        .collect::<Vec<String>>()
        .join(",");

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Preflight: (a) detect instance_id reassignment under a
    // different `worker_id` (reject per RFC-025 §4); (b) decide
    // Registered-vs-Refreshed from row presence — SQLite lacks the
    // PG `xmax = 0` upsert-origin signal, so the preflight SELECT
    // IS the signal.
    let existing_worker_id: Option<String> = sqlx::query_scalar(
        "SELECT worker_id FROM ff_worker_registry \
         WHERE namespace = ? AND worker_instance_id = ?",
    )
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if let Some(existing) = existing_worker_id.as_deref()
        && existing != args.worker_id.as_str()
    {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "instance_id reassigned".into(),
        });
    }
    let was_present = existing_worker_id.is_some();

    sqlx::query(
        "INSERT INTO ff_worker_registry (\
             namespace, worker_instance_id, worker_id, \
             lanes, capabilities_csv, last_heartbeat_ms, liveness_ttl_ms, \
             registered_at_ms\
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(namespace, worker_instance_id) DO UPDATE SET \
             worker_id = excluded.worker_id, \
             lanes = excluded.lanes, \
             capabilities_csv = excluded.capabilities_csv, \
             last_heartbeat_ms = excluded.last_heartbeat_ms, \
             liveness_ttl_ms = excluded.liveness_ttl_ms",
    )
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.worker_id.as_str())
    .bind(lanes_csv.as_str())
    .bind(caps_csv.as_str())
    .bind(args.now.0)
    .bind(args.liveness_ttl_ms as i64)
    .bind(args.now.0)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        "INSERT OR IGNORE INTO ff_worker_registry_event \
             (namespace, worker_instance_id, event_kind, event_at_ms, reason) \
         VALUES (?, ?, 'registered', ?, NULL)",
    )
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.now.0)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(if was_present {
        RegisterWorkerOutcome::Refreshed
    } else {
        RegisterWorkerOutcome::Registered
    })
}

// ── heartbeat_worker ─────────────────────────────────────────────

pub async fn heartbeat_worker(
    pool: &SqlitePool,
    args: HeartbeatWorkerArgs,
) -> Result<HeartbeatWorkerOutcome, EngineError> {
    // Refresh `last_heartbeat_ms` only when the row is still within
    // its declared TTL window. If the TTL has lapsed, surface
    // `NotRegistered` — the sweep will drop it on its next tick.
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let ttl_row: Option<i64> = sqlx::query_scalar(
        "UPDATE ff_worker_registry SET last_heartbeat_ms = ?3 \
         WHERE namespace = ?1 AND worker_instance_id = ?2 \
           AND last_heartbeat_ms + liveness_ttl_ms > ?3 \
         RETURNING liveness_ttl_ms",
    )
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.now.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(ttl_ms) = ttl_row else {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(HeartbeatWorkerOutcome::NotRegistered);
    };

    sqlx::query(
        "INSERT OR IGNORE INTO ff_worker_registry_event \
             (namespace, worker_instance_id, event_kind, event_at_ms, reason) \
         VALUES (?, ?, 'heartbeat', ?, NULL)",
    )
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.now.0)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let next_expiry_ms = TimestampMs::from_millis(args.now.0.saturating_add(ttl_ms));
    Ok(HeartbeatWorkerOutcome::Refreshed { next_expiry_ms })
}

// ── mark_worker_dead ─────────────────────────────────────────────

pub async fn mark_worker_dead(
    pool: &SqlitePool,
    args: MarkWorkerDeadArgs,
) -> Result<MarkWorkerDeadOutcome, EngineError> {
    // Mirrors PG + Valkey validation: 256-byte cap + no control
    // characters.
    if args.reason.len() > MARK_WORKER_DEAD_REASON_MAX_BYTES {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "reason: exceeds {} bytes (got {})",
                MARK_WORKER_DEAD_REASON_MAX_BYTES,
                args.reason.len()
            ),
        });
    }
    if args.reason.chars().any(|c| c.is_control()) {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "reason: must not contain control characters".into(),
        });
    }

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let deleted: Option<i64> = sqlx::query_scalar(
        "DELETE FROM ff_worker_registry \
         WHERE namespace = ? AND worker_instance_id = ? \
         RETURNING 1",
    )
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if deleted.is_none() {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(MarkWorkerDeadOutcome::NotRegistered);
    }

    sqlx::query(
        "INSERT OR IGNORE INTO ff_worker_registry_event \
             (namespace, worker_instance_id, event_kind, event_at_ms, reason) \
         VALUES (?, ?, 'marked_dead', ?, ?)",
    )
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.now.0)
    .bind(args.reason.as_str())
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(MarkWorkerDeadOutcome::Marked)
}

// ── list_expired_leases ──────────────────────────────────────────

#[cfg(feature = "suspension")]
pub async fn list_expired_leases(
    pool: &SqlitePool,
    args: ListExpiredLeasesArgs,
) -> Result<ListExpiredLeasesResult, EngineError> {
    let limit = args
        .limit
        .unwrap_or(LIST_EXPIRED_LEASES_DEFAULT_LIMIT)
        .min(LIST_EXPIRED_LEASES_MAX_LIMIT) as i64;
    // SQLite has no ZSET fan-out knob; accepted-ignored.
    let _ = args.max_partitions_per_call;

    let (cursor_expiry, cursor_eid_str): (Option<i64>, Option<String>) = match args.after.as_ref() {
        Some(c) => (Some(c.expires_at_ms.0), Some(c.execution_id.to_string())),
        None => (None, None),
    };
    let namespace_filter: Option<&str> = args.namespace.as_ref().map(|n| n.as_str());

    // Join `ff_attempt` (lease state) with `ff_exec_core`
    // (partition-key reconstruction + namespace filter). Unlike PG
    // where `ff_exec_core.namespace` is a column in the outstanding
    // review trail, SQLite stashes `namespace` inside the
    // `raw_fields` JSON blob (matching `ff-backend-sqlite/queries/*`
    // convention); we project it via `json_extract`. The
    // `ff_attempt_lease_expiry_idx` partial index keys the scan.
    //
    // Cursor tuple `(lease_expires_at_ms, execution_id_text)` is
    // strict-greater so pagination is stable under equal-expiry.
    // Execution id is BLOB (16 bytes); cursor compare uses the same
    // textual form the caller receives (`"{fp:N}:uuid"`) — rebuild
    // before comparing.
    let rows = sqlx::query(
        "SELECT a.partition_key, a.execution_id, a.attempt_index, a.lease_epoch, \
                a.worker_instance_id, a.lease_expires_at_ms, \
                json_extract(c.raw_fields, '$.namespace') AS namespace \
           FROM ff_attempt a \
           JOIN ff_exec_core c \
             ON c.partition_key = a.partition_key AND c.execution_id = a.execution_id \
          WHERE a.lease_expires_at_ms IS NOT NULL \
            AND a.lease_expires_at_ms <= ?1 \
            AND a.worker_instance_id IS NOT NULL \
            AND c.public_state IN ('claimed', 'running') \
            AND (?2 IS NULL OR json_extract(c.raw_fields, '$.namespace') = ?2) \
            AND (?3 IS NULL \
                 OR (a.lease_expires_at_ms > ?3) \
                 OR (a.lease_expires_at_ms = ?3 \
                     AND ('{fp:' || a.partition_key || '}:' || lower(hex(a.execution_id))) > ?4)) \
          ORDER BY a.lease_expires_at_ms ASC, a.partition_key ASC, a.execution_id ASC \
          LIMIT ?5",
    )
    .bind(args.as_of.0)
    .bind(namespace_filter)
    .bind(cursor_expiry)
    .bind(cursor_eid_str.as_deref().unwrap_or(""))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut entries: Vec<ExpiredLeaseInfo> = Vec::with_capacity(rows.len());
    for row in &rows {
        let partition_key: i64 = row.try_get("partition_key").map_err(map_sqlx_error)?;
        let exec_uuid: Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
        let attempt_index: i64 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
        let lease_epoch: i64 = row.try_get("lease_epoch").map_err(map_sqlx_error)?;
        let worker_inst: String = row.try_get("worker_instance_id").map_err(map_sqlx_error)?;
        let expires_at_ms: i64 = row.try_get("lease_expires_at_ms").map_err(map_sqlx_error)?;

        let eid_str = format!("{{fp:{}}}:{}", partition_key, exec_uuid);
        let execution_id = ExecutionId::parse(&eid_str).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("list_expired_leases: bad execution_id {eid_str:?}: {e}"),
        })?;

        let attempt_index_i32 = i32::try_from(attempt_index).unwrap_or(i32::MAX);
        let lease_id = LeaseId::from_uuid(synthetic_lease_uuid(
            exec_uuid,
            attempt_index_i32,
            lease_epoch,
        ));
        let attempt_index_u = u32::try_from(attempt_index.max(0)).unwrap_or(0);
        let lease_epoch_u = u64::try_from(lease_epoch.max(0)).unwrap_or(0);

        entries.push(ExpiredLeaseInfo::new(
            execution_id,
            lease_id,
            LeaseEpoch::new(lease_epoch_u),
            WorkerInstanceId::new(worker_inst),
            TimestampMs::from_millis(expires_at_ms),
            AttemptIndex::new(attempt_index_u),
        ));
    }

    let page_full = rows.len() as i64 >= limit;
    let cursor = if page_full {
        entries
            .last()
            .map(|e| ExpiredLeasesCursor::new(e.expires_at_ms, e.execution_id.clone()))
    } else {
        None
    };
    Ok(ListExpiredLeasesResult::new(entries, cursor))
}

// ── list_workers ─────────────────────────────────────────────────

pub async fn list_workers(
    pool: &SqlitePool,
    args: ListWorkersArgs,
) -> Result<ListWorkersResult, EngineError> {
    // RFC-025 §9.4: cross-namespace enumeration requires a two-key
    // cursor (namespace, worker_instance_id) because instance_ids
    // collide across namespaces. Phase-1's cursor is single-key
    // `Option<WorkerInstanceId>`. Mirror the PG Phase-3 decision:
    // reject cross-ns with `Unavailable` and let operator tooling
    // loop per namespace until a tuple-cursor RFC lands.
    let Some(ns) = args.namespace.as_ref() else {
        return Err(EngineError::Unavailable {
            op: "list_workers (cross-namespace on SQLite — pass namespace explicitly)",
        });
    };
    let limit = args.limit.unwrap_or(1000) as i64;
    let namespace_filter: &str = ns.as_str();
    let after_cursor: Option<&str> = args.after.as_ref().map(|w| w.as_str());

    let rows = sqlx::query(
        "SELECT worker_id, worker_instance_id, namespace, lanes, \
                capabilities_csv, last_heartbeat_ms, liveness_ttl_ms, registered_at_ms \
           FROM ff_worker_registry \
          WHERE namespace = ?1 \
            AND (?2 IS NULL OR worker_instance_id > ?2) \
          ORDER BY worker_instance_id ASC \
          LIMIT ?3",
    )
    .bind(namespace_filter)
    .bind(after_cursor)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut entries: Vec<WorkerInfo> = Vec::with_capacity(rows.len());
    for row in &rows {
        let worker_id: String = row.try_get("worker_id").map_err(map_sqlx_error)?;
        let worker_inst: String = row.try_get("worker_instance_id").map_err(map_sqlx_error)?;
        let namespace: String = row.try_get("namespace").map_err(map_sqlx_error)?;
        let lanes_csv: String = row.try_get("lanes").map_err(map_sqlx_error)?;
        let caps_csv: String = row.try_get("capabilities_csv").map_err(map_sqlx_error)?;
        let last_hb_ms: i64 = row.try_get("last_heartbeat_ms").map_err(map_sqlx_error)?;
        let liveness_ttl_ms: i64 = row.try_get("liveness_ttl_ms").map_err(map_sqlx_error)?;
        let registered_at_ms: i64 = row.try_get("registered_at_ms").map_err(map_sqlx_error)?;

        let lanes: BTreeSet<LaneId> = lanes_csv
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| LaneId(s.to_owned()))
            .collect();
        let capabilities: BTreeSet<String> = caps_csv
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();

        entries.push(WorkerInfo::new(
            WorkerId::new(worker_id),
            WorkerInstanceId::new(worker_inst),
            Namespace::new(namespace),
            lanes,
            capabilities,
            TimestampMs::from_millis(last_hb_ms),
            u64::try_from(liveness_ttl_ms.max(0)).unwrap_or(0),
            TimestampMs::from_millis(registered_at_ms),
        ));
    }

    let page_full = rows.len() as i64 >= limit;
    let cursor = if page_full {
        entries.last().map(|w| w.worker_instance_id.clone())
    } else {
        None
    };
    Ok(ListWorkersResult::new(entries, cursor))
}

// ── TTL-sweep scanner ────────────────────────────────────────────

/// Single-writer TTL sweep. Rows whose
/// `last_heartbeat_ms + liveness_ttl_ms < now_ms` are deleted and
/// a `ttl_swept` event appended to the audit log. SQLite's
/// WITH-CTE-DELETE-INSERT parses, but sqlite's WITH + DML support
/// on INSERTs with source CTEs via `INSERT ... SELECT FROM cte` is
/// tight; we use a two-statement transaction for simplicity.
///
/// Returns a `ScanReport` for the supervisor's per-tick log.
pub async fn ttl_sweep_tick(pool: &SqlitePool) -> Result<ScanReport, EngineError> {
    let now_ms: i64 = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX);

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Capture swept rows via DELETE ... RETURNING for audit
    // logging, then fan them into ff_worker_registry_event. One
    // transaction = atomic: concurrent `mark_worker_dead` on the
    // same row either beats the sweep (DELETE finds zero rows) or
    // races after it (same outcome, different `event_kind`).
    let swept = sqlx::query(
        "DELETE FROM ff_worker_registry \
         WHERE last_heartbeat_ms + liveness_ttl_ms < ? \
         RETURNING namespace, worker_instance_id",
    )
    .bind(now_ms)
    .fetch_all(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    for row in &swept {
        let namespace: String = row.try_get("namespace").map_err(map_sqlx_error)?;
        let worker_inst: String = row.try_get("worker_instance_id").map_err(map_sqlx_error)?;
        sqlx::query(
            "INSERT OR IGNORE INTO ff_worker_registry_event \
                 (namespace, worker_instance_id, event_kind, event_at_ms, reason) \
             VALUES (?, ?, 'ttl_swept', ?, NULL)",
        )
        .bind(namespace)
        .bind(worker_inst)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(ScanReport {
        processed: u32::try_from(swept.len()).unwrap_or(u32::MAX),
        errors: 0,
    })
}
