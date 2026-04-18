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

pub mod admin;
pub mod config;
pub mod task;
pub mod worker;

// Re-exports for convenience
pub use admin::{
    FlowFabricAdminClient, RotateWaitpointSecretRequest, RotateWaitpointSecretResponse,
};
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

    /// HTTP transport error from the admin REST surface. Carries
    /// the underlying `reqwest::Error` via `#[source]` so callers
    /// can inspect `is_timeout()` / `is_connect()` / etc. for
    /// finer-grained retry logic. Distinct from
    /// [`SdkError::Valkey`]: this fires on the HTTP/JSON surface,
    /// not on the Lua/Valkey hot path.
    #[error("http: {context}: {source}")]
    Http {
        #[source]
        source: reqwest::Error,
        context: String,
    },

    /// The admin REST endpoint returned a non-2xx response.
    ///
    /// Fields surface the server-side `ErrorBody` JSON shape
    /// (`{ error, kind?, retryable? }`) as structured values so
    /// cairn-fabric and other consumers can match without
    /// re-parsing the body:
    ///
    /// * `status` — HTTP status code.
    /// * `message` — the `error` string from the JSON body (or
    ///   the raw body if it didn't parse as JSON).
    /// * `kind` — server-supplied Valkey `ErrorKind` label for 5xxs
    ///   backed by a transport error; `None` for 4xxs.
    /// * `retryable` — server-supplied hint; `None` for 4xxs.
    /// * `raw_body` — the full response body, preserved for logging
    ///   when the JSON shape doesn't match.
    #[error("admin api: {status}: {message}")]
    AdminApi {
        status: u16,
        message: String,
        kind: Option<String>,
        retryable: Option<bool>,
        raw_body: String,
    },
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
            // HTTP/admin-surface errors carry no ferriskey::ErrorKind;
            // the admin path never touches Valkey directly from the
            // SDK side. Use `AdminApi.kind` for the server-supplied
            // label when present.
            Self::Config(_) | Self::WorkerAtCapacity | Self::Http { .. } | Self::AdminApi { .. } => {
                None
            }
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
            // HTTP transport: timeouts and connect failures are
            // retryable (transient network state); body-decode or
            // request-build errors are terminal (caller must fix
            // the code). `reqwest::Error` exposes both predicates.
            Self::Http { source, .. } => source.is_timeout() || source.is_connect(),
            // Admin API errors: trust the server's `retryable` hint
            // when present; otherwise fall back to the HTTP-standard
            // retryable-status set (429, 503, 504). 5xxs without a
            // hint are conservatively non-retryable — the caller can
            // override with `AdminApi.status`-based logic if needed.
            Self::AdminApi {
                status, retryable, ..
            } => retryable.unwrap_or(matches!(*status, 429 | 503 | 504)),
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

    #[test]
    fn is_retryable_admin_api_uses_server_hint_when_present() {
        let err = SdkError::AdminApi {
            status: 429,
            message: "throttled".into(),
            kind: None,
            retryable: Some(false),
            raw_body: String::new(),
        };
        assert!(!err.is_retryable());

        let err = SdkError::AdminApi {
            status: 500,
            message: "valkey timeout".into(),
            kind: Some("IoError".into()),
            retryable: Some(true),
            raw_body: String::new(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn is_retryable_admin_api_falls_back_to_standard_retryable_statuses() {
        for s in [429u16, 503, 504] {
            let err = SdkError::AdminApi {
                status: s,
                message: "x".into(),
                kind: None,
                retryable: None,
                raw_body: String::new(),
            };
            assert!(err.is_retryable(), "status {s} should be retryable");
        }
        for s in [400u16, 401, 403, 404, 500] {
            let err = SdkError::AdminApi {
                status: s,
                message: "x".into(),
                kind: None,
                retryable: None,
                raw_body: String::new(),
            };
            assert!(!err.is_retryable(), "status {s} should NOT be retryable without hint");
        }
    }

    #[test]
    fn valkey_kind_none_for_admin_surface() {
        let err = SdkError::AdminApi {
            status: 500,
            message: "x".into(),
            kind: Some("IoError".into()),
            retryable: Some(true),
            raw_body: String::new(),
        };
        assert_eq!(err.valkey_kind(), None);
    }
}
