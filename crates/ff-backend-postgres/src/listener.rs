//! Shared LISTEN connection + in-memory notifier (placeholder).
//!
//! **RFC-v0.7 Wave 0 — placeholder only.** The spec (Q1 completion
//! transport + Q2 XREAD-BLOCK semantics) pins the architecture:
//!
//! * ONE long-lived `tokio-postgres` connection per `PostgresBackend`
//!   subscribed to every `ff_stream_<eid>_<aidx>` channel with a
//!   registered waiter.
//! * An in-memory `channel → Vec<Notify>` (or `broadcast`/`mpsc`)
//!   map routes NOTIFY payloads to parked waiters.
//! * Reconnect contract with exponential backoff + jittered
//!   `poll-now` wake-up after re-subscribe (round-1 K-3).
//! * PgBouncer transaction-pool mode is incompatible; session-pool
//!   mode (or direct) is required.
//!
//! Wave 4 (Agent 8) fills this in. Wave 0 leaves the placeholder
//! struct so upstream types can name it (`Arc<StreamNotifier>` on
//! `PostgresBackend` in a later wave) without breaking API shape.

use std::sync::Arc;

/// Placeholder for the shared LISTEN notifier. Wave 4 fills in the
/// `channel → waiters` routing table + reconnect task.
///
/// Intentionally opaque at Wave 0: no public fields, no public
/// constructor surface beyond [`StreamNotifier::placeholder`], which
/// exists solely so call sites compile. Once Wave 4 lands the real
/// fields + constructor, the placeholder is either deleted or
/// rewrapped with `#[doc(hidden)]` + `#[deprecated]`.
pub struct StreamNotifier {
    _private: (),
}

impl StreamNotifier {
    /// Wave 0 placeholder constructor. Returns a non-functional
    /// notifier; do not call from production paths. Wave 4 replaces
    /// this with a `spawn_on(pool: &PgPool, ...)` async constructor
    /// that owns the LISTEN task lifetime.
    pub fn placeholder() -> Arc<Self> {
        Arc::new(Self { _private: () })
    }
}
