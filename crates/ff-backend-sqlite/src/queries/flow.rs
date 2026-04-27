//! SQLite dialect-forked queries for flow-header create / cancel.
//!
//! Populated in Phase 2b.1 per RFC-023 §4.1. Mirrors the PG reference
//! at `ff-backend-postgres/src/flow.rs` + `ff-backend-postgres/src/flow_staging.rs`
//! statement-by-statement; the only dialect changes are `jsonb` → TEXT
//! JSON (JSON1), `$N` → `?N`, and the removal of `FOR UPDATE` /
//! partition-aware casts since SQLite runs single-writer under
//! `BEGIN IMMEDIATE` (§4.1 A3).

// ── create_flow ─────────────────────────────────────────────────────────

/// Idempotent flow-header insert. `ON CONFLICT DO NOTHING` — caller
/// detects duplicate via post-insert changes() == 0.
///
/// Binds:
///   1. partition_key (i64)
///   2. flow_id (Uuid)
///   3. created_at_ms (i64)
///   4. raw_fields (TEXT JSON: flow_kind / namespace / node_count=0 /
///      edge_count=0 / last_mutation_at_ms)
pub(crate) const INSERT_FLOW_CORE_SQL: &str = r#"
    INSERT INTO ff_flow_core
        (partition_key, flow_id, graph_revision, public_flow_state,
         created_at_ms, raw_fields)
    VALUES (?1, ?2, 0, 'open', ?3, ?4)
    ON CONFLICT (partition_key, flow_id) DO NOTHING
"#;

// Lifecycle-phase literal sets used by cancel_flow member selection.
// Kept as SQL-inline literals (rather than Rust constants bound at
// query time) because SQLite does not support IN-list parameter
// arrays; each literal is a deployment-wide invariant string that
// `ff_exec_core.lifecycle_phase` can take, never user input.
// Centralizing them in this module so any future lifecycle-phase
// vocabulary change touches exactly one file.
//
// `TERMINAL_PHASES` — an exec row in any of these is already finished,
// so cancel_flow skips it. Mirrors PG's `NOT IN (...)` filter at
// `ff-backend-postgres/src/flow.rs:648-649` plus the RFC-023 SQLite
// 'terminal' literal (see `queries/exec_core.rs::UPDATE_EXEC_CORE_COMPLETE_SQL`
// which writes 'terminal' rather than PG's multiple terminal literals).
//
// `PRE_RUNNABLE_PHASES` — the `CancelPending` policy only flips rows
// whose execution hasn't started yet.

// ── cancel_flow ─────────────────────────────────────────────────────────

/// Atomic flip of flow_core to cancelled, recording the requested
/// cancellation policy in `raw_fields`. The PG path uses a `RETURNING`
/// to detect flow-not-found; SQLite uses `changes()` after execute
/// (caller reads `ExecuteResult::rows_affected`).
///
/// Binds:
///   1. partition_key (i64)
///   2. flow_id (Uuid)
///   3. now_ms (i64) — consumed by the COALESCE(terminal_at_ms, ?3)
///   4. policy_str (TEXT)
pub(crate) const UPDATE_FLOW_CORE_CANCEL_SQL: &str = r#"
    UPDATE ff_flow_core
       SET public_flow_state = 'cancelled',
           terminal_at_ms = COALESCE(terminal_at_ms, ?3),
           raw_fields = json_set(raw_fields, '$.cancellation_policy', ?4)
     WHERE partition_key = ?1 AND flow_id = ?2
"#;

/// Enumerate member executions for cancel_flow. Returns rows filtered
/// by the policy-specific `lifecycle_phase` set.
///
/// NOTE: the state filter is embedded at format-time (not bound) because
/// SQLite prepares the statement by string shape and the NOT-IN literal
/// list is the simplest dialect-portable shape. The three literals are
/// hard-coded constants in `backend.rs::cancel_flow_impl`, so there is no
/// user-controlled string concatenation.
pub(crate) const SELECT_FLOW_MEMBERS_CANCEL_ALL_SQL: &str = r#"
    SELECT execution_id
      FROM ff_exec_core
     WHERE partition_key = ?1
       AND flow_id = ?2
       AND lifecycle_phase NOT IN ('completed', 'failed', 'cancelled', 'expired', 'terminal')
"#;

pub(crate) const SELECT_FLOW_MEMBERS_CANCEL_PENDING_SQL: &str = r#"
    SELECT execution_id
      FROM ff_exec_core
     WHERE partition_key = ?1
       AND flow_id = ?2
       AND lifecycle_phase IN ('pending', 'blocked', 'eligible', 'runnable', 'submitted')
"#;

/// Flip one member exec_core row to cancelled. Mirror of PG at
/// `ff-backend-postgres/src/flow.rs:672-687`.
///
/// Binds:
///   1. partition_key (i64)
///   2. execution_id (Uuid)
///   3. now_ms (i64)
pub(crate) const UPDATE_EXEC_CORE_CANCEL_MEMBER_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'cancelled',
           eligibility_state = 'cancelled',
           public_state = 'cancelled',
           attempt_state = 'cancelled',
           terminal_at_ms = COALESCE(terminal_at_ms, ?3),
           cancellation_reason = COALESCE(cancellation_reason, 'flow_cancelled'),
           cancelled_by = COALESCE(cancelled_by, 'cancel_flow')
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// RFC-016 Stage C bookkeeping: enqueue a pending-cancel row for
/// every edge_group with `running_count > 0` on the cancelled flow
/// (the Wave-5 dispatcher reads this).
pub(crate) const INSERT_PENDING_CANCEL_GROUPS_SQL: &str = r#"
    INSERT OR IGNORE INTO ff_pending_cancel_groups
        (partition_key, flow_id, downstream_eid, enqueued_at_ms)
    SELECT partition_key, flow_id, downstream_eid, ?3
      FROM ff_edge_group
     WHERE partition_key = ?1 AND flow_id = ?2 AND running_count > 0
"#;
