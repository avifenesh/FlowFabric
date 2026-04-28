//! SQLite dialect-forked queries for RFC-024 `ff_claim_grant` table.
//!
//! Landed by PR-E of the RFC-024 series (SQLite lease-reclaim wiring).
//! The SQL strings are module-level `const`s so `reclaim.rs` call
//! sites reference them by name and cross-dialect review lines them
//! up against the PG reference.
//!
//! # Transaction contract
//!
//! All callers wrap these statements in a `BEGIN IMMEDIATE` txn per
//! RFC-023 §4.3 — SQLite's RESERVED lock covers the full read-modify-
//! write window, so no explicit `FOR UPDATE` equivalent is needed.

/// Read the execution's ownership + phase fields plus the current
/// reclaim counter. Used by `issue_reclaim_grant_impl` to validate
/// that the target execution is in a reclaimable state
/// (`lifecycle_phase = 'active'` AND
/// `ownership_state IN ('lease_expired_reclaimable', 'lease_revoked')`).
pub(crate) const SELECT_EXEC_CORE_FOR_RECLAIM_SQL: &str = r#"
    SELECT lifecycle_phase,
           ownership_state,
           eligibility_state,
           attempt_state,
           attempt_index,
           lane_id,
           lease_reclaim_count
      FROM ff_exec_core
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Insert a reclaim-kind grant row.
pub(crate) const INSERT_RECLAIM_GRANT_SQL: &str = r#"
    INSERT INTO ff_claim_grant (
        partition_key,
        grant_id,
        execution_id,
        kind,
        worker_id,
        worker_instance_id,
        lane_id,
        capability_hash,
        worker_capabilities,
        route_snapshot_json,
        admission_summary,
        grant_ttl_ms,
        issued_at_ms,
        expires_at_ms
    ) VALUES (?1, ?2, ?3, 'reclaim', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
"#;

/// Read the reclaim-kind grant row for an execution under a partition.
pub(crate) const SELECT_RECLAIM_GRANT_BY_EXEC_SQL: &str = r#"
    SELECT grant_id,
           worker_id,
           worker_instance_id,
           lane_id,
           expires_at_ms
      FROM ff_claim_grant
     WHERE partition_key = ?1 AND execution_id = ?2 AND kind = 'reclaim'
     ORDER BY issued_at_ms DESC
     LIMIT 1
"#;

/// Consume (DELETE) a reclaim grant row.
pub(crate) const DELETE_RECLAIM_GRANT_SQL: &str = r#"
    DELETE FROM ff_claim_grant
     WHERE partition_key = ?1 AND grant_id = ?2
"#;

/// Reclaim-time re-validation read. Used by
/// `reclaim_execution_inner` after the grant has been located but
/// before any state mutation, to defend against:
///
/// 1. **Lifecycle/ownership drift between grant issuance and grant
///    consumption.** The exec may have transitioned to
///    `terminal`/`cancelled` through another path (e.g. operator
///    cancel) while the grant was in-flight. Mirrors PG PR-D
///    `claim_grant.rs::reclaim_execution_once` and Lua
///    `flowfabric.lua:3049+` gate.
/// 2. **Caller-supplied stale `current_attempt_index`.** The Rust
///    API takes this from caller args; a stale value could backward-
///    pin `attempt_index` or PK-collide with an existing row.
///    Returning the authoritative value lets the caller derive
///    `new_attempt_index = stored + 1` server-side.
/// 3. **Lease-epoch monotonicity.** The prior attempt row's
///    `lease_epoch` is the floor for the new attempt's epoch
///    (Lua `next_epoch = current_lease_epoch + 1`). The joined
///    `ff_attempt.lease_epoch` covers the first-reclaim case where
///    the `ff_exec_core` lease fields may already be cleared.
pub(crate) const SELECT_EXEC_CORE_RECLAIM_GATE_SQL: &str = r#"
    SELECT ec.lease_reclaim_count,
           ec.lifecycle_phase,
           ec.ownership_state,
           ec.attempt_index,
           COALESCE(a.lease_epoch, 0) AS prior_lease_epoch
      FROM ff_exec_core ec
      LEFT JOIN ff_attempt a
        ON a.partition_key = ec.partition_key
       AND a.execution_id  = ec.execution_id
       AND a.attempt_index = ec.attempt_index
     WHERE ec.partition_key = ?1 AND ec.execution_id = ?2
"#;

/// Bump `lease_reclaim_count` and flip `ff_exec_core` to the
/// new-attempt active/leased posture. Mirrors `UPDATE_EXEC_CORE_RECLAIM_SQL`
/// in `queries/lease.rs` but also bumps `attempt_index` (new attempt)
/// and `lease_reclaim_count` (RFC-024 §3.3 counter).
pub(crate) const UPDATE_EXEC_CORE_FOR_NEW_RECLAIM_ATTEMPT_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'active',
           ownership_state = 'leased',
           eligibility_state = 'not_applicable',
           attempt_state = 'running_attempt',
           attempt_index = ?1,
           lease_reclaim_count = lease_reclaim_count + 1
     WHERE partition_key = ?2 AND execution_id = ?3
"#;

/// Transition `ff_exec_core` to `terminal_failed` on reclaim-cap
/// exceeded. Mirrors the Lua `max_reclaims_exceeded` branch at
/// `flowfabric.lua:3049-3080`.
pub(crate) const UPDATE_EXEC_CORE_RECLAIM_CAP_EXCEEDED_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'terminal',
           ownership_state = 'unowned',
           eligibility_state = 'not_applicable',
           attempt_state = 'attempt_terminal',
           public_state = 'failed',
           terminal_at_ms = ?1
     WHERE partition_key = ?2 AND execution_id = ?3
"#;

/// Mark the prior attempt as `interrupted_reclaimed`. Mirrors the
/// Valkey Lua at `flowfabric.lua:3112`.
pub(crate) const UPDATE_PRIOR_ATTEMPT_INTERRUPTED_RECLAIMED_SQL: &str = r#"
    UPDATE ff_attempt
       SET outcome = 'interrupted_reclaimed',
           terminal_at_ms = ?1
     WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4
"#;

/// Insert a fresh `ff_attempt` row for the reclaimed attempt. The
/// `lease_epoch` bind is the caller-derived `prior_lease_epoch + 1`
/// (see `SELECT_EXEC_CORE_RECLAIM_GATE_SQL`), matching Lua
/// `flowfabric.lua:3106` (`next_epoch = current_lease_epoch + 1`).
/// Previous PR-E had this hard-coded to `0`; that broke epoch
/// monotonicity for lease fencing across the distributed system.
pub(crate) const INSERT_NEW_RECLAIM_ATTEMPT_SQL: &str = r#"
    INSERT INTO ff_attempt (
        partition_key,
        execution_id,
        attempt_index,
        worker_id,
        worker_instance_id,
        lease_epoch,
        lease_expires_at_ms,
        started_at_ms,
        policy
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
"#;

/// Clear lease fields on the prior attempt row when reclaim
/// cap-exceeded fires. Mirrors Lua `flowfabric.lua:3064-3079`
/// (clears `current_lease_id`/`current_worker_*` on exec_core +
/// DEL on lease_current_key). On SQLite the lease-fencing fields
/// live on the attempt row, so we clear there and leave the prior
/// outcome = 'interrupted_reclaimed' for parity with the
/// successful-reclaim branch.
pub(crate) const CLEAR_PRIOR_ATTEMPT_LEASE_ON_CAP_EXCEEDED_SQL: &str = r#"
    UPDATE ff_attempt
       SET outcome = 'interrupted_reclaimed',
           terminal_at_ms = ?1,
           worker_id = NULL,
           worker_instance_id = NULL,
           lease_expires_at_ms = NULL
     WHERE partition_key = ?2 AND execution_id = ?3 AND attempt_index = ?4
"#;
