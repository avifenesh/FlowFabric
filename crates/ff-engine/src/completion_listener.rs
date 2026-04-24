//! Push-based DAG promotion dispatch loop (Batch C item 6, issue #9;
//! backend-agnostic refactor in issue #90).
//!
//! Consumes a [`CompletionStream`] from a
//! [`ff_core::completion_backend::CompletionBackend`] and dispatches
//! `resolve_dependency` for each received completion. This turns the
//! `dependency_reconciler` scanner into a pure safety net —
//! user-perceived DAG latency drops from `interval × levels` to
//! `RTT × levels` under normal operation.
//!
//! # Design notes
//!
//! - **Backend-agnostic.** The subscribe/reconnect wire work lives in
//!   the backend crate (`ff-backend-valkey` for Valkey; a future
//!   Postgres backend would implement it over LISTEN/NOTIFY).
//!   ff-engine only sees `CompletionPayload`s off the stream.
//!
//! - **Safety net.** Messages missed during subscriber restart,
//!   server disconnect, or the (rare) outage window are picked up by
//!   the `dependency_reconciler` scanner at its configured interval.
//!   The push path is an optimization, not a correctness dependency.
//!
//! - **Dedup-free.** Duplicate dispatches are harmless —
//!   `ff_resolve_dependency` is idempotent — but in cluster mode a
//!   single PUBLISH fans out to every node, so every ff-server
//!   instance that runs this dispatch loop receives every
//!   completion. If contention surfaces, a follow-up can switch the
//!   backend to sharded pubsub (`SPUBLISH`/`SSUBSCRIBE`) keyed by
//!   flow partition.

use std::sync::Arc;

use ff_core::backend::CompletionPayload;
use ff_core::completion_backend::CompletionStream;
use futures::Stream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::partition_router::{dispatch_dependency_resolution, PartitionRouter};

/// Spawn the completion dispatch loop. Returns a `JoinHandle` that
/// resolves when the shutdown watch fires or the stream ends.
///
/// `stream` is produced by
/// [`CompletionBackend::subscribe_completions`]
/// (e.g. `ValkeyBackend`'s RESP3 subscriber). `dispatch_client` is
/// the ferriskey client used to route `ff_resolve_dependency` FCALLs
/// — a separate client from the one the backend uses internally for
/// the subscription.
pub fn spawn_dispatch_loop(
    router: Arc<PartitionRouter>,
    dispatch_client: ferriskey::Client,
    stream: CompletionStream,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `StreamExt::next` would drag the full `futures` crate into
        // ff-engine just for one method; hand-roll via `poll_next`
        // wrapped in `poll_fn` to stay on `futures-core`.
        let mut stream = stream;
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    // `changed()` errors when all senders drop —
                    // treat as shutdown.
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                payload = next_payload(&mut stream) => {
                    let Some(payload) = payload else {
                        tracing::warn!(
                            "completion dispatch: stream ended, loop exiting"
                        );
                        return;
                    };
                    // Spawn per-dispatch so slow ff_resolve_dependency
                    // calls don't head-of-line-block subsequent
                    // completions. Dispatch is idempotent; redundant
                    // re-entry on the same eid is harmless.
                    let router = router.clone();
                    let client = dispatch_client.clone();
                    tokio::spawn(async move {
                        handle_completion(&router, &client, payload).await;
                    });
                }
            }
        }
    })
}

/// Poll the stream for the next item. Extracted so the caller can
/// use it inside `tokio::select!` without depending on
/// `futures::StreamExt`.
async fn next_payload(stream: &mut CompletionStream) -> Option<CompletionPayload> {
    std::future::poll_fn(|cx| {
        let pinned: std::pin::Pin<&mut CompletionStream> = std::pin::Pin::new(stream);
        // `CompletionStream` is `Pin<Box<dyn Stream + Unpin>>` so the
        // outer `Pin<&mut _>` is safe to poll — `poll_next` on a
        // pinned `Box` delegates to the boxed stream.
        Stream::poll_next(pinned.get_mut().as_mut(), cx)
    })
    .await
}

async fn handle_completion(
    router: &PartitionRouter,
    dispatch_client: &ferriskey::Client,
    payload: CompletionPayload,
) {
    let flow_id_str = payload.flow_id.as_ref().map(|f| f.to_string());
    tracing::debug!(
        execution_id = %payload.execution_id,
        flow_id = ?flow_id_str,
        outcome = %payload.outcome,
        "completion dispatch: resolving dependencies"
    );

    dispatch_dependency_resolution(
        dispatch_client,
        router,
        &payload.execution_id,
        flow_id_str.as_deref(),
    )
    .await;
}

// ── Postgres completion listener (Wave 6d) ───────────────────────────
//
// Drains the `ff_completion_event` outbox via the Postgres
// `CompletionBackend` stream (Wave 4h) and invokes
// `ff_backend_postgres::dispatch::dispatch_completion(pool, event_id)`
// (Wave 5a) per event to cascade DAG promotion.
//
// ## event_id plumbing
//
// `CompletionPayload` (ff-core) does NOT carry `event_id`. The Wave 4h
// stream emits payloads in strict `event_id ASC` order but drops the
// cursor on the way out. Rather than extend the ff-core type (which
// would leak a Postgres-specific field into the Valkey backend), we
// resolve each payload to its event_id by looking up the smallest
// `event_id > last_seen` that matches the payload's
// `(execution_id_uuid, occurred_at_ms)`. `last_seen` is a monotonic
// cursor seeded from `max(event_id)` at startup — matching the Wave 4h
// stream's own starting cursor, so we never dispatch an event that
// existed before we subscribed (the dependency_reconciler is the
// backstop for those).
//
// ## Failure handling
//
// Per Wave 6a contract, `dispatch_completion` failures are logged and
// the listener continues — the Wave 6a reconciler claims any event
// whose `dispatched_at_ms` stays NULL past its grace window.

#[cfg(feature = "postgres")]
pub use postgres_listener::run_completion_listener_postgres;

#[cfg(feature = "postgres")]
mod postgres_listener {
    use super::*;
    use ff_core::backend::ScannerFilter;
    use ff_core::completion_backend::CompletionBackend;
    use ff_core::engine_error::EngineError;
    use sqlx::PgPool;

    /// Shutdown signal for the Postgres completion listener.
    ///
    /// Accepts the same `watch::Receiver<bool>` that the Valkey
    /// dispatch loop uses (set to `true` to shut down) so the engine's
    /// existing shutdown plumbing drives both branches.
    pub type ShutdownRx = watch::Receiver<bool>;

    /// Run the Postgres completion-listener drain loop. Returns when
    /// `shutdown` fires or the stream ends.
    ///
    /// `backend` is a `CompletionBackend` (typically a
    /// `PostgresBackend`) whose `subscribe_completions` yields
    /// `ff_completion_event` rows; `pool` is the dispatch pool used by
    /// `dispatch_completion` to run the cascade tx.
    pub async fn run_completion_listener_postgres(
        backend: Arc<dyn CompletionBackend>,
        pool: PgPool,
        shutdown: ShutdownRx,
    ) -> Result<(), EngineError> {
        let stream = backend.subscribe_completions().await?;
        run_with_stream(stream, pool, shutdown).await
    }

    /// Same contract with a pre-built `ScannerFilter` for multi-tenant
    /// deployments (issue #122).
    #[allow(dead_code)]
    pub async fn run_completion_listener_postgres_filtered(
        backend: Arc<dyn CompletionBackend>,
        pool: PgPool,
        filter: ScannerFilter,
        shutdown: ShutdownRx,
    ) -> Result<(), EngineError> {
        let stream = backend.subscribe_completions_filtered(&filter).await?;
        run_with_stream(stream, pool, shutdown).await
    }

    async fn run_with_stream(
        mut stream: CompletionStream,
        pool: PgPool,
        mut shutdown: ShutdownRx,
    ) -> Result<(), EngineError> {
        // `last_seen` starts at the outbox's current max so we never
        // bind to a pre-subscribe event_id. The Wave 4h stream uses the
        // same starting cursor internally; this keeps the two cursors
        // aligned so the `event_id > last_seen` lookup always finds the
        // row the stream just emitted.
        let mut last_seen: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(event_id), 0) FROM ff_completion_event")
                .fetch_one(&pool)
                .await
                .map_err(|_| EngineError::Unavailable {
                    op: "pg.completion_listener (max(event_id))",
                })?;

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                maybe_payload = next_payload(&mut stream) => {
                    let Some(payload) = maybe_payload else {
                        tracing::warn!(
                            "pg.completion_listener: stream ended, loop exiting"
                        );
                        return Ok(());
                    };
                    if let Some(event_id) = resolve_event_id(&pool, &payload, last_seen).await {
                        last_seen = event_id;
                        if let Err(e) = ff_backend_postgres::dispatch::dispatch_completion(
                            &pool, event_id,
                        ).await {
                            tracing::warn!(
                                event_id,
                                error = %e,
                                "pg.completion_listener: dispatch failed, reconciler will retry"
                            );
                        }
                    } else {
                        tracing::warn!(
                            execution_id = %payload.execution_id,
                            occurred_at_ms = payload.produced_at_ms.0,
                            "pg.completion_listener: could not resolve event_id; \
                             reconciler will claim"
                        );
                    }
                }
            }
        }
    }

    /// Look up the next unseen `event_id` for this payload's
    /// `(execution_id_uuid, occurred_at_ms)`. Returns `None` if the
    /// outbox lookup fails or no matching row exists above the cursor.
    async fn resolve_event_id(
        pool: &PgPool,
        payload: &CompletionPayload,
        last_seen: i64,
    ) -> Option<i64> {
        // Extract the bare uuid from the `{fp:N}:<uuid>` form.
        let eid_str = payload.execution_id.as_str();
        let uuid_str = eid_str.rsplit_once(':').map(|(_, u)| u)?;
        let uuid = uuid::Uuid::parse_str(uuid_str).ok()?;

        sqlx::query_scalar::<_, i64>(
            "SELECT event_id FROM ff_completion_event \
             WHERE event_id > $1 AND execution_id = $2 AND occurred_at_ms = $3 \
             ORDER BY event_id ASC LIMIT 1",
        )
        .bind(last_seen)
        .bind(uuid)
        .bind(payload.produced_at_ms.0)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
    }
}
