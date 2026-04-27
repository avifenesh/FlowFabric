//! SQLite dialect-forked queries for the RFC-015 stream surface.
//!
//! Populated in Phase 2a.3 per RFC-023 §4.1 (write path) + Phase
//! 2b.2.2 (read path). Mirrors `ff-backend-postgres/src/stream.rs`
//! statement-for-statement: `append_frame`, `read_stream`,
//! `tail_stream` (same SELECT shape; in-Rust broadcast replaces PG's
//! `LISTEN/NOTIFY` at the backend layer — see
//! `crate::pubsub::PubSub::stream_frame`), and `read_summary`.
//!
//! # Dialect notes
//!
//! * `ff_stream_summary.document_json` is TEXT (not `jsonb`) in the
//!   SQLite port; JSON Merge Patch is applied in Rust via
//!   `crate::backend::apply_json_merge_patch` and written back whole.
//!   Same observable behaviour as the PG `jsonb` path.
//! * BestEffortLive trim uses a subquery-IN delete with the same
//!   shape as PG — SQLite supports correlated subqueries in `DELETE`.
//! * `pg_advisory_xact_lock` is replaced by the enclosing
//!   `BEGIN IMMEDIATE` lock (§4.1 A3 single-writer).

/// Read `MAX(seq)` for the current `(pkey, eid, aidx, ts_ms)` tuple so
/// the caller can mint the next sequence under the txn lock. Mirror of
/// PG at `ff-backend-postgres/src/stream.rs:163-176`.
pub(crate) const SELECT_MAX_SEQ_SQL: &str = r#"
    SELECT MAX(seq) AS s FROM ff_stream_frame
     WHERE partition_key = ?1 AND execution_id = ?2
       AND attempt_index = ?3 AND ts_ms = ?4
"#;

/// Insert one frame row into `ff_stream_frame`. Mirror of PG at
/// `ff-backend-postgres/src/stream.rs:178-193`.
pub(crate) const INSERT_STREAM_FRAME_SQL: &str = r#"
    INSERT INTO ff_stream_frame (
        partition_key, execution_id, attempt_index,
        ts_ms, seq, fields, mode, created_at_ms
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
"#;

/// Fetch the current `ff_stream_summary` document + version for a
/// given `(pkey, eid, aidx)`. Caller merges the patch in Rust.
/// Mirror of PG's `FOR UPDATE` read at
/// `ff-backend-postgres/src/stream.rs:210-220` — `BEGIN IMMEDIATE`
/// already serializes writers on SQLite.
pub(crate) const SELECT_STREAM_SUMMARY_SQL: &str = r#"
    SELECT document_json, version
      FROM ff_stream_summary
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

/// Insert a fresh `ff_stream_summary` row (first-time document). Mirror
/// of PG at `ff-backend-postgres/src/stream.rs:234-251`.
pub(crate) const INSERT_STREAM_SUMMARY_SQL: &str = r#"
    INSERT INTO ff_stream_summary (
        partition_key, execution_id, attempt_index,
        document_json, version, patch_kind,
        last_updated_ms, first_applied_ms
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
"#;

/// Update an existing `ff_stream_summary` row with the merged document
/// + bumped version. Mirror of PG at
///   `ff-backend-postgres/src/stream.rs:253-267`.
pub(crate) const UPDATE_STREAM_SUMMARY_SQL: &str = r#"
    UPDATE ff_stream_summary
       SET document_json = ?4,
           version = ?5,
           patch_kind = ?6,
           last_updated_ms = ?7
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

/// Fetch the BestEffort EMA state (`ema_rate_hz`, `last_append_ts_ms`).
/// Mirror of PG at `ff-backend-postgres/src/stream.rs:274-284`.
pub(crate) const SELECT_STREAM_META_SQL: &str = r#"
    SELECT ema_rate_hz, last_append_ts_ms
      FROM ff_stream_meta
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

/// UPSERT the BestEffort EMA + last-append meta. Mirror of PG at
/// `ff-backend-postgres/src/stream.rs:299-318`.
pub(crate) const UPSERT_STREAM_META_SQL: &str = r#"
    INSERT INTO ff_stream_meta (
        partition_key, execution_id, attempt_index,
        ema_rate_hz, last_append_ts_ms, maxlen_applied_last
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
    ON CONFLICT (partition_key, execution_id, attempt_index) DO UPDATE SET
        ema_rate_hz = excluded.ema_rate_hz,
        last_append_ts_ms = excluded.last_append_ts_ms,
        maxlen_applied_last = excluded.maxlen_applied_last
"#;

/// Trim `ff_stream_frame` to the most-recent `?4` rows for the tuple.
/// Mirror of PG at `ff-backend-postgres/src/stream.rs:322-331`.
pub(crate) const TRIM_STREAM_FRAMES_SQL: &str = r#"
    DELETE FROM ff_stream_frame
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
       AND (ts_ms, seq) NOT IN (
           SELECT ts_ms, seq FROM ff_stream_frame
            WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
            ORDER BY ts_ms DESC, seq DESC
            LIMIT ?4
       )
"#;

/// Count frames for a tuple post-append — returned via
/// [`ff_core::backend::AppendFrameOutcome`].
pub(crate) const COUNT_STREAM_FRAMES_SQL: &str = r#"
    SELECT COUNT(*) AS c FROM ff_stream_frame
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

// ── Phase 2b.2.2 read surface ─────────────────────────────────────────

/// `read_stream` — XRANGE-equivalent over the `(ts_ms, seq)` tuple
/// ordering. Binds: `?1=partition_key`, `?2=execution_id` (BLOB),
/// `?3=attempt_index`, `?4=from_ts`, `?5=from_seq`, `?6=to_ts`,
/// `?7=to_seq`, `?8=limit`. Mirror of PG at
/// `ff-backend-postgres/src/stream.rs:414-418` — SQLite supports the
/// same row-value comparison shape `(a, b) >= (x, y)` so the SQL
/// ports verbatim.
pub(crate) const READ_STREAM_RANGE_SQL: &str = r#"
    SELECT ts_ms, seq, fields FROM ff_stream_frame
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
       AND (ts_ms, seq) >= (?4, ?5) AND (ts_ms, seq) <= (?6, ?7)
     ORDER BY ts_ms, seq
     LIMIT ?8
"#;

/// `tail_stream` — rows strictly after `(after_ts, after_seq)`. Binds:
/// `?1=partition_key`, `?2=execution_id` (BLOB), `?3=attempt_index`,
/// `?4=after_ts`, `?5=after_seq`, `?6=limit`. Mirror of PG at
/// `ff-backend-postgres/src/stream.rs:500-504`.
pub(crate) const TAIL_STREAM_AFTER_SQL: &str = r#"
    SELECT ts_ms, seq, fields FROM ff_stream_frame
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
       AND (ts_ms, seq) > (?4, ?5)
     ORDER BY ts_ms, seq
     LIMIT ?6
"#;

/// `tail_stream` with `TailVisibility::ExcludeBestEffort` — additive
/// `mode <> 'best_effort'` filter. Binds identical to
/// [`TAIL_STREAM_AFTER_SQL`].
pub(crate) const TAIL_STREAM_AFTER_EXCLUDE_BE_SQL: &str = r#"
    SELECT ts_ms, seq, fields FROM ff_stream_frame
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
       AND (ts_ms, seq) > (?4, ?5)
       AND mode <> 'best_effort'
     ORDER BY ts_ms, seq
     LIMIT ?6
"#;

/// `read_summary` — fetch the full summary row for caller consumption.
/// Binds: `?1=partition_key`, `?2=execution_id`, `?3=attempt_index`.
/// Mirror of PG at `ff-backend-postgres/src/stream.rs:576-580`.
pub(crate) const READ_SUMMARY_FULL_SQL: &str = r#"
    SELECT document_json, version, patch_kind, last_updated_ms, first_applied_ms
      FROM ff_stream_summary
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

// ── Phase 2b.2.2 outbox-cursor-reader SQL ─────────────────────────────

/// Cursor-resume tail of `ff_stream_frame` for the OutboxCursorReader
/// primitive. Unlike the row-value `(ts_ms, seq)` cursor used by
/// `tail_stream`, the outbox-cursor reader uses the table's ROWID as
/// the monotonic event id — matches the `last_insert_rowid()` shape
/// the producer emits via the `stream_frame` broadcast channel.
///
/// Binds: `?1=partition_key`, `?2=cursor_rowid`, `?3=batch_size`.
///
/// Only referenced from `outbox_cursor::tests` today — Phase 3's
/// `subscribe_stream_frame` trait impl will be the first non-test
/// consumer.
#[cfg(test)]
pub(crate) const OUTBOX_TAIL_STREAM_FRAME_SQL: &str = r#"
    SELECT _rowid_ AS event_id,
           partition_key, execution_id, attempt_index, ts_ms, seq, fields, mode
      FROM ff_stream_frame
     WHERE partition_key = ?1 AND _rowid_ > ?2
     ORDER BY _rowid_ ASC
     LIMIT ?3
"#;
