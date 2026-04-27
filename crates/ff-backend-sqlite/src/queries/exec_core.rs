//! SQLite dialect-forked queries for execution core (create / claim /
//! complete / fail).
//!
//! Populated in Phase 2a.2 per RFC-023 §4.1. The SQL strings are kept
//! as module-level `const`s so `backend.rs` call sites reference them
//! by name and migration-parity review can line them up against the
//! PG reference statement-by-statement.

/// Flip exec_core to `active/leased/running` on a successful claim
/// (mirror of PG's claim-path UPDATE at
/// `ff-backend-postgres/src/attempt.rs:236-250`).
pub(crate) const UPDATE_EXEC_CORE_CLAIM_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'active',
           ownership_state = 'leased',
           eligibility_state = 'not_applicable',
           attempt_state = 'running_attempt'
     WHERE partition_key = ?1 AND execution_id = ?2
"#;

/// Flip exec_core to terminal success, recording the result payload
/// (mirror of `ff-backend-postgres/src/attempt.rs:554-572`).
pub(crate) const UPDATE_EXEC_CORE_COMPLETE_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'terminal',
           ownership_state = 'unowned',
           eligibility_state = 'not_applicable',
           attempt_state = 'attempt_terminal',
           terminal_at_ms = ?1,
           result = ?2
     WHERE partition_key = ?3 AND execution_id = ?4
"#;

/// Re-enqueue the execution for a retry attempt: flip lifecycle back to
/// runnable, bump attempt_index, stash the last failure message in the
/// TEXT-encoded `raw_fields` JSON document via SQLite's `json_set` (JSON1).
/// Mirrors PG's `jsonb_build_object(...)` concat at
/// `ff-backend-postgres/src/attempt.rs:647-664`.
pub(crate) const UPDATE_EXEC_CORE_FAIL_RETRY_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'runnable',
           ownership_state = 'unowned',
           eligibility_state = 'eligible_now',
           attempt_state = 'pending_retry_attempt',
           attempt_index = attempt_index + 1,
           raw_fields = json_set(raw_fields, '$.last_failure_message', ?1)
     WHERE partition_key = ?2 AND execution_id = ?3
"#;

/// Flip exec_core to terminal failed — retry budget exhausted or
/// classification was permanent. Mirror of PG at
/// `ff-backend-postgres/src/attempt.rs:700-718`.
pub(crate) const UPDATE_EXEC_CORE_FAIL_TERMINAL_SQL: &str = r#"
    UPDATE ff_exec_core
       SET lifecycle_phase = 'terminal',
           ownership_state = 'unowned',
           eligibility_state = 'not_applicable',
           attempt_state = 'attempt_terminal',
           terminal_at_ms = ?1,
           raw_fields = json_set(raw_fields, '$.last_failure_message', ?2)
     WHERE partition_key = ?3 AND execution_id = ?4
"#;
