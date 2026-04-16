//! Error classification for FlowFabric.
//!
//! `ErrorClass` lives here so `ff-core` stays a pure types/contracts crate.
//! The `ScriptError` enum and its `ferriskey::Error` transport variant live
//! in `ff-script::error` to keep `ff-core` free of the Valkey client stack.

use serde::{Deserialize, Serialize};

/// Classification of how the SDK should handle each error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Stop work. Do NOT retry.
    Terminal,
    /// Retry after backoff.
    Retryable,
    /// Take specific action then stop.
    Cooperative,
    /// No-op, log and continue.
    Informational,
    /// Log warning only.
    Advisory,
    /// Should never happen — alert.
    Bug,
    /// Normal empty state.
    Expected,
    /// Log warning, may continue.
    SoftError,
}
