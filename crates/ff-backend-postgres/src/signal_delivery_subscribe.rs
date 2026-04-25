//! RFC-019 Stage B — `subscribe_signal_delivery` on Postgres.
//!
//! Mirror of [`crate::lease_event_subscribe::subscribe`] but against
//! the `ff_signal_event` outbox + `pg_notify('ff_signal_event', ...)`
//! trigger. See `migrations/0007_signal_event_outbox.sql`.
//!
//! Cursor encoding: `POSTGRES_CURSOR_PREFIX (0x02)` + `event_id`
//! (i64, big-endian) — identical to the lease-history subscriber.
//!
//! Partition scope: one subscription tails the backend's configured
//! partition. Cross-partition consumers instantiate one backend per
//! partition and merge streams consumer-side (RFC-019 §Backend
//! Semantics).

use std::time::Duration;

use ff_core::engine_error::EngineError;
use ff_core::stream_subscribe::{
    decode_postgres_event_cursor, encode_postgres_event_cursor, StreamCursor, StreamEvent,
    StreamFamily, StreamSubscription,
};
use ff_core::types::{ExecutionId, TimestampMs};
use futures_core::Stream;
use sqlx::postgres::{PgListener, PgPool};
use sqlx::Row;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Channel fired by `ff_notify_signal_event()` on every
/// `ff_signal_event` INSERT.
pub const SIGNAL_EVENT_CHANNEL: &str = "ff_signal_event";

/// Bounded fan-out capacity (matches `lease_event_subscribe`).
const STREAM_CAPACITY: usize = 1024;

/// Max rows pulled per wake.
const REPLAY_BATCH: i64 = 256;

/// Reconnect backoff when the LISTEN connection drops.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

struct SignalEventRow {
    event_id: i64,
    execution_id: String,
    signal_id: String,
    waitpoint_id: Option<String>,
    source_identity: Option<String>,
    delivered_at_ms: i64,
    partition_key: i32,
}

/// Subscribe to `ff_signal_event` rows strictly after `cursor` for the
/// given partition. Empty cursor tails from `max(event_id)` at
/// subscribe time.
pub(crate) async fn subscribe(
    pool: &PgPool,
    partition_key: i16,
    cursor: StreamCursor,
) -> Result<StreamSubscription, EngineError> {
    let start = decode_postgres_event_cursor(&cursor).map_err(|msg| {
        EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: msg.to_string(),
        }
    })?;

    let last_seen: i64 = match start {
        Some(v) => v,
        None => sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_id), 0) FROM ff_signal_event WHERE partition_key = $1",
        )
        .bind(i32::from(partition_key))
        .fetch_one(pool)
        .await
        .map_err(|_| EngineError::Unavailable {
            op: "pg.subscribe_signal_delivery",
        })?,
    };

    let (tx, rx) = mpsc::channel::<Result<StreamEvent, EngineError>>(STREAM_CAPACITY);
    let pool_clone = pool.clone();
    tokio::spawn(subscriber_loop(pool_clone, partition_key, tx, last_seen));

    Ok(Box::pin(Adapter { rx }))
}

struct Adapter {
    rx: mpsc::Receiver<Result<StreamEvent, EngineError>>,
}

impl Stream for Adapter {
    type Item = Result<StreamEvent, EngineError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

async fn subscriber_loop(
    pool: PgPool,
    partition_key: i16,
    tx: mpsc::Sender<Result<StreamEvent, EngineError>>,
    mut last_seen: i64,
) {
    loop {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "pg.signal_delivery.subscribe: PgListener::connect_with failed; retrying"
                );
                if wait_or_exit(&tx, RECONNECT_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        if let Err(e) = listener.listen(SIGNAL_EVENT_CHANNEL).await {
            tracing::warn!(
                error = %e,
                "pg.signal_delivery.subscribe: LISTEN ff_signal_event failed; retrying"
            );
            if wait_or_exit(&tx, RECONNECT_BACKOFF).await {
                return;
            }
            continue;
        }

        if !replay(&pool, partition_key, &tx, &mut last_seen).await {
            return;
        }

        loop {
            tokio::select! {
                _ = tx.closed() => return,
                res = listener.recv() => {
                    match res {
                        Ok(_notif) => {
                            if !replay(&pool, partition_key, &tx, &mut last_seen).await {
                                return;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "pg.signal_delivery.subscribe: listener.recv() error; reconnecting"
                            );
                            let _ = tx
                                .send(Err(EngineError::StreamDisconnected {
                                    cursor: encode_postgres_event_cursor(last_seen),
                                }))
                                .await;
                            break;
                        }
                    }
                }
            }
        }

        if wait_or_exit(&tx, RECONNECT_BACKOFF).await {
            return;
        }
    }
}

async fn wait_or_exit(
    tx: &mpsc::Sender<Result<StreamEvent, EngineError>>,
    d: Duration,
) -> bool {
    tokio::select! {
        _ = tx.closed() => true,
        _ = tokio::time::sleep(d) => false,
    }
}

async fn replay(
    pool: &PgPool,
    partition_key: i16,
    tx: &mpsc::Sender<Result<StreamEvent, EngineError>>,
    last_seen: &mut i64,
) -> bool {
    loop {
        let rows = match sqlx::query(
            "SELECT event_id, execution_id, signal_id, waitpoint_id, source_identity, \
                    delivered_at_ms, partition_key \
             FROM ff_signal_event \
             WHERE partition_key = $1 AND event_id > $2 \
             ORDER BY event_id ASC \
             LIMIT $3",
        )
        .bind(i32::from(partition_key))
        .bind(*last_seen)
        .bind(REPLAY_BATCH)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "pg.signal_delivery.replay: query failed");
                return !tx.is_closed();
            }
        };

        if rows.is_empty() {
            return !tx.is_closed();
        }

        for row in rows {
            let Ok(event_id) = row.try_get::<i64, _>("event_id") else {
                continue;
            };
            let Ok(execution_id) = row.try_get::<String, _>("execution_id") else {
                continue;
            };
            let Ok(signal_id) = row.try_get::<String, _>("signal_id") else {
                continue;
            };
            let waitpoint_id: Option<String> = row
                .try_get::<Option<String>, _>("waitpoint_id")
                .unwrap_or(None);
            let source_identity: Option<String> = row
                .try_get::<Option<String>, _>("source_identity")
                .unwrap_or(None);
            let Ok(delivered_at_ms) = row.try_get::<i64, _>("delivered_at_ms") else {
                continue;
            };
            let Ok(partition_key) = row.try_get::<i32, _>("partition_key") else {
                continue;
            };

            let decoded = SignalEventRow {
                event_id,
                execution_id,
                signal_id,
                waitpoint_id,
                source_identity,
                delivered_at_ms,
                partition_key,
            };
            *last_seen = decoded.event_id;

            let payload = serde_json::json!({
                "execution_id": decoded.execution_id,
                "signal_id": decoded.signal_id,
                "waitpoint_id": decoded.waitpoint_id,
                "source_identity": decoded.source_identity,
                "delivered_at_ms": decoded.delivered_at_ms,
                "partition_key": decoded.partition_key,
            });
            let payload_bytes = match serde_json::to_vec(&payload) {
                Ok(b) => bytes::Bytes::from(b),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "pg.signal_delivery.replay: payload serialize failed"
                    );
                    continue;
                }
            };

            let cursor = encode_postgres_event_cursor(decoded.event_id);
            let mut event = StreamEvent::new(
                StreamFamily::SignalDelivery,
                cursor,
                TimestampMs::from_millis(decoded.delivered_at_ms),
                payload_bytes,
            );
            if let Ok(uuid) = Uuid::parse_str(&decoded.execution_id) {
                let full = format!("{{fp:{}}}:{}", decoded.partition_key, uuid);
                if let Ok(eid) = ExecutionId::parse(&full) {
                    event = event.with_execution_id(eid);
                }
            }
            if tx.send(Ok(event)).await.is_err() {
                return false;
            }
        }
    }
}
