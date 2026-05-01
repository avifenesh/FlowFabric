//! External-callback "signal bridge" helpers.
//!
//! A signal-bridge is the control-plane glue that sits between an
//! external actor (webhook callback, email reply, third-party API,
//! human approver) and FlowFabric's signal delivery surface. The
//! external actor presents a waitpoint HMAC token it was handed at
//! suspend time; the bridge authenticates the token against what the
//! engine has stored for the waitpoint, and on match forwards through
//! [`FlowFabricWorker::deliver_signal`].
//!
//! This module exposes the two consumer-facing pieces of that shape —
//! the error enum and the verify-and-deliver helper — so callers don't
//! rebuild them. The helper was extracted from the v0.13
//! `examples/external-callback/` example after cairn's signal-bridge
//! code review surfaced it as rough-edge API (every consumer ends up
//! reinventing `UnknownWaitpoint` / `TokenMismatch` / `Backend`).
//!
//! # Security notes
//!
//! * Token comparison is constant-time via [`subtle::ConstantTimeEq`].
//!   This guards against byte-timing oracles on the HMAC digest; the
//!   token length is not a secret (all valid tokens share the
//!   `<kid>:<hex-digest>` shape) and an early-return on length mismatch
//!   is acceptable.
//! * The bridge's token check is **defense-in-depth**: the engine
//!   re-verifies the HMAC server-side in [`EngineBackend::deliver_signal`]
//!   (via the `waitpoint_token` field), so a bridge bug that
//!   mis-routes a bad token does not bypass authentication. The bridge
//!   check exists to fail fast at the network edge and produce a
//!   structured error surface for operators.
//! * The helper stamps `waitpoint_token` on the outgoing [`Signal`]
//!   from the `presented` parameter — any `waitpoint_token` the caller
//!   set on the [`Signal`] they passed is overwritten so the engine
//!   re-verifies against the same bytes the bridge checked, not
//!   something else.

use std::sync::Arc;

use ff_core::engine_backend::EngineBackend;
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::{ExecutionId, WaitpointId, WaitpointToken};
use subtle::ConstantTimeEq;

use crate::task::{Signal, SignalOutcome};
use crate::worker::FlowFabricWorker;
use crate::SdkError;

/// Outcome of a signal-bridge authentication + dispatch attempt.
#[derive(Debug)]
pub enum SignalBridgeError {
    /// The waitpoint has no stored token — it was consumed, expired,
    /// or never existed. Real bridges typically map this to HTTP 404.
    UnknownWaitpoint,
    /// The token presented by the external actor did not match the
    /// stored token. Real bridges typically map this to HTTP 401.
    TokenMismatch,
    /// Underlying SDK / engine failure (read-waitpoint-token backend
    /// error, network fault, deliver-signal rejection).
    Backend(SdkError),
}

impl std::fmt::Display for SignalBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownWaitpoint => {
                f.write_str("unknown waitpoint (consumed, expired, or unknown id)")
            }
            Self::TokenMismatch => f.write_str("token mismatch (presented HMAC does not verify)"),
            Self::Backend(e) => write!(f, "backend: {e}"),
        }
    }
}

impl std::error::Error for SignalBridgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SdkError> for SignalBridgeError {
    fn from(e: SdkError) -> Self {
        Self::Backend(e)
    }
}

/// Authenticate an external-actor callback against the stored
/// waitpoint token and, on match, deliver `signal` through the worker.
///
/// The helper:
///
/// 1. Reads the stored token via
///    [`EngineBackend::read_waitpoint_token`]. No stored token (the
///    backend returns `Ok(None)`) → [`SignalBridgeError::UnknownWaitpoint`].
/// 2. Compares the stored token to `presented` in constant time. No
///    match → [`SignalBridgeError::TokenMismatch`].
/// 3. Stamps `signal.waitpoint_token = presented.clone()` (so the
///    engine's server-side re-verification uses the same bytes the
///    bridge already verified) and dispatches via
///    [`FlowFabricWorker::deliver_signal`].
///
/// # Arguments
///
/// * `backend` — the `EngineBackend` trait object the bridge reads the
///   stored token through. Typically the same `Arc<dyn EngineBackend>`
///   the caller constructed their `FlowFabricWorker` with; accepts a
///   reference so callers can pass it without cloning.
/// * `worker` — the [`FlowFabricWorker`] that fronts the engine-side
///   signal delivery. Must be configured against the same backend the
///   stored token lives on.
/// * `execution_id` — the suspended execution the waitpoint belongs
///   to. The partition key is derived from the exec id's `{fp:N}:`
///   prefix.
/// * `waitpoint_id` — the waitpoint to authenticate.
/// * `presented` — the HMAC token the external actor presented.
/// * `signal` — caller-owned [`Signal`] payload (name, category,
///   source, body). The helper overwrites `signal.waitpoint_token` so
///   the caller's value for that field is ignored.
///
/// # Errors
///
/// See [`SignalBridgeError`].
pub async fn verify_and_deliver(
    backend: &dyn EngineBackend,
    worker: &FlowFabricWorker,
    execution_id: &ExecutionId,
    waitpoint_id: &WaitpointId,
    presented: &WaitpointToken,
    mut signal: Signal,
) -> Result<SignalOutcome, SignalBridgeError> {
    let partition = PartitionKey::from(&Partition {
        family: PartitionFamily::Flow,
        index: execution_id.partition(),
    });

    let stored = backend
        .read_waitpoint_token(partition, waitpoint_id)
        .await
        .map_err(|e| SignalBridgeError::Backend(SdkError::from(e)))?
        .ok_or(SignalBridgeError::UnknownWaitpoint)?;

    // Constant-time compare. `ConstantTimeEq::ct_eq` returns `Choice`;
    // `bool::from(Choice)` is the idiomatic conversion.
    let stored_bytes = stored.as_bytes();
    let presented_bytes = presented.as_str().as_bytes();
    if !bool::from(stored_bytes.ct_eq(presented_bytes)) {
        return Err(SignalBridgeError::TokenMismatch);
    }

    signal.waitpoint_token = presented.clone();
    worker
        .deliver_signal(execution_id, waitpoint_id, signal)
        .await
        .map_err(SignalBridgeError::Backend)
}

/// Convenience wrapper for consumers that hold an `Arc<dyn EngineBackend>`
/// rather than a bare trait reference — the more common shape in code
/// that already follows the [`FlowFabricWorker::connect_with`] pattern.
/// Dispatches to [`verify_and_deliver`]; no behavioural difference.
pub async fn verify_and_deliver_arc(
    backend: &Arc<dyn EngineBackend>,
    worker: &FlowFabricWorker,
    execution_id: &ExecutionId,
    waitpoint_id: &WaitpointId,
    presented: &WaitpointToken,
    signal: Signal,
) -> Result<SignalOutcome, SignalBridgeError> {
    verify_and_deliver(
        backend.as_ref(),
        worker,
        execution_id,
        waitpoint_id,
        presented,
        signal,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_each_variant() {
        assert!(SignalBridgeError::UnknownWaitpoint
            .to_string()
            .contains("unknown waitpoint"));
        assert!(SignalBridgeError::TokenMismatch
            .to_string()
            .contains("token mismatch"));
        let wrapped = SignalBridgeError::Backend(SdkError::Config {
            context: "test".into(),
            field: None,
            message: "boom".into(),
        });
        let msg = wrapped.to_string();
        assert!(msg.starts_with("backend: "), "got: {msg}");
    }

    #[test]
    fn error_source_wraps_backend() {
        let inner = SdkError::Config {
            context: "test".into(),
            field: None,
            message: "boom".into(),
        };
        let e = SignalBridgeError::Backend(inner);
        assert!(
            std::error::Error::source(&e).is_some(),
            "Backend variant must expose source"
        );
        assert!(
            std::error::Error::source(&SignalBridgeError::UnknownWaitpoint).is_none(),
            "UnknownWaitpoint has no underlying source"
        );
    }

    #[test]
    fn from_sdk_error() {
        let inner = SdkError::Config {
            context: "test".into(),
            field: None,
            message: "boom".into(),
        };
        match SignalBridgeError::from(inner) {
            SignalBridgeError::Backend(_) => {}
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
