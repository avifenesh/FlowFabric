//! SQLite dialect-forked queries for outbox writes + broadcast
//! wakeup drains (RFC-023 §4.2).
//!
//! The outbox rows are the **durable** record of each dispatch event;
//! the broadcast channel is the wakeup. Subscribers catch up via
//! `event_id > cursor` reads on these tables.

// `INSERT_COMPLETION_EVENT_SQL` lives in `queries::attempt` (it pre-dates
// Phase 2b.1 — the attempt-ops family wrote completion events from
// Phase 2a.2 onward). We reuse that shape here rather than duplicating.
// See `crates/ff-backend-sqlite/src/queries/attempt.rs`.

// ── lease_event outbox ────────────────────────────────────────────────

/// Insert one lease-lifecycle outbox row. Mirror of the in-line SQL
/// previously embedded in `backend.rs::insert_lease_event` (Phase 2a.2);
/// centralized here so post-commit broadcast emit picks up the
/// `event_id` with one read.
///
/// Binds:
///   1. execution_id (TEXT — UUID string)
///   2. event_type (TEXT)
///   3. occurred_at_ms (i64)
///   4. partition_key (i64)
pub(crate) const INSERT_LEASE_EVENT_SQL: &str = r#"
    INSERT INTO ff_lease_event
        (execution_id, lease_id, event_type, occurred_at_ms, partition_key)
    VALUES (?1, NULL, ?2, ?3, ?4)
"#;

// ── signal_event outbox ───────────────────────────────────────────────

/// Insert one signal-delivery outbox row. Consumer-side (Phase 2b.2
/// `deliver_signal` + subscribe_signal_delivery) drains via
/// `event_id > cursor`.
///
/// Binds:
///   1. execution_id (TEXT)
///   2. signal_id (TEXT)
///   3. waitpoint_id (Option<TEXT>)
///   4. source_identity (Option<TEXT>)
///   5. delivered_at_ms (i64)
///   6. partition_key (i64)
#[allow(dead_code)] // Consumed by Phase 2b.2 deliver_signal
pub(crate) const INSERT_SIGNAL_EVENT_SQL: &str = r#"
    INSERT INTO ff_signal_event
        (execution_id, signal_id, waitpoint_id, source_identity,
         delivered_at_ms, partition_key)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
"#;

// ── operator_event outbox ─────────────────────────────────────────────

/// Insert one operator-event outbox row. Used by Wave-9 admin ops
/// (Phase 2b.2+); the migration CHECK clause restricts `event_type`
/// to the allowed literal set.
///
/// Binds:
///   1. execution_id (TEXT)
///   2. event_type (TEXT)
///   3. details (Option<TEXT JSON>)
///   4. occurred_at_ms (i64)
///   5. partition_key (i64)
///   6. namespace (Option<TEXT>)
///   7. instance_tag (Option<TEXT>)
#[allow(dead_code)] // Consumed by Phase 2b.2 change_priority / replay / cancel_flow_header
pub(crate) const INSERT_OPERATOR_EVENT_SQL: &str = r#"
    INSERT INTO ff_operator_event
        (execution_id, event_type, details, occurred_at_ms, partition_key,
         namespace, instance_tag)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#;
