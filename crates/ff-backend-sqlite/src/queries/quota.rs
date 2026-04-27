//! Quota-family SQL — SQLite dialect port of the quota-policy INSERT
//! used by `create_quota_policy`. RFC-023 Phase 3.4 / RFC-020 Wave 9
//! Standalone-1 §4.4.1.
//!
//! The `ff_quota_window` + `ff_quota_admitted` tables are defined by
//! migration 0012 but not written on `create_quota_policy` — they are
//! populated by the (future) admission path. The Rev 6 "3 tables"
//! language covers schema availability, not per-write fan-out; the PG
//! reference (`ff-backend-postgres/src/budget.rs::create_quota_policy_impl`)
//! writes only the policy row on create, mirrored here.

pub(crate) const INSERT_QUOTA_POLICY_SQL: &str = "\
    INSERT INTO ff_quota_policy \
        (partition_key, quota_policy_id, requests_per_window_seconds, \
         max_requests_per_window, active_concurrency_cap, \
         active_concurrency, created_at_ms, updated_at_ms) \
    VALUES (?, ?, ?, ?, ?, 0, ?, ?) \
    ON CONFLICT (partition_key, quota_policy_id) DO NOTHING \
    RETURNING created_at_ms";
