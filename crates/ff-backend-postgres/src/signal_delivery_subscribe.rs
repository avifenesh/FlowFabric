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

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use ff_core::stream_events::{SignalDeliveryEffect, SignalDeliveryEvent, SignalDeliverySubscription};
use ff_core::stream_subscribe::{
    decode_postgres_event_cursor, encode_postgres_event_cursor, StreamCursor,
};
use ff_core::types::{ExecutionId, SignalId, TimestampMs, WaitpointId};
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
    filter: ScannerFilter,
) -> Result<SignalDeliverySubscription, EngineError> {
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

    let (tx, rx) = mpsc::channel::<Result<SignalDeliveryEvent, EngineError>>(STREAM_CAPACITY);
    let pool_clone = pool.clone();
    tokio::spawn(subscriber_loop(pool_clone, partition_key, tx, last_seen, filter));

    Ok(Box::pin(Adapter { rx }))
}

struct Adapter {
    rx: mpsc::Receiver<Result<SignalDeliveryEvent, EngineError>>,
}

impl Stream for Adapter {
    type Item = Result<SignalDeliveryEvent, EngineError>;

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
    tx: mpsc::Sender<Result<SignalDeliveryEvent, EngineError>>,
    mut last_seen: i64,
    filter: ScannerFilter,
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

        if !replay(&pool, partition_key, &tx, &mut last_seen, &filter).await {
            return;
        }

        loop {
            tokio::select! {
                _ = tx.closed() => return,
                res = listener.recv() => {
                    match res {
                        Ok(_notif) => {
                            if !replay(&pool, partition_key, &tx, &mut last_seen, &filter).await {
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
    tx: &mpsc::Sender<Result<SignalDeliveryEvent, EngineError>>,
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
    tx: &mpsc::Sender<Result<SignalDeliveryEvent, EngineError>>,
    last_seen: &mut i64,
    filter: &ScannerFilter,
) -> bool {
    loop {
        // #282 — unfiltered SELECT; `ScannerFilter` gates in-memory via
        // the denormalised columns (see sibling comment in
        // `lease_event_subscribe::replay`).
        let rows = match sqlx::query(
            "SELECT event_id, execution_id, signal_id, waitpoint_id, source_identity, \
                    delivered_at_ms, partition_key, namespace, instance_tag \
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
            // #282 — denormalised filter columns (migration 0009).
            let namespace: Option<String> =
                row.try_get::<Option<String>, _>("namespace").unwrap_or(None);
            let instance_tag: Option<String> = row
                .try_get::<Option<String>, _>("instance_tag")
                .unwrap_or(None);

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

            // #282 — apply filter after `last_seen` is advanced so
            // dropped rows don't re-appear on the next replay pass.
            if !crate::lease_event_subscribe::passes_filter(
                filter,
                namespace.as_deref(),
                instance_tag.as_deref(),
            ) {
                continue;
            }

            let cursor = encode_postgres_event_cursor(decoded.event_id);

            let execution_id = match Uuid::parse_str(&decoded.execution_id)
                .ok()
                .and_then(|uuid| {
                    ExecutionId::parse(&format!(
                        "{{fp:{}}}:{}",
                        decoded.partition_key, uuid
                    ))
                    .ok()
                }) {
                Some(eid) => eid,
                None => {
                    tracing::warn!(
                        execution_id = %decoded.execution_id,
                        event_id = decoded.event_id,
                        "pg.signal_delivery.replay: skipping row with unparseable execution_id"
                    );
                    continue;
                }
            };

            let signal_id = match SignalId::parse(&decoded.signal_id) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        event_id = decoded.event_id,
                        "pg.signal_delivery.replay: skipping row with bad signal_id"
                    );
                    continue;
                }
            };
            let waitpoint_id = decoded
                .waitpoint_id
                .as_deref()
                .and_then(|s| WaitpointId::parse(s).ok());

            // PG outbox does not yet persist the delivery effect or a
            // dedup flag; treat every delivered row as `Satisfied`.
            // When the outbox gains an `effect` column, map here.
            let event = SignalDeliveryEvent::new(
                cursor,
                execution_id,
                signal_id,
                waitpoint_id,
                decoded.source_identity,
                SignalDeliveryEffect::Satisfied,
                TimestampMs::from_millis(decoded.delivered_at_ms),
            );
            if tx.send(Ok(event)).await.is_err() {
                return false;
            }
        }
    }
}
