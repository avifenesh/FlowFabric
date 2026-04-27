//! RFC-023 Phase 3.1 — `subscribe_signal_delivery` on SQLite.
//!
//! Tails `ff_signal_event` via the
//! [`OutboxCursorReader`](crate::outbox_cursor) primitive.
//! Mirror of `ff-backend-postgres/src/signal_delivery_subscribe.rs`.
//!
//! Unlike the lease/completion producers, the SQLite signal producer
//! DOES populate `namespace` + `instance_tag` from `ff_exec_core`
//! via `json_extract` (see `queries/signal.rs::INSERT_SIGNAL_EVENT_SQL`),
//! so `ScannerFilter::instance_tag` is observable end-to-end.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use sqlx::Row;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use uuid::Uuid;

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::stream_events::{SignalDeliveryEffect, SignalDeliveryEvent, SignalDeliverySubscription};
use ff_core::stream_subscribe::{
    StreamCursor, decode_postgres_event_cursor, encode_postgres_event_cursor,
};
use ff_core::types::{ExecutionId, SignalId, TimestampMs, WaitpointId};

use crate::completion_subscribe::passes_filter;
use crate::outbox_cursor::{self, OutboxCursorConfig, OutboxCursorStream, RowDecoder};
use crate::pubsub::OutboxEvent;

pub(crate) const SELECT_SIGNAL_EVENTS_SQL: &str = r#"
    SELECT event_id, execution_id, signal_id, waitpoint_id,
           source_identity, delivered_at_ms, partition_key,
           namespace, instance_tag
      FROM ff_signal_event
     WHERE partition_key = ?1 AND event_id > ?2
  ORDER BY event_id ASC
     LIMIT ?3
"#;

const PARTITION_KEY: i64 = 0;
const REPLAY_BATCH: i64 = 256;

pub(crate) async fn subscribe(
    pool: SqlitePool,
    wakeup: broadcast::Receiver<OutboxEvent>,
    cursor: StreamCursor,
    filter: ScannerFilter,
) -> Result<SignalDeliverySubscription, EngineError> {
    let start = decode_postgres_event_cursor(&cursor).map_err(|msg| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: msg.to_string(),
    })?;

    let last_seen: i64 = match start {
        Some(v) => v,
        None => sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_id), 0) FROM ff_signal_event \
             WHERE partition_key = ?1",
        )
        .bind(PARTITION_KEY)
        .fetch_one(&pool)
        .await
        .map_err(|_| EngineError::Unavailable {
            op: "sqlite.subscribe_signal_delivery",
        })?,
    };

    let stream = outbox_cursor::spawn(OutboxCursorConfig {
        pool,
        select_sql: SELECT_SIGNAL_EVENTS_SQL,
        partition_key: PARTITION_KEY,
        cursor: last_seen,
        batch_size: REPLAY_BATCH,
        wakeup,
        decoder: make_decoder(filter),
        row_event_id: extract_event_id,
    });

    Ok(Box::pin(TypedStream { inner: stream }))
}

struct TypedStream {
    inner: OutboxCursorStream<SignalDeliveryEvent>,
}

impl Stream for TypedStream {
    type Item = Result<SignalDeliveryEvent, EngineError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

fn extract_event_id(row: &sqlx::sqlite::SqliteRow) -> Result<i64, EngineError> {
    row.try_get("event_id")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_signal_event.event_id: {e}"),
        })
}

fn make_decoder(filter: ScannerFilter) -> RowDecoder<SignalDeliveryEvent> {
    Box::new(move |row| decode_row(row, &filter))
}

fn decode_row(
    row: &sqlx::sqlite::SqliteRow,
    filter: &ScannerFilter,
) -> Result<Option<SignalDeliveryEvent>, EngineError> {
    let event_id: i64 = row
        .try_get("event_id")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("event_id: {e}"),
        })?;

    let namespace: Option<String> = row
        .try_get::<Option<String>, _>("namespace")
        .unwrap_or(None);
    let instance_tag: Option<String> = row
        .try_get::<Option<String>, _>("instance_tag")
        .unwrap_or(None);
    if !passes_filter(filter, namespace.as_deref(), instance_tag.as_deref()) {
        return Ok(None);
    }

    let exec_text: String = row
        .try_get("execution_id")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("execution_id: {e}"),
        })?;
    let partition_key: i64 = row
        .try_get("partition_key")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("partition_key: {e}"),
        })?;
    let exec_uuid = match Uuid::parse_str(&exec_text) {
        Ok(u) => u,
        Err(_) => {
            tracing::warn!(
                execution_id = %exec_text,
                event_id = event_id,
                "sqlite.signal_delivery: skipping row with unparseable execution_id"
            );
            return Ok(None);
        }
    };
    let execution_id = ExecutionId::parse(&format!("{{fp:{partition_key}}}:{exec_uuid}"))
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_signal_event.execution_id: {e}"),
        })?;

    let signal_id_text: String =
        row.try_get("signal_id").map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("signal_id: {e}"),
        })?;
    let signal_id = match SignalId::parse(&signal_id_text) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                error = %e,
                event_id = event_id,
                "sqlite.signal_delivery: skipping row with bad signal_id"
            );
            return Ok(None);
        }
    };

    let waitpoint_text: Option<String> = row
        .try_get::<Option<String>, _>("waitpoint_id")
        .unwrap_or(None);
    let waitpoint_id = waitpoint_text
        .as_deref()
        .and_then(|s| WaitpointId::parse(s).ok());
    let source_identity: Option<String> = row
        .try_get::<Option<String>, _>("source_identity")
        .unwrap_or(None);
    let delivered_at_ms: i64 =
        row.try_get("delivered_at_ms")
            .map_err(|e| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("delivered_at_ms: {e}"),
            })?;

    let cursor = encode_postgres_event_cursor(event_id);
    // SQLite outbox does not yet persist the delivery effect; surface
    // `Satisfied` as the PG reference does.
    Ok(Some(SignalDeliveryEvent::new(
        cursor,
        execution_id,
        signal_id,
        waitpoint_id,
        source_identity,
        SignalDeliveryEffect::Satisfied,
        TimestampMs::from_millis(delivered_at_ms),
    )))
}
