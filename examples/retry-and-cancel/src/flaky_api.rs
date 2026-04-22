//! A deterministic flaky "API" used to drive retry exhaustion in the demo.
//!
//! The handler simply returns an error every time. That lets the example
//! observe the full retry schedule — each invocation bumps the attempt
//! counter server-side, and once `max_retries` is exhausted the execution
//! transitions to the terminal `failed` state.

use serde::{Deserialize, Serialize};

/// Payload stored on the execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakyPayload {
    /// Attempt label surfaced in logs (purely for readability).
    pub label: String,
}

/// The handler's "work" — always fails with a transient-looking error so
/// that the retry policy schedules another attempt until the retry budget
/// is exhausted.
pub fn run(payload: &FlakyPayload, attempt_idx: u32) -> Result<Vec<u8>, String> {
    let _ = payload;
    Err(format!(
        "simulated upstream 503 on attempt {attempt_idx} (flaky-api demo)"
    ))
}
