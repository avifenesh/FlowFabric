//! SQL statements for waitpoint + HMAC secret ops
//! (RFC-023 Phase 2b.2.1).
//!
//! Mirrors `ff-backend-postgres/src/signal.rs` +
//! `ff-backend-postgres/src/suspend_ops.rs` at the statement level;
//! dialect deltas are the usual `jsonb → TEXT`, `$n → ?n`, no
//! `ON CONFLICT (cols) DO UPDATE SET col = EXCLUDED.col` array
//! binding (we bind lists via JSON strings where PG used `text[]`).

// ── HMAC keystore (ff_waitpoint_hmac) ─────────────────────────────────

pub const SELECT_ACTIVE_HMAC_SQL: &str = "SELECT kid, secret FROM ff_waitpoint_hmac \
     WHERE active = 1 \
     ORDER BY rotated_at_ms DESC LIMIT 1";

pub const SELECT_HMAC_SECRET_BY_KID_SQL: &str =
    "SELECT secret FROM ff_waitpoint_hmac WHERE kid = ?1";

pub const DEACTIVATE_ALL_HMAC_SQL: &str =
    "UPDATE ff_waitpoint_hmac SET active = 0 WHERE active = 1";

pub const INSERT_HMAC_ROW_SQL: &str = "INSERT INTO ff_waitpoint_hmac \
     (kid, secret, rotated_at_ms, active) \
     VALUES (?1, ?2, ?3, 1)";

pub const SELECT_ACTIVE_KID_SQL: &str = "SELECT kid FROM ff_waitpoint_hmac \
     WHERE active = 1 \
     ORDER BY rotated_at_ms DESC LIMIT 1";

// ── Pending waitpoint row (ff_waitpoint_pending) ──────────────────────

/// Insert-or-overwrite a waitpoint row. `required_signal_names` goes in
/// as a JSON-encoded TEXT column (SQLite has no native text[]).
/// Binds: 1=partition_key, 2=waitpoint_id, 3=execution_id,
///        4=token_kid, 5=token, 6=created_at_ms, 7=expires_at_ms,
///        8=waitpoint_key, 9=required_signal_names_json.
pub const UPSERT_WAITPOINT_PENDING_ACTIVE_SQL: &str = "INSERT INTO ff_waitpoint_pending \
     (partition_key, waitpoint_id, execution_id, token_kid, token, \
      created_at_ms, expires_at_ms, waitpoint_key, \
      state, required_signal_names, activated_at_ms) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?6) \
     ON CONFLICT (partition_key, waitpoint_id) DO UPDATE SET \
       token_kid = excluded.token_kid, token = excluded.token, \
       waitpoint_key = excluded.waitpoint_key, \
       state = excluded.state, \
       required_signal_names = excluded.required_signal_names, \
       activated_at_ms = excluded.activated_at_ms";

/// Fresh pending-waitpoint row for `create_waitpoint` — state stays
/// `pending`, no `activated_at_ms`.
/// Binds: 1=partition, 2=waitpoint_id, 3=execution_id, 4=kid,
///        5=token, 6=created_at_ms, 7=expires_at_ms, 8=waitpoint_key.
pub const INSERT_WAITPOINT_PENDING_SQL: &str = "INSERT INTO ff_waitpoint_pending \
     (partition_key, waitpoint_id, execution_id, token_kid, token, \
      created_at_ms, expires_at_ms, waitpoint_key, state, required_signal_names) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', '[]')";

pub const SELECT_WAITPOINT_KEY_BY_ID_SQL: &str =
    "SELECT waitpoint_key FROM ff_waitpoint_pending \
     WHERE partition_key = ?1 AND waitpoint_id = ?2";

/// For deliver_signal: read kid + token + wp_key + bound execution.
/// Returns (token_kid, token, waitpoint_key, execution_id).
pub const SELECT_WAITPOINT_FOR_DELIVER_SQL: &str =
    "SELECT token_kid, token, waitpoint_key, execution_id \
       FROM ff_waitpoint_pending \
      WHERE partition_key = ?1 AND waitpoint_id = ?2";

/// Delete every waitpoint row for a resolved execution.
pub const DELETE_WAITPOINTS_BY_EXEC_SQL: &str =
    "DELETE FROM ff_waitpoint_pending \
      WHERE partition_key = ?1 AND execution_id = ?2";

// ── list_pending_waitpoints (RFC-020 §4.5, Phase 3.3) ──────────────

/// Existence probe — matches PG reference's `EXISTS exec_core`
/// pre-check so a non-existent execution surfaces `NotFound` rather
/// than an empty page. Binds: ?1 partition_key, ?2 execution_id BLOB.
pub const SELECT_EXEC_EXISTS_SQL: &str =
    "SELECT 1 FROM ff_exec_core WHERE partition_key = ?1 AND execution_id = ?2";

/// Cursor-paginated scan of `ff_waitpoint_pending` for one execution.
/// Filters to `state IN ('pending','active')` (matches Valkey's
/// client-side keep filter). Fetches `limit + 1` to detect "more to
/// come" without a second round-trip.
///
/// Binds:
///   1. partition_key (i64)
///   2. execution_id BLOB
///   3. after_waitpoint_id BLOB — NULL → no cursor
///   4. limit_plus_one (i64)
pub const SELECT_PENDING_WAITPOINTS_PAGE_SQL: &str =
    "SELECT waitpoint_id, waitpoint_key, state, required_signal_names, \
            created_at_ms, activated_at_ms, expires_at_ms, token_kid, token \
       FROM ff_waitpoint_pending \
      WHERE partition_key = ?1 \
        AND execution_id  = ?2 \
        AND state IN ('pending', 'active') \
        AND (?3 IS NULL OR waitpoint_id > ?3) \
      ORDER BY waitpoint_id \
      LIMIT ?4";
