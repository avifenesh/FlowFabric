//! SQL statements for signal delivery ops (RFC-023 Phase 2b.2.1).
//!
//! Mirrors `ff-backend-postgres/src/signal_event.rs` +
//! the signal-delivery bodies in `suspend_ops.rs`.

/// Append one signal-delivery event to the outbox.
/// Binds: 1=execution_uuid (TEXT), 2=signal_id (TEXT),
///        3=waitpoint_id (TEXT, nullable), 4=source_identity (TEXT, nullable),
///        5=delivered_at_ms, 6=partition_key.
///
/// The SQLite ff_signal_event table populates `namespace` +
/// `instance_tag` from `ff_exec_core.raw_fields` via a co-transactional
/// SELECT — same pattern as the PG reference
/// (`ff-backend-postgres/src/signal_event.rs:33-49`).
/// `json_extract` is the SQLite JSON1 analogue of PG's `->>` operator.
pub const INSERT_SIGNAL_EVENT_SQL: &str = "INSERT INTO ff_signal_event \
     (execution_id, signal_id, waitpoint_id, source_identity, \
      delivered_at_ms, partition_key, namespace, instance_tag) \
     SELECT ?1, ?2, ?3, ?4, ?5, ?6, \
            json_extract(raw_fields, '$.namespace'), \
            json_extract(raw_fields, '$.tags.\"cairn.instance_id\"') \
       FROM ff_exec_core \
      WHERE partition_key = ?6 AND execution_id = ?7 \
     UNION ALL \
     SELECT ?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL \
      WHERE NOT EXISTS ( \
          SELECT 1 FROM ff_exec_core \
           WHERE partition_key = ?6 AND execution_id = ?7 \
      )";

/// Completion outbox insert for the `resumable` transition when a
/// signal satisfies a waitpoint. `flow_id` + `namespace` + `instance_tag`
/// columns on `ff_completion_event` default to NULL when not provided —
/// matches the PG path's minimal insert shape at
/// `ff-backend-postgres/src/suspend_ops.rs:845-854`.
pub const INSERT_COMPLETION_RESUMABLE_SQL: &str = "INSERT INTO ff_completion_event \
     (partition_key, execution_id, outcome, occurred_at_ms) \
     VALUES (?1, ?2, 'resumable', ?3)";
