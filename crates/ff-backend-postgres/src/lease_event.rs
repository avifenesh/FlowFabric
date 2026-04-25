//! RFC-019 Stage B — `ff_lease_event` outbox producer helper.
//!
//! Every lease-lifecycle transition (acquire / renew / expire /
//! reclaim / revoke) INSERTs a row here inside the same transaction
//! as the state change. The `ff_lease_event_notify_trg` trigger fires
//! `pg_notify('ff_lease_event', event_id::text)` at COMMIT; the
//! `subscribe_lease_history` loop wakes, runs its catch-up query, and
//! forwards matching rows to the subscriber as `StreamEvent`s.
//!
//! See `migrations/0006_lease_event_outbox.sql` for the schema.

use sqlx::Postgres;
use uuid::Uuid;

use crate::error::map_sqlx_error;
use ff_core::engine_error::EngineError;

/// Wire-string lease-event types. Match the Valkey-side semantics
/// surfaced via `subscribe_lease_history`.
pub const EVENT_ACQUIRED: &str = "acquired";
pub const EVENT_RENEWED: &str = "renewed";
pub const EVENT_EXPIRED: &str = "expired";
pub const EVENT_RECLAIMED: &str = "reclaimed";
pub const EVENT_REVOKED: &str = "revoked";

/// Append one lease lifecycle event to the outbox within an in-flight
/// transaction. The trigger fires NOTIFY at COMMIT.
///
/// `lease_id` is optional — Postgres does not persist a stable
/// `lease_id` on `ff_attempt` (identity is `(lease_epoch,
/// attempt_index, execution_id)`), so producer sites pass `None`
/// except when a caller-supplied `LeaseId` is available on the
/// handle.
pub(crate) async fn emit<'c>(
    tx: &mut sqlx::Transaction<'c, Postgres>,
    partition_key: i16,
    execution_uuid: Uuid,
    lease_id: Option<&str>,
    event_type: &'static str,
    occurred_at_ms: i64,
) -> Result<(), EngineError> {
    sqlx::query(
        "INSERT INTO ff_lease_event \
         (execution_id, lease_id, event_type, occurred_at_ms, partition_key) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(execution_uuid.to_string())
    .bind(lease_id)
    .bind(event_type)
    .bind(occurred_at_ms)
    .bind(i32::from(partition_key))
    .execute(&mut **tx)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}
