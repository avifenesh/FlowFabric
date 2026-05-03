//! RFC-025 Phase 3 — Postgres worker-registry bodies + TTL-sweep.
//!
//! Backs the five `EngineBackend` worker-registry methods
//! (`register_worker`, `heartbeat_worker`, `mark_worker_dead`,
//! `list_expired_leases`, `list_workers`) on Postgres, plus the
//! `worker_registry_ttl_sweep` reconciler that prunes rows whose
//! `last_heartbeat_ms + liveness_ttl_ms` has elapsed.
//!
//! Schema shape lives in `migrations/0021_worker_registry.sql`
//! (current-state table + append-only event log). Partition key is
//! `(fnv1a_u64(worker_instance_id.as_bytes()) % 256) as i16` — identical
//! to the Valkey-side hashing so `register` / `heartbeat` /
//! `mark_dead` / `ttl_sweep` all land on the same row for the same
//! instance id.

use std::collections::BTreeSet;

use sqlx::{PgPool, Row};
use uuid::Uuid;

use ff_core::contracts::{
    ExpiredLeaseInfo, ExpiredLeasesCursor, HeartbeatWorkerArgs, HeartbeatWorkerOutcome,
    LIST_EXPIRED_LEASES_DEFAULT_LIMIT, LIST_EXPIRED_LEASES_MAX_LIMIT, ListExpiredLeasesArgs,
    ListExpiredLeasesResult, ListWorkersArgs, ListWorkersResult, MARK_WORKER_DEAD_REASON_MAX_BYTES,
    MarkWorkerDeadArgs, MarkWorkerDeadOutcome, RegisterWorkerArgs, RegisterWorkerOutcome,
    WorkerInfo,
};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::hash::fnv1a_u64;
use ff_core::types::{
    AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, Namespace, TimestampMs, WorkerId,
    WorkerInstanceId,
};

use crate::error::map_sqlx_error;
use crate::reconcilers::ScanReport;

/// Derive the `ff_worker_registry.partition_key` for an instance id.
/// Must match the Valkey-side hashing (both paths own the same row).
pub fn worker_partition_key(worker_instance_id: &str) -> i16 {
    (fnv1a_u64(worker_instance_id.as_bytes()) % 256) as i16
}

/// Postgres has no stable `lease_id` column on `ff_attempt` (identity
/// is `(execution_id, attempt_index, lease_epoch)`; see
/// `operator::synthetic_lease_id`). For `list_expired_leases` we need
/// to surface a `LeaseId` — derive a deterministic UUID from the same
/// triple so two callers that observe the same lease see the same id.
fn synthetic_lease_uuid(exec_uuid: Uuid, attempt_index: i32, lease_epoch: i64) -> Uuid {
    // 16 bytes of seed material: exec UUID (16) XOR-folded with
    // attempt_index (4) + lease_epoch (8) packed into the tail. Not a
    // cryptographic derivation — just stable + collision-resistant
    // enough within a single execution's attempts.
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
    pool: &PgPool,
    args: RegisterWorkerArgs,
) -> Result<RegisterWorkerOutcome, EngineError> {
    let partition_key = worker_partition_key(args.worker_instance_id.as_str());
    let lanes_sorted: Vec<String> = args.lanes.iter().map(|l| l.0.clone()).collect();
    let caps_csv: String = args
        .capabilities
        .iter()
        .cloned()
        .collect::<Vec<String>>()
        .join(",");

    // Single tx: (1) reject instance_id reassignment under a different
    // `worker_id`; (2) upsert the row + return whether it was a fresh
    // insert or an overwrite (`xmax = 0` returns true iff the row was
    // inserted); (3) append a `registered` event.
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Conflict check — if a row exists with the same (partition_key,
    // namespace, worker_instance_id) but a different `worker_id`,
    // reject per RFC-025 §4.
    let existing_worker_id: Option<String> = sqlx::query_scalar(
        "SELECT worker_id FROM ff_worker_registry \
         WHERE partition_key = $1 AND namespace = $2 AND worker_instance_id = $3 \
         FOR UPDATE",
    )
    .bind(partition_key)
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

    // "Was this an INSERT vs a conflict-UPDATE?" signal.
    //
    // Postgres 16 + sqlx 0.8.x rejects any `RETURNING` projection
    // that references the `xmax` system column with SQLSTATE 0A000
    // "cannot retrieve a system column in this context"
    // (tts_virtual_getsysattr, execTuples.c:144 — cairn bug FF #508).
    // This is true even for `xmax::text` / `(xmax = 0)::bool` etc.
    // because sqlx's portal path uses a virtual tuple slot that PG
    // 16 rejects system-column access from.
    //
    // Workaround: issue a separate SELECT to learn if the target
    // row existed before the UPSERT. Wrap both statements in the
    // transaction we already hold so the read is consistent with
    // the write.
    let pre_existed: Option<i64> = sqlx::query_scalar(
        "SELECT 1::bigint FROM ff_worker_registry \
          WHERE partition_key = $1 \
            AND namespace = $2 \
            AND worker_instance_id = $3",
    )
    .bind(partition_key)
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        "INSERT INTO ff_worker_registry (\
             partition_key, namespace, worker_instance_id, worker_id, \
             lanes, capabilities_csv, last_heartbeat_ms, liveness_ttl_ms, \
             registered_at_ms\
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (partition_key, namespace, worker_instance_id) DO UPDATE SET \
             worker_id = EXCLUDED.worker_id, \
             lanes = EXCLUDED.lanes, \
             capabilities_csv = EXCLUDED.capabilities_csv, \
             last_heartbeat_ms = EXCLUDED.last_heartbeat_ms, \
             liveness_ttl_ms = EXCLUDED.liveness_ttl_ms",
    )
    .bind(partition_key)
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.worker_id.as_str())
    .bind(&lanes_sorted)
    .bind(caps_csv.as_str())
    .bind(args.now.0)
    .bind(args.liveness_ttl_ms as i64)
    .bind(args.now.0)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let registered = pre_existed.is_none();

    sqlx::query(
        "INSERT INTO ff_worker_registry_event \
             (partition_key, namespace, worker_instance_id, event_kind, event_at_ms, reason) \
         VALUES ($1, $2, $3, 'registered', $4, NULL) \
         ON CONFLICT DO NOTHING",
    )
    .bind(partition_key)
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .bind(args.now.0)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(if registered {
        RegisterWorkerOutcome::Registered
    } else {
        RegisterWorkerOutcome::Refreshed
    })
}

// ── heartbeat_worker ─────────────────────────────────────────────

pub async fn heartbeat_worker(
    pool: &PgPool,
    args: HeartbeatWorkerArgs,
) -> Result<HeartbeatWorkerOutcome, EngineError> {
    let partition_key = worker_partition_key(args.worker_instance_id.as_str());

    // Refresh `last_heartbeat_ms` only when the row is still within
    // its declared TTL window. If the TTL has lapsed, the TTL-sweep
    // scanner will drop the row on its next tick — surface
    // `NotRegistered` rather than re-upping a logically-dead worker.
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let ttl_row: Option<i64> = sqlx::query_scalar(
        "UPDATE ff_worker_registry SET last_heartbeat_ms = $4 \
         WHERE partition_key = $1 AND namespace = $2 AND worker_instance_id = $3 \
           AND last_heartbeat_ms + liveness_ttl_ms > $4 \
         RETURNING liveness_ttl_ms",
    )
    .bind(partition_key)
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
        "INSERT INTO ff_worker_registry_event \
             (partition_key, namespace, worker_instance_id, event_kind, event_at_ms, reason) \
         VALUES ($1, $2, $3, 'heartbeat', $4, NULL) \
         ON CONFLICT DO NOTHING",
    )
    .bind(partition_key)
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
    pool: &PgPool,
    args: MarkWorkerDeadArgs,
) -> Result<MarkWorkerDeadOutcome, EngineError> {
    // Mirrors the Valkey body's validation (§Rev-2 item 9): 256-byte
    // reason cap + no control characters. Reject oversize / invalid
    // before touching storage.
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

    let partition_key = worker_partition_key(args.worker_instance_id.as_str());

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let deleted: i64 = sqlx::query_scalar(
        "WITH d AS (\
             DELETE FROM ff_worker_registry \
             WHERE partition_key = $1 AND namespace = $2 AND worker_instance_id = $3 \
             RETURNING 1 AS x\
         ) SELECT COUNT(*) FROM d",
    )
    .bind(partition_key)
    .bind(args.namespace.as_str())
    .bind(args.worker_instance_id.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if deleted == 0 {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(MarkWorkerDeadOutcome::NotRegistered);
    }

    sqlx::query(
        "INSERT INTO ff_worker_registry_event \
             (partition_key, namespace, worker_instance_id, event_kind, event_at_ms, reason) \
         VALUES ($1, $2, $3, 'marked_dead', $4, $5) \
         ON CONFLICT DO NOTHING",
    )
    .bind(partition_key)
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

pub async fn list_expired_leases(
    pool: &PgPool,
    args: ListExpiredLeasesArgs,
) -> Result<ListExpiredLeasesResult, EngineError> {
    let limit = args
        .limit
        .unwrap_or(LIST_EXPIRED_LEASES_DEFAULT_LIMIT)
        .min(LIST_EXPIRED_LEASES_MAX_LIMIT) as i64;
    // `max_partitions_per_call` is a Valkey ZSET fan-out knob; on PG
    // the partial index `ff_attempt_lease_expiry_idx` already covers
    // all partitions in one scan, so the value is accepted-and-ignored.
    let _ = args.max_partitions_per_call;

    // Cursor tuple: `(expires_at_ms, execution_id)` strict-greater
    // than `(cursor.expires_at_ms, cursor.execution_id)` so pagination
    // is stable under equal-expiry.
    let (cursor_expiry, cursor_eid_str): (Option<i64>, Option<String>) = match args.after.as_ref() {
        Some(c) => (Some(c.expires_at_ms.0), Some(c.execution_id.to_string())),
        None => (None, None),
    };
    let namespace_filter: Option<&str> = args.namespace.as_ref().map(|n| n.as_str());

    // Join `ff_attempt` (lease state) with `ff_exec_core` (namespace
    // filter + partition-prefix reconstruction of the ExecutionId
    // wire form). `ff_attempt_lease_expiry_idx` (partial, from
    // migration 0001) keys the scan on `(partition_key,
    // lease_expires_at_ms)`. Cross-partition order is enforced at
    // the `ORDER BY` level.
    // `ff_exec_core` has no `namespace` column — namespace lives in
    // `raw_fields jsonb` (see migrations/0001_initial.sql:98-122).
    // Phase 3 referenced `c.namespace` directly which would crash at
    // runtime; `raw_fields->>'namespace'` is the authoritative read
    // path.
    let rows = sqlx::query(
        "SELECT a.partition_key, a.execution_id, a.attempt_index, a.lease_epoch, \
                a.worker_instance_id, a.lease_expires_at_ms \
           FROM ff_attempt a \
           JOIN ff_exec_core c \
             ON c.partition_key = a.partition_key AND c.execution_id = a.execution_id \
          WHERE a.lease_expires_at_ms IS NOT NULL \
            AND a.lease_expires_at_ms <= $1 \
            AND a.worker_instance_id IS NOT NULL \
            AND c.public_state IN ('claimed', 'running') \
            AND ($2::text IS NULL OR c.raw_fields->>'namespace' = $2) \
            AND ($3::bigint IS NULL \
                 OR (a.lease_expires_at_ms, a.execution_id::text) > ($3, $4)) \
          ORDER BY a.lease_expires_at_ms ASC, a.execution_id ASC \
          LIMIT $5",
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
        let partition_key: i16 = row.try_get("partition_key").map_err(map_sqlx_error)?;
        let exec_uuid: Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
        let attempt_index: i32 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
        let lease_epoch: i64 = row.try_get("lease_epoch").map_err(map_sqlx_error)?;
        let worker_inst: String = row.try_get("worker_instance_id").map_err(map_sqlx_error)?;
        let expires_at_ms: i64 = row.try_get("lease_expires_at_ms").map_err(map_sqlx_error)?;

        let eid_str = format!("{{fp:{}}}:{}", partition_key, exec_uuid);
        let execution_id = ExecutionId::parse(&eid_str).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("list_expired_leases: bad execution_id {eid_str:?}: {e}"),
        })?;

        let lease_id = LeaseId::from_uuid(synthetic_lease_uuid(exec_uuid, attempt_index, lease_epoch));
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
    pool: &PgPool,
    args: ListWorkersArgs,
) -> Result<ListWorkersResult, EngineError> {
    // RFC-025 §9.4: cross-namespace enumeration requires a two-key
    // cursor (namespace, worker_instance_id) because instance_ids
    // collide across namespaces. The Phase-1 contract's cursor is
    // single-key `Option<WorkerInstanceId>`; expanding to a tuple
    // would reopen Phase 1. Reject cross-ns with `Unavailable` to
    // match Phase-2 Valkey behaviour; operator tooling can loop per
    // namespace until the tuple-cursor RFC lands.
    let Some(ns) = args.namespace.as_ref() else {
        return Err(EngineError::Unavailable {
            op: "list_workers (cross-namespace on Postgres — pass namespace explicitly)",
        });
    };
    let limit = args.limit.unwrap_or(1000) as i64;
    let namespace_filter: &str = ns.as_str();
    let after_cursor: Option<&str> = args.after.as_ref().map(|w| w.as_str());

    let rows = sqlx::query(
        "SELECT worker_id, worker_instance_id, namespace, lanes, \
                capabilities_csv, last_heartbeat_ms, liveness_ttl_ms, registered_at_ms \
           FROM ff_worker_registry \
          WHERE namespace = $1 \
            AND ($2::text IS NULL OR worker_instance_id > $2) \
          ORDER BY worker_instance_id ASC \
          LIMIT $3",
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
        let lanes_vec: Vec<String> = row.try_get("lanes").map_err(map_sqlx_error)?;
        let caps_csv: String = row.try_get("capabilities_csv").map_err(map_sqlx_error)?;
        let last_hb_ms: i64 = row.try_get("last_heartbeat_ms").map_err(map_sqlx_error)?;
        let liveness_ttl_ms: i64 = row.try_get("liveness_ttl_ms").map_err(map_sqlx_error)?;
        let registered_at_ms: i64 = row.try_get("registered_at_ms").map_err(map_sqlx_error)?;

        let lanes: BTreeSet<LaneId> = lanes_vec.into_iter().map(LaneId).collect();
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

/// Per-partition TTL sweep. Mirrors Valkey's native PEXPIRE behaviour
/// for PG/SQLite: rows whose `last_heartbeat_ms + liveness_ttl_ms`
/// falls strictly below `now_ms` are deleted and a `ttl_swept` event
/// is appended to the audit log.
///
/// Returns a `ScanReport` for the supervisor's per-tick log.
pub async fn ttl_sweep_tick(pool: &PgPool, partition_key: i16) -> Result<ScanReport, EngineError> {
    let now_ms: i64 = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX);

    // DELETE + append-to-event-log in a single statement via CTE so
    // the sweep is atomic per row: never a row deleted without an
    // event, never an event without a delete. Concurrent
    // `mark_worker_dead` on the same row deletes first + writes its
    // own event; the sweep's DELETE finds zero rows, the INSERT-SELECT
    // inserts zero rows. Idempotent.
    let report_rows = sqlx::query(
        "WITH swept AS (\
             DELETE FROM ff_worker_registry \
             WHERE partition_key = $1 \
               AND last_heartbeat_ms + liveness_ttl_ms < $2 \
             RETURNING partition_key, namespace, worker_instance_id\
         ), ev AS (\
             INSERT INTO ff_worker_registry_event \
                 (partition_key, namespace, worker_instance_id, event_kind, event_at_ms, reason) \
             SELECT partition_key, namespace, worker_instance_id, 'ttl_swept', $2, NULL \
               FROM swept \
             ON CONFLICT DO NOTHING \
             RETURNING 1\
         ) SELECT COUNT(*) AS swept FROM swept",
    )
    .bind(partition_key)
    .bind(now_ms)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx_error)?;

    let processed: i64 = report_rows.try_get("swept").map_err(map_sqlx_error)?;
    Ok(ScanReport {
        processed: u32::try_from(processed.max(0)).unwrap_or(u32::MAX),
        errors: 0,
    })
}
