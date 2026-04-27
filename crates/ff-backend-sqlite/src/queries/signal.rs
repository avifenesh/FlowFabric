//! SQL statements for signal delivery ops (RFC-023 Phase 2b.2.1).
//!
//! Mirrors `ff-backend-postgres/src/signal_event.rs` +
//! the signal-delivery bodies in `suspend_ops.rs`.

/// Append one signal-delivery event to the outbox.
///
/// Binds:
/// * 1 = execution_uuid stringified — `ff_signal_event.execution_id`
///   is TEXT (mirrors PG at `ff-backend-postgres/migrations/0007`).
/// * 2 = signal_id stringified — TEXT column.
/// * 3 = waitpoint_id stringified (nullable) — TEXT column.
/// * 4 = source_identity (nullable) — TEXT.
/// * 5 = delivered_at_ms.
/// * 6 = partition_key (reused in the `WHERE` clause).
/// * 7 = execution_uuid as BLOB — `ff_exec_core.execution_id` is
///   stored as a 16-byte BLOB; the co-transactional SELECT looks it
///   up by BLOB equality while emitting the TEXT stringification
///   onto `ff_signal_event`.
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
/// signal satisfies a waitpoint. Back-fills `namespace` +
/// `instance_tag` from `ff_exec_core.raw_fields` via a
/// co-transactional SELECT so tag-filtered subscribers receive the
/// event (Phase 3.2 fix — pre-fix both columns landed NULL).
/// `flow_id` stays NULL here (matches the PG minimal insert shape at
/// `ff-backend-postgres/src/suspend_ops.rs:845-854`).
///
/// Binds: ?1 partition_key, ?2 execution_id (BLOB — reused for the
/// exec_core lookup), ?3 occurred_at_ms.
pub const INSERT_COMPLETION_RESUMABLE_SQL: &str = "INSERT INTO ff_completion_event \
     (partition_key, execution_id, outcome, occurred_at_ms, namespace, instance_tag) \
     SELECT ?1, ?2, 'resumable', ?3, \
            json_extract(raw_fields, '$.namespace'), \
            json_extract(raw_fields, '$.tags.\"cairn.instance_id\"') \
       FROM ff_exec_core \
      WHERE partition_key = ?1 AND execution_id = ?2 \
     UNION ALL \
     SELECT ?1, ?2, 'resumable', ?3, NULL, NULL \
      WHERE NOT EXISTS ( \
          SELECT 1 FROM ff_exec_core \
           WHERE partition_key = ?1 AND execution_id = ?2 \
      )";
