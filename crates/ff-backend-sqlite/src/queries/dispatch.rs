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

/// Insert one lease-lifecycle outbox row, back-filling `namespace` +
/// `instance_tag` from the co-transactional `ff_exec_core.raw_fields`
/// row so tag-filtered subscribers do not silently drop the event
/// (Phase 3.2 fix — pre-fix, both columns landed NULL and a filter
/// with `instance_tag=...` matched zero rows). Mirrors the
/// `INSERT_SIGNAL_EVENT_SQL` SELECT+UNION-ALL shape from
/// `queries/signal.rs`: the first branch reads exec_core (usual
/// path); the second branch fires only when the exec row does not
/// exist so the insert is still guaranteed by the 4 shipped binds
/// and never lost.
///
/// Binds:
///   1. execution_id (TEXT — UUID string) — emitted on the outbox row
///   2. event_type (TEXT)
///   3. occurred_at_ms (i64)
///   4. partition_key (i64) — used both on the outbox row and the
///      co-transactional exec_core lookup
///   5. execution_id (BLOB) — `ff_exec_core.execution_id` is BLOB, so
///      the lookup binds the Uuid-as-bytes form
pub(crate) const INSERT_LEASE_EVENT_SQL: &str = r#"
    INSERT INTO ff_lease_event
        (execution_id, lease_id, event_type, occurred_at_ms, partition_key,
         namespace, instance_tag)
    SELECT ?1, NULL, ?2, ?3, ?4,
           json_extract(raw_fields, '$.namespace'),
           json_extract(raw_fields, '$.tags."cairn.instance_id"')
      FROM ff_exec_core
     WHERE partition_key = ?4 AND execution_id = ?5
    UNION ALL
    SELECT ?1, NULL, ?2, ?3, ?4, NULL, NULL
     WHERE NOT EXISTS (
         SELECT 1 FROM ff_exec_core
          WHERE partition_key = ?4 AND execution_id = ?5
     )
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

// Operator-event outbox inserts live in `queries::operator`
// (Phase 3.2). See `queries/operator.rs::INSERT_OPERATOR_EVENT_SQL` —
// the Wave-9 producer back-fills namespace + instance_tag from
// exec_core.raw_fields via co-transactional SELECT, matching the
// lease/completion/signal producer shape.
