//! RFC-020 Wave 9 Spine-A pt.2 — `ff_operator_event` outbox producer helper.
//!
//! Mirrors [`crate::lease_event::emit`] for the operator-control
//! channel. Every `change_priority` / `replay_execution` /
//! `cancel_flow_header` INSERTs exactly one row here inside the same
//! SERIALIZABLE transaction as the state mutation. The
//! `ff_operator_event_notify_trg` trigger fires
//! `pg_notify('ff_operator_event', event_id::text)` at COMMIT so
//! RFC-019 subscribers wake and drain via the `event_id > $cursor`
//! catch-up query.
//!
//! `ack_cancel_member` does **not** emit (§4.2.7 matrix — parity with
//! Valkey's quiet-ack).
//!
//! See `migrations/0010_operator_event_outbox.sql` for the schema.

use serde_json::Value as JsonValue;
use sqlx::Postgres;
use uuid::Uuid;

use crate::error::map_sqlx_error;
use ff_core::engine_error::EngineError;

/// Wire-string operator-event types. Match the set enforced by the
/// migration 0010 CHECK constraint.
pub const EVENT_PRIORITY_CHANGED: &str = "priority_changed";
pub const EVENT_REPLAYED: &str = "replayed";
pub const EVENT_FLOW_CANCEL_REQUESTED: &str = "flow_cancel_requested";

/// Append one operator-control event to the outbox within an
/// in-flight transaction.
///
/// `namespace` / `instance_tag` are looked up co-transactionally from
/// `ff_exec_core` when `execution_uuid` is a real exec id so RFC-019
/// subscriber filters work. For flow-level events where no single
/// exec is authoritative (`flow_cancel_requested`), callers pass the
/// flow_id-as-uuid and the same lookup gracefully falls back to NULL
/// namespace/tag (no matching exec row).
pub(crate) async fn emit<'c>(
    tx: &mut sqlx::Transaction<'c, Postgres>,
    partition_key: i16,
    execution_uuid: Uuid,
    event_type: &'static str,
    details: JsonValue,
    occurred_at_ms: i64,
) -> Result<(), EngineError> {
    sqlx::query(
        "INSERT INTO ff_operator_event \
         (execution_id, event_type, details, occurred_at_ms, partition_key, \
          namespace, instance_tag) \
         SELECT $1, $2, $3, $4, $5, \
                raw_fields->>'namespace', \
                raw_fields->'tags'->>'cairn.instance_id' \
         FROM ff_exec_core \
         WHERE partition_key = $5 AND execution_id = $6::uuid \
         UNION ALL \
         SELECT $1, $2, $3, $4, $5, NULL, NULL \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM ff_exec_core \
             WHERE partition_key = $5 AND execution_id = $6::uuid \
         )",
    )
    .bind(execution_uuid.to_string())
    .bind(event_type)
    .bind(details)
    .bind(occurred_at_ms)
    .bind(i32::from(partition_key))
    .bind(execution_uuid)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}
