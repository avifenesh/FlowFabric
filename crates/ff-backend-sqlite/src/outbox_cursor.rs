//! RFC-023 Phase 2b.2.2 — generic outbox cursor-resume + broadcast-wakeup
//! reader primitive.
//!
//! Phase 3 `subscribe_completion` / `subscribe_lease_history` /
//! `subscribe_signal_delivery` / `subscribe_stream_frame` /
//! `subscribe_operator_event` all share the same shape: tail one outbox
//! table by monotonic `event_id`, park on a
//! [`tokio::sync::broadcast::Receiver`] for wakeup, re-select since the
//! last cursor on wake, emit typed rows downstream.
//!
//! This module factors that shape into one reusable helper. The first
//! consumer is in-crate today — `crate::backend::tail_stream_impl` wakes
//! on the `stream_frame` broadcast channel (proving the primitive is
//! sound against a real producer). Phase 3 `subscribe_*` trait-impl
//! bodies become thin row-decoder shells around [`OutboxCursorReader`].
//!
//! # Design shape
//!
//! Passing the row-decoder in as a `Fn`-closure keeps the primitive
//! free of a trait-bound zoo — each outbox table has a different
//! column set (`ff_stream_frame` has `ts_ms/seq/fields`,
//! `ff_lease_event` has `event_type/occurred_at_ms/lease_id`, etc.),
//! so a shared `OutboxRow` trait would either bloat into one
//! monomorph per table anyway, or paper over the column divergence
//! with a `HashMap<String, Value>`-style bag. A closure is cheaper:
//! the primitive owns cursor + broadcast + pool plumbing; the caller
//! owns row-typing.
//!
//! # Ordering + catch-up invariants (RFC-023 §4.2 A2)
//!
//! 1. **Outbox rows land INSIDE the producer's transaction** (see
//!    `backend::dispatch_pending_emits`). This reader can therefore
//!    treat the table as the durable record of "what committed"; the
//!    broadcast is wakeup-only.
//! 2. **Cursor-resume across disconnects.** On subscribe, the reader
//!    first does a catch-up SELECT for rows strictly after the
//!    caller-supplied cursor, then attaches the broadcast receiver.
//!    Between those two steps, any new producer write goes onto the
//!    broadcast ring — so the wake fires, we re-SELECT, and catch the
//!    row via its outbox id. No event is lost to the subscribe-race.
//! 3. **Lagged broadcast recovery.** If the broadcast ring overflows
//!    before the consumer polls ([`tokio::sync::broadcast::error::RecvError::Lagged`]),
//!    the reader falls back to a plain cursor-select. The outbox is
//!    the durable record, so Lagged is harmless — just extra polls
//!    on the catch-up side. A `Closed` receiver (producer dropped)
//!    is taken as a shutdown signal.
//! 4. **Clean shutdown.** When the consumer drops the returned
//!    `Stream`, the spawned background task exits on the next iter
//!    (mpsc `send` returns Err; pool drop also surfaces as SELECT
//!    error, handled as terminal).
//!
//! The primitive is consumed in earnest by Phase 3 `subscribe_*`
//! trait impls (completion / lease-history / signal-delivery /
//! stream-frame / operator-event). Integration tests at
//! `crates/ff-backend-sqlite/tests/outbox_cursor.rs` exercise the
//! primitive end-to-end against a real `stream_frame` producer; until
//! Phase 3 lands, the `pub(crate)` items below are only hit by those
//! tests, hence the module-level `dead_code` allow. The surface is
//! deliberate, not aspirational.
#![allow(dead_code)]

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc};

use ff_core::engine_error::EngineError;

use crate::pubsub::OutboxEvent;

/// Bounded fan-out capacity for the Stream adapter. Matches the PG
/// reference (`lease_event_subscribe::STREAM_CAPACITY = 1024`) so
/// cross-backend backpressure behaves the same.
const STREAM_CAPACITY: usize = 1024;

/// Decoder closure signature. Given one SQLite row from the outbox
/// table, returns either a typed emit or a soft-skip (row was shaped
/// correctly but doesn't match a filter predicate). Hard decode errors
/// propagate as `EngineError` and surface on the stream.
pub(crate) type RowDecoder<T> =
    Box<dyn Fn(&sqlx::sqlite::SqliteRow) -> Result<Option<T>, EngineError> + Send + Sync>;

/// Configuration for one cursor-resume reader.
pub(crate) struct OutboxCursorConfig<T> {
    /// Pool to SELECT from.
    pub pool: SqlitePool,
    /// SQL to run on each wake; binds `?1=partition_key`, `?2=cursor`,
    /// `?3=batch_size`. Must `ORDER BY` the outbox's monotonic cursor
    /// column ASCending. Callers source these from `queries::stream`
    /// and friends.
    pub select_sql: &'static str,
    /// Partition scope — each subscription tails one partition. Under
    /// single-writer SQLite this is always `0`, but the field exists
    /// for topology symmetry.
    pub partition_key: i64,
    /// Monotonic cursor watermark. The first catch-up SELECT returns
    /// rows with `event_id > cursor`.
    pub cursor: i64,
    /// Per-wake row budget. Matches PG's `REPLAY_BATCH = 256`.
    pub batch_size: i64,
    /// Broadcast receiver for the matching channel. Lagged → fall back
    /// to cursor-select; Closed → shutdown.
    pub wakeup: broadcast::Receiver<OutboxEvent>,
    /// Row decoder; see [`RowDecoder`].
    pub decoder: RowDecoder<T>,
    /// Extractor returning the outbox `event_id` for cursor advancement.
    /// Kept separate from the decoder because a soft-skip still needs
    /// to bump the cursor past the skipped row (otherwise a filtered
    /// subscriber wakes on the same row forever).
    pub row_event_id: fn(&sqlx::sqlite::SqliteRow) -> Result<i64, EngineError>,
}

/// Readonly handle returned to the trait-impl consumer. Wraps an
/// mpsc receiver so dropping the handle cleanly terminates the
/// spawned subscriber task.
pub(crate) struct OutboxCursorStream<T> {
    rx: mpsc::Receiver<Result<T, EngineError>>,
}

impl<T> Stream for OutboxCursorStream<T> {
    type Item = Result<T, EngineError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Spawn one cursor-resume reader and return its `Stream` adapter. The
/// spawned task exits when:
///  * the consumer drops the returned stream (`mpsc::Sender::send` → Err),
///  * the broadcast channel is `Closed` (producer dropped), or
///  * a non-transient SELECT error on a pool that will not recover.
pub(crate) fn spawn<T>(config: OutboxCursorConfig<T>) -> OutboxCursorStream<T>
where
    T: Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<T, EngineError>>(STREAM_CAPACITY);
    tokio::spawn(reader_loop(config, tx));
    OutboxCursorStream { rx }
}

async fn reader_loop<T>(mut config: OutboxCursorConfig<T>, tx: mpsc::Sender<Result<T, EngineError>>)
where
    T: Send + 'static,
{
    // Catch-up before parking on broadcast — matches the PG
    // `replay → listen` handshake. A new broadcast tick that arrived
    // between `subscribe()` and this first SELECT is still captured
    // because the tick fires AFTER the producer's tx.commit(), and the
    // SELECT runs after `tx.commit()` is visible.
    if !replay(&mut config, &tx).await {
        return;
    }

    loop {
        if tx.is_closed() {
            return;
        }
        let wake = config.wakeup.recv().await;
        match wake {
            Ok(_ev) => {
                if !replay(&mut config, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // Broadcast ring overflowed. Outbox is durable so a
                // plain cursor-select catches every missed row; we only
                // lost wakeups, not events.
                tracing::debug!(
                    table.sql = config.select_sql,
                    skipped = skipped,
                    "sqlite.outbox_cursor: broadcast lagged; falling back to cursor-select"
                );
                if !replay(&mut config, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                // Producer side dropped the last broadcast sender —
                // under `SqliteBackend`'s lifecycle that means the
                // backend itself is dropping. Do one last catch-up to
                // flush any events that landed just before close.
                let _ = replay(&mut config, &tx).await;
                return;
            }
        }
    }
}

/// Drain every row strictly after `config.cursor`; emit via the mpsc
/// sender; advance the cursor past each row. Returns `false` iff the
/// consumer has dropped the stream (caller should exit).
async fn replay<T>(
    config: &mut OutboxCursorConfig<T>,
    tx: &mpsc::Sender<Result<T, EngineError>>,
) -> bool {
    loop {
        let rows = match sqlx::query(config.select_sql)
            .bind(config.partition_key)
            .bind(config.cursor)
            .bind(config.batch_size)
            .fetch_all(&config.pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                // Pool-error path: surface once on the stream, exit
                // the subscriber. Consumer can resubscribe with the
                // same cursor if they want to retry.
                let _ = tx.send(Err(crate::errors::map_sqlx_error(e))).await;
                return false;
            }
        };

        if rows.is_empty() {
            return !tx.is_closed();
        }

        for row in &rows {
            // Advance the cursor first so a soft-skip still moves the
            // watermark — otherwise a filtered row wakes us forever.
            let event_id = match (config.row_event_id)(row) {
                Ok(id) => id,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return false;
                }
            };
            config.cursor = event_id;

            match (config.decoder)(row) {
                Ok(Some(item)) => {
                    if tx.send(Ok(item)).await.is_err() {
                        return false; // consumer dropped
                    }
                }
                Ok(None) => {
                    // Soft-skip; cursor already advanced.
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the cursor-resume primitive. Exercised against a
    //! real `stream_frame` producer via `SqliteBackend::append_frame` so
    //! the wakeup + durable-replay interaction is end-to-end.

    use super::*;
    use crate::SqliteBackend;
    use crate::queries::stream as q_stream;
    use ff_core::backend::{CapabilitySet, ClaimPolicy, Frame, FrameKind};
    use ff_core::engine_backend::EngineBackend;
    use ff_core::types::{ExecutionId, LaneId, WorkerId, WorkerInstanceId};
    use serial_test::serial;
    use sqlx::Row;
    use std::sync::Arc;
    use std::time::Duration;
    use uuid::Uuid;

    async fn fresh_backend() -> Arc<SqliteBackend> {
        // SAFETY: test-only env mutation; every caller is
        // `#[serial(ff_dev_mode)]`.
        unsafe {
            std::env::set_var("FF_DEV_MODE", "1");
        }
        use std::time::{SystemTime, UNIX_EPOCH};
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tid = std::thread::current().id();
        let tag = format!("{ns}-{tid:?}").replace([':', ' '], "-");
        let uri = format!("file:rfc-023-outbox-cursor-{tag}?mode=memory&cache=shared");
        SqliteBackend::new(&uri).await.expect("construct backend")
    }

    async fn seed_and_claim(backend: &SqliteBackend) -> (ExecutionId, ff_core::backend::Handle) {
        let pool = backend.pool_for_test();
        let exec_uuid = Uuid::new_v4();
        let exec_id = ExecutionId::parse(&format!("{{fp:0}}:{exec_uuid}")).expect("exec id");
        sqlx::query(
            r#"
            INSERT INTO ff_exec_core (
                partition_key, execution_id, lane_id, attempt_index,
                lifecycle_phase, ownership_state, eligibility_state,
                public_state, attempt_state, priority, created_at_ms
            ) VALUES (0, ?1, 'default', 0,
                      'runnable', 'unowned', 'eligible_now',
                      'pending', 'initial', 0, 1)
            "#,
        )
        .bind(exec_uuid)
        .execute(pool)
        .await
        .expect("seed exec_core");
        let caps = CapabilitySet::new::<_, &str>([]);
        let h = backend
            .claim(
                &LaneId::new("default"),
                &caps,
                ClaimPolicy::new(
                    WorkerId::new("w"),
                    WorkerInstanceId::new("wi"),
                    30_000,
                    None,
                ),
            )
            .await
            .expect("claim")
            .expect("handle");
        (exec_id, h)
    }

    /// Tiny typed-event shape for the test. Mirrors the column subset
    /// Phase 3 `subscribe_stream_frame` will emit.
    #[derive(Debug, Clone)]
    struct TestFrameEvent {
        event_id: i64,
        ts_ms: i64,
        seq: i64,
        payload: String,
    }

    fn test_decoder() -> RowDecoder<TestFrameEvent> {
        Box::new(
            |row: &sqlx::sqlite::SqliteRow| -> Result<Option<TestFrameEvent>, EngineError> {
                let event_id: i64 =
                    row.try_get("event_id")
                        .map_err(|e| EngineError::Validation {
                            kind: ff_core::engine_error::ValidationKind::Corruption,
                            detail: format!("event_id: {e}"),
                        })?;
                let ts_ms: i64 = row.try_get("ts_ms").map_err(|e| EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::Corruption,
                    detail: format!("ts_ms: {e}"),
                })?;
                let seq: i64 = row.try_get("seq").map_err(|e| EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::Corruption,
                    detail: format!("seq: {e}"),
                })?;
                let fields_text: String = row.try_get("fields").unwrap_or_default();
                let payload: String = serde_json::from_str::<serde_json::Value>(&fields_text)
                    .ok()
                    .and_then(|v| {
                        v.get("payload")
                            .and_then(|p| p.as_str().map(ToOwned::to_owned))
                    })
                    .unwrap_or_default();
                Ok(Some(TestFrameEvent {
                    event_id,
                    ts_ms,
                    seq,
                    payload,
                }))
            },
        )
    }

    fn test_row_event_id(row: &sqlx::sqlite::SqliteRow) -> Result<i64, EngineError> {
        row.try_get("event_id")
            .map_err(|e| EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::Corruption,
                detail: format!("event_id: {e}"),
            })
    }

    /// Collect up to `n` items from a `Stream` with a timeout. Drops
    /// the stream when done so the subscriber task exits cleanly.
    async fn drain_n(
        mut stream: OutboxCursorStream<TestFrameEvent>,
        n: usize,
        timeout: Duration,
    ) -> Vec<TestFrameEvent> {
        use futures_core::stream::Stream as _;
        use std::future::poll_fn;
        use std::pin::Pin;

        let mut out = Vec::with_capacity(n);
        let deadline = tokio::time::Instant::now() + timeout;
        while out.len() < n {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                break;
            }
            let poll_once = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx));
            match tokio::time::timeout(remaining, poll_once).await {
                Ok(Some(Ok(ev))) => out.push(ev),
                Ok(Some(Err(e))) => panic!("stream yielded error: {e:?}"),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        out
    }

    #[tokio::test]
    #[serial(ff_dev_mode)]
    async fn outbox_cursor_catch_up_then_live() {
        let backend = fresh_backend().await;
        let (_eid, h) = seed_and_claim(&backend).await;

        // Write 2 frames BEFORE subscribing — proves cursor-resume
        // from the beginning catches the historical rows.
        backend
            .append_frame(&h, Frame::new(b"pre-1".to_vec(), FrameKind::Stdout))
            .await
            .expect("append pre-1");
        backend
            .append_frame(&h, Frame::new(b"pre-2".to_vec(), FrameKind::Stdout))
            .await
            .expect("append pre-2");

        // Subscribe from cursor = 0 so both pre-writes replay.
        let pool = backend.pool_for_test().clone();
        let rx = backend.stream_frame_receiver_for_test();
        let stream = spawn(OutboxCursorConfig {
            pool,
            select_sql: q_stream::OUTBOX_TAIL_STREAM_FRAME_SQL,
            partition_key: 0,
            cursor: 0,
            batch_size: 64,
            wakeup: rx,
            decoder: test_decoder(),
            row_event_id: test_row_event_id,
        });

        // Produce one LIVE frame that fires a broadcast wake.
        let backend2 = backend.clone();
        let h2 = h.clone();
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            backend2
                .append_frame(&h2, Frame::new(b"live".to_vec(), FrameKind::Stdout))
                .await
                .expect("append live");
        });

        let collected = drain_n(stream, 3, Duration::from_millis(2_000)).await;
        producer.await.expect("producer");
        assert_eq!(collected.len(), 3, "expected 3 events, got {collected:?}");
        let payloads: Vec<&str> = collected.iter().map(|e| e.payload.as_str()).collect();
        assert_eq!(payloads, vec!["pre-1", "pre-2", "live"]);

        // Cursors advance monotonically.
        for pair in collected.windows(2) {
            assert!(
                pair[1].event_id > pair[0].event_id,
                "event_id not monotonic: {:?}",
                pair
            );
        }
        // ts_ms/seq shape sanity.
        assert!(collected[0].ts_ms > 0);
        let _ = collected[0].seq; // just touch the field
    }

    #[tokio::test]
    #[serial(ff_dev_mode)]
    async fn outbox_cursor_resume_skips_seen() {
        let backend = fresh_backend().await;
        let (_eid, h) = seed_and_claim(&backend).await;

        // Produce 2 pre-frames, then subscribe at cursor = after the
        // first one so the reader only yields the 2nd pre-frame + any
        // subsequent frames.
        backend
            .append_frame(&h, Frame::new(b"one".to_vec(), FrameKind::Stdout))
            .await
            .expect("append 1");
        backend
            .append_frame(&h, Frame::new(b"two".to_vec(), FrameKind::Stdout))
            .await
            .expect("append 2");

        // Query the event_id of the first row so we can resume past it.
        let pool = backend.pool_for_test();
        let first_id: i64 =
            sqlx::query_scalar("SELECT MIN(_rowid_) FROM ff_stream_frame WHERE partition_key = 0")
                .fetch_one(pool)
                .await
                .expect("min rowid");

        let rx = backend.stream_frame_receiver_for_test();
        let stream = spawn(OutboxCursorConfig {
            pool: pool.clone(),
            select_sql: q_stream::OUTBOX_TAIL_STREAM_FRAME_SQL,
            partition_key: 0,
            cursor: first_id,
            batch_size: 64,
            wakeup: rx,
            decoder: test_decoder(),
            row_event_id: test_row_event_id,
        });

        let collected = drain_n(stream, 1, Duration::from_millis(500)).await;
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].payload, "two");
    }

    #[tokio::test]
    #[serial(ff_dev_mode)]
    async fn outbox_cursor_clean_shutdown_on_drop() {
        let backend = fresh_backend().await;
        let (_eid, _h) = seed_and_claim(&backend).await;

        let pool = backend.pool_for_test().clone();
        let rx = backend.stream_frame_receiver_for_test();
        let stream = spawn(OutboxCursorConfig::<TestFrameEvent> {
            pool,
            select_sql: q_stream::OUTBOX_TAIL_STREAM_FRAME_SQL,
            partition_key: 0,
            cursor: 0,
            batch_size: 64,
            wakeup: rx,
            decoder: test_decoder(),
            row_event_id: test_row_event_id,
        });

        // Drop the stream handle — the spawned task must exit without
        // panicking. We assert via a short sleep then a follow-up
        // append to ensure the producer side still functions.
        drop(stream);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Follow-up append should succeed (backend still healthy).
        let (_eid2, h2) = seed_and_claim(&backend).await;
        backend
            .append_frame(&h2, Frame::new(b"after-drop".to_vec(), FrameKind::Stdout))
            .await
            .expect("append post-drop");
    }
}
