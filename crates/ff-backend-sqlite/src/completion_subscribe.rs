//! RFC-023 Phase 3.1 — `subscribe_completion` on SQLite.
//!
//! Tails the `ff_completion_event` outbox via the Phase 2b.2.2
//! [`OutboxCursorReader`](crate::outbox_cursor) primitive: catch-up
//! SELECT on subscribe, park on the `completion` broadcast channel
//! for post-commit wakeups, re-SELECT on each wake. Mirrors
//! `ff-backend-postgres/src/lib.rs::subscribe_completion` semantics
//! (catch-up → NOTIFY-wake replay loop) but swaps `LISTEN/NOTIFY`
//! for the in-process broadcast fan-out defined in
//! [`crate::pubsub`].
//!
//! # Cursor encoding
//!
//! Reuses the Postgres family prefix (`0x02 ++ event_id(BE8)`) —
//! per the RFC-023 autonomy note the SQLite event_id is i64 like PG,
//! so sharing the codec keeps the cursor wire-stable.
//!
//! # Filter application
//!
//! `ScannerFilter` is evaluated in the row decoder (mirror of
//! `ff-backend-postgres/src/lease_event_subscribe::passes_filter`):
//! the outbox stores denormalised `namespace` + `instance_tag`
//! columns; NULL columns never match a non-None filter dimension.
//! This matches the "filtered subscribers silently drop NULL-column
//! rows" invariant documented on migration 0008/0009.
//!
//! Note: the current SQLite `INSERT_COMPLETION_EVENT_SQL` writes
//! NULL for both filter columns on the completion path (see
//! `queries/attempt.rs`), so on a non-noop filter every completion
//! row is dropped today. That mirrors the behaviour any PG
//! deployment would see before the outbox is taught to populate
//! those columns. Populating the columns is producer-side work
//! outside Phase 3.1 scope.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use sqlx::Row;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use uuid::Uuid;

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::stream_events::{CompletionEvent, CompletionOutcome, CompletionSubscription};
use ff_core::stream_subscribe::{
    StreamCursor, decode_postgres_event_cursor, encode_postgres_event_cursor,
};
use ff_core::types::{ExecutionId, TimestampMs};

use crate::outbox_cursor::{self, OutboxCursorConfig, OutboxCursorStream, RowDecoder};
use crate::pubsub::OutboxEvent;

/// Rows above `partition_key = ?1 AND event_id > ?2`, ASC, limited.
/// Reads the denormalised filter columns so the row decoder can apply
/// [`ScannerFilter`] without a per-row RTT.
pub(crate) const SELECT_COMPLETION_EVENTS_SQL: &str = r#"
    SELECT event_id, execution_id, outcome, occurred_at_ms,
           partition_key, namespace, instance_tag
      FROM ff_completion_event
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
) -> Result<CompletionSubscription, EngineError> {
    let start = decode_postgres_event_cursor(&cursor).map_err(|msg| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: msg.to_string(),
    })?;

    // Empty cursor → tail from `max(event_id)` at subscribe time. This
    // matches the PG `subscribe_completion` contract: a fresh
    // subscription observes only events committed after the call
    // returns, not historical events. A concrete cursor from a prior
    // session replays strictly above that position.
    let last_seen: i64 = match start {
        Some(v) => v,
        None => sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_id), 0) FROM ff_completion_event \
             WHERE partition_key = ?1",
        )
        .bind(PARTITION_KEY)
        .fetch_one(&pool)
        .await
        .map_err(|_| EngineError::Unavailable {
            op: "sqlite.subscribe_completion",
        })?,
    };

    let stream = outbox_cursor::spawn(OutboxCursorConfig {
        pool,
        select_sql: SELECT_COMPLETION_EVENTS_SQL,
        partition_key: PARTITION_KEY,
        cursor: last_seen,
        batch_size: REPLAY_BATCH,
        wakeup,
        decoder: make_decoder(filter),
        row_event_id: extract_event_id,
    });

    Ok(Box::pin(TypedStream { inner: stream }))
}

/// Wrap the untyped [`OutboxCursorStream`] in the
/// `CompletionSubscription` return alias.
struct TypedStream {
    inner: OutboxCursorStream<CompletionEvent>,
}

impl Stream for TypedStream {
    type Item = Result<CompletionEvent, EngineError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

fn extract_event_id(row: &sqlx::sqlite::SqliteRow) -> Result<i64, EngineError> {
    row.try_get("event_id")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_completion_event.event_id: {e}"),
        })
}

fn make_decoder(filter: ScannerFilter) -> RowDecoder<CompletionEvent> {
    Box::new(move |row| decode_row(row, &filter))
}

fn decode_row(
    row: &sqlx::sqlite::SqliteRow,
    filter: &ScannerFilter,
) -> Result<Option<CompletionEvent>, EngineError> {
    let event_id: i64 = row
        .try_get("event_id")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("event_id: {e}"),
        })?;

    // Denormalised filter columns. NULL never matches a non-None
    // filter dimension — parity with PG + Valkey.
    let namespace: Option<String> = row
        .try_get::<Option<String>, _>("namespace")
        .unwrap_or(None);
    let instance_tag: Option<String> = row
        .try_get::<Option<String>, _>("instance_tag")
        .unwrap_or(None);
    if !passes_filter(filter, namespace.as_deref(), instance_tag.as_deref()) {
        return Ok(None);
    }

    // Execution id is stored as a 16-byte BLOB in SQLite (diverges from
    // PG's `Uuid`). Re-attach the `{fp:N}:<uuid>` hash-tag so downstream
    // consumers see the canonical shape.
    let partition_key: i64 = row
        .try_get("partition_key")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("partition_key: {e}"),
        })?;
    let exec_uuid: Uuid = row
        .try_get("execution_id")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("execution_id: {e}"),
        })?;
    let execution_id = ExecutionId::parse(&format!("{{fp:{partition_key}}}:{exec_uuid}"))
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_completion_event.execution_id: {e}"),
        })?;

    let outcome: String = row.try_get("outcome").map_err(|e| EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: format!("outcome: {e}"),
    })?;
    let occurred_at_ms: i64 = row
        .try_get("occurred_at_ms")
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("occurred_at_ms: {e}"),
        })?;

    let cursor = encode_postgres_event_cursor(event_id);
    Ok(Some(CompletionEvent::new(
        cursor,
        execution_id,
        CompletionOutcome::from_wire(&outcome),
        TimestampMs::from_millis(occurred_at_ms),
    )))
}

/// Shared filter predicate — mirror of
/// `ff-backend-postgres/src/lease_event_subscribe::passes_filter`.
pub(crate) fn passes_filter(
    filter: &ScannerFilter,
    row_namespace: Option<&str>,
    row_instance_tag: Option<&str>,
) -> bool {
    if let Some(ref want_ns) = filter.namespace {
        match row_namespace {
            Some(have) if have == want_ns.as_str() => {}
            _ => return false,
        }
    }
    if let Some((_, ref want_value)) = filter.instance_tag {
        match row_instance_tag {
            Some(have) if have == want_value.as_str() => {}
            _ => return false,
        }
    }
    true
}
