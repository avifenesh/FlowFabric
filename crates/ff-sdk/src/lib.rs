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
            Self::Config(_) => None,
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
