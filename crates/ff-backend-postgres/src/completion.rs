//! `CompletionBackend` implementation for Postgres (Wave 4h / Q1).
//!
//! Transport design (per RFC-v0.7 Q1 adjudication):
//!
//! * Durable source of truth is the `ff_completion_event` outbox
//!   table — a `bigserial` event_id column sequenced across all 256
//!   hash partitions gives a single total order.
//! * `pg_notify('ff_completion', event_id::text)` fires from the
//!   `ff_completion_event_notify_trg` trigger (see
//!   `migrations/0001_initial.sql` §Section 8). NOTIFY is the wake
//!   signal only — the payload carries only the event_id cursor so
//!   we sidestep Postgres's 8 KB NOTIFY payload cap and the natural
//!   coalescing (`pg` collapses duplicate payloads on the same
//!   channel within a transaction).
//! * Each subscriber owns a long-lived tokio task holding a
//!   [`PgListener`] (one dedicated pg connection) + issues
//!   `SELECT ... WHERE event_id > $last_seen` on every wake to
//!   catch bursts NOTIFY coalesced.
//! * On LISTEN-connection loss (pg restart, PgBouncer eviction,
//!   network drop), `PgListener::recv()` surfaces an error; we
//!   re-establish LISTEN + replay from `last_seen_event_id`.
//! * Subscriber drop ends the task via `tx.closed()`.
//!
//! Missed NOTIFY between commit and LISTEN start is covered by
//! `dependency_reconciler`'s scan — out of scope here.
//!
//! Coordination with Wave 4e (stream LISTEN/NOTIFY via
//! [`super::listener::StreamNotifier`]): completion subscribers own
//! their own `PgListener` instance; they do NOT share the
//! `StreamNotifier` task. Completion fanout is low cardinality
//! (engines subscribe, not every execution) so a per-subscriber
//! connection is cheap — reuse is a future optimisation once Wave 4e
//! stabilises.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{CompletionPayload, ScannerFilter};
use ff_core::completion_backend::{CompletionBackend, CompletionStream};
use ff_core::engine_error::EngineError;
use ff_core::types::{ExecutionId, FlowId, Namespace, TimestampMs};
use futures_core::Stream;
use sqlx::postgres::{PgListener, PgPool};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::PostgresBackend;

/// Channel name fired by `ff_notify_completion_event()` after every
/// `ff_completion_event` INSERT.
pub const COMPLETION_CHANNEL: &str = "ff_completion";

/// Bounded fan-out capacity. Matches the Valkey backend's
/// [`ff_backend_valkey::completion::STREAM_CAPACITY`] so consumer
/// code sees equivalent backpressure behaviour across backends.
const STREAM_CAPACITY: usize = 1024;

/// Max rows pulled per wake; caps catch-up query cost.
const REPLAY_BATCH: i64 = 256;

/// Reconnect backoff when the LISTEN connection drops.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// Stream adapter returned to callers. Holds the `mpsc::Receiver`
/// plus the task handle that feeds it; dropping the stream drops the
/// receiver, `tx.closed()` fires in the task, and the task exits —
/// which aborts the owned `JoinHandle` cleanly on drop.
pub struct PostgresCompletionStream {
    rx: mpsc::Receiver<CompletionPayload>,
    /// Task handle kept alive for the stream's lifetime. Aborted on
    /// drop (no need to `.await` — the task also exits when the
    /// receiver is dropped via `tx.closed()`).
    handle: JoinHandle<()>,
}

impl Drop for PostgresCompletionStream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Stream for PostgresCompletionStream {
    type Item = CompletionPayload;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Decoded outbox row. Mirrors the `ff_completion_event` columns we
/// need to build a [`CompletionPayload`] + apply [`ScannerFilter`].
struct CompletionEventRow {
    event_id: i64,
    execution_id: Uuid,
    flow_id: Option<Uuid>,
    outcome: String,
    namespace: Option<String>,
    instance_tag: Option<String>,
    occurred_at_ms: i64,
    partition_key: i16,
}

impl CompletionEventRow {
    /// Build a [`CompletionPayload`] from the outbox row.
    ///
    /// The stored `execution_id` is a bare UUID; we re-attach the
    /// `{fp:N}:<uuid>` hash-tag using the row's `partition_key` so
    /// downstream consumers see the same execution-id shape they
    /// would get from the Valkey backend.
    fn into_payload(self) -> CompletionPayload {
        let eid_string = format!("{{fp:{}}}:{}", self.partition_key, self.execution_id);
        // Parse goes through the canonical shape check; on a
        // well-formed Wave-3 row this is infallible.
        let execution_id = ExecutionId::parse(&eid_string)
            .expect("ff_completion_event row produced malformed ExecutionId");
        let mut payload = CompletionPayload::new(
            execution_id,
            self.outcome,
            None, // payload_bytes — not persisted in Wave 3 outbox
            TimestampMs(self.occurred_at_ms),
        );
        if let Some(fid) = self.flow_id {
            payload = payload.with_flow_id(FlowId::from_uuid(fid));
        }
        payload
    }

    /// Apply a [`ScannerFilter`]. The filter's `namespace` dimension
    /// is checked against the row's `namespace` column; the
    /// `instance_tag` dimension is checked against the row's
    /// `instance_tag` column, which the Wave-3 schema stores as the
    /// denormalised `"key=value"` pair written by the completion
    /// emitter. When the filter specifies a tag the row must carry a
    /// matching exact pair; missing `instance_tag` never matches.
    fn passes(&self, filter: &ScannerFilter) -> bool {
        if let Some(ref want_ns) = filter.namespace {
            let have: Option<Namespace> = self.namespace.as_deref().map(Namespace::from);
            if have.as_ref() != Some(want_ns) {
                return false;
            }
        }
        if let Some((ref k, ref v)) = filter.instance_tag {
            let want = format!("{k}={v}");
            match self.instance_tag {
                Some(ref have) if have == &want => {}
                _ => return false,
            }
        }
        true
    }
}

/// Public entrypoint — subscribe to completions with an optional
/// [`ScannerFilter`].
///
/// Starting cursor is `max(event_id)` at subscribe time: we tail new
/// events only. Replay from an older cursor is a future feature and
/// is deliberately out of scope for v0.7.
pub(crate) async fn subscribe(
    pool: &PgPool,
    filter: Option<ScannerFilter>,
) -> Result<CompletionStream, EngineError> {
    let filter = filter.unwrap_or(ScannerFilter::NOOP);

    // Resolve the starting cursor BEFORE we spawn the listen task so
    // synchronous-setup errors (pool exhausted, schema missing) bubble
    // back to the caller as `EngineError` per the trait contract.
    let start: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(event_id), 0) FROM ff_completion_event")
        .fetch_one(pool)
        .await
        .map_err(|e| EngineError::Unavailable {
            op: match &e {
                sqlx::Error::Database(_) => "pg.subscribe_completions (max(event_id))",
                _ => "pg.subscribe_completions (connect)",
            },
        })?;

    let (tx, rx) = mpsc::channel::<CompletionPayload>(STREAM_CAPACITY);
    let pool_clone = pool.clone();
    let handle = tokio::spawn(subscriber_loop(pool_clone, tx, filter, start));

    let stream = PostgresCompletionStream { rx, handle };
    Ok(Box::pin(stream))
}

/// Long-lived LISTEN + replay loop. Ends when `tx.closed()` fires
/// (consumer dropped the stream) or an unrecoverable setup error
/// occurs — logged at `tracing::error!` and leaves the receiver to
/// observe end-of-stream.
async fn subscriber_loop(
    pool: PgPool,
    tx: mpsc::Sender<CompletionPayload>,
    filter: ScannerFilter,
    mut last_seen: i64,
) {
    loop {
        // (Re)establish the dedicated LISTEN connection.
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "pg.completion.subscribe: PgListener::connect_with failed; retrying"
                );
                if wait_or_exit(&tx, RECONNECT_BACKOFF).await {
                    return;
                }
                continue;
            }
        };
        if let Err(e) = listener.listen(COMPLETION_CHANNEL).await {
            tracing::warn!(
                error = %e,
                "pg.completion.subscribe: LISTEN ff_completion failed; retrying"
            );
            if wait_or_exit(&tx, RECONNECT_BACKOFF).await {
                return;
            }
            continue;
        }

        // Catch-up replay immediately after (re)subscribe — covers
        // any events committed between last_seen and the LISTEN
        // registration that NOTIFY missed for us.
        if !replay(&pool, &tx, &filter, &mut last_seen).await {
            return; // tx closed
        }

        // Steady-state: recv one NOTIFY, drain all events above the
        // cursor, repeat. `recv()` also surfaces connection drops as
        // Err; on error we break to the outer reconnect loop.
        loop {
            tokio::select! {
                _ = tx.closed() => return,
                res = listener.recv() => {
                    match res {
                        Ok(_notification) => {
                            if !replay(&pool, &tx, &filter, &mut last_seen).await {
                                return;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "pg.completion.subscribe: listener.recv() error; reconnecting"
                            );
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

/// Sleep for `d` OR return `true` if the subscriber dropped during
/// the wait (caller should exit).
async fn wait_or_exit(tx: &mpsc::Sender<CompletionPayload>, d: Duration) -> bool {
    tokio::select! {
        _ = tx.closed() => true,
        _ = tokio::time::sleep(d) => false,
    }
}

/// Drain all events above `last_seen`, filter, forward. Returns
/// `false` iff the consumer has dropped the stream (caller must
/// exit).
async fn replay(
    pool: &PgPool,
    tx: &mpsc::Sender<CompletionPayload>,
    filter: &ScannerFilter,
    last_seen: &mut i64,
) -> bool {
    loop {
        let rows: Vec<CompletionEventRow> = match sqlx::query_as::<_, (i64, Uuid, Option<Uuid>, String, Option<String>, Option<String>, i64, i16)>(
            "SELECT event_id, execution_id, flow_id, outcome, namespace, instance_tag, occurred_at_ms, partition_key \
             FROM ff_completion_event \
             WHERE event_id > $1 \
             ORDER BY event_id ASC \
             LIMIT $2"
        )
        .bind(*last_seen)
        .bind(REPLAY_BATCH)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows
                .into_iter()
                .map(|(event_id, execution_id, flow_id, outcome, namespace, instance_tag, occurred_at_ms, partition_key)| CompletionEventRow {
                    event_id,
                    execution_id,
                    flow_id,
                    outcome,
                    namespace,
                    instance_tag,
                    occurred_at_ms,
                    partition_key,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "pg.completion.replay: query failed");
                return !tx.is_closed();
            }
        };

        if rows.is_empty() {
            return !tx.is_closed();
        }

        for row in rows {
            *last_seen = row.event_id;
            let passes = row.passes(filter);
            if !passes {
                continue;
            }
            let payload = row.into_payload();
            if tx.send(payload).await.is_err() {
                return false; // consumer dropped
            }
        }
        // Loop: there may be more rows than REPLAY_BATCH. Keep
        // draining until we hit an empty page.
    }
}

/// Compile-time proof that `PostgresBackend` stays dyn-compatible
/// under the `CompletionBackend` trait. Mirrors the sibling guard in
/// `ff_core::completion_backend::_assert_dyn_compatible`.
#[allow(dead_code)]
fn _assert_pg_dyn_completion(b: std::sync::Arc<PostgresBackend>) -> std::sync::Arc<dyn CompletionBackend> {
    b
}

#[async_trait]
impl CompletionBackend for PostgresBackend {
    async fn subscribe_completions(&self) -> Result<CompletionStream, EngineError> {
        subscribe(&self.pool, None).await
    }

    async fn subscribe_completions_filtered(
        &self,
        filter: &ScannerFilter,
    ) -> Result<CompletionStream, EngineError> {
        if filter.is_noop() {
            return self.subscribe_completions().await;
        }
        subscribe(&self.pool, Some(filter.clone())).await
    }
}
