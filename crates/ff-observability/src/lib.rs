//! FlowFabric observability — OTEL metrics registry + typed handles.
//!
//! This crate is the single place that touches OpenTelemetry / Prometheus.
//! Consumers (`ff-server`, `ff-engine`, `ff-scheduler`) take an optional dep
//! on `ff-observability` behind their own `observability` feature; enabling
//! the consumer-feature transitively enables `ff-observability/enabled`.
//!
//! ## Feature model
//!
//! * `enabled` **off** (default) — all types compile to zero-cost no-op
//!   shims. No OTEL / Prometheus crates in the dep tree. Call sites use
//!   the same `Metrics::new()` entry point as the real backend; every
//!   instrument method is a no-op.
//! * `enabled` **on** — real OTEL `MeterProvider` + Prometheus exporter.
//!   `Metrics::new()` registers all instruments; [`Metrics::render`]
//!   returns the text-exposition body for `/metrics`.
//!
//! Call sites under both features use **identical call shape** — that's
//! the whole point of the indirection. If the shim ever grew a feature
//! skew we'd lose the "same source compiles both ways" guarantee.

#![forbid(unsafe_code)]

#[cfg(feature = "enabled")]
mod real;
#[cfg(not(feature = "enabled"))]
mod shim;

#[cfg(feature = "enabled")]
pub use real::Metrics;
#[cfg(not(feature = "enabled"))]
pub use shim::Metrics;

/// Terminal attempt outcome label for `ff_attempt_outcome_total`.
///
/// The variant set is fixed at 5 by the Observability RFC prereq #4
/// adjudication so cardinality stays bounded at `5 × N lanes`. Kept
/// feature-agnostic (defined here, not in `real`/`shim`) so call sites
/// can construct the value identically whether metrics are compiled in
/// or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Attempt completed successfully (terminal-ok / `complete` FCALL).
    Ok,
    /// Attempt failed terminally — no retries left.
    Error,
    /// Attempt failed with classification = `Timeout`.
    Timeout,
    /// Attempt was cancelled (worker or flow-level cancel).
    Cancelled,
    /// Attempt failed but a retry was scheduled.
    Retry,
}

impl AttemptOutcome {
    /// Stable `&'static str` label — matches the Prometheus exposition.
    pub fn as_stable_str(&self) -> &'static str {
        match self {
            AttemptOutcome::Ok => "ok",
            AttemptOutcome::Error => "error",
            AttemptOutcome::Timeout => "timeout",
            AttemptOutcome::Cancelled => "cancelled",
            AttemptOutcome::Retry => "retry",
        }
    }
}

// Sentry error-reporting module. Gated behind the `sentry` feature,
// orthogonal to `enabled` (OTEL metrics) — consumers can turn either
// on without pulling the other. See [`sentry`] module docs for the
// env-var contract and usage.
#[cfg(feature = "sentry")]
pub mod sentry;

#[cfg(feature = "sentry")]
pub use sentry::{init_sentry, tracing_layer as sentry_tracing_layer};
