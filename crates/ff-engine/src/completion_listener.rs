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
