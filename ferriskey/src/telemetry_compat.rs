//! Backwards-compatibility stub for the removed `telemetrylib` crate.
//!
//! Prior releases of ferriskey re-exported `telemetrylib::Telemetry` from
//! the crate root. The `telemetrylib` subcrate — a `lazy_static
//! RwLock<Telemetry>` of global counters plus an `OnceLock`-cached
//! OpenTelemetry tracer — has been deleted in favour of the standard
//! Rust ecosystem pattern: `tracing` spans/events everywhere (zero-cost
//! when no subscriber is installed), optional OpenTelemetry integration
//! behind the `otel` / `metrics` Cargo features.
//!
//! This module preserves the `Telemetry` type name + method signatures
//! so callers compile. Every method is a deprecated no-op: counter
//! reads return `0`, mutations are discarded. If an existing dashboard
//! or test was reading these numbers, it was reading counters that were
//! already inconsistent (see the `StandaloneClient::Drop` decrement-on-
//! every-clone bug the redesign fixes) — a no-op stub is honestly
//! indistinguishable from "telemetry never got incremented."
//!
//! Migration: replace `ferriskey::Telemetry::incr_foo(1)` with
//! `tracing::info!(target = "ferriskey", event = "foo", ...)` at the
//! call site, or wire a `metrics` crate recorder and enable the
//! `metrics` feature for typed counters.

/// Backwards-compatibility stub. All methods are no-ops; counter reads
/// return the identity element (`0`). See the module-level comment for
/// migration guidance.
#[deprecated(
    since = "0.2.0",
    note = "Replace with `tracing::info_span!` / `tracing::instrument`. \
            See ferriskey CHANGELOG §telemetry-redesign for migration."
)]
pub struct Telemetry;

#[allow(deprecated)]
impl Telemetry {
    /// No-op. Returns `0`.
    pub fn incr_total_connections(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn decr_total_connections(_decr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_total_clients(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn decr_total_clients(_decr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn total_connections() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn total_clients() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_total_values_compressed(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn total_values_compressed() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_total_values_decompressed(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn total_values_decompressed() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_total_original_bytes(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn total_original_bytes() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_total_bytes_compressed(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn total_bytes_compressed() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_total_bytes_decompressed(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn total_bytes_decompressed() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_compression_skipped_count(_incr_by: usize) -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn compression_skipped_count() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn incr_subscription_out_of_sync() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn subscription_out_of_sync_count() -> usize {
        0
    }
    /// No-op. Returns `0`.
    pub fn update_subscription_last_sync_timestamp(_timestamp: u64) -> u64 {
        0
    }
    /// No-op. Returns `0`.
    pub fn subscription_last_sync_timestamp() -> u64 {
        0
    }
    /// No-op.
    pub fn reset() {}
}
