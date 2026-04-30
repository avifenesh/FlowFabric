//! Push-based DAG promotion dispatch loop (Batch C item 6, issue #9;
//! backend-agnostic refactor in issue #90; trait-routed in PR-7b
//! Cluster 4).
//!
//! Consumes a [`CompletionStream`] from a
//! [`ff_core::completion_backend::CompletionBackend`] and invokes
//! [`EngineBackend::cascade_completion`] for each received payload.
//! This turns the `dependency_reconciler` scanner into a pure safety
//! net — user-perceived DAG latency drops from `interval × levels` to
//! `RTT × levels` under normal operation.
//!
//! # Design notes
//!
//! - **Trait-routed.** Pre-PR-7b this module held per-backend
//!   branching: a `ferriskey::Client`-driven cascade for Valkey and a
//!   separate `run_completion_listener_postgres` for PG's outbox.
//!   PR-7b moves the per-backend orchestration behind
//!   `EngineBackend::cascade_completion`, so the loop itself is now
//!   one method call per payload regardless of backend.
//!
//! - **Timing semantics differ by backend.** See the
//!   [`EngineBackend::cascade_completion`] rustdoc: Valkey drives the
//!   full recursive cascade inline before returning; Postgres
//!   resolves the payload to its outbox row and kicks off the
//!   per-hop-tx dispatch, with further hops riding their own outbox
//!   events. Consumer-visible divergence is documented in
//!   `docs/POSTGRES_PARITY_MATRIX.md` + `docs/CONSUMER_MIGRATION_0.13.md`.
//!
//! - **Safety net.** Messages missed during subscriber restart,
//!   server disconnect, or the (rare) outage window are picked up by
//!   the `dependency_reconciler` scanner at its configured interval.
//!   The push path is an optimization, not a correctness dependency.
//!
//! - **Dedup-free.** Duplicate dispatches are harmless —
//!   `cascade_completion` is idempotent on both backends (Valkey
//!   `ff_resolve_dependency` returns `already_resolved`; PG
//!   `dispatch_completion` short-circuits on a non-NULL
//!   `dispatched_at_ms`).

use std::sync::Arc;

use ff_core::backend::CompletionPayload;
use ff_core::completion_backend::CompletionStream;
use ff_core::engine_backend::EngineBackend;
use futures::Stream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Spawn the completion dispatch loop. Returns a `JoinHandle` that
/// resolves when the shutdown watch fires or the stream ends.
///
/// `stream` is produced by
/// [`ff_core::completion_backend::CompletionBackend::subscribe_completions`]
/// (e.g. `ValkeyBackend`'s RESP3 subscriber or `PostgresBackend`'s
/// outbox LISTEN drain). `backend` is the `EngineBackend` whose
/// `cascade_completion` is invoked per payload — typically the same
/// backend instance the engine was started with.
pub fn spawn_dispatch_loop(
    backend: Arc<dyn EngineBackend>,
    stream: CompletionStream,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
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
                    // Spawn per-dispatch so a slow cascade doesn't
                    // head-of-line-block subsequent completions.
                    // Cascade is idempotent; redundant re-entry on the
                    // same payload is harmless on both backends.
                    let backend = backend.clone();
                    tokio::spawn(async move {
                        handle_completion(&*backend, payload).await;
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
        Stream::poll_next(pinned.get_mut().as_mut(), cx)
    })
    .await
}

async fn handle_completion(backend: &dyn EngineBackend, payload: CompletionPayload) {
    let flow_id_str = payload.flow_id.as_ref().map(|f| f.to_string());
    tracing::debug!(
        execution_id = %payload.execution_id,
        flow_id = ?flow_id_str,
        outcome = %payload.outcome,
        "completion dispatch: cascading"
    );

    if let Err(e) = backend.cascade_completion(&payload).await {
        tracing::warn!(
            execution_id = %payload.execution_id,
            error = %e,
            "completion dispatch: cascade_completion failed (reconciler will retry)"
        );
    }
}

// ── Postgres back-compat shims (PR-7b Cluster 4) ─────────────────────
//
// Pre-PR-7b callers drove the PG outbox drain via
// `run_completion_listener_postgres`, which inlined the
// event_id-resolution + `dispatch_completion` logic. That logic now
// lives inside `PostgresBackend::cascade_completion`. The shims below
// preserve the pre-existing entry points for in-tree tests
// (`pg_completion_listener.rs`) by delegating to the unified loop.

#[cfg(feature = "postgres")]
pub use postgres_listener::{
    run_completion_listener_postgres, run_completion_listener_postgres_filtered,
};

#[cfg(feature = "postgres")]
mod postgres_listener {
    use super::*;
    use ff_core::backend::ScannerFilter;
    use ff_core::completion_backend::CompletionBackend;
    use ff_core::engine_error::EngineError;
    use sqlx::PgPool;

    /// Shutdown signal for the Postgres completion listener.
    pub type ShutdownRx = watch::Receiver<bool>;

    /// Back-compat wrapper that subscribes to the PG completion
    /// stream and invokes `cascade_completion` per payload.
    ///
    /// `engine_backend` must be the same concrete backend (a
    /// `PostgresBackend`) that produces the stream — it implements both
    /// `CompletionBackend` (the subscription source) and
    /// `EngineBackend` (the cascade dispatcher). `pool` is accepted for
    /// API compatibility with pre-PR-7b callers and is no longer
    /// consulted directly; the PG `EngineBackend` impl owns the pool
    /// internally.
    pub async fn run_completion_listener_postgres(
        engine_backend: Arc<dyn EngineBackend>,
        completion_backend: Arc<dyn CompletionBackend>,
        _pool: PgPool,
        shutdown: ShutdownRx,
    ) -> Result<(), EngineError> {
        let stream = completion_backend.subscribe_completions().await?;
        run_loop(engine_backend, stream, shutdown).await
    }

    /// Filtered variant (multi-tenant deployments — issue #122).
    #[allow(dead_code)]
    pub async fn run_completion_listener_postgres_filtered(
        engine_backend: Arc<dyn EngineBackend>,
        completion_backend: Arc<dyn CompletionBackend>,
        _pool: PgPool,
        filter: ScannerFilter,
        shutdown: ShutdownRx,
    ) -> Result<(), EngineError> {
        let stream = completion_backend.subscribe_completions_filtered(&filter).await?;
        run_loop(engine_backend, stream, shutdown).await
    }

    async fn run_loop(
        engine_backend: Arc<dyn EngineBackend>,
        mut stream: CompletionStream,
        mut shutdown: ShutdownRx,
    ) -> Result<(), EngineError> {
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
                    if let Err(e) = engine_backend.cascade_completion(&payload).await {
                        tracing::warn!(
                            execution_id = %payload.execution_id,
                            error = %e,
                            "pg.completion_listener: cascade_completion failed, \
                             reconciler will retry"
                        );
                    }
                }
            }
        }
    }
}
