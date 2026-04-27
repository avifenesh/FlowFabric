//! SQLite dialect-forked queries for lease lifecycle (renew / reclaim).
//!
//! Populated in Phase 2a.3 per RFC-023 §4.1. The SQL strings are
//! module-level `const`s so `backend.rs` call sites reference them
//! by name and cross-dialect review lines them up against the PG
//! reference at `ff-backend-postgres/src/attempt.rs` statement-by-
//! statement.
//!
//! # Fence-triple contract
//!
//! Same as `queries/attempt.rs`: `BEGIN IMMEDIATE` on SQLite escalates
//! the txn to RESERVED for the full read-modify-write window; the
//! fence CAS is a plain SELECT of `ff_attempt.lease_epoch` (see
//! `queries/attempt.rs::SELECT_ATTEMPT_EPOCH_SQL`).

/// Advance the lease expiry for a live attempt. Caller has already
/// fence-checked the handle's epoch against the row. The epoch is NOT
/// bumped — renewal keeps the same epoch per the PG reference at
/// `ff-backend-postgres/src/attempt.rs:446-461`.
pub(crate) const UPDATE_ATTEMPT_RENEW_SQL: &str = r#"
    UPDATE ff_attempt
       SET lease_expires_at_ms = ?1
     WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4
"#;

/// Look up the latest attempt row for a reclaim. Returns
/// `(attempt_index, lease_epoch, lease_expires_at_ms)`; the
/// caller checks expiry (NULL or `<= now`) before minting a fresh
/// handle. Mirror of PG at
/// `ff-backend-postgres/src/attempt.rs:294-308` — without
/// `FOR UPDATE` because `BEGIN IMMEDIATE` already holds RESERVED.
pub(crate) const SELECT_LATEST_ATTEMPT_FOR_RECLAIM_SQL: &str = r#"
    SELECT attempt_index, lease_epoch, lease_expires_at_ms
      FROM ff_attempt
     WHERE partition_key = ?1 AND execution_id = ?2
     ORDER BY attempt_index DESC
     LIMIT 1
"#;

/// Bump the epoch, rotate worker identity, install a fresh lease
/// expiry, and clear the outcome so the attempt is live again.
/// Mirror of PG at `ff-backend-postgres/src/attempt.rs:332-353`.
pub(crate) const UPDATE_ATTEMPT_RECLAIM_SQL: &str = r#"
    UPDATE ff_attempt
       SET worker_id = ?1,
           worker_instance_id = ?2,
           lease_epoch = lease_epoch + 1,
           lease_expires_at_ms = ?3,
           started_at_ms = ?4,
           outcome = NULL
     WHERE partition_key = ?5 AND execution_id = ?6 AND attempt_index = ?7
"#;

/// Flip `ff_exec_core` back to active/leased for a reclaimed attempt.
/// Mirror of PG at `ff-backend-postgres/src/attempt.rs:355-369`.
pub(crate) const UPDATE_EXEC_CORE_RECLAIM_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'active',
           ownership_state = 'leased',
           eligibility_state = 'not_applicable',
           attempt_state = 'running_attempt'
     WHERE partition_key = ?1 AND execution_id = ?2
"#;
