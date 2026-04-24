//! Stream-family trait methods on the Postgres backend (RFC-015).
//!
//! This module houses the four stream methods called from
//! `EngineBackend` on [`crate::PostgresBackend`]:
//!
//! 1. [`append_frame`] — Durable, DurableSummary (with JSON Merge
//!    Patch apply), and BestEffortLive (EMA-driven dynamic MAXLEN).
//! 2. [`read_stream`] — XRANGE-equivalent over `(ts_ms, seq)`.
//! 3. [`tail_stream`] — blocking tail via the shared
//!    [`crate::listener::StreamNotifier`]; NO pg connection is held
//!    while parked (Q2).
//! 4. [`read_summary`] — reads the rolling DurableSummary document.

use std::collections::BTreeMap;
use std::time::Duration;

use ff_core::backend::{
    AppendFrameOutcome, Frame, FrameKind, Handle, PatchKind, StreamMode, SummaryDocument,
    TailVisibility,
};
use ff_core::contracts::{StreamCursor, StreamFrame, StreamFrames};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::partition::PartitionConfig;
use ff_core::types::{AttemptIndex, ExecutionId};
use serde_json::Value as Json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;
use crate::handle_codec::decode_handle;
use crate::listener::{channel_name, StreamNotifier};

/// Extract the UUID tail of an `ExecutionId` ({fp:N}:<uuid>).
fn exec_uuid(eid: &ExecutionId) -> Result<Uuid, EngineError> {
    let s = eid.as_str();
    // After the "}:" the rest is the uuid.
    let after = s.split_once("}:").map(|(_, r)| r).unwrap_or(s);
    Uuid::parse_str(after).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("invalid execution_id uuid tail: {e}"),
    })
}

fn partition_key_of(eid: &ExecutionId) -> i16 {
    // The partition index is mod'd by 256 (migration schema uses
    // HASH 256). Our PartitionConfig default is 256, so partition()
    // already fits in smallint; downcast is safe.
    (eid.partition() % 256) as i16
}

fn frame_type_of(frame: &Frame) -> String {
    if frame.frame_type.is_empty() {
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
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build the `fields` jsonb for a frame — mirrors the valkey
/// stream entry shape so downstream parsers stay symmetric.
fn build_fields_json(frame: &Frame) -> Json {
    let payload_str = String::from_utf8_lossy(&frame.bytes).into_owned();
    let mut map = serde_json::Map::new();
    map.insert("frame_type".into(), Json::String(frame_type_of(frame)));
    map.insert("payload".into(), Json::String(payload_str));
    map.insert("encoding".into(), Json::String("utf8".into()));
    map.insert("source".into(), Json::String("worker".into()));
    if let Some(corr) = &frame.correlation_id {
        map.insert("correlation_id".into(), Json::String(corr.clone()));
    }
    Json::Object(map)
}

// ── JSON Merge Patch (RFC 7396) with ff sentinel rewrite ──

/// Apply an RFC 7396 JSON Merge Patch. The `__ff_null__` sentinel is
/// rewritten to JSON `null` at leaf positions so callers can express
/// "set leaf to null" vs. "delete key" (7396 uses bare null for
/// delete). Sentinel handling matches the Lua implementation.
pub(crate) fn apply_json_merge_patch(target: &mut Json, patch: &Json) {
    if let Json::Object(patch_map) = patch {
        if !target.is_object() {
            *target = Json::Object(serde_json::Map::new());
        }
        let target_map = target.as_object_mut().expect("just ensured object");
        for (k, v) in patch_map {
            match v {
                Json::Null => {
                    target_map.remove(k);
                }
                Json::String(s) if s == ff_core::backend::SUMMARY_NULL_SENTINEL => {
                    target_map.insert(k.clone(), Json::Null);
                }
                Json::Object(_) => {
                    let entry = target_map.entry(k.clone()).or_insert(Json::Null);
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

// ── append_frame ──

pub async fn append_frame(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    handle: &Handle,
    frame: Frame,
) -> Result<AppendFrameOutcome, EngineError> {
    let payload = decode_handle(handle)?;
    let eid_uuid = exec_uuid(&payload.execution_id)?;
    let pkey = partition_key_of(&payload.execution_id);
    let aidx = payload.attempt_index.0 as i32;

    let mode_wire = frame.mode.wire_str();
    let fields = build_fields_json(&frame);

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Authoritative (ts_ms, seq) minting: row-lock the meta row per
    // (eid, aidx), look up current max seq for this ts_ms, INSERT.
    // One INSERT retry on unique-constraint violation (clock moved
    // backwards or concurrent append within same ms).
    let ts_ms: i64 = now_ms();

    // Row-level serialisation via advisory xact lock keyed on
    // (pkey, hash(eid, aidx)). Cheap; scoped to txn.
    let lock_key: i64 = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        (eid_uuid.as_bytes(), aidx).hash(&mut h);
        h.finish() as i64
    };
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock_key)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // Seq = max(seq WHERE ts_ms = $ts_ms) + 1, default 0.
    let next_seq: i32 = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT MAX(seq) FROM ff_stream_frame \
         WHERE partition_key=$1 AND execution_id=$2 \
         AND attempt_index=$3 AND ts_ms=$4",
    )
    .bind(pkey)
    .bind(eid_uuid)
    .bind(aidx)
    .bind(ts_ms)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?
    .map(|s| s + 1)
    .unwrap_or(0);

    sqlx::query(
        "INSERT INTO ff_stream_frame \
         (partition_key, execution_id, attempt_index, ts_ms, seq, fields, mode, created_at_ms) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(pkey)
    .bind(eid_uuid)
    .bind(aidx)
    .bind(ts_ms)
    .bind(next_seq)
    .bind(&fields)
    .bind(mode_wire)
    .bind(ts_ms)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let mut summary_version: Option<u64> = None;

    // DurableSummary: merge the frame's payload (as JSON) into
    // ff_stream_summary.document_json; bump version.
    if let StreamMode::DurableSummary { patch_kind } = &frame.mode {
        // Parse the frame payload as JSON — callers supply JSON
        // Merge Patch bytes.
        let patch: Json = serde_json::from_slice(&frame.bytes).map_err(|e| {
            EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("summary patch not valid JSON: {e}"),
            }
        })?;

        // UPSERT: fetch existing, merge in-Rust, write back.
        let existing: Option<(Json, i32)> = sqlx::query_as(
            "SELECT document_json, version FROM ff_stream_summary \
             WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3 \
             FOR UPDATE",
        )
        .bind(pkey)
        .bind(eid_uuid)
        .bind(aidx)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        let (mut doc, prev_version): (Json, i32) = match existing {
            Some((d, v)) => (d, v),
            None => (Json::Object(serde_json::Map::new()), 0),
        };

        match patch_kind {
            PatchKind::JsonMergePatch => apply_json_merge_patch(&mut doc, &patch),
            _ => apply_json_merge_patch(&mut doc, &patch),
        }

        let new_version = prev_version + 1;
        let patch_kind_wire = "json-merge-patch";
        if prev_version == 0 {
            sqlx::query(
                "INSERT INTO ff_stream_summary \
                 (partition_key, execution_id, attempt_index, document_json, \
                  version, patch_kind, last_updated_ms, first_applied_ms) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            )
            .bind(pkey)
            .bind(eid_uuid)
            .bind(aidx)
            .bind(&doc)
            .bind(new_version)
            .bind(patch_kind_wire)
            .bind(ts_ms)
            .bind(ts_ms)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        } else {
            sqlx::query(
                "UPDATE ff_stream_summary SET document_json=$4, version=$5, \
                  patch_kind=$6, last_updated_ms=$7 \
                 WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3",
            )
            .bind(pkey)
            .bind(eid_uuid)
            .bind(aidx)
            .bind(&doc)
            .bind(new_version)
            .bind(patch_kind_wire)
            .bind(ts_ms)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        }
        summary_version = Some(new_version as u64);
    }

    // BestEffortLive: update EMA + trim beyond dynamic MAXLEN.
    if let StreamMode::BestEffortLive { config } = &frame.mode {
        let meta: Option<(f64, i64)> = sqlx::query_as(
            "SELECT ema_rate_hz, last_append_ts_ms FROM ff_stream_meta \
             WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3 \
             FOR UPDATE",
        )
        .bind(pkey)
        .bind(eid_uuid)
        .bind(aidx)
        .fetch_optional(&mut *tx)
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
        let k_raw = (ema_new * (config.ttl_ms as f64) / 1000.0).ceil() as i64 * 2;
        let k = k_raw
            .max(config.maxlen_floor as i64)
            .min(config.maxlen_ceiling as i64);

        // Upsert meta.
        sqlx::query(
            "INSERT INTO ff_stream_meta \
             (partition_key, execution_id, attempt_index, ema_rate_hz, \
              last_append_ts_ms, maxlen_applied_last) \
             VALUES ($1,$2,$3,$4,$5,$6) \
             ON CONFLICT (partition_key, execution_id, attempt_index) DO UPDATE \
             SET ema_rate_hz = EXCLUDED.ema_rate_hz, \
                 last_append_ts_ms = EXCLUDED.last_append_ts_ms, \
                 maxlen_applied_last = EXCLUDED.maxlen_applied_last",
        )
        .bind(pkey)
        .bind(eid_uuid)
        .bind(aidx)
        .bind(ema_new)
        .bind(ts_ms)
        .bind(k as i32)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Trim: delete frames beyond the most recent K.
        // Piggy-back on append (per K-round-2: prefer this over
        // per-row triggers).
        sqlx::query(
            "DELETE FROM ff_stream_frame \
             WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3 \
             AND (ts_ms, seq) NOT IN ( \
                 SELECT ts_ms, seq FROM ff_stream_frame \
                 WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3 \
                 ORDER BY ts_ms DESC, seq DESC \
                 LIMIT $4)",
        )
        .bind(pkey)
        .bind(eid_uuid)
        .bind(aidx)
        .bind(k)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    // Frame count after the append.
    let frame_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_stream_frame \
         WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3",
    )
    .bind(pkey)
    .bind(eid_uuid)
    .bind(aidx)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    let stream_id = format!("{ts_ms}-{next_seq}");
    let mut out = AppendFrameOutcome::new(stream_id, frame_count as u64);
    if let Some(v) = summary_version {
        out = out.with_summary_version(v);
    }
    Ok(out)
}

// ── Cursor parsing ──

fn parse_cursor_lower(c: &StreamCursor) -> Result<(i64, i32), EngineError> {
    match c {
        StreamCursor::Start => Ok((i64::MIN, i32::MIN)),
        StreamCursor::End => Ok((i64::MAX, i32::MAX)),
        StreamCursor::At(s) => parse_at(s),
    }
}

fn parse_cursor_upper(c: &StreamCursor) -> Result<(i64, i32), EngineError> {
    match c {
        StreamCursor::Start => Ok((i64::MIN, i32::MIN)),
        StreamCursor::End => Ok((i64::MAX, i32::MAX)),
        StreamCursor::At(s) => parse_at(s),
    }
}

fn parse_at(s: &str) -> Result<(i64, i32), EngineError> {
    let (ms, seq) = match s.split_once('-') {
        Some((a, b)) => (a, b),
        None => (s, "0"),
    };
    let ms: i64 = ms.parse().map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("bad stream cursor '{s}' (ms)"),
    })?;
    let sq: i32 = seq.parse().map_err(|_| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("bad stream cursor '{s}' (seq)"),
    })?;
    Ok((ms, sq))
}

// ── read_stream ──

pub async fn read_stream(
    pool: &PgPool,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    from: StreamCursor,
    to: StreamCursor,
    count_limit: u64,
) -> Result<StreamFrames, EngineError> {
    let eid_uuid = exec_uuid(execution_id)?;
    let pkey = partition_key_of(execution_id);
    let aidx = attempt_index.0 as i32;
    let (from_ms, from_seq) = parse_cursor_lower(&from)?;
    let (to_ms, to_seq) = parse_cursor_upper(&to)?;
    let lim = count_limit.min(ff_core::contracts::STREAM_READ_HARD_CAP) as i64;

    let rows = sqlx::query(
        "SELECT ts_ms, seq, fields FROM ff_stream_frame \
         WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3 \
         AND (ts_ms, seq) >= ($4, $5) AND (ts_ms, seq) <= ($6, $7) \
         ORDER BY ts_ms, seq LIMIT $8",
    )
    .bind(pkey)
    .bind(eid_uuid)
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
        let ts: i64 = row.get("ts_ms");
        let seq: i32 = row.get("seq");
        let fields: Json = row.get("fields");
        frames.push(row_to_frame(ts, seq, fields));
    }
    Ok(StreamFrames {
        frames,
        closed_at: None,
        closed_reason: None,
    })
}

fn row_to_frame(ts_ms: i64, seq: i32, fields: Json) -> StreamFrame {
    let mut out = BTreeMap::new();
    if let Json::Object(map) = fields {
        for (k, v) in map {
            let s = match v {
                Json::String(s) => s,
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

// ── tail_stream ──

#[allow(clippy::too_many_arguments)] // mirrors the EngineBackend trait method signature
pub async fn tail_stream(
    pool: &PgPool,
    notifier: &StreamNotifier,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    after: StreamCursor,
    block_ms: u64,
    count_limit: u64,
    visibility: TailVisibility,
) -> Result<StreamFrames, EngineError> {
    let eid_uuid = exec_uuid(execution_id)?;
    let pkey = partition_key_of(execution_id);
    let aidx = attempt_index.0 as i32;
    let (after_ms, after_seq) = match &after {
        StreamCursor::At(s) => parse_at(s)?,
        _ => {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "tail_stream requires concrete after cursor".into(),
            });
        }
    };
    let lim = count_limit.min(ff_core::contracts::STREAM_READ_HARD_CAP) as i64;
    let visibility_filter = match visibility {
        TailVisibility::ExcludeBestEffort => "AND mode <> 'best_effort'",
        _ => "",
    };

    // Subscribe BEFORE the first SELECT so we never miss a NOTIFY
    // that fires between the SELECT and our park.
    let chan = channel_name(&eid_uuid, aidx as u32);
    let mut rx = notifier.subscribe(&chan).await;

    let do_select = |pool: PgPool| async move {
        let sql = format!(
            "SELECT ts_ms, seq, fields FROM ff_stream_frame \
             WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3 \
             AND (ts_ms, seq) > ($4, $5) {visibility_filter} \
             ORDER BY ts_ms, seq LIMIT $6"
        );
        sqlx::query(&sql)
            .bind(pkey)
            .bind(eid_uuid)
            .bind(aidx)
            .bind(after_ms)
            .bind(after_seq)
            .bind(lim)
            .fetch_all(&pool)
            .await
            .map_err(map_sqlx_error)
    };

    // First opportunistic SELECT — returns fast-path if frames
    // already exist.
    let rows = do_select(pool.clone()).await?;
    if !rows.is_empty() || block_ms == 0 {
        return Ok(rows_to_frames(rows));
    }

    // Park on the broadcast receiver — NO pg conn held here.
    // Loop until timeout OR the re-SELECT returns a non-empty set:
    // wake-ups may come from `Reconnect` signals that carry no new
    // frames, so we must re-check the cursor and keep waiting.
    let start = std::time::Instant::now();
    let total = Duration::from_millis(block_ms);
    loop {
        let remaining = match total.checked_sub(start.elapsed()) {
            Some(r) if !r.is_zero() => r,
            _ => break,
        };
        let _ = tokio::time::timeout(remaining, rx.recv()).await;
        let rows = do_select(pool.clone()).await?;
        if !rows.is_empty() {
            return Ok(rows_to_frames(rows));
        }
        // If timed out (elapsed >= total), break and return empty.
        if start.elapsed() >= total {
            break;
        }
    }

    Ok(StreamFrames::empty_open())
}

fn rows_to_frames(rows: Vec<sqlx::postgres::PgRow>) -> StreamFrames {
    let mut frames = Vec::with_capacity(rows.len());
    for row in rows {
        let ts: i64 = row.get("ts_ms");
        let seq: i32 = row.get("seq");
        let fields: Json = row.get("fields");
        frames.push(row_to_frame(ts, seq, fields));
    }
    StreamFrames {
        frames,
        closed_at: None,
        closed_reason: None,
    }
}

// ── read_summary ──

pub async fn read_summary(
    pool: &PgPool,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
) -> Result<Option<SummaryDocument>, EngineError> {
    let eid_uuid = exec_uuid(execution_id)?;
    let pkey = partition_key_of(execution_id);
    let aidx = attempt_index.0 as i32;

    let row = sqlx::query(
        "SELECT document_json, version, patch_kind, last_updated_ms, first_applied_ms \
         FROM ff_stream_summary \
         WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=$3",
    )
    .bind(pkey)
    .bind(eid_uuid)
    .bind(aidx)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else { return Ok(None) };
    let doc: Json = row.get("document_json");
    let version: i32 = row.get("version");
    let patch_kind_wire: Option<String> = row.get("patch_kind");
    let last_updated: i64 = row.get("last_updated_ms");
    let first_applied: i64 = row.get("first_applied_ms");

    let bytes = serde_json::to_vec(&doc).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("summary document not serialisable: {e}"),
    })?;
    let patch_kind = match patch_kind_wire.as_deref() {
        Some("json-merge-patch") => PatchKind::JsonMergePatch,
        _ => PatchKind::JsonMergePatch,
    };
    Ok(Some(SummaryDocument::new(
        bytes,
        version as u64,
        patch_kind,
        last_updated as u64,
        first_applied as u64,
    )))
}

