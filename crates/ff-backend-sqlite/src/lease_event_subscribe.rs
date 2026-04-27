//! RFC-023 Phase 3.1 — `subscribe_lease_history` on SQLite.
//!
//! Tails `ff_lease_event` via [`OutboxCursorReader`](crate::outbox_cursor).
//! Mirror of `ff-backend-postgres/src/lease_event_subscribe.rs`.
//! Cursor encoding, filter semantics, and event-type → typed-variant
//! mapping all match the PG reference so cross-backend consumer code
//! is identical.
//!
//! Notes on SQLite producer shape (current as of Phase 2b.1):
//!  * `execution_id` column is TEXT (UUID string), not BLOB.
//!  * `lease_id` is always NULL on insert (PG reference is likewise
//!    NULL in most paths).
//!  * `namespace` + `instance_tag` are always NULL on insert today —
//!    populating them is producer-side follow-up work; filtered
//!    subscribers silently drop NULL-column rows, which matches the
//!    cross-backend invariant on migrations 0008.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use sqlx::Row;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use uuid::Uuid;

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::stream_events::{LeaseHistoryEvent, LeaseHistorySubscription};
use ff_core::stream_subscribe::{
    StreamCursor, decode_postgres_event_cursor, encode_postgres_event_cursor,
};
use ff_core::types::{ExecutionId, LeaseId, TimestampMs};

use crate::completion_subscribe::passes_filter;
use crate::outbox_cursor::{self, OutboxCursorConfig, OutboxCursorStream, RowDecoder};
use crate::pubsub::OutboxEvent;

pub(crate) const SELECT_LEASE_EVENTS_SQL: &str = r#"
    SELECT event_id, execution_id, lease_id, event_type, occurred_at_ms,
           partition_key, namespace, instance_tag
      FROM ff_lease_event
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
) -> Result<LeaseHistorySubscription, EngineError> {
    let start = decode_postgres_event_cursor(&cursor).map_err(|msg| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: msg.to_string(),
    })?;

    let last_seen: i64 = match start {
        Some(v) => v,
        None => sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_id), 0) FROM ff_lease_event \
             WHERE partition_key = ?1",
        )
        .bind(PARTITION_KEY)
        .fetch_one(&pool)
        .await
        .map_err(|_| EngineError::Unavailable {
            op: "sqlite.subscribe_lease_history",
        })?,
    };

    let stream = outbox_cursor::spawn(OutboxCursorConfig {
        pool,
        select_sql: SELECT_LEASE_EVENTS_SQL,
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
    inner: OutboxCursorStream<LeaseHistoryEvent>,
}

impl Stream for TypedStream {
    type Item = Result<LeaseHistoryEvent, EngineError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

fn extract_event_id(row: &sqlx::sqlite::SqliteRow) -> Result<i64, EngineError> {
    row.try_get("event_id")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_lease_event.event_id: {e}"),
        })
}

fn make_decoder(filter: ScannerFilter) -> RowDecoder<LeaseHistoryEvent> {
    Box::new(move |row| decode_row(row, &filter))
}

fn decode_row(
    row: &sqlx::sqlite::SqliteRow,
    filter: &ScannerFilter,
) -> Result<Option<LeaseHistoryEvent>, EngineError> {
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
                "sqlite.lease_history: skipping row with unparseable execution_id"
            );
            return Ok(None);
        }
    };
    let execution_id = ExecutionId::parse(&format!("{{fp:{partition_key}}}:{exec_uuid}"))
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_lease_event.execution_id: {e}"),
        })?;

    let lease_id_str: Option<String> = row
        .try_get::<Option<String>, _>("lease_id")
        .unwrap_or(None);
    let lease_id = lease_id_str
        .as_deref()
        .and_then(|s| LeaseId::parse(s).ok());

    let event_type: String = row
        .try_get("event_type")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("event_type: {e}"),
        })?;
    let occurred_at_ms: i64 =
        row.try_get("occurred_at_ms")
            .map_err(|e| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("occurred_at_ms: {e}"),
            })?;

    let cursor = encode_postgres_event_cursor(event_id);
    let at = TimestampMs::from_millis(occurred_at_ms);
    // SQLite outbox does not yet persist the owning worker instance
    // or the revoke source — mirror the PG reference's `None` /
    // `"operator"` defaults.
    let event = match event_type.as_str() {
        "acquired" => LeaseHistoryEvent::Acquired {
            cursor,
            execution_id,
            lease_id,
            worker_instance_id: None,
            at,
        },
        "renewed" => LeaseHistoryEvent::Renewed {
            cursor,
            execution_id,
            lease_id,
            worker_instance_id: None,
            at,
        },
        "expired" => LeaseHistoryEvent::Expired {
            cursor,
            execution_id,
            lease_id,
            prev_owner: None,
            at,
        },
        "reclaimed" => LeaseHistoryEvent::Reclaimed {
            cursor,
            execution_id,
            new_lease_id: lease_id,
            new_owner: None,
            at,
        },
        "revoked" => LeaseHistoryEvent::Revoked {
            cursor,
            execution_id,
            lease_id,
            revoked_by: "operator".to_string(),
            at,
        },
        other => {
            tracing::warn!(
                event_id = event_id,
                event_type = %other,
                "sqlite.lease_history: unknown event_type, skipping"
            );
            return Ok(None);
        }
    };
    Ok(Some(event))
}
