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
//!   `Metrics::disabled()` and every instrument method is a no-op.
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
