//! RFC-019 Stage B — `ff_signal_event` outbox producer helper.
//!
//! Every successful `deliver_signal` call INSERTs a row here inside
//! the same SERIALIZABLE transaction as the signal state-write. The
//! `ff_signal_event_notify_trg` trigger fires
//! `pg_notify('ff_signal_event', event_id::text)` at COMMIT; the
//! `subscribe_signal_delivery` loop wakes, runs its catch-up query,
//! and forwards matching rows to the subscriber as `StreamEvent`s.
//!
//! See `migrations/0007_signal_event_outbox.sql` for the schema.

use sqlx::Postgres;
use uuid::Uuid;

use crate::error::map_sqlx_error;
use ff_core::engine_error::EngineError;

/// Append one signal-delivery event to the outbox within an in-flight
/// transaction. The trigger fires NOTIFY at COMMIT.
pub(crate) async fn emit<'c>(
    tx: &mut sqlx::Transaction<'c, Postgres>,
    partition_key: i16,
    execution_uuid: Uuid,
    signal_id: &str,
    waitpoint_id: Option<&str>,
    source_identity: Option<&str>,
    delivered_at_ms: i64,
) -> Result<(), EngineError> {
    // #282 — populate `namespace` + `instance_tag` via co-transactional
    // SELECT on `ff_exec_core`. See `lease_event::emit` for rationale.
    sqlx::query(
        "INSERT INTO ff_signal_event \
         (execution_id, signal_id, waitpoint_id, source_identity, \
          delivered_at_ms, partition_key, namespace, instance_tag) \
         SELECT $1, $2, $3, $4, $5, $6, \
                raw_fields->>'namespace', \
                raw_fields->'tags'->>'cairn.instance_id' \
         FROM ff_exec_core \
         WHERE partition_key = $6 AND execution_id = $7::uuid \
         UNION ALL \
         SELECT $1, $2, $3, $4, $5, $6, NULL, NULL \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM ff_exec_core \
             WHERE partition_key = $6 AND execution_id = $7::uuid \
         )",
    )
    .bind(execution_uuid.to_string())
    .bind(signal_id)
    .bind(waitpoint_id)
    .bind(source_identity)
    .bind(delivered_at_ms)
    .bind(i32::from(partition_key))
    .bind(execution_uuid)
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}
