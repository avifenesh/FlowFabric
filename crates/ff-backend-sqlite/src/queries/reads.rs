//! SQL statements for Wave 9 read-model methods (RFC-023 Phase 3.3).
//!
//! Mirrors `ff-backend-postgres/src/exec_core.rs` §4.1 read impls at
//! the statement level. SQLite translations:
//!
//! * `LEFT JOIN LATERAL (... LIMIT 1)` → `LEFT JOIN` on a correlated
//!   subquery expression. SQLite executes the subquery per-outer-row
//!   which is equivalent for LIMIT 1 projections.
//! * `FOR UPDATE` — no-op under `BEGIN IMMEDIATE` single-writer.
//! * `BYTEA` → `BLOB`. `uuid` column bound as 16-byte BLOB.
//! * JSON extraction via `json_extract` (JSON1) instead of `->>`.

// ── read_execution_state (§4.1) ────────────────────────────────────

/// Single-column point read on `ff_exec_core.public_state`. Binds:
/// ?1 partition_key, ?2 execution_id BLOB.
pub(crate) const SELECT_PUBLIC_STATE_SQL: &str = r#"
    SELECT public_state
      FROM ff_exec_core
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

// ── read_execution_info (§4.1) ─────────────────────────────────────

/// Multi-column projection of `ff_exec_core` joined with the current
/// attempt row (for `outcome`) + the earliest started attempt row
/// (for `started_at`). Mirrors PG's LATERAL joins with SQLite
/// correlated subqueries in the SELECT list.
///
/// Binds: ?1 partition_key, ?2 execution_id BLOB.
pub(crate) const SELECT_EXECUTION_INFO_SQL: &str = r#"
    SELECT ec.flow_id,
           ec.lane_id,
           ec.priority,
           ec.lifecycle_phase,
           ec.ownership_state,
           ec.eligibility_state,
           ec.public_state,
           ec.attempt_state,
           ec.blocking_reason,
           ec.attempt_index,
           ec.created_at_ms,
           ec.terminal_at_ms,
           ec.raw_fields,
           (SELECT outcome
              FROM ff_attempt a
             WHERE a.partition_key = ec.partition_key
               AND a.execution_id  = ec.execution_id
               AND a.attempt_index = ec.attempt_index) AS attempt_outcome,
           (SELECT started_at_ms
              FROM ff_attempt a
             WHERE a.partition_key = ec.partition_key
               AND a.execution_id  = ec.execution_id
               AND a.started_at_ms IS NOT NULL
             ORDER BY a.attempt_index ASC
             LIMIT 1) AS first_started_at_ms
      FROM ff_exec_core ec
     WHERE ec.partition_key = ?1 AND ec.execution_id = ?2
"#;

// ── get_execution_result (§4.1 + §7.8) ─────────────────────────────

/// `result` BLOB point read from `ff_exec_core`. Current-attempt
/// semantics per RFC-020 Rev 7 Fork 3. Binds: ?1 partition_key, ?2
/// execution_id BLOB.
pub(crate) const SELECT_EXECUTION_RESULT_SQL: &str = r#"
    SELECT result
      FROM ff_exec_core
     WHERE partition_key = ?1 AND execution_id = ?2
"#;
