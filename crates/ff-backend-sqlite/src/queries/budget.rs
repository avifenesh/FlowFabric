//! Budget-family SQL — SQLite dialect port of
//! `ff-backend-postgres/src/budget.rs` raw-SQL constants.
//!
//! RFC-023 Phase 3.4 / RFC-020 Wave 9 Standalone-1.
//!
//! Placeholders are `?`-positional to match sqlx-sqlite; the PG
//! reference uses `$1..$N`. Table shapes are identical (see
//! `migrations/0002_budget.sql` + `migrations/0013_budget_policy_extensions.sql`).
//!
//! **Lock-mode note.** The PG reference uses `FOR NO KEY UPDATE` to
//! serialise the breach-UPDATE path. SQLite's single-writer invariant
//! (`BEGIN IMMEDIATE` acquires RESERVED) already serialises any two
//! writers on the same connection pool, so no row-level locking hint
//! is emitted. Write paths wrap in `retry_serializable` to absorb
//! SQLITE_BUSY under contention per RFC-023 §4.3.

// ── ff_budget_usage_dedup ──

/// Idempotent dedup-row reservation. Inserts a placeholder row on
/// first call; no-ops on replay. The `applied_at_ms` is returned by
/// the caller via `RETURNING` for dedup-owned bookkeeping.
pub(crate) const INSERT_DEDUP_PLACEHOLDER_SQL: &str = "\
    INSERT INTO ff_budget_usage_dedup \
        (partition_key, dedup_key, outcome_json, applied_at_ms, expires_at_ms) \
    VALUES (?, ?, '{}', ?, ?) \
    ON CONFLICT (partition_key, dedup_key) DO NOTHING \
    RETURNING applied_at_ms";

pub(crate) const SELECT_DEDUP_OUTCOME_SQL: &str = "\
    SELECT outcome_json FROM ff_budget_usage_dedup \
    WHERE partition_key = ? AND dedup_key = ?";

pub(crate) const UPDATE_DEDUP_OUTCOME_SQL: &str = "\
    UPDATE ff_budget_usage_dedup SET outcome_json = ? \
    WHERE partition_key = ? AND dedup_key = ?";

// ── ff_budget_policy ──

/// `create_budget` upsert. Seeds `next_reset_at_ms = now + interval`
/// when interval > 0 (matches Valkey `ZADD resets_zset` scheduling at
/// flowfabric.lua:6522-6526). Idempotent on `(partition_key, budget_id)`.
pub(crate) const INSERT_BUDGET_POLICY_SQL: &str = "\
    INSERT INTO ff_budget_policy \
        (partition_key, budget_id, policy_json, scope_type, scope_id, \
         enforcement_mode, breach_count, soft_breach_count, \
         last_breach_at_ms, last_breach_dim, next_reset_at_ms, \
         created_at_ms, updated_at_ms) \
    VALUES (?, ?, ?, ?, ?, ?, 0, 0, NULL, NULL, \
            CASE WHEN ? > 0 THEN ? + ? ELSE NULL END, \
            ?, ?) \
    ON CONFLICT (partition_key, budget_id) DO NOTHING \
    RETURNING created_at_ms";

pub(crate) const SELECT_BUDGET_POLICY_ROW_SQL: &str = "\
    SELECT policy_json FROM ff_budget_policy \
    WHERE partition_key = ? AND budget_id = ?";

pub(crate) const UPDATE_BUDGET_HARD_BREACH_SQL: &str = "\
    UPDATE ff_budget_policy \
    SET breach_count      = breach_count + 1, \
        last_breach_at_ms = ?, \
        last_breach_dim   = ?, \
        updated_at_ms     = ? \
    WHERE partition_key = ? AND budget_id = ?";

pub(crate) const UPDATE_BUDGET_SOFT_BREACH_SQL: &str = "\
    UPDATE ff_budget_policy \
    SET soft_breach_count = soft_breach_count + 1, \
        updated_at_ms     = ? \
    WHERE partition_key = ? AND budget_id = ?";

/// Zero breach metadata + reschedule `next_reset_at_ms` on reset.
/// Returns the new `next_reset_at_ms` (possibly NULL when no interval).
pub(crate) const UPDATE_BUDGET_RESET_SQL: &str = "\
    UPDATE ff_budget_policy \
    SET last_breach_at_ms = NULL, \
        last_breach_dim   = NULL, \
        updated_at_ms     = ?, \
        next_reset_at_ms  = CASE WHEN ? > 0 THEN ? + ? ELSE NULL END \
    WHERE partition_key = ? AND budget_id = ? \
    RETURNING next_reset_at_ms";

pub(crate) const SELECT_BUDGET_STATUS_SQL: &str = "\
    SELECT scope_type, scope_id, enforcement_mode, \
           breach_count, soft_breach_count, \
           last_breach_at_ms, last_breach_dim, \
           next_reset_at_ms, created_at_ms, \
           policy_json \
    FROM ff_budget_policy \
    WHERE partition_key = ? AND budget_id = ?";

// ── ff_budget_usage ──

pub(crate) const INSERT_USAGE_ROW_SQL: &str = "\
    INSERT INTO ff_budget_usage \
        (partition_key, budget_id, dimensions_key, current_value, updated_at_ms) \
    VALUES (?, ?, ?, 0, ?) \
    ON CONFLICT (partition_key, budget_id, dimensions_key) DO NOTHING";

pub(crate) const SELECT_USAGE_ROW_SQL: &str = "\
    SELECT current_value FROM ff_budget_usage \
    WHERE partition_key = ? AND budget_id = ? AND dimensions_key = ?";

pub(crate) const UPDATE_USAGE_INCREMENT_SQL: &str = "\
    UPDATE ff_budget_usage \
    SET current_value = current_value + ?, updated_at_ms = ? \
    WHERE partition_key = ? AND budget_id = ? AND dimensions_key = ?";

pub(crate) const UPDATE_USAGE_ZERO_ALL_SQL: &str = "\
    UPDATE ff_budget_usage \
    SET current_value = 0, last_reset_at_ms = ?, updated_at_ms = ? \
    WHERE partition_key = ? AND budget_id = ?";

pub(crate) const SELECT_ALL_USAGE_ROWS_SQL: &str = "\
    SELECT dimensions_key, current_value FROM ff_budget_usage \
    WHERE partition_key = ? AND budget_id = ?";
