//! RFC-019 Stage B — `subscribe_lease_history` on Postgres.
//!
//! Mirror of [`crate::completion::subscribe`] but against the
//! `ff_lease_event` outbox + `pg_notify('ff_lease_event', ...)`
//! trigger. See `migrations/0006_lease_event_outbox.sql`.
//!
//! Cursor encoding: `POSTGRES_CURSOR_PREFIX (0x02)` + `event_id`
//! (i64, big-endian). Resume-from-cursor is fully plumbed — the
//! subscribe call decodes the caller's cursor, catch-up replays
//! rows strictly after it, and steady-state NOTIFY wakes re-run
//! the catch-up query.
//!
//! Partition scope: one subscription tails the backend's configured
//! partition. Cross-partition consumers instantiate one backend
//! per partition and merge streams consumer-side (RFC-019
//! §Backend Semantics).

use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use ff_core::stream_events::{LeaseHistoryEvent, LeaseHistorySubscription};
use ff_core::stream_subscribe::{
    decode_postgres_event_cursor, encode_postgres_event_cursor, StreamCursor,
};
use ff_core::types::{ExecutionId, LeaseId, TimestampMs};
use futures_core::Stream;
use sqlx::postgres::{PgListener, PgPool};
use sqlx::Row;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Channel fired by `ff_notify_lease_event()` on every
/// `ff_lease_event` INSERT.
pub const LEASE_EVENT_CHANNEL: &str = "ff_lease_event";

/// Bounded fan-out capacity (matches `completion::STREAM_CAPACITY`).
const STREAM_CAPACITY: usize = 1024;

/// Max rows pulled per wake.
const REPLAY_BATCH: i64 = 256;

/// Reconnect backoff when the LISTEN connection drops.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// #282 — in-memory `ScannerFilter` check over an outbox row's
/// denormalised `namespace` + `instance_tag` columns. Namespace
/// compares exact; `instance_tag` compares the caller-supplied VALUE
/// (the key half of the tuple is denormalisation-choice at write time,
/// not a query-time input — matches the Valkey/dependency-reconciler
/// convention). NULL columns never match a non-None filter dimension.
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

struct LeaseEventRow {
    event_id: i64,
    execution_id: String,
    lease_id: Option<String>,
    event_type: String,
    occurred_at_ms: i64,
    partition_key: i32,
}

/// Subscribe to `ff_lease_event` rows strictly after `cursor` for the
/// given partition. Empty cursor tails from `max(event_id)` at
/// subscribe time.
pub(crate) async fn subscribe(
    pool: &PgPool,
    partition_key: i16,
    cursor: StreamCursor,
    filter: ScannerFilter,
) -> Result<LeaseHistorySubscription, EngineError> {
    // Decode + validate the caller's cursor before spawning so
    // malformed cursors fail loudly at subscribe time.
    let start = decode_postgres_event_cursor(&cursor).map_err(|msg| {
        EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: msg.to_string(),
        }
    })?;

    // Empty cursor → tail-from-now (resolve max at subscribe time so
    // early committers do not slip between subscribe + LISTEN).
    let last_seen: i64 = match start {
        Some(v) => v,
        None => sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_id), 0) FROM ff_lease_event WHERE partition_key = $1",
        )
        .bind(i32::from(partition_key))
        .fetch_one(pool)
        .await
        .map_err(|_| EngineError::Unavailable {
            op: "pg.subscribe_lease_history",
        })?,
    };

    let (tx, rx) = mpsc::channel::<Result<LeaseHistoryEvent, EngineError>>(STREAM_CAPACITY);
    let pool_clone = pool.clone();
    tokio::spawn(subscriber_loop(
        pool_clone,
        partition_key,
        tx,
        last_seen,
        filter,
    ));

    Ok(Box::pin(Adapter { rx }))
}

/// `mpsc::Receiver` → `Stream` adapter.
struct Adapter {
    rx: mpsc::Receiver<Result<LeaseHistoryEvent, EngineError>>,
}

impl Stream for Adapter {
    type Item = Result<LeaseHistoryEvent, EngineError>;

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
    tx: mpsc::Sender<Result<LeaseHistoryEvent, EngineError>>,
    mut last_seen: i64,
    filter: ScannerFilter,
) {
    loop {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "pg.lease_history.subscribe: PgListener::connect_with failed; retrying"
                );
                if wait_or_exit(&tx, RECONNECT_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        if let Err(e) = listener.listen(LEASE_EVENT_CHANNEL).await {
            tracing::warn!(
                error = %e,
                "pg.lease_history.subscribe: LISTEN ff_lease_event failed; retrying"
            );
            if wait_or_exit(&tx, RECONNECT_BACKOFF).await {
                return;
            }
            continue;
        }

        // Catch-up replay.
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
                                "pg.lease_history.subscribe: listener.recv() error; reconnecting"
                            );
                            // Surface a non-terminal disconnect notice
                            // carrying the current cursor so the
                            // consumer can choose to re-subscribe.
                            let _ = tx
                                .send(Err(EngineError::StreamDisconnected {
                                    cursor: encode_postgres_event_cursor(last_seen),
                                }))
                                .await;
                            break; // outer loop rebuilds the listener
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
    tx: &mpsc::Sender<Result<LeaseHistoryEvent, EngineError>>,
    d: Duration,
) -> bool {
    tokio::select! {
        _ = tx.closed() => true,
        _ = tokio::time::sleep(d) => false,
    }
}

/// Drain rows above `last_seen`; forward each as a typed
/// `LeaseHistoryEvent`. Returns `false` iff the consumer dropped
/// the subscription.
async fn replay(
    pool: &PgPool,
    partition_key: i16,
    tx: &mpsc::Sender<Result<LeaseHistoryEvent, EngineError>>,
    last_seen: &mut i64,
    filter: &ScannerFilter,
) -> bool {
    // #282 — read the denormalised filter columns on every row so the
    // in-memory `ScannerFilter::matches`-shaped check can admit / drop
    // without a per-event RTT. SELECT is unfiltered (matches the
    // `completion::replay` pattern + keeps `last_seen` advancing past
    // dropped rows) — the bandwidth cost of two extra TEXT columns is
    // dwarfed by the round-trip savings of not re-querying skipped
    // rows on every LISTEN wake.
    loop {
        let rows = match sqlx::query(
            "SELECT event_id, execution_id, lease_id, event_type, occurred_at_ms, \
                    partition_key, namespace, instance_tag \
             FROM ff_lease_event \
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
                tracing::warn!(error = %e, "pg.lease_history.replay: query failed");
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
            let lease_id: Option<String> =
                row.try_get::<Option<String>, _>("lease_id").unwrap_or(None);
            let Ok(event_type) = row.try_get::<String, _>("event_type") else {
                continue;
            };
            let Ok(occurred_at_ms) = row.try_get::<i64, _>("occurred_at_ms") else {
                continue;
            };
            let Ok(partition_key) = row.try_get::<i32, _>("partition_key") else {
                continue;
            };
            // #282 — denormalised filter columns (added in migration
            // 0008). NULL-safe via `Option<String>`; unfiltered
            // subscribers bypass the compare entirely.
            let namespace: Option<String> =
                row.try_get::<Option<String>, _>("namespace").unwrap_or(None);
            let instance_tag: Option<String> = row
                .try_get::<Option<String>, _>("instance_tag")
                .unwrap_or(None);

            let decoded = LeaseEventRow {
                event_id,
                execution_id,
                lease_id,
                event_type,
                occurred_at_ms,
                partition_key,
            };
            *last_seen = decoded.event_id;

            // #282 — apply `ScannerFilter` inline. Match semantics
            // mirror the Valkey `FilterGate`: namespace equality +
            // instance-tag-value equality. A NULL-column row on a
            // non-noop filter is silently dropped (matches the
            // "filtered subscribers silently drop NULL-column rows"
            // invariant documented in migration 0008).
            if !passes_filter(filter, namespace.as_deref(), instance_tag.as_deref()) {
                continue;
            }

            let cursor = encode_postgres_event_cursor(decoded.event_id);

            // Re-attach the `{fp:N}:<uuid>` hash-tag so the inline
            // `execution_id` matches what Valkey would emit. Rows
            // without a parseable UUID are defensively skipped —
            // producers always write a UUID, so a bad row is a
            // schema corruption surface, not a skip.
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
                        "pg.lease_history.replay: skipping row with unparseable execution_id"
                    );
                    continue;
                }
            };

            let lease_id = decoded
                .lease_id
                .as_deref()
                .and_then(|s| LeaseId::parse(s).ok());
            let at = TimestampMs::from_millis(decoded.occurred_at_ms);

            // PG outbox does not persist the owning worker instance —
            // the fence triple is rebuilt from attempt rows at lookup
            // time. Surface `None` for now; consumers who need it go
            // through `read_execution_state`. `lease_id` is also
            // typically `None` on PG since `ff_attempt` identity is
            // `(lease_epoch, attempt_index, execution_id)` rather than
            // a stable uuid (see `lease_event::emit`).
            let event = match decoded.event_type.as_str() {
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
                    // PG outbox does not yet persist the revoke source;
                    // match the Valkey default. Taxonomy typed-up when
                    // the outbox schema gains a `revoked_by` column.
                    revoked_by: "operator".to_string(),
                    at,
                },
                other => {
                    tracing::warn!(
                        event_id = decoded.event_id,
                        event_type = %other,
                        "pg.lease_history.replay: unknown event_type, skipping"
                    );
                    continue;
                }
            };

            if tx.send(Ok(event)).await.is_err() {
                return false; // consumer dropped
            }
        }
    }
}
