//! SQLite dialect-forked queries for the attempt hot path.
//!
//! Populated in Phase 2a.2 per RFC-023 §4.1. The SQL strings are
//! module-level `const`s so `backend.rs` call sites reference them
//! by name and cross-dialect review lines them up against the PG
//! reference at `ff-backend-postgres/src/attempt.rs` statement-by-
//! statement.
//!
//! # Dialect translation summary
//!
//! | PG pattern                      | SQLite fork                                     |
//! | ------------------------------- | ----------------------------------------------- |
//! | `FOR UPDATE SKIP LOCKED`        | `BEGIN IMMEDIATE` txn + plain SELECT (§4.1 A3)  |
//! | `text[] @> ARRAY[...]`          | junction-table SELECT + Rust subset match (A4)  |
//! | `jsonb_build_object(...)`       | `json_set(raw_fields, '$.field', ?)`            |
//! | `BIGSERIAL RETURNING`           | `INTEGER PRIMARY KEY AUTOINCREMENT` + RETURNING |
//! | `BYTEA` binds                   | `BLOB` binds (sqlx auto-encodes `&[u8]`)        |
//!
//! # Fence-triple contract
//!
//! `BEGIN IMMEDIATE` on SQLite escalates the txn to RESERVED, so the
//! single-writer invariant (§3.2) holds for the full read-modify-write
//! window. Fence CAS is expressed as a plain SELECT of the attempt
//! row's `lease_epoch` under the txn lock; a mismatch against the
//! caller's handle surfaces as
//! [`ff_core::engine_error::ContentionKind::LeaseConflict`] without
//! retrying (it is a semantic conflict, not transient busy).

/// Scan up to `?3` eligible rows in the partition/lane ordered by
/// `(priority DESC, created_at_ms ASC)`. No `FOR UPDATE SKIP LOCKED`
/// — the enclosing `BEGIN IMMEDIATE` already serializes writers per
/// §4.1 A3.
///
/// The batch shape (vs. `LIMIT 1`) lets `claim_impl` walk past rows
/// whose required capabilities the current worker lacks without
/// starving lower-priority-but-eligible members of the same lane —
/// caught in PR-375 review. Bounded budget keeps the lock window
/// predictable even when the top-priority row has an exotic cap set.
pub(crate) const SELECT_ELIGIBLE_EXEC_SQL: &str = r#"
    SELECT execution_id, attempt_index
      FROM ff_exec_core
     WHERE partition_key = ?1
       AND lane_id = ?2
       AND lifecycle_phase = 'runnable'
       AND eligibility_state = 'eligible_now'
     ORDER BY priority DESC, created_at_ms ASC
     LIMIT ?3
"#;

/// Fetch the capability tokens bound to an execution via the junction
/// table (RFC-023 §4.1 A4). Caller collects the returned rows into a
/// [`ff_core::caps::CapabilityRequirement`] and runs
/// `caps::matches(&req, worker)` in Rust — same shape as the PG path
/// (see `ff-backend-postgres/src/attempt.rs:170-182`), just reading
/// from the junction instead of a PG `text[]` column.
pub(crate) const SELECT_EXEC_CAPABILITIES_SQL: &str = r#"
    SELECT capability
      FROM ff_execution_capabilities
     WHERE execution_id = ?1
"#;

/// UPSERT the attempt row on claim. On first claim the row is fresh
/// (`lease_epoch = 1`); on a retry-attempt re-claim the PK matches
/// the prior (attempt_index, execution_id) and we bump
/// `lease_epoch = prior + 1`, rotate worker identity, clear the
/// outcome. Mirror of the PG ON CONFLICT DO UPDATE at
/// `ff-backend-postgres/src/attempt.rs:192-218`.
pub(crate) const UPSERT_ATTEMPT_ON_CLAIM_SQL: &str = r#"
    INSERT INTO ff_attempt (
        partition_key, execution_id, attempt_index,
        worker_id, worker_instance_id,
        lease_epoch, lease_expires_at_ms, started_at_ms
    ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
    ON CONFLICT (partition_key, execution_id, attempt_index)
    DO UPDATE SET
        worker_id = excluded.worker_id,
        worker_instance_id = excluded.worker_instance_id,
        lease_epoch = ff_attempt.lease_epoch + 1,
        lease_expires_at_ms = excluded.lease_expires_at_ms,
        started_at_ms = excluded.started_at_ms,
        outcome = NULL
    RETURNING lease_epoch
"#;

/// Fence check: read the attempt row's current `lease_epoch` so the
/// caller can compare it against the handle-embedded epoch before any
/// terminal write. Cheaper shape than PG's `SELECT ... FOR UPDATE` —
/// SQLite's `BEGIN IMMEDIATE` already holds the RESERVED lock for the
/// full txn, so a plain SELECT is sufficient.
pub(crate) const SELECT_ATTEMPT_EPOCH_SQL: &str = r#"
    SELECT lease_epoch
      FROM ff_attempt
     WHERE partition_key = ?1 AND execution_id = ?2 AND attempt_index = ?3
"#;

/// Mark the attempt row as terminal-success. Drops the lease by
/// nulling `lease_expires_at_ms` so the scanner does not re-issue
/// reclaim grants. Mirror of PG at
/// `ff-backend-postgres/src/attempt.rs:538-552`.
pub(crate) const UPDATE_ATTEMPT_COMPLETE_SQL: &str = r#"
    UPDATE ff_attempt
       SET terminal_at_ms = ?1,
           outcome = 'success',
           lease_expires_at_ms = NULL
     WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4
"#;

/// Mark the attempt row as retry-scheduled. `outcome = 'retry'` is the
/// PG/Valkey-parity token; the `exec_core` flip to runnable +
/// attempt_index bump lives in `exec_core::UPDATE_EXEC_CORE_FAIL_RETRY_SQL`.
/// Mirror of PG at `ff-backend-postgres/src/attempt.rs:630-645`.
pub(crate) const UPDATE_ATTEMPT_FAIL_RETRY_SQL: &str = r#"
    UPDATE ff_attempt
       SET terminal_at_ms = ?1,
           outcome = 'retry',
           lease_expires_at_ms = NULL
     WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4
"#;

/// Mark the attempt row as terminal-failed (retry budget exhausted or
/// classification was permanent). Mirror of PG at
/// `ff-backend-postgres/src/attempt.rs:684-698`.
pub(crate) const UPDATE_ATTEMPT_FAIL_TERMINAL_SQL: &str = r#"
    UPDATE ff_attempt
       SET terminal_at_ms = ?1,
           outcome = 'failed',
           lease_expires_at_ms = NULL
     WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4
"#;

/// Outbox write for the completion event. The AFTER-INSERT `pg_notify`
/// trigger from the PG schema is intentionally dropped (RFC-023 §4.2 —
/// broadcast moves into a Rust post-commit path); durable replay still
/// rides `event_id > cursor` catch-up against this table, so the
/// insert shape is identical to the PG statement at
/// `ff-backend-postgres/src/attempt.rs:575-592` (and `:720-737` for
/// the `failed` variant).
pub(crate) const INSERT_COMPLETION_EVENT_SQL: &str = r#"
    INSERT INTO ff_completion_event (
        partition_key, execution_id, flow_id, outcome,
        namespace, instance_tag, occurred_at_ms
    )
    SELECT partition_key, execution_id, flow_id, ?1,
           json_extract(raw_fields, '$.namespace'),
           json_extract(raw_fields, '$.tags."cairn.instance_id"'),
           ?2
      FROM ff_exec_core
     WHERE partition_key = ?3 AND execution_id = ?4
"#;
