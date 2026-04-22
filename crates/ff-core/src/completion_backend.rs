//! The [`CompletionBackend`] trait — backend-agnostic completion
//! subscription surface (issue #90).
//!
//! ff-engine's DAG promotion listener used to `SUBSCRIBE
//! ff:dag:completions` directly on a dedicated RESP3 `ferriskey::Client`
//! — a Valkey-specific wire detail baked into an otherwise
//! backend-agnostic crate. RFC-012 trait-ifies the write surface via
//! [`EngineBackend`]; this trait closes the symmetric gap on the
//! read-side notification path. A Postgres backend would implement
//! this over `LISTEN/NOTIFY`; the Valkey backend keeps the pubsub
//! wiring but hides the channel string behind the trait.
//!
//! # Object safety
//!
//! `CompletionBackend` is object-safe: the single method is
//! `async fn` behind `#[async_trait]` and takes `&self`. Consumers
//! can hold `Arc<dyn CompletionBackend>` alongside
//! `Arc<dyn EngineBackend>` for heterogeneous-backend deployments.
//! A compile-time assertion ([`_assert_dyn_compatible`]) guards
//! future method additions against accidental dyn-incompatibility.
//!
//! # Fanout policy
//!
//! Each `subscribe_completions()` call returns an independent
//! [`CompletionStream`]. Backends are free to implement fanout
//! however they like — the Valkey backend today opens one
//! `SUBSCRIBE` per call on a dedicated RESP3 connection. Callers
//! that want to share a single subscription across many consumers
//! should wrap the stream in a broadcast themselves; the trait does
//! not assume shared-subscription semantics.
//!
//! # Reconnect + transient errors
//!
//! The stream yields [`CompletionPayload`] (not `Result<_, _>`)
//! indefinitely until the consumer drops it. Backends handle
//! reconnect, resubscribe, and transient-error logging internally;
//! completions produced during a reconnect window may be missed
//! and are picked up by ff-engine's `dependency_reconciler` safety
//! net. Issue #90 does NOT promise at-least-once delivery through
//! the stream — that contract stays with the reconciler scanner.
//!
//! # Scope: no `publish_completion`
//!
//! The publisher side of the completion channel is backend-internal.
//! On Valkey, `ff_complete_execution` / `ff_fail_execution` /
//! `ff_cancel_execution` PUBLISH from inside Lua (FCALL-atomic); on
//! Postgres, the stored procedure would NOTIFY. Rust never
//! publishes. There is intentionally no `publish_completion` method
//! on this trait — if you find yourself wanting one, you are on the
//! wrong side of the atomicity boundary.

use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;

use crate::backend::CompletionPayload;
use crate::engine_error::EngineError;

/// A backend-agnostic stream of completion events.
///
/// Boxed because the concrete stream type is backend-specific
/// (tokio `mpsc::Receiver` adapter on Valkey; `LISTEN` notification
/// adapter on Postgres). `Unpin` keeps `.next().await` ergonomic in
/// loop bodies without manual pinning. `Send` so the stream is
/// usable from any tokio task.
pub type CompletionStream = Pin<Box<dyn Stream<Item = CompletionPayload> + Send + Unpin>>;

/// Backend surface for subscribing to completion events.
///
/// The channel string (Valkey: `ff:dag:completions`) is a backend
/// implementation detail and deliberately does NOT appear on the
/// trait. Callers route through `subscribe_completions()` and
/// consume [`CompletionPayload`]s; they never see the wire channel.
#[async_trait]
pub trait CompletionBackend: Send + Sync + 'static {
    /// Subscribe to the completion event stream.
    ///
    /// Each call opens its own subscription (per-call bounded mpsc
    /// fanout on Valkey). The returned stream is independent of all
    /// other outstanding streams; dropping it releases the
    /// backend-side subscription.
    ///
    /// Returns `EngineError` only for synchronous setup failures
    /// (e.g. connection pool exhausted at subscribe-time). Transient
    /// errors after the stream is returned are handled silently by
    /// the backend's reconnect loop — callers do not see them.
    async fn subscribe_completions(&self) -> Result<CompletionStream, EngineError>;
}

/// Object-safety assertion: `dyn CompletionBackend` compiles iff
/// every method is dyn-compatible. Compile-time guard so a future
/// trait change that accidentally breaks dyn-safety fails the build
/// at this site rather than at every downstream
/// `Arc<dyn CompletionBackend>` use. Mirrors the sibling assertion
/// in `engine_backend.rs`.
#[allow(dead_code)]
fn _assert_dyn_compatible(_: &dyn CompletionBackend) {}
