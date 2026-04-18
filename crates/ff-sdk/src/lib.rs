//! FlowFabric Worker SDK — public API for worker authors.
//!
//! This crate depends on `ff-script` for the Lua-function types, Lua error
//! kinds (`ScriptError`), and retry helpers (`is_retryable_kind`,
//! `kind_to_stable_str`). Consumers using `ff-sdk` do not need to import
//! `ff-script` directly for normal worker operations, but can if they need
//! the `ScriptError` or retry types.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use ff_sdk::{FlowFabricWorker, WorkerConfig};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), ff_sdk::SdkError> {
//!     let config = WorkerConfig::new(
//!         "localhost",
//!         6379,
//!         "my-worker",
//!         "my-worker-instance-1",
//!         "default",
//!         "main",
//!     );
//!
//!     let worker = FlowFabricWorker::connect(config).await?;
//!
//!     loop {
//!         match worker.claim_next().await? {
//!             Some(task) => {
//!                 println!("claimed: {}", task.execution_id());
//!                 // Process task...
//!                 task.complete(Some(b"done".to_vec())).await?;
//!             }
//!             None => {
//!                 tokio::time::sleep(Duration::from_secs(1)).await;
//!             }
//!         }
//!     }
//! }
//! ```
//!
//! # Migration: `insecure-direct-claim` → scheduler-issued grants
//!
//! The `insecure-direct-claim` cargo feature — which gates
//! [`FlowFabricWorker::claim_next`] — is **deprecated** in favour of
//! the pair of scheduler-issued grant entry points:
//!
//! * [`FlowFabricWorker::claim_from_grant`] — fresh claims. Use
//!   `ff_scheduler::Scheduler::claim_for_worker` to obtain the
//!   [`ClaimGrant`], then hand it to the SDK.
//! * [`FlowFabricWorker::claim_from_reclaim_grant`] — resumed claims
//!   for an `attempt_interrupted` execution. Wraps a
//!   [`ReclaimGrant`].
//!
//! `claim_next` bypasses budget and quota admission control; the
//! grant-based path does not. See each method's rustdoc for the
//! exact migration recipe.
//!
//! [`ClaimGrant`]: ff_core::contracts::ClaimGrant
//! [`ReclaimGrant`]: ff_core::contracts::ReclaimGrant

pub mod config;
pub mod task;
pub mod worker;

// Re-exports for convenience
pub use config::WorkerConfig;
pub use task::{
    read_stream, tail_stream, AppendFrameOutcome, ClaimedTask, ConditionMatcher, FailOutcome,
    Signal, SignalOutcome, StreamFrames, SuspendOutcome, TimeoutBehavior, MAX_TAIL_BLOCK_MS,
    STREAM_READ_HARD_CAP,
};
pub use worker::FlowFabricWorker;

/// SDK error type.
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// Valkey connection or command error (preserves ErrorKind for caller inspection).
    #[error("valkey: {0}")]
    Valkey(#[from] ferriskey::Error),

    /// Valkey error with additional context.
    #[error("valkey: {context}: {source}")]
    ValkeyContext {
        #[source]
        source: ferriskey::Error,
        context: String,
    },

    /// FlowFabric Lua script error.
    #[error("script: {0}")]
    Script(#[from] ff_script::error::ScriptError),

    /// Configuration error.
    #[error("config: {0}")]
    Config(String),

    /// Worker is at its configured `max_concurrent_tasks` capacity —
    /// the caller should retry later. Returned by
    /// [`FlowFabricWorker::claim_from_grant`] and
    /// [`FlowFabricWorker::claim_from_reclaim_grant`] when the
    /// concurrency semaphore is saturated. Distinct from `Ok(None)`:
    /// a `ClaimGrant`/`ReclaimGrant` represents real work already
    /// selected by the scheduler, so silently dropping it would waste
    /// the grant and let the grant TTL elapse. Surfacing the
    /// saturation lets the caller release the grant (or wait +
    /// retry).
    ///
    /// # Classification
    ///
    /// * [`SdkError::is_retryable`] returns `true` — saturation is
    ///   transient: any in-flight task's
    ///   complete/fail/cancel/drop releases a permit. Retry after
    ///   milliseconds, not a retry loop with backoff for a Valkey
    ///   transport failure.
    /// * [`SdkError::valkey_kind`] returns `None` — this is not a
    ///   Valkey transport or Lua error, so there is no
    ///   `ferriskey::ErrorKind` to inspect. Callers that fan out
    ///   on `valkey_kind()` should match `WorkerAtCapacity`
    ///   explicitly (or use `is_retryable()`).
    ///
    /// [`FlowFabricWorker::claim_from_grant`]: crate::FlowFabricWorker::claim_from_grant
    /// [`FlowFabricWorker::claim_from_reclaim_grant`]: crate::FlowFabricWorker::claim_from_reclaim_grant
    #[error("worker at capacity: max_concurrent_tasks reached")]
    WorkerAtCapacity,
}

impl SdkError {
    /// Returns the underlying ferriskey `ErrorKind` if this error carries one.
    /// Covers transport variants (`Valkey`, `ValkeyContext`) directly and
    /// `Script(ScriptError::Valkey(...))` via delegation.
    pub fn valkey_kind(&self) -> Option<ferriskey::ErrorKind> {
        match self {
            Self::Valkey(e) => Some(e.kind()),
            Self::ValkeyContext { source, .. } => Some(source.kind()),
            Self::Script(e) => e.valkey_kind(),
            Self::Config(_) | Self::WorkerAtCapacity => None,
        }
    }

    /// Whether this error is safely retryable by a caller. For transport
    /// variants, delegates to [`ff_script::retry::is_retryable_kind`]. For
    /// `Script` errors, returns `true` iff the Lua error's classification
    /// is `ErrorClass::Retryable`. `Config` errors are never retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Valkey(e) | Self::ValkeyContext { source: e, .. } => {
                ff_script::retry::is_retryable_kind(e.kind())
            }
            Self::Script(e) => {
                matches!(e.class(), ff_core::error::ErrorClass::Retryable)
            }
            // WorkerAtCapacity is retryable: the saturation is transient
            // and clears as soon as a concurrent task completes.
            Self::WorkerAtCapacity => true,
            Self::Config(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferriskey::ErrorKind;
    use ff_script::error::ScriptError;

    fn mk_fk_err(kind: ErrorKind) -> ferriskey::Error {
        ferriskey::Error::from((kind, "synthetic"))
    }

    #[test]
    fn valkey_kind_direct_and_context() {
        assert_eq!(
            SdkError::Valkey(mk_fk_err(ErrorKind::IoError)).valkey_kind(),
            Some(ErrorKind::IoError)
        );
        assert_eq!(
            SdkError::ValkeyContext {
                source: mk_fk_err(ErrorKind::BusyLoadingError),
                context: "connect".into()
            }
            .valkey_kind(),
            Some(ErrorKind::BusyLoadingError)
        );
    }

    #[test]
    fn valkey_kind_delegates_through_script_transport() {
        let err = SdkError::Script(ScriptError::Valkey(mk_fk_err(ErrorKind::ClusterDown)));
        assert_eq!(err.valkey_kind(), Some(ErrorKind::ClusterDown));
    }

    #[test]
    fn valkey_kind_none_for_lua_and_config() {
        assert_eq!(
            SdkError::Script(ScriptError::LeaseExpired).valkey_kind(),
            None
        );
        assert_eq!(SdkError::Config("bad host".into()).valkey_kind(), None);
    }

    #[test]
    fn is_retryable_transport() {
        assert!(SdkError::Valkey(mk_fk_err(ErrorKind::IoError)).is_retryable());
        assert!(!SdkError::Valkey(mk_fk_err(ErrorKind::FatalReceiveError)).is_retryable());
        assert!(!SdkError::Valkey(mk_fk_err(ErrorKind::AuthenticationFailed)).is_retryable());
    }

    #[test]
    fn is_retryable_script_delegates_to_class() {
        // NoEligibleExecution is classified Retryable in ScriptError::class().
        assert!(SdkError::Script(ScriptError::NoEligibleExecution).is_retryable());
        // StaleLease is Terminal.
        assert!(!SdkError::Script(ScriptError::StaleLease).is_retryable());
        // Script::Valkey(IoError) is Retryable via class() delegation.
        assert!(
            SdkError::Script(ScriptError::Valkey(mk_fk_err(ErrorKind::IoError))).is_retryable()
        );
    }

    #[test]
    fn is_retryable_config_false() {
        assert!(!SdkError::Config("at least one lane is required".into()).is_retryable());
    }
}
