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
//! The production claim path is
//! [`FlowFabricWorker::claim_from_grant`]: obtain a
//! [`ClaimGrant`] from `ff_scheduler::Scheduler::claim_for_worker`
//! (the scheduler enforces budget, quota, and capability checks),
//! then hand it to the SDK. `claim_next` is gated behind the
//! default-off `direct-valkey-claim` feature and bypasses admission
//! control — fine for benchmarks, not production.
//!
//! ```rust,ignore
//! use ff_sdk::{FlowFabricWorker, WorkerConfig};
//! use ff_core::backend::BackendConfig;
//! use ff_core::types::{LaneId, Namespace, WorkerId, WorkerInstanceId};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), ff_sdk::SdkError> {
//!     let config = WorkerConfig {
//!         backend: BackendConfig::valkey("localhost", 6379),
//!         worker_id: WorkerId::new("my-worker"),
//!         worker_instance_id: WorkerInstanceId::new("my-worker-instance-1"),
//!         namespace: Namespace::new("default"),
//!         lanes: vec![LaneId::new("main")],
//!         capabilities: Vec::new(),
//!         lease_ttl_ms: 30_000,
//!         claim_poll_interval_ms: 1_000,
//!         max_concurrent_tasks: 1,
//!     };
//!
//!     let worker = FlowFabricWorker::connect(config).await?;
//!     let lane = LaneId::new("main");
//!
//!     // In a real deployment `grant` is obtained from the
//!     // scheduler's `claim_for_worker` RPC/helper; it carries the
//!     // execution id, capability match, and admission result.
//!     # let grant: ff_core::contracts::ClaimGrant = unimplemented!();
//!     let task = worker.claim_from_grant(lane, grant).await?;
//!     println!("claimed: {}", task.execution_id());
//!     // Process task...
//!     task.complete(Some(b"done".to_vec())).await?;
//!     Ok(())
//! }
//! ```
//!
//! # Migration: `direct-valkey-claim` → scheduler-issued grants
//!
//! The `direct-valkey-claim` cargo feature — which gates
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

#[cfg(feature = "valkey-default")]
pub mod admin;
pub mod config;
pub mod engine_error;
#[cfg(feature = "valkey-default")]
pub mod snapshot;
#[cfg(feature = "valkey-default")]
pub mod task;
#[cfg(feature = "valkey-default")]
pub mod worker;

// Re-exports for convenience
#[cfg(feature = "valkey-default")]
pub use admin::{
    rotate_waitpoint_hmac_secret_all_partitions, FlowFabricAdminClient, PartitionRotationOutcome,
    RotateWaitpointSecretRequest, RotateWaitpointSecretResponse,
};
pub use config::WorkerConfig;
pub use engine_error::{
    BugKind, ConflictKind, ContentionKind, EngineError, StateKind, ValidationKind,
};
// #88: backend-agnostic transport error surface. Consumers that
// previously matched on `ferriskey::ErrorKind` via `valkey_kind()`
// now match on `BackendErrorKind` via `backend_kind()`.
pub use ff_core::engine_error::{BackendError, BackendErrorKind};
// `FailOutcome` is ff-core-native (Stage 1a move); re-export
// unconditionally so consumers can name `ff_sdk::FailOutcome` even
// under `--no-default-features`.
pub use ff_core::backend::FailOutcome;
// `ResumeSignal` is also ff-core-native (Stage 0 move).
pub use ff_core::backend::ResumeSignal;
#[cfg(feature = "valkey-default")]
pub use task::{
    read_stream, tail_stream, AppendFrameOutcome, ClaimedTask, ConditionMatcher, Signal,
    SignalOutcome, StreamCursor, StreamFrames, SuspendOutcome, TimeoutBehavior,
    MAX_TAIL_BLOCK_MS, STREAM_READ_HARD_CAP,
};
#[cfg(feature = "valkey-default")]
pub use worker::FlowFabricWorker;

/// SDK error type.
#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    /// Backend transport error. Previously wrapped `ferriskey::Error`
    /// directly (#88); now carries a backend-agnostic
    /// [`BackendError`] so consumers match on
    /// [`BackendErrorKind`] instead of ferriskey's native taxonomy.
    /// The ferriskey → [`BackendError`] mapping lives in
    /// `ff_backend_valkey::backend_error::backend_error_from_ferriskey`.
    #[error("backend: {0}")]
    Backend(#[from] BackendError),

    /// Backend error with additional context (e.g. call-site label).
    /// Previously `ValkeyContext { source: ferriskey::Error }` (#88).
    #[error("backend: {context}: {source}")]
    BackendContext {
        #[source]
        source: BackendError,
        context: String,
    },

    /// FlowFabric engine error — typed sum over Lua error codes + transport
    /// faults. See [`EngineError`] for the variant-granularity contract.
    /// Replaces the previous `Script(ScriptError)` carrier (#58.6).
    ///
    /// `Box`ed to keep `SdkError`'s stack footprint small: the richest
    /// variant (`ConflictKind::DependencyAlreadyExists { existing:
    /// EdgeSnapshot }`) is ~200 bytes. Boxing keeps `Result<T, SdkError>`
    /// at the same width every other variant pays.
    #[error("engine: {0}")]
    Engine(Box<EngineError>),

    /// Configuration error. `context` identifies the call site / logical
    /// operation (e.g. `"describe_execution: exec_core"`, `"admin_client"`).
    /// `field` names the specific offending field when the error is
    /// field-scoped (e.g. `Some("public_state")`), or `None` for
    /// whole-object validation (e.g. `"at least one lane is required"`).
    /// `message` carries dynamic detail: source-error rendering, the
    /// offending raw value, etc.
    #[error("{}", fmt_config(.context, .field.as_deref(), .message))]
    Config {
        context: String,
        field: Option<String>,
        message: String,
    },

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
    ///   milliseconds, not a retry loop with backoff for a backend
    ///   transport failure.
    /// * [`SdkError::backend_kind`] returns `None` — this is not a
    ///   backend transport error, so there is no
    ///   [`BackendErrorKind`] to inspect. Callers that fan out on
    ///   `backend_kind()` should match `WorkerAtCapacity` explicitly
    ///   (or use `is_retryable()`).
    ///
    /// [`FlowFabricWorker::claim_from_grant`]: crate::FlowFabricWorker::claim_from_grant
    /// [`FlowFabricWorker::claim_from_reclaim_grant`]: crate::FlowFabricWorker::claim_from_reclaim_grant
    #[error("worker at capacity: max_concurrent_tasks reached")]
    WorkerAtCapacity,

    /// HTTP transport error from the admin REST surface. Carries
    /// the underlying `reqwest::Error` via `#[source]` so callers
    /// can inspect `is_timeout()` / `is_connect()` / etc. for
    /// finer-grained retry logic. Distinct from
    /// [`SdkError::Backend`] — this fires on the HTTP/JSON surface,
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

/// Renders `SdkError::Config` as `config: <context>[.<field>]: <message>`.
/// The `field` slot is omitted when `None` (whole-object validation).
fn fmt_config(context: &str, field: Option<&str>, message: &str) -> String {
    match field {
        Some(f) => format!("config: {context}.{f}: {message}"),
        None => format!("config: {context}: {message}"),
    }
}

/// Lift a native `ferriskey::Error` into [`SdkError::Backend`] via
/// [`ff_backend_valkey::backend_error_from_ferriskey`] (#88). Keeps
/// `?`-propagation ergonomic at FCALL/transport call sites while
/// the public variant stays backend-agnostic.
#[cfg(feature = "valkey-default")]
impl From<ferriskey::Error> for SdkError {
    fn from(err: ferriskey::Error) -> Self {
        Self::Backend(ff_backend_valkey::backend_error_from_ferriskey(&err))
    }
}

/// Build an [`SdkError::BackendContext`] from a native
/// `ferriskey::Error` and a call-site label, preserving the
/// backend-agnostic shape on the public surface (#88).
#[cfg(feature = "valkey-default")]
pub(crate) fn backend_context(
    err: ferriskey::Error,
    context: impl Into<String>,
) -> SdkError {
    SdkError::BackendContext {
        source: ff_backend_valkey::backend_error_from_ferriskey(&err),
        context: context.into(),
    }
}

/// Preserves the ergonomic `?`-propagation from FCALL sites that
/// return `Result<_, ScriptError>`. Routes through `EngineError`'s
/// typed classification so every call site gets the same
/// variant-level detail without hand-written conversion.
impl From<ff_script::error::ScriptError> for SdkError {
    fn from(err: ff_script::error::ScriptError) -> Self {
        // ff-script's `From<ScriptError> for EngineError` owns the
        // mapping table (#58.6). See `ff_script::engine_error_ext`.
        Self::Engine(Box::new(EngineError::from(err)))
    }
}

impl From<EngineError> for SdkError {
    fn from(err: EngineError) -> Self {
        Self::Engine(Box::new(err))
    }
}

impl SdkError {
    /// Returns the classified [`BackendErrorKind`] if this error
    /// carries a backend transport fault. Covers the direct
    /// [`SdkError::Backend`] / [`SdkError::BackendContext`] variants
    /// and `Engine(EngineError::Transport { .. })` via the
    /// ScriptError-aware downcast in `ff_script::engine_error_ext`.
    ///
    /// Renamed from `valkey_kind` in #88 — the previous return type
    /// `Option<ferriskey::ErrorKind>` leaked ferriskey into every
    /// consumer doing retry classification.
    pub fn backend_kind(&self) -> Option<BackendErrorKind> {
        match self {
            Self::Backend(be) => Some(be.kind()),
            Self::BackendContext { source, .. } => Some(source.kind()),
            #[cfg(feature = "valkey-default")]
            Self::Engine(e) => ff_script::engine_error_ext::valkey_kind(e)
                .map(ff_backend_valkey::classify_ferriskey_kind),
            #[cfg(not(feature = "valkey-default"))]
            Self::Engine(_) => None,
            // HTTP/admin-surface errors carry no backend fault;
            // the admin path never touches the backend directly from
            // the SDK side. Use `AdminApi.kind` for the server-supplied
            // label when present.
            Self::Config { .. }
            | Self::WorkerAtCapacity
            | Self::Http { .. }
            | Self::AdminApi { .. } => None,
        }
    }

    /// Whether this error is safely retryable by a caller. For backend
    /// transport variants, delegates to
    /// [`BackendErrorKind::is_retryable`]. For `Engine` errors, returns
    /// `true` iff the typed classification is
    /// `ErrorClass::Retryable`. `Config` errors are never retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Backend(be) | Self::BackendContext { source: be, .. } => {
                be.kind().is_retryable()
            }
            Self::Engine(e) => {
                matches!(
                    ff_script::engine_error_ext::class(e),
                    ff_core::error::ErrorClass::Retryable
                )
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
            // retryable-status set (429, 502, 503, 504). 5xxs without
            // a hint are conservatively non-retryable — the caller can
            // override with `AdminApi.status`-based logic if needed.
            // 502 covers reverse-proxy transients (ALB/nginx returning
            // Bad Gateway when ff-server restarts mid-request).
            Self::AdminApi {
                status, retryable, ..
            } => retryable.unwrap_or(matches!(*status, 429 | 502 | 503 | 504)),
            Self::Config { .. } => false,
        }
    }
}

#[cfg(all(test, feature = "valkey-default"))]
mod tests {
    use super::*;
    use ferriskey::ErrorKind;
    use ff_script::error::ScriptError;

    fn mk_fk_err(kind: ErrorKind) -> ferriskey::Error {
        ferriskey::Error::from((kind, "synthetic"))
    }

    #[test]
    fn backend_kind_direct_and_context() {
        assert_eq!(
            SdkError::from(mk_fk_err(ErrorKind::IoError)).backend_kind(),
            Some(BackendErrorKind::Transport)
        );
        assert_eq!(
            crate::backend_context(mk_fk_err(ErrorKind::BusyLoadingError), "connect")
                .backend_kind(),
            Some(BackendErrorKind::BusyLoading)
        );
    }

    #[test]
    fn backend_kind_delegates_through_engine_transport() {
        let err = SdkError::from(ScriptError::Valkey(mk_fk_err(ErrorKind::ClusterDown)));
        assert_eq!(err.backend_kind(), Some(BackendErrorKind::Cluster));
    }

    #[test]
    fn backend_kind_none_for_lua_and_config() {
        assert_eq!(
            SdkError::from(ScriptError::LeaseExpired).backend_kind(),
            None
        );
        assert_eq!(
            SdkError::Config {
                context: "worker_config".into(),
                field: Some("bearer_token".into()),
                message: "bad host".into(),
            }
            .backend_kind(),
            None
        );
    }

    #[test]
    fn is_retryable_transport() {
        // Transport-bucketed kinds (IoError, FatalSend/Receive,
        // ProtocolDesync) are retryable under the #88 classifier.
        assert!(SdkError::from(mk_fk_err(ErrorKind::IoError)).is_retryable());
        // Auth-bucketed kinds are terminal.
        assert!(!SdkError::from(mk_fk_err(ErrorKind::AuthenticationFailed)).is_retryable());
        // Protocol-bucketed kinds (ResponseError, ParseError, TypeError,
        // InvalidClientConfig, etc.) are terminal.
        assert!(!SdkError::from(mk_fk_err(ErrorKind::ResponseError)).is_retryable());
    }

    #[test]
    fn is_retryable_engine_delegates_to_class() {
        // NoEligibleExecution is classified Retryable via EngineError::class().
        assert!(SdkError::from(ScriptError::NoEligibleExecution).is_retryable());
        // StaleLease is Terminal.
        assert!(!SdkError::from(ScriptError::StaleLease).is_retryable());
        // Transport(Valkey(IoError)) is Retryable via class() delegation.
        assert!(
            SdkError::from(ScriptError::Valkey(mk_fk_err(ErrorKind::IoError))).is_retryable()
        );
    }

    /// Regression (#98): `SdkError::Config` carries `context`, optional
    /// `field`, and `message` separately so consumers can pattern-match on
    /// the offending field without parsing the Display string. Test covers
    /// both the field-scoped and whole-object renderings.
    #[test]
    fn config_structured_fields_render_and_match() {
        let with_field = SdkError::Config {
            context: "admin_client".into(),
            field: Some("bearer_token".into()),
            message: "is empty or all-whitespace".into(),
        };
        assert_eq!(
            with_field.to_string(),
            "config: admin_client.bearer_token: is empty or all-whitespace"
        );
        assert!(matches!(
            &with_field,
            SdkError::Config { field: Some(f), .. } if f == "bearer_token"
        ));

        let whole_object = SdkError::Config {
            context: "worker_config".into(),
            field: None,
            message: "at least one lane is required".into(),
        };
        assert_eq!(
            whole_object.to_string(),
            "config: worker_config: at least one lane is required"
        );
        assert!(matches!(
            &whole_object,
            SdkError::Config { field: None, .. }
        ));
    }

    #[test]
    fn is_retryable_config_false() {
        assert!(
            !SdkError::Config {
                context: "worker_config".into(),
                field: None,
                message: "at least one lane is required".into(),
            }
            .is_retryable()
        );
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
        // 502 covers ALB/nginx Bad Gateway transients on ff-server
        // restart — same retry-is-safe as 503/504 because rotation
        // is idempotent server-side.
        for s in [429u16, 502, 503, 504] {
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
        assert_eq!(err.backend_kind(), None);
    }
}
