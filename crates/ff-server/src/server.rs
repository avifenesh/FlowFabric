use std::sync::Arc;
use std::time::Duration;

use ferriskey::ClientBuilder;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, BudgetStatus, CancelExecutionArgs,
    CancelExecutionResult, CancelFlowArgs, CancelFlowResult, ChangePriorityResult,
    CreateBudgetArgs, CreateBudgetResult, CreateExecutionArgs, CreateExecutionResult,
    CreateFlowArgs, CreateFlowResult, CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    ApplyDependencyToChildArgs, ApplyDependencyToChildResult,
    DeliverSignalArgs, DeliverSignalResult, ExecutionInfo,
    ListExecutionsPage, ReplayExecutionResult,
    ReportUsageArgs, ReportUsageResult, ResetBudgetResult,
    RevokeLeaseResult,
    StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
use ff_core::partition::{execution_partition, flow_partition, PartitionConfig};
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_engine::Engine;
use ff_script::retry::is_retryable_kind;

use crate::config::{BackendKind, ServerConfig};

/// RFC-017 §9.0: backends that may boot as of this Stage. Postgres
/// joins at Stage E (v0.8.0). Compiled into the binary by design —
/// see RFC-017 §9.0 "Fleet-wide cutover requirement" for the
/// rolling-upgrade implication.
const BACKEND_STAGE_READY: &[&str] = &["valkey", "postgres"];

/// Upper bound on `member_execution_ids` returned in the
/// [`CancelFlowResult::Cancelled`] response when the flow was already in a
/// terminal state (idempotent retry). The first (non-idempotent) cancel call
/// returns the full list; retries only need a sample.
const ALREADY_TERMINAL_MEMBER_CAP: usize = 1000;

/// FlowFabric server — connects everything together.
///
/// Manages the Valkey connection, Lua library loading, background scanners,
/// and provides a minimal API for Phase 1.
pub struct Server {
    /// Server-wide Semaphore(1) gating admin rotate calls. Legitimate
    /// operators rotate ~monthly and can afford to serialize; concurrent
    /// rotate requests are an attack or misbehaving script. Holding the
    /// permit also guards against interleaved partial rotations on the
    /// Server side of the per-partition locks, surfacing contention as
    /// HTTP 429 instead of silently queueing and blowing past the 120s
    /// HTTP timeout. See `rotate_waitpoint_secret` handler.
    admin_rotate_semaphore: Arc<tokio::sync::Semaphore>,
    /// Valkey engine (14 scanners). `None` on the Postgres boot path
    /// — engine scanners are Valkey-only (RFC-017 Wave 8 Stage E1;
    /// Postgres reconcilers run out-of-process via
    /// `ff-backend-postgres::reconcilers` + a separate scanner
    /// supervisor that Stage E2/E3 will wire).
    engine: Option<Engine>,
    config: ServerConfig,
    // RFC-017 Wave 8 Stage E3 (§9.3 / F8+Q4): `Server::scheduler` field
    // removed. The Valkey `ff_scheduler::Scheduler` is owned by
    // `ValkeyBackend` (installed via `with_scheduler` during boot);
    // the Postgres twin is constructed per-call inside
    // `PostgresBackend::claim_for_worker`. Server-side dispatch of
    // `claim_for_worker` now flows through `self.backend` only —
    // trait cutover per §4 row 9.
    /// Background tasks spawned by async handlers (e.g. cancel_flow member
    /// dispatch). Drained on shutdown with a bounded timeout.
    background_tasks: Arc<AsyncMutex<JoinSet<()>>>,
    /// PR-94: observability registry. Always present; the no-op shim
    /// takes zero runtime cost when the `observability` feature is
    /// off, and the real OTEL-backed registry is passed in via
    /// [`Server::start_with_metrics`] when on. Same `Arc` is shared
    /// with [`Engine::start_with_metrics`] and
    /// [`ff_scheduler::Scheduler::with_metrics`] so a single scrape
    /// sees everything the process produces.
    metrics: Arc<ff_observability::Metrics>,
    /// RFC-017 Stage A: backend trait object for the data-plane
    /// migration. Dual-field posture — the existing `client` /
    /// `tail_client` / `engine` / `scheduler` fields still serve the
    /// unmigrated handlers during Stages A-D; migrated handlers
    /// dispatch here. Stages B-D progressively retire the Client
    /// fields per RFC-017 §9.
    backend: Arc<dyn EngineBackend>,
}

/// Server error type.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Backend transport error. Previously wrapped `ferriskey::Error`
    /// directly (#88); now carries a backend-agnostic
    /// [`ff_core::BackendError`] so consumers match on
    /// [`ff_core::BackendErrorKind`] instead of ferriskey's native
    /// taxonomy. The ferriskey → [`ff_core::BackendError`] mapping
    /// lives in `ff_backend_valkey::backend_error_from_ferriskey`.
    #[error("backend: {0}")]
    Backend(#[from] ff_core::BackendError),
    /// Backend error with additional context. Previously
    /// `ValkeyContext { source: ferriskey::Error }` (#88).
    #[error("backend ({context}): {source}")]
    BackendContext {
        #[source]
        source: ff_core::BackendError,
        context: String,
    },
    #[error("config: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("library load: {0}")]
    LibraryLoad(#[from] ff_script::loader::LoadError),
    #[error("partition mismatch: {0}")]
    PartitionMismatch(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("operation failed: {0}")]
    OperationFailed(String),
    #[error("script: {0}")]
    Script(String),
    /// Server-wide concurrency limit reached on a labelled pool. Surfaces
    /// as HTTP 429 at the REST boundary so load balancers and clients can
    /// retry with backoff. The `source` label ("stream_ops", "admin_rotate",
    /// etc.) identifies WHICH pool is exhausted so operators aren't
    /// misled by a single "tail unavailable" string when the real fault
    /// is rotation contention.
    ///
    /// Fields: (source_label, max_permits).
    #[error("too many concurrent {0} calls (max: {1})")]
    ConcurrencyLimitExceeded(&'static str, u32),
    /// Detected Valkey version is below the RFC-011 §13 minimum. The engine
    /// depends on Valkey Functions (stabilized in 7.2), RESP3 (7.2+), and
    /// hash-tag routing; older versions do not implement the required
    /// primitives. Operator action: upgrade Valkey.
    #[error(
        "valkey version too low: detected {detected}, required >= {required} (RFC-011 §13)"
    )]
    ValkeyVersionTooLow {
        detected: String,
        required: String,
    },
    /// RFC-017 §9.0 hard-gate: selected backend is not yet permitted
    /// to boot in this `ff-server` binary. Operator action is to
    /// either (a) select `FF_BACKEND=valkey`, or (b) upgrade to a
    /// Stage E binary once v0.8.0 ships.
    ///
    /// `stage` names the current stage ("A"/"B"/"C"/"D") so operator
    /// tooling can correlate the refusal with the migration plan.
    #[error(
        "backend not ready: {backend} (not in BACKEND_STAGE_READY; current stage {stage}). \
         Set FF_BACKEND=valkey or FF_BACKEND=postgres."
    )]
    BackendNotReady {
        backend: &'static str,
        stage: &'static str,
    },
    /// RFC-017 Stage A: an `EngineBackend` trait method returned a
    /// typed error that is not one of the specific business outcomes
    /// existing `ServerError` variants model. Includes transport
    /// faults, validation/corruption, and `Unavailable` for methods
    /// the backend has not implemented yet.
    ///
    /// Stage B/C migrations may refine individual arms into richer
    /// `ServerError` variants as more handlers route through the
    /// trait; Stage A keeps this catch-all so the migration lands
    /// additively. Boxed to keep `ServerError` small (clippy
    /// `result_large_err` — `EngineError` is ~200 bytes).
    #[error("engine: {0}")]
    Engine(#[from] Box<ff_core::engine_error::EngineError>),
}

/// Lift a native `ferriskey::Error` into [`ServerError::Backend`] via
/// [`ff_backend_valkey::backend_error_from_ferriskey`] (#88). Keeps
/// `?`-propagation ergonomic at ferriskey call sites while the
/// public variant stays backend-agnostic.
impl From<ferriskey::Error> for ServerError {
    fn from(err: ferriskey::Error) -> Self {
        Self::Backend(ff_backend_valkey::backend_error_from_ferriskey(&err))
    }
}

/// Lift an unboxed `EngineError` into [`ServerError::Engine`]. The
/// variant stores a `Box<EngineError>` to keep `ServerError` small
/// (clippy `result_large_err`); this `From` restores `?`-propagation
/// from trait-dispatched handler paths.
impl From<ff_core::engine_error::EngineError> for ServerError {
    fn from(err: ff_core::engine_error::EngineError) -> Self {
        Self::Engine(Box::new(err))
    }
}

/// Build a [`ServerError::BackendContext`] from a native
/// `ferriskey::Error` and a call-site label (#88).
pub(crate) fn backend_context(
    err: ferriskey::Error,
    context: impl Into<String>,
) -> ServerError {
    ServerError::BackendContext {
        source: ff_backend_valkey::backend_error_from_ferriskey(&err),
        context: context.into(),
    }
}

impl ServerError {
    /// Returns the classified [`ff_core::BackendErrorKind`] if this
    /// error carries a backend transport fault. Covers direct
    /// Backend variants and library-load failures.
    ///
    /// Renamed from `valkey_kind` in #88 — the previous return type
    /// `Option<ferriskey::ErrorKind>` leaked ferriskey into every
    /// consumer doing retry/error classification.
    pub fn backend_kind(&self) -> Option<ff_core::BackendErrorKind> {
        match self {
            Self::Backend(be) | Self::BackendContext { source: be, .. } => Some(be.kind()),
            Self::LibraryLoad(e) => e
                .valkey_kind()
                .map(ff_backend_valkey::classify_ferriskey_kind),
            // RFC-017 Stage A: Engine(EngineError) arm is intentionally
            // lumped in with the rest (no backend_kind — variants like
            // Validation / NotFound are business-logic, and Transport
            // variants already surface under Backend upstream in most
            // paths). Stage B/C will revisit when more handlers route
            // through the trait.
            _ => None,
        }
    }

    /// Whether this error is safely retryable by a caller. For
    /// backend transport variants, delegates to
    /// [`ff_core::BackendErrorKind::is_retryable`]. Business-logic
    /// rejections (NotFound, InvalidInput, OperationFailed, Script,
    /// Config, PartitionMismatch) return false — those won't change
    /// on retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Backend(be) | Self::BackendContext { source: be, .. } => {
                be.kind().is_retryable()
            }
            Self::LibraryLoad(load_err) => load_err
                .valkey_kind()
                .map(is_retryable_kind)
                .unwrap_or(false),
            Self::Config(_)
            | Self::PartitionMismatch(_)
            | Self::NotFound(_)
            | Self::InvalidInput(_)
            | Self::OperationFailed(_)
            | Self::Script(_) => false,
            // Back off and retry — the bound is a server-side permit pool,
            // so the retry will succeed once a permit frees up. Applies
            // equally to stream ops, admin rotate, etc.
            Self::ConcurrencyLimitExceeded(_, _) => true,
            // Operator must upgrade Valkey; a retry at the caller won't help.
            Self::ValkeyVersionTooLow { .. } => false,
            // Operator must change FF_BACKEND; not retryable.
            Self::BackendNotReady { .. } => false,
            // EngineError's classification helpers handle transport
            // retry semantics; mirror them so trait-dispatched handlers
            // keep the same retry policy as the Client path.
            Self::Engine(e) => matches!(
                e.as_ref(),
                EngineError::Transport { .. } | EngineError::Contextual { .. }
            ),
        }
    }
}

impl Server {
    /// Start the FlowFabric server.
    ///
    /// Boot sequence:
    /// 1. Connect to Valkey
    /// 2. Validate or create partition config key
    /// 3. Load the FlowFabric Lua library
    /// 4. Start engine (14 background scanners)
    pub async fn start(config: ServerConfig) -> Result<Self, ServerError> {
        Self::start_with_metrics(config, Arc::new(ff_observability::Metrics::new())).await
    }

    /// PR-94: boot the server with a shared observability registry.
    ///
    /// Scanner cycle + scheduler metrics record into this registry;
    /// `main.rs` threads the same handle into the router so `/metrics`
    /// exposes what the engine produces. The no-arg [`Server::start`]
    /// forwards here with a fresh `Metrics::new()` — under the default
    /// build that's the shim, under `observability` it's a real
    /// registry not shared with any HTTP route (useful for tests
    /// exercising the engine in isolation).
    ///
    /// **RFC-017 Stage A:** this path gates `config.backend` against
    /// [`BACKEND_STAGE_READY`] (refuses `BackendKind::Postgres` at
    /// boot per §9.0), dials the Valkey cluster through the legacy
    /// path, then synthesises an `Arc<ValkeyBackend>` around the
    /// dialed client and populates `Server.backend`. The dual-field
    /// posture is explicit through Stage D; Stage E retires the
    /// legacy Client fields. See [`Server::start_with_backend`] for
    /// the test-injection entry point that takes a caller-supplied
    /// backend.
    pub async fn start_with_metrics(
        config: ServerConfig,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Self, ServerError> {
        // RFC-017 §9.0 hard-gate. At v0.8.0 (Stage E4) both `valkey` and
        // `postgres` are ready; this check remains as defence-in-depth
        // so future backend additions must explicitly opt into the list.
        // The `FF_BACKEND_ACCEPT_UNREADY` / `FF_ENV=development` dev-mode
        // override was retired at Stage E4 because it is no longer
        // needed — both supported backends now boot without override.
        let label = config.backend.as_str();
        if !BACKEND_STAGE_READY.contains(&label) {
            return Err(ServerError::BackendNotReady {
                backend: match config.backend {
                    BackendKind::Postgres => "postgres",
                    BackendKind::Valkey => "valkey",
                },
                stage: "E4",
            });
        }

        // RFC-017 Wave 8 Stage E1: Postgres dial branch. Stage E4 flips
        // `BACKEND_STAGE_READY` to include "postgres"; until then the
        // dev-mode override above is the only way in. On the Postgres
        // path we skip the Valkey-specific engine/scanner + scheduler
        // construction entirely (E3 wires the scheduler twin).
        if matches!(config.backend, BackendKind::Postgres) {
            return Self::start_postgres_branch(config, metrics).await;
        }
        // Step 1: Connect to Valkey via ClientBuilder
        tracing::info!(
            host = %config.host, port = config.port,
            tls = config.tls, cluster = config.cluster,
            "connecting to Valkey"
        );
        let mut builder = ClientBuilder::new()
            .host(&config.host, config.port)
            .connect_timeout(Duration::from_secs(10))
            .request_timeout(Duration::from_millis(5000));
        if config.tls {
            builder = builder.tls();
        }
        if config.cluster {
            builder = builder.cluster();
        }
        let client = builder
            .build()
            .await
            .map_err(|e| crate::server::backend_context(e, "connect"))?;

        // Verify connectivity
        let pong: String = client
            .cmd("PING")
            .execute()
            .await
            .map_err(|e| crate::server::backend_context(e, "PING"))?;
        if pong != "PONG" {
            return Err(ServerError::OperationFailed(format!(
                "unexpected PING response: {pong}"
            )));
        }
        tracing::info!("Valkey connection established");

        // RFC-017 Wave 8 Stage D (§4 row 12): the five Valkey-specific
        // deployment-initialisation steps (version verify, partition
        // config, HMAC install, FUNCTION LOAD, lanes seed) now live
        // behind `ValkeyBackend::initialize_deployment`. Load-bearing
        // ordering contract preserved in that method's doc-comment.
        // The call is sequenced after the connection but before the
        // engine + backend wiring below so the pre-relocation boot
        // ordering is byte-for-byte identical.
        let init_backend = ff_backend_valkey::ValkeyBackend::from_client_and_partitions(
            client.clone(),
            config.partition_config,
        );
        init_backend
            .initialize_deployment(
                &config.waitpoint_hmac_secret,
                &config.lanes,
                config.skip_library_load,
            )
            .await
            .map_err(|e| ServerError::Engine(Box::new(e)))?;

        // Step 4: Start engine with scanners
        // Build a fresh EngineConfig rather than cloning (EngineConfig doesn't derive Clone).
        let engine_cfg = ff_engine::EngineConfig {
            partition_config: config.partition_config,
            lanes: config.lanes.clone(),
            lease_expiry_interval: config.engine_config.lease_expiry_interval,
            delayed_promoter_interval: config.engine_config.delayed_promoter_interval,
            index_reconciler_interval: config.engine_config.index_reconciler_interval,
            attempt_timeout_interval: config.engine_config.attempt_timeout_interval,
            suspension_timeout_interval: config.engine_config.suspension_timeout_interval,
            pending_wp_expiry_interval: config.engine_config.pending_wp_expiry_interval,
            retention_trimmer_interval: config.engine_config.retention_trimmer_interval,
            budget_reset_interval: config.engine_config.budget_reset_interval,
            budget_reconciler_interval: config.engine_config.budget_reconciler_interval,
            quota_reconciler_interval: config.engine_config.quota_reconciler_interval,
            unblock_interval: config.engine_config.unblock_interval,
            dependency_reconciler_interval: config.engine_config.dependency_reconciler_interval,
            flow_projector_interval: config.engine_config.flow_projector_interval,
            execution_deadline_interval: config.engine_config.execution_deadline_interval,
            cancel_reconciler_interval: config.engine_config.cancel_reconciler_interval,
            edge_cancel_dispatcher_interval: config.engine_config.edge_cancel_dispatcher_interval,
            edge_cancel_reconciler_interval: config.engine_config.edge_cancel_reconciler_interval,
            scanner_filter: config.engine_config.scanner_filter.clone(),
        };
        // Engine scanners keep running on the MAIN `client` mux — NOT on
        // `tail_client`. Scanner cadence is foreground-latency-coupled by
        // design (an incident that blocks all FCALLs should also visibly
        // block scanners), and keeping scanners off the tail client means a
        // long-poll tail can never starve lease-expiry, retention-trim,
        // etc. Do not change this without revisiting RFC-006 Impl Notes.
        // Build the Valkey completion backend (issue #90) and subscribe.
        // This replaces the pre-#90 `CompletionListenerConfig` path:
        // the wire subscription now lives in ff-backend-valkey, the
        // engine just consumes the resulting stream.
        let mut valkey_conn = ff_core::backend::ValkeyConnection::new(
            config.host.clone(),
            config.port,
        );
        valkey_conn.tls = config.tls;
        valkey_conn.cluster = config.cluster;
        let completion_backend = ff_backend_valkey::ValkeyBackend::from_client_partitions_and_connection(
            client.clone(),
            config.partition_config,
            valkey_conn,
        );
        let completion_stream = <ff_backend_valkey::ValkeyBackend as ff_core::completion_backend::CompletionBackend>::subscribe_completions(&completion_backend)
            .await
            .map_err(|e| ServerError::OperationFailed(format!(
                "subscribe_completions: {e}"
            )))?;

        let engine = Engine::start_with_completions(
            engine_cfg,
            client.clone(),
            metrics.clone(),
            completion_stream,
        );

        // RFC-017 Stage B: the dedicated `tail_client`, the
        // `stream_semaphore`, and the `xread_block_lock` moved into
        // `ValkeyBackend` (§6). After issue #204's switch to per-call
        // `duplicate_connection()` inside `tail_stream_impl`, the
        // dedicated tail mux + serialising mutex are no longer
        // required — each `XREAD BLOCK` gets its own socket — so
        // the encapsulated impl only needs the bounded semaphore to
        // preserve the existing 429-on-contention contract at the
        // REST boundary.
        tracing::info!(
            max_concurrent_stream_ops = config.max_concurrent_stream_ops,
            "stream-op semaphore lives inside ValkeyBackend (RFC-017 Stage B)"
        );

        // Admin surface warning. /v1/admin/* endpoints (waitpoint HMAC
        // rotation, etc.) are only protected by the global Bearer
        // middleware in api.rs — which is only installed when
        // config.api_token is set. Without FF_API_TOKEN, a public
        // deployment exposes secret rotation to anyone that can reach
        // the listen_addr. Warn loudly so operators can't miss it; we
        // don't fail-start because single-tenant dev uses this path.
        if config.api_token.is_none() {
            tracing::warn!(
                listen_addr = %config.listen_addr,
                "FF_API_TOKEN is unset — /v1/admin/* endpoints (including \
                 rotate-waitpoint-secret) are UNAUTHENTICATED. Set \
                 FF_API_TOKEN for any deployment reachable from untrusted \
                 networks."
            );
            // Explicit callout for the credential-bearing read endpoints.
            // Auditors grep for these on a per-endpoint basis; lumping
            // into the admin warning alone hides the fact that
            // /pending-waitpoints returns HMAC tokens and /result
            // returns completion payload bytes.
            tracing::warn!(
                listen_addr = %config.listen_addr,
                "FF_API_TOKEN is unset — GET /v1/executions/{{id}}/pending-waitpoints \
                 returns HMAC waitpoint_tokens (bearer credentials for signal delivery) \
                 and GET /v1/executions/{{id}}/result returns raw completion payload \
                 bytes (may contain PII). Both are UNAUTHENTICATED in this \
                 configuration."
            );
        }

        // Partition counts — post-RFC-011 there are three physical families.
        // Execution keys co-locate with their parent flow's partition (§2 of
        // the RFC), so `num_flow_partitions` governs both exec and flow
        // routing; no separate `num_execution_partitions` count exists.
        tracing::info!(
            flow_partitions = config.partition_config.num_flow_partitions,
            budget_partitions = config.partition_config.num_budget_partitions,
            quota_partitions = config.partition_config.num_quota_partitions,
            lanes = ?config.lanes.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            listen_addr = %config.listen_addr,
            "FlowFabric server started. Partitions (flow/budget/quota): {}/{}/{}. Scanners: 14 active.",
            config.partition_config.num_flow_partitions,
            config.partition_config.num_budget_partitions,
            config.partition_config.num_quota_partitions,
        );

        let scheduler = Arc::new(ff_scheduler::Scheduler::with_metrics(
            client.clone(),
            config.partition_config,
            metrics.clone(),
        ));

        // RFC-017 Stage A: synthesise an `Arc<ValkeyBackend>` around the
        // already-dialed client. Zero additional round-trips; the
        // backend shares the same ferriskey connection as the legacy
        // path so migrated + legacy handlers observe identical state.
        // Stage B relocates `tail_client` / `stream_semaphore` into
        // the backend; Stage E retires the Client fields entirely.
        let mut valkey_backend = ff_backend_valkey::ValkeyBackend::from_client_partitions_and_connection(
            client.clone(),
            config.partition_config,
            {
                let mut c = ff_core::backend::ValkeyConnection::new(
                    config.host.clone(),
                    config.port,
                );
                c.tls = config.tls;
                c.cluster = config.cluster;
                c
            },
        );
        // RFC-017 Stage B: size the backend's stream-op semaphore
        // before handing out the `Arc`. `get_mut` succeeds here
        // because `valkey_backend` is the sole `Arc` owner at this
        // point.
        if !valkey_backend.with_stream_semaphore_permits(config.max_concurrent_stream_ops) {
            return Err(ServerError::OperationFailed(
                "ValkeyBackend stream semaphore sizing failed (unexpected Arc sharing)".into(),
            ));
        }
        // RFC-017 Stage C: install the scheduler handle so the
        // backend's `claim_for_worker` trait impl dispatches through
        // it. Stage E3 removed the redundant `Server::scheduler`
        // field — the backend is the sole owner of the scheduler
        // after this install.
        if !valkey_backend.with_scheduler(scheduler) {
            return Err(ServerError::OperationFailed(
                "ValkeyBackend scheduler wiring failed (unexpected Arc sharing)".into(),
            ));
        }
        let backend: Arc<dyn EngineBackend> = valkey_backend as Arc<dyn EngineBackend>;

        Ok(Self {
            // Single-permit semaphore: only one rotate-waitpoint-secret can
            // be mid-flight server-wide. Attackers or misbehaving scripts
            // firing parallel rotations fail fast with 429 instead of
            // queueing HTTP handlers.
            admin_rotate_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            engine: Some(engine),
            config,
            background_tasks: Arc::new(AsyncMutex::new(JoinSet::new())),
            metrics,
            backend,
        })
    }

    /// RFC-017 Wave 8 Stage E1/E2: Postgres boot branch for
    /// [`Server::start_with_metrics`]. Called after the §9.0 hard-gate
    /// admitted the boot under the dev-mode override.
    ///
    /// **Scope (Stage E2):**
    /// * Dial Postgres + wrap an `Arc<PostgresBackend>` as the
    ///   `backend` field (all migrated HTTP handlers dispatch through
    ///   the trait — create_execution / create_flow / add member /
    ///   stage edge / apply dep / describe_flow / cancel_flow / etc.).
    /// * **No Valkey dial.** The E1 residual ambient-Valkey client is
    ///   retired in E2 — `Server::client` is gone, `cancel_flow_header`
    ///   / `ack_cancel_member` / `read_execution_info` /
    ///   `read_execution_state` / `fetch_waitpoint_token_v07` all flow
    ///   through the trait. Legacy reads that Postgres does not yet
    ///   implement surface as `EngineError::Unavailable` (Wave 9).
    /// * Skip engine + scheduler. Stage E3 wires the Postgres-specific
    ///   `claim_for_worker` via the trait; until then the scheduler
    ///   field stays `None`.
    /// * Run `apply_migrations` against the Postgres pool so an empty
    ///   database becomes usable without operator out-of-band steps.
    async fn start_postgres_branch(
        config: ServerConfig,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Self, ServerError> {
        if config.postgres.url.is_empty() {
            return Err(ServerError::InvalidInput(
                "FF_BACKEND=postgres requires FF_POSTGRES_URL".into(),
            ));
        }
        tracing::info!(
            pool_size = config.postgres.pool_size,
            "dialing Postgres backend (RFC-017 Stage E3)"
        );
        let mut pg_backend_arc = ff_backend_postgres::PostgresBackend::connect_with_metrics(
            config.postgres_config(),
            config.partition_config,
            metrics.clone(),
        )
        .await
        .map_err(|e| ServerError::Engine(Box::new(e)))?;

        // Apply schema migrations idempotently so an empty target
        // database becomes usable. The underlying pool is shared with
        // the backend — one pool, one migration run.
        ff_backend_postgres::apply_migrations(pg_backend_arc.pool())
            .await
            .map_err(|e| {
                ServerError::OperationFailed(format!("postgres apply_migrations: {e}"))
            })?;

        // RFC-017 Wave 8 Stage E3: spawn the six Postgres reconcilers
        // (attempt_timeout, lease_expiry, suspension_timeout,
        // dependency, edge_cancel_dispatcher, edge_cancel_reconciler)
        // as background tick loops owned by the backend. Drained on
        // `Server::shutdown` via `backend.shutdown_prepare(grace)`.
        let scanner_cfg = ff_backend_postgres::PostgresScannerConfig {
            attempt_timeout_interval: config.engine_config.attempt_timeout_interval,
            lease_expiry_interval: config.engine_config.lease_expiry_interval,
            suspension_timeout_interval: config.engine_config.suspension_timeout_interval,
            dependency_reconciler_interval: config.engine_config.dependency_reconciler_interval,
            edge_cancel_dispatcher_interval: config.engine_config.edge_cancel_dispatcher_interval,
            edge_cancel_reconciler_interval: config.engine_config.edge_cancel_reconciler_interval,
            dependency_stale_threshold_ms:
                ff_backend_postgres::PostgresScannerConfig::DEFAULT_DEP_STALE_MS,
            scanner_filter: config.engine_config.scanner_filter.clone(),
            partition_config: config.partition_config,
        };
        if !pg_backend_arc.with_scanners(scanner_cfg) {
            return Err(ServerError::OperationFailed(
                "PostgresBackend scanner install failed (unexpected Arc sharing)".into(),
            ));
        }

        let backend: Arc<dyn EngineBackend> = pg_backend_arc;

        tracing::info!(
            flow_partitions = config.partition_config.num_flow_partitions,
            lanes = ?config.lanes.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            "FlowFabric server started (Postgres backend, Stage E3). \
             6 Postgres reconcilers active; claim_for_worker routed to \
             PostgresScheduler. No ambient Valkey client."
        );

        Ok(Self {
            admin_rotate_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            engine: None,
            config,
            background_tasks: Arc::new(AsyncMutex::new(JoinSet::new())),
            metrics,
            backend,
        })
    }

    /// RFC-017 Stage A: test-injection + future-embedded-user entry
    /// point. Takes a caller-constructed `Arc<dyn EngineBackend>` +
    /// the Valkey connection/engine scaffolding
    /// [`Server::start_with_metrics`] normally dials for itself.
    ///
    /// **Stage A scope:** Stage A is still dual-field — the legacy
    /// `client` / `tail_client` / `engine` / `scheduler` fields are
    /// constructed here exactly as in the main boot path, because
    /// unmigrated handlers still need them. The caller-supplied
    /// `backend` populates the new trait-object field and services
    /// the handlers migrated in this stage (see RFC-017 §4
    /// migration table).
    ///
    /// **Stage D evolution:** once the boot path relocates into each
    /// backend's `connect_with_metrics` (RFC-017 §9 Stage D), this
    /// entry point becomes the sole constructor — `Server::start` and
    /// `Server::start_with_metrics` are thin shims that build the
    /// backend first, then forward here.
    ///
    /// Today (Stage A) this path is exercised by `MockBackend` in
    /// `tests/parity_stage_a.rs`; it does NOT replace the Valkey
    /// dial under the main binary.
    pub async fn start_with_backend(
        config: ServerConfig,
        backend: Arc<dyn EngineBackend>,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Self, ServerError> {
        // Stage A: forward through the legacy dial so unmigrated
        // handlers keep working, then overwrite `backend` with the
        // caller-supplied handle. Stage D rewires this so the
        // caller's backend drives the whole boot.
        let mut server = Self::start_with_metrics(config, metrics).await?;
        server.backend = backend;
        Ok(server)
    }

    /// RFC-017 Stage A: access the backend trait-object driving
    /// migrated handlers. Stable surface for tests that need to
    /// inspect the backend directly (e.g. `backend_label()`
    /// assertions). The Server will dispatch more handlers through
    /// this handle as Stages B-D land.
    pub fn backend(&self) -> &Arc<dyn EngineBackend> {
        &self.backend
    }

    /// PR-94: access the shared observability registry.
    pub fn metrics(&self) -> &Arc<ff_observability::Metrics> {
        &self.metrics
    }

    // RFC-017 Stage E2 (§9 Stage D bullet completed): `Server::client()`
    // accessor + underlying `Client` field both removed. External
    // callers route ping / healthz through the backend trait
    // (`self.backend.ping()` → `ValkeyBackend::ping`). The
    // `ff_cancel_flow` header FCALL, its `flow_already_terminal`
    // HMGET/SMEMBERS fallback, the per-member `ff_ack_cancel_member`
    // backlog drain, and the `get_execution*` / `fetch_waitpoint_token_v07`
    // reads are all reachable through the Stage E2 trait additions
    // (`cancel_flow_header`, `ack_cancel_member`, `read_execution_info`,
    // `read_execution_state`, `fetch_waitpoint_token_v07`).

    /// Get the server config.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Get the partition config.
    pub fn partition_config(&self) -> &PartitionConfig {
        &self.config.partition_config
    }

    // ── Minimal Phase 1 API ──

    /// Create a new execution. RFC-017 Stage D2: delegates through the
    /// backend trait. The KEYS/ARGV build + FCALL dispatch + result parse
    /// live verbatim in `ValkeyBackend::create_execution`.
    pub async fn create_execution(
        &self,
        args: &CreateExecutionArgs,
    ) -> Result<CreateExecutionResult, ServerError> {
        Ok(self.backend.create_execution(args.clone()).await?)
    }

    /// Cancel an execution. RFC-017 Stage D2: delegates through the
    /// backend trait.
    pub async fn cancel_execution(
        &self,
        args: &CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, ServerError> {
        Ok(self.backend.cancel_execution(args.clone()).await?)
    }

    /// Get the public state of an execution.
    ///
    /// Reads `public_state` from the exec_core hash. Returns the parsed
    /// PublicState enum. If the execution is not found, returns an error.
    pub async fn get_execution_state(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<PublicState, ServerError> {
        // RFC-017 Stage E2: routed through the backend trait.
        match self.backend.read_execution_state(execution_id).await? {
            Some(s) => Ok(s),
            None => Err(ServerError::NotFound(format!(
                "execution not found: {execution_id}"
            ))),
        }
    }

    /// Read the raw result payload written by `ff_complete_execution`.
    ///
    /// The Lua side stores the payload at `ctx.result()` via plain `SET`.
    /// No FCALL — this is a direct GET; returns `Ok(None)` when the
    /// execution is missing, not yet complete, or (in a future
    /// retention-policy world) when the result was trimmed.
    ///
    /// # Contract vs `get_execution_state`
    ///
    /// `get_execution_state` is the authoritative completion signal. If
    /// a caller observes `state == completed` but `get_execution_result`
    /// returns `None`, the result bytes are unavailable — not a caller
    /// bug and not a server bug, just the retention policy trimming the
    /// blob. V1 sets no retention, so callers on v1 can treat
    /// `state == completed` + `Ok(None)` as a server bug.
    ///
    /// # Ordering
    ///
    /// Callers MUST wait for `state == completed` before calling this
    /// method; polls issued before the state transition may hit a
    /// narrow window where the completion Lua has written
    /// `public_state = completed` but the `result` key SET is still
    /// on-wire. The current Lua `ff_complete_execution` writes both in
    /// the same atomic script, so the window is effectively zero for
    /// direct callers — but retries via `ff_replay_execution` open it
    /// briefly.
    pub async fn get_execution_result(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Option<Vec<u8>>, ServerError> {
        // RFC-017 Stage E2: routed through the backend trait. The
        // Valkey impl preserves binary-safe semantics via ferriskey's
        // `Vec<u8>` FromValue; Postgres returns Unavailable until the
        // result-store migration lands.
        Ok(self.backend.get_execution_result(execution_id).await?)
    }


    // ── Budget / Quota API ──

    /// Create a new budget policy.
    /// Create a new budget policy. RFC-017 Stage D2: delegates through
    /// the backend trait.
    pub async fn create_budget(
        &self,
        args: &CreateBudgetArgs,
    ) -> Result<CreateBudgetResult, ServerError> {
        Ok(self.backend.create_budget(args.clone()).await?)
    }

    /// Create a new quota/rate-limit policy. RFC-017 Stage D2: delegates
    /// through the backend trait.
    pub async fn create_quota_policy(
        &self,
        args: &CreateQuotaPolicyArgs,
    ) -> Result<CreateQuotaPolicyResult, ServerError> {
        Ok(self.backend.create_quota_policy(args.clone()).await?)
    }

    /// Read-only budget status for operator visibility. RFC-017 Stage
    /// D2: delegates through the backend trait.
    pub async fn get_budget_status(
        &self,
        budget_id: &BudgetId,
    ) -> Result<BudgetStatus, ServerError> {
        Ok(self.backend.get_budget_status(budget_id).await?)
    }

    /// Report usage against a budget and check limits. RFC-017 Stage D2:
    /// delegates through the backend trait's admin variant
    /// (`report_usage_admin` — no worker handle required on the admin
    /// path).
    pub async fn report_usage(
        &self,
        budget_id: &BudgetId,
        args: &ReportUsageArgs,
    ) -> Result<ReportUsageResult, ServerError> {
        let mut admin_args = ff_core::contracts::ReportUsageAdminArgs::new(
            args.dimensions.clone(),
            args.deltas.clone(),
            args.now,
        );
        if let Some(key) = args.dedup_key.as_ref() {
            admin_args = admin_args.with_dedup_key(key.clone());
        }
        Ok(self.backend.report_usage_admin(budget_id, admin_args).await?)
    }

    /// Reset a budget's usage counters and schedule the next reset.
    /// RFC-017 Stage D2: delegates through the backend trait.
    pub async fn reset_budget(
        &self,
        budget_id: &BudgetId,
    ) -> Result<ResetBudgetResult, ServerError> {
        let args = ff_core::contracts::ResetBudgetArgs {
            budget_id: budget_id.clone(),
            now: TimestampMs::now(),
        };
        Ok(self.backend.reset_budget(args).await?)
    }

    // ── Flow API ──

    /// Create a new flow container. RFC-017 Stage D2: delegates through
    /// the backend trait.
    pub async fn create_flow(
        &self,
        args: &CreateFlowArgs,
    ) -> Result<CreateFlowResult, ServerError> {
        Ok(self.backend.create_flow(args.clone()).await?)
    }

    /// Add an execution to a flow.
    ///
    /// # Atomic single-FCALL commit (RFC-011 §7.3)
    ///
    /// Post-RFC-011 phase-3, exec_core co-locates with flow_core under
    /// hash-tag routing (both hash to `{fp:N}` via the exec id's
    /// embedded partition). A single atomic FCALL writes:
    ///
    ///   - `members_set` SADD (flow membership)
    ///   - `exec_core.flow_id` HSET (back-pointer)
    ///   - `flow_index` SADD (self-heal)
    ///   - `flow_core` HINCRBY node_count / graph_revision +
    ///     HSET last_mutation_at
    ///
    /// All four writes commit atomically or none do (Valkey scripting
    /// contract: validates-before-writing in the Lua body means
    /// `flow_not_found` / `flow_already_terminal` early-returns fire
    /// BEFORE any `redis.call()` mutation, and a mid-body error after
    /// writes is not expected because all writes are on the same slot).
    ///
    /// The pre-RFC-011 two-phase contract (membership FCALL on `{fp:N}` plus separate HSET on `{p:N}`), orphan window, and reconciliation-scanner plan (issue #21, now superseded) are all retired.
    ///
    /// # Consumer contract
    ///
    /// The caller's `args.execution_id` **must** be co-located with
    /// `args.flow_id`'s partition — i.e. minted via
    /// `ExecutionId::for_flow(&args.flow_id, config)`. Passing a
    /// `solo`-minted id (or any exec id hashing to a different
    /// `{fp:N}` than the flow's) will fail at the Valkey level with
    /// `CROSSSLOT` on a clustered deploy.
    ///
    /// Callers with a flow context in scope always use `for_flow`;
    /// this is the only supported mint path for flow-member execs
    /// post-RFC-011. Test fixtures that pre-date the co-location
    /// contract use `TestCluster::new_execution_id_on_partition` to
    /// pin to a specific hash-tag index for `fcall_create_flow`-style
    /// helpers that hard-code their flow partition.
    pub async fn add_execution_to_flow(
        &self,
        args: &AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, ServerError> {
        // Preserve the typed `ServerError::PartitionMismatch` pre-flight
        // check — the backend trait's implementation returns an
        // `EngineError` on CROSSSLOT, which would surface as
        // `ServerError::Engine(_)` and hide the consumer-contract
        // violation. Keep the explicit check at the facade boundary.
        let flow_part = flow_partition(&args.flow_id, &self.config.partition_config);
        let exec_part = execution_partition(&args.execution_id, &self.config.partition_config);
        if exec_part.index != flow_part.index {
            return Err(ServerError::PartitionMismatch(format!(
                "add_execution_to_flow: execution_id's partition {exec_p} != flow_id's partition {flow_p}. \
                 Post-RFC-011 §7.3 co-location requires mint via `ExecutionId::for_flow(&flow_id, config)` \
                 so the exec's hash-tag matches the flow's `{{fp:N}}`.",
                exec_p = exec_part.index,
                flow_p = flow_part.index,
            )));
        }
        Ok(self.backend.add_execution_to_flow(args.clone()).await?)
    }

    /// Cancel a flow.
    ///
    /// Flips `public_flow_state` to `cancelled` atomically via
    /// `ff_cancel_flow` on `{fp:N}`. For `cancel_all` policy, member
    /// executions must be cancelled cross-partition; this dispatch runs in
    /// the background and the call returns [`CancelFlowResult::CancellationScheduled`]
    /// immediately. For all other policies (or flows with no members), or
    /// when the flow was already in a terminal state (idempotent retry),
    /// the call returns [`CancelFlowResult::Cancelled`].
    ///
    /// Clients that need synchronous completion can call [`Self::cancel_flow_wait`].
    ///
    /// # Backpressure
    ///
    /// Each call that hits the async dispatch path spawns a new task into
    /// the shared background `JoinSet`. Rapid repeated calls against the
    /// same flow will spawn *multiple* overlapping dispatch tasks. This is
    /// not a correctness issue — each member cancel is idempotent and
    /// terminal flows short-circuit via [`ff_core::contracts::CancelFlowHeader::AlreadyTerminal`]
    /// — but heavy burst callers should either use `?wait=true` (serialises
    /// the dispatch on the HTTP thread, giving natural backpressure) or
    /// implement client-side deduplication on `flow_id`. The `JoinSet` is
    /// drained with a 15s timeout on [`Self::shutdown`], so very long
    /// dispatch tails may be aborted during graceful shutdown.
    ///
    /// # Orphan-member semantics on shutdown abort
    ///
    /// If shutdown fires `JoinSet::abort_all()` after its drain timeout
    /// while a dispatch loop is mid-iteration, the already-issued
    /// `ff_cancel_execution` FCALLs (atomic Lua) complete cleanly with
    /// `terminal_outcome = cancelled` and the caller-supplied reason. The
    /// members not yet visited are abandoned mid-loop. They remain in
    /// whichever state they were in (active/eligible/suspended) until the
    /// natural lifecycle scanners reach them: active leases expire
    /// (`lease_expiry`) and attempt-timeout them to `expired`, suspended
    /// members time out to `skipped`, eligible ones sit until retention
    /// trim. So no orphan state — but the terminal_outcome for the
    /// abandoned members will be `expired`/`skipped` rather than
    /// `cancelled`, and the operator-supplied `reason` is lost for them.
    /// Audit tooling that requires reason fidelity across shutdowns should
    /// use `?wait=true`.
    pub async fn cancel_flow(
        &self,
        args: &CancelFlowArgs,
    ) -> Result<CancelFlowResult, ServerError> {
        self.cancel_flow_inner(args, false).await
    }

    /// Cancel a flow and wait for all member cancellations to complete
    /// inline. Slower than [`Self::cancel_flow`] for large flows, but
    /// guarantees every member is in a terminal state on return.
    pub async fn cancel_flow_wait(
        &self,
        args: &CancelFlowArgs,
    ) -> Result<CancelFlowResult, ServerError> {
        self.cancel_flow_inner(args, true).await
    }

    async fn cancel_flow_inner(
        &self,
        args: &CancelFlowArgs,
        wait: bool,
    ) -> Result<CancelFlowResult, ServerError> {
        // RFC-017 Stage E2: the header FCALL + AlreadyTerminal fetch
        // now dispatch through the backend trait. The Server no longer
        // owns a ferriskey `Client`; `self.backend.cancel_flow_header`
        // encapsulates the Valkey-specific FCALL + reload-on-failover
        // + HMGET/SMEMBERS-for-AlreadyTerminal work previously inlined
        // here.
        let header = self.backend.cancel_flow_header(args.clone()).await?;

        let (policy, members) = match header {
            ff_core::contracts::CancelFlowHeader::Cancelled {
                cancellation_policy,
                member_execution_ids,
            } => (cancellation_policy, member_execution_ids),
            // Idempotent retry: flow was already cancelled/completed/failed.
            // Return Cancelled with the *stored* policy + (capped) member
            // list so observability tooling gets the real historical state
            // rather than echoing the caller's retry intent. The backend
            // has already done the HMGET + SMEMBERS; the Server just caps
            // the member list to bound wire bandwidth.
            ff_core::contracts::CancelFlowHeader::AlreadyTerminal {
                stored_cancellation_policy,
                stored_cancel_reason,
                member_execution_ids,
            } => {
                let total_members = member_execution_ids.len();
                let stored_members: Vec<String> = member_execution_ids
                    .into_iter()
                    .take(ALREADY_TERMINAL_MEMBER_CAP)
                    .collect();
                tracing::debug!(
                    flow_id = %args.flow_id,
                    stored_policy = stored_cancellation_policy.as_deref().unwrap_or(""),
                    stored_reason = stored_cancel_reason.as_deref().unwrap_or(""),
                    total_members,
                    returned_members = stored_members.len(),
                    "cancel_flow: flow already terminal, returning idempotent Cancelled"
                );
                return Ok(CancelFlowResult::Cancelled {
                    cancellation_policy: stored_cancellation_policy
                        .unwrap_or_else(|| args.cancellation_policy.clone()),
                    member_execution_ids: stored_members,
                });
            }
            // `CancelFlowHeader` is `#[non_exhaustive]`. Any future
            // variant must be reviewed at this match site before it
            // reaches the wire; fall closed with a typed server error.
            other => {
                return Err(ServerError::OperationFailed(format!(
                    "cancel_flow_header: unknown CancelFlowHeader variant: {other:?}"
                )));
            }
        };
        let needs_dispatch = policy == "cancel_all" && !members.is_empty();

        if !needs_dispatch {
            return Ok(CancelFlowResult::Cancelled {
                cancellation_policy: policy,
                member_execution_ids: members,
            });
        }

        if wait {
            // Synchronous dispatch — cancel every member inline before returning.
            // Collect per-member failures so the caller sees a
            // PartiallyCancelled outcome instead of a false-positive
            // Cancelled when any member cancel faulted. The
            // cancel-backlog reconciler still retries the unacked
            // members; surfacing the partial state lets operator
            // tooling alert without polling per-member state.
            // RFC-017 Stage E2: ack drain dispatches via the backend
            // trait's `ack_cancel_member` — the Server no longer owns
            // a raw ferriskey `Client`.
            let mut failed: Vec<String> = Vec::new();
            for eid_str in &members {
                let Ok(eid) = ExecutionId::parse(eid_str) else {
                    failed.push(eid_str.clone());
                    continue;
                };
                let cancel_args = ff_core::contracts::CancelExecutionArgs {
                    execution_id: eid.clone(),
                    reason: args.reason.clone(),
                    source: ff_core::types::CancelSource::OperatorOverride,
                    lease_id: None,
                    lease_epoch: None,
                    attempt_id: None,
                    now: args.now,
                };
                match self.backend.cancel_execution(cancel_args).await {
                    Ok(_) => {
                        ack_cancel_member_via_backend(
                            self.backend.as_ref(),
                            &args.flow_id,
                            &eid,
                            eid_str,
                        )
                        .await;
                    }
                    Err(e) => {
                        if is_terminal_ack_engine_error(&e) {
                            ack_cancel_member_via_backend(
                                self.backend.as_ref(),
                                &args.flow_id,
                                &eid,
                                eid_str,
                            )
                            .await;
                            continue;
                        }
                        tracing::warn!(
                            execution_id = %eid_str,
                            error = %e,
                            "cancel_flow(wait): individual execution cancel failed \
                             (transport/contract fault; reconciler will retry if transient)"
                        );
                        failed.push(eid_str.clone());
                    }
                }
            }
            if failed.is_empty() {
                return Ok(CancelFlowResult::Cancelled {
                    cancellation_policy: policy,
                    member_execution_ids: members,
                });
            }
            return Ok(CancelFlowResult::PartiallyCancelled {
                cancellation_policy: policy,
                member_execution_ids: members,
                failed_member_execution_ids: failed,
            });
        }

        // Asynchronous dispatch — spawn into the shared JoinSet so
        // Server::shutdown can wait for pending cancellations (bounded
        // by a shutdown timeout). RFC-017 Stage E2: both the
        // per-member cancel and the backlog ack dispatch through the
        // backend trait (the Server no longer holds a ferriskey handle).
        let backend = self.backend.clone();
        let reason = args.reason.clone();
        let now = args.now;
        let dispatch_members = members.clone();
        let flow_id = args.flow_id.clone();
        // Every async cancel_flow contends on this lock, but the
        // critical section is tiny: try_join_next drain + spawn.
        let mut guard = self.background_tasks.lock().await;

        // Reap completed background dispatches before spawning the next.
        while let Some(joined) = guard.try_join_next() {
            if let Err(e) = joined {
                tracing::warn!(
                    error = %e,
                    "cancel_flow: background dispatch task panicked or was aborted"
                );
            }
        }

        guard.spawn(async move {
            // Bounded parallel dispatch via futures::stream::buffer_unordered.
            use futures::stream::StreamExt;
            const CONCURRENCY: usize = 16;

            let member_count = dispatch_members.len();
            let flow_id_for_log = flow_id.clone();
            futures::stream::iter(dispatch_members)
                .map(|eid_str| {
                    let backend = backend.clone();
                    let reason = reason.clone();
                    let flow_id = flow_id.clone();
                    async move {
                        let Ok(eid) = ExecutionId::parse(&eid_str) else {
                            tracing::warn!(
                                flow_id = %flow_id,
                                execution_id = %eid_str,
                                "cancel_flow(async): member id failed to parse; skipping"
                            );
                            return;
                        };
                        let cancel_args = ff_core::contracts::CancelExecutionArgs {
                            execution_id: eid.clone(),
                            reason: reason.clone(),
                            source: ff_core::types::CancelSource::OperatorOverride,
                            lease_id: None,
                            lease_epoch: None,
                            attempt_id: None,
                            now,
                        };
                        match backend.cancel_execution(cancel_args).await {
                            Ok(_) => {
                                ack_cancel_member_via_backend(
                                    backend.as_ref(),
                                    &flow_id,
                                    &eid,
                                    &eid_str,
                                )
                                .await;
                            }
                            Err(e) => {
                                if is_terminal_ack_engine_error(&e) {
                                    ack_cancel_member_via_backend(
                                        backend.as_ref(),
                                        &flow_id,
                                        &eid,
                                        &eid_str,
                                    )
                                    .await;
                                } else {
                                    tracing::warn!(
                                        flow_id = %flow_id,
                                        execution_id = %eid_str,
                                        error = %e,
                                        "cancel_flow(async): individual execution cancel failed \
                                         (transport/contract fault; reconciler will retry if transient)"
                                    );
                                }
                            }
                        }
                    }
                })
                .buffer_unordered(CONCURRENCY)
                .for_each(|()| async {})
                .await;

            tracing::debug!(
                flow_id = %flow_id_for_log,
                member_count,
                concurrency = CONCURRENCY,
                "cancel_flow: background member dispatch complete"
            );
        });
        drop(guard);

        let member_count = u32::try_from(members.len()).unwrap_or(u32::MAX);
        Ok(CancelFlowResult::CancellationScheduled {
            cancellation_policy: policy,
            member_count,
            member_execution_ids: members,
        })
    }

    /// Stage a dependency edge between two executions in a flow.
    ///
    /// Runs on the flow partition {fp:N}.
    /// KEYS (6), ARGV (8) — matches lua/flow.lua ff_stage_dependency_edge.
    pub async fn stage_dependency_edge(
        &self,
        args: &StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, ServerError> {
        Ok(self.backend.stage_dependency_edge(args.clone()).await?)
    }

    /// Apply a staged dependency edge to the child execution. RFC-017
    /// Stage D2: delegates through the backend trait.
    pub async fn apply_dependency_to_child(
        &self,
        args: &ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, ServerError> {
        Ok(self.backend.apply_dependency_to_child(args.clone()).await?)
    }

    // ── Execution operations API ──

    /// Deliver a signal to a suspended (or pending-waitpoint) execution.
    ///
    /// Pre-reads exec_core for waitpoint/suspension fields needed for KEYS.
    /// KEYS (13), ARGV (17) — matches lua/signal.lua ff_deliver_signal.
    pub async fn deliver_signal(
        &self,
        args: &DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, ServerError> {
        // RFC-017 Stage A migration: dispatch through the backend
        // trait. The previous body (lane pre-read + KEYS(14) + ARGV(18)
        // FCALL dispatch + `parse_deliver_signal_result`) lives
        // verbatim inside `ValkeyBackend::deliver_signal` →
        // `deliver_signal_impl`. Clone required because the trait
        // method takes `DeliverSignalArgs` by value (see
        // `ff_core::engine_backend::EngineBackend::deliver_signal`).
        Ok(self.backend.deliver_signal(args.clone()).await?)
    }

    /// Change an execution's priority. RFC-017 Stage D2: delegates
    /// through the backend trait. Empty `lane_id` triggers the backend-
    /// internal HGET pre-read (matches legacy inherent behaviour).
    pub async fn change_priority(
        &self,
        execution_id: &ExecutionId,
        new_priority: i32,
    ) -> Result<ChangePriorityResult, ServerError> {
        let args = ff_core::contracts::ChangePriorityArgs {
            execution_id: execution_id.clone(),
            new_priority,
            lane_id: LaneId::new(""),
            now: TimestampMs::now(),
        };
        Ok(self.backend.change_priority(args).await?)
    }

    /// Scheduler-routed claim entry point.
    ///
    /// RFC-017 Wave 8 Stage E3 (§7): dispatches through the backend
    /// trait. The Valkey backend forwards to its wired
    /// [`ff_scheduler::Scheduler`]; the Postgres backend forwards to
    /// [`ff_backend_postgres::scheduler::PostgresScheduler`]'s
    /// `FOR UPDATE SKIP LOCKED` admission pipeline. Returns
    /// `Ok(None)` when no eligible execution exists on the lane at
    /// this scan cycle — the enum-typed trait outcome
    /// (`ClaimForWorkerOutcome::NoWork`) is collapsed to `Option::None`
    /// for the inherent-call contract pre-existing Stage E.
    ///
    /// Error mapping: scheduler-class errors arrive as
    /// [`EngineError`] via the trait boundary and thread through
    /// `ServerError::Engine`'s HTTP arm
    /// (budget / capability / unavailable classes land on the
    /// documented 400/409/503 response codes — see `api::ApiError::into_response`).
    pub async fn claim_for_worker(
        &self,
        lane: &LaneId,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        worker_capabilities: &std::collections::BTreeSet<String>,
        grant_ttl_ms: u64,
    ) -> Result<Option<ff_core::contracts::ClaimGrant>, ServerError> {
        let args = ff_core::contracts::ClaimForWorkerArgs::new(
            lane.clone(),
            worker_id.clone(),
            worker_instance_id.clone(),
            worker_capabilities.clone(),
            grant_ttl_ms,
        );
        match self.backend.claim_for_worker(args).await? {
            ff_core::contracts::ClaimForWorkerOutcome::Granted(g) => Ok(Some(g)),
            ff_core::contracts::ClaimForWorkerOutcome::NoWork => Ok(None),
            // `#[non_exhaustive]` — any future additive variant
            // surfaces as a typed Engine error so callers see a
            // loud miss instead of a silent `None`.
            _ => Err(ServerError::Engine(Box::new(
                ff_core::engine_error::EngineError::Unavailable {
                    op: "claim_for_worker: unknown ClaimForWorkerOutcome variant",
                },
            ))),
        }
    }

    /// Revoke an active lease (operator-initiated). RFC-017 Stage D2:
    /// delegates through the backend trait. The backend's trait impl
    /// returns `RevokeLeaseResult::AlreadySatisfied` when no active
    /// lease is present; the Server facade preserves its pre-migration
    /// `ServerError::NotFound` behaviour by re-mapping that variant.
    pub async fn revoke_lease(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<RevokeLeaseResult, ServerError> {
        let args = ff_core::contracts::RevokeLeaseArgs {
            execution_id: execution_id.clone(),
            expected_lease_id: None,
            worker_instance_id: WorkerInstanceId::new(""),
            reason: "operator_revoke".to_owned(),
        };
        match self.backend.revoke_lease(args).await? {
            RevokeLeaseResult::AlreadySatisfied { reason } if reason == "no_active_lease" => {
                Err(ServerError::NotFound(format!(
                    "no active lease for execution {execution_id} (no current_worker_instance_id)"
                )))
            }
            other => Ok(other),
        }
    }

    /// Get full execution info (HGETALL-shape on Valkey; SELECT-shape on
    /// Postgres once Wave 9 wires it). RFC-017 Stage E2: routed through
    /// the backend trait's [`ff_core::engine_backend::EngineBackend::read_execution_info`].
    pub async fn get_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ExecutionInfo, ServerError> {
        match self.backend.read_execution_info(execution_id).await? {
            Some(info) => Ok(info),
            None => Err(ServerError::NotFound(format!(
                "execution not found: {execution_id}"
            ))),
        }
    }

    /// Partition-scoped forward-only cursor listing of executions.
    ///
    /// Parity-wrapper around the Valkey body of
    /// [`ff_core::engine_backend::EngineBackend::list_executions`].
    /// Issue #182 replaced the previous offset + lane + state-filter
    /// shape with this cursor-based API (per owner adjudication:
    /// cursor-everywhere, HTTP surface unreleased). Reads
    /// `ff:idx:{p:N}:all_executions`, sorts lexicographically on
    /// `ExecutionId`, filters `> cursor`, and trims to `limit`.
    pub async fn list_executions_page(
        &self,
        partition_id: u16,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListExecutionsPage, ServerError> {
        // RFC-017 Stage A migration: dispatch through the backend
        // trait. The previous body (SMEMBERS + parse + lex-sort +
        // filter + cap) is preserved verbatim inside
        // `ValkeyBackend::list_executions`. One deliberate behaviour
        // change: corrupt members now surface as
        // `EngineError::Validation { kind: Corruption, .. }` (→
        // `ServerError::Engine`), where the legacy path warn-logged
        // and skipped them. This matches RFC-012's fail-loud contract
        // for read-surface corruption.
        let partition = ff_core::partition::Partition {
            family: ff_core::partition::PartitionFamily::Execution,
            index: partition_id,
        };
        let partition_key = ff_core::partition::PartitionKey::from(&partition);
        Ok(self
            .backend
            .list_executions(partition_key, cursor, limit)
            .await?)
    }

    /// Replay a terminal execution. RFC-017 Stage D2: delegates through
    /// the backend trait; the variadic-KEYS pre-read (HMGET + SMEMBERS
    /// for inbound edges on skipped flow members) now lives inside
    /// `ValkeyBackend::replay_execution`.
    pub async fn replay_execution(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ReplayExecutionResult, ServerError> {
        let args = ff_core::contracts::ReplayExecutionArgs {
            execution_id: execution_id.clone(),
            now: TimestampMs::now(),
        };
        Ok(self.backend.replay_execution(args).await?)
    }

    /// Read frames from an attempt's stream (XRANGE wrapper) plus terminal
    /// markers (`closed_at`, `closed_reason`) so consumers can stop polling
    /// when the producer finalizes.
    ///
    /// `from_id` and `to_id` accept XRANGE special markers: `"-"` for
    /// earliest, `"+"` for latest. `count_limit` MUST be `>= 1` —
    /// `0` returns a `ServerError::InvalidInput` (matches the REST boundary
    /// and the Lua-side reject).
    ///
    /// Cluster-safe: the attempt's `{p:N}` partition is derived from the
    /// execution id, so all KEYS share the same slot.
    pub async fn read_attempt_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from_id: &str,
        to_id: &str,
        count_limit: u64,
    ) -> Result<ff_core::contracts::StreamFrames, ServerError> {
        if count_limit == 0 {
            return Err(ServerError::InvalidInput(
                "count_limit must be >= 1".to_owned(),
            ));
        }
        // RFC-017 Stage B row 10: delegate through the trait. The
        // backend owns the stream-op semaphore + XRANGE dispatch; the
        // 429-on-contention semantics round-trip as
        // `EngineError::ResourceExhausted → ServerError::Engine →
        // HTTP 429` (see `ServerError::from` below).
        let from = wire_str_to_stream_cursor(from_id);
        let to = wire_str_to_stream_cursor(to_id);
        Ok(self
            .backend
            .read_stream(execution_id, attempt_index, from, to, count_limit)
            .await?)
    }

    /// Tail a live attempt's stream (XREAD BLOCK wrapper). Returns frames
    /// plus the terminal signal so a polling consumer can exit when the
    /// producer closes the stream.
    ///
    /// `last_id` is exclusive — XREAD returns entries with id > last_id.
    /// Pass `"0-0"` to read from the beginning.
    ///
    /// `block_ms == 0` → non-blocking peek (returns immediately).
    /// `block_ms > 0`  → blocks up to that many ms. Empty `frames` +
    /// `closed_at=None` → timeout, no new data, still open.
    ///
    /// `count_limit` MUST be `>= 1`; `0` returns `InvalidInput`.
    ///
    /// Implemented as a direct XREAD command (not FCALL) because blocking
    /// commands are rejected inside Valkey Functions. The terminal
    /// markers come from a companion HMGET on `stream_meta` — see
    /// `ff_script::stream_tail` module docs.
    pub async fn tail_attempt_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        last_id: &str,
        block_ms: u64,
        count_limit: u64,
    ) -> Result<ff_core::contracts::StreamFrames, ServerError> {
        if count_limit == 0 {
            return Err(ServerError::InvalidInput(
                "count_limit must be >= 1".to_owned(),
            ));
        }
        // RFC-017 Stage B row 10: delegate through the trait. The
        // backend owns the stream-op semaphore + XREAD BLOCK dispatch
        // (via `duplicate_connection()` per #204, so neither a shared
        // tail client nor a serialising mutex is needed). Saturation
        // round-trips as `EngineError::ResourceExhausted → HTTP 429`;
        // a post-shutdown arrival round-trips as
        // `EngineError::Unavailable → HTTP 503`.
        let after = wire_str_to_stream_cursor(last_id);
        Ok(self
            .backend
            .tail_stream(
                execution_id,
                attempt_index,
                after,
                block_ms,
                count_limit,
                ff_core::backend::TailVisibility::All,
            )
            .await?)
    }

    /// Graceful shutdown — stops scanners, drains background handler tasks
    /// (e.g. cancel_flow member dispatch) with a bounded timeout, then waits
    /// for scanners to finish.
    ///
    /// Shutdown order is chosen so in-flight stream ops (read/tail) drain
    /// cleanly without new arrivals piling up:
    ///
    /// 1. `stream_semaphore.close()` — new read/tail attempts fail fast
    ///    with `ServerError::OperationFailed("stream semaphore closed …")`
    ///    which the REST layer surfaces as a 500 with `retryable=false`
    ///    (ops tooling may choose to wait + retry on 503-class responses;
    ///    the body clearly names the shutdown reason).
    /// 2. Drain handler-spawned background tasks with a 15s ceiling.
    /// 3. `engine.shutdown()` stops scanners.
    ///
    /// Existing in-flight tails finish on their natural `block_ms`
    /// boundary (up to ~30s); the `tail_client` is dropped when `Server`
    /// is dropped after this function returns. We do NOT wait for tails
    /// to drain explicitly — the semaphore-close + natural-timeout
    /// combination bounds shutdown to roughly `block_ms + 15s` in the
    /// worst case. Callers observing a dropped connection retry against
    /// whatever replacement is coming up.
    pub async fn shutdown(self) {
        tracing::info!("shutting down FlowFabric server");

        // Step 1: RFC-017 Stage B — delegate stream-op pool closure
        // + drain to the backend's `shutdown_prepare` hook. The
        // Valkey impl closes its semaphore (no new read/tail starts)
        // and awaits in-flight permits up to `grace`. A timeout here
        // is logged + counted on `ff_shutdown_timeout_total`; we
        // continue with best-effort drain of the server's own
        // background tasks rather than blocking shutdown behind a
        // single slow tail.
        let drain_timeout = Duration::from_secs(15);
        match self.backend.shutdown_prepare(drain_timeout).await {
            Ok(()) => tracing::info!(
                "backend shutdown_prepare complete (stream-op pool drained)"
            ),
            Err(ff_core::engine_error::EngineError::Timeout { elapsed, .. }) => {
                self.metrics.inc_shutdown_timeout();
                tracing::warn!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    "shutdown_prepare exceeded grace; proceeding best-effort"
                );
            }
            Err(e) => {
                // Non-timeout errors don't block shutdown either, but
                // they're unexpected — log at warn so operators see
                // the signal without tripping an alert.
                tracing::warn!(
                    err = %e,
                    "shutdown_prepare returned error; proceeding best-effort"
                );
            }
        }

        // Step 2: Drain handler-spawned background tasks with the same
        // ceiling as Engine::shutdown. If dispatch is still running at
        // the deadline, drop the JoinSet to abort remaining tasks.
        let background = self.background_tasks.clone();
        let drain = async move {
            let mut guard = background.lock().await;
            while guard.join_next().await.is_some() {}
        };
        match tokio::time::timeout(drain_timeout, drain).await {
            Ok(()) => {}
            Err(_) => {
                tracing::warn!(
                    timeout_s = drain_timeout.as_secs(),
                    "shutdown: background tasks did not finish in time, aborting"
                );
                self.background_tasks.lock().await.abort_all();
            }
        }

        if let Some(engine) = self.engine {
            engine.shutdown().await;
        }
        tracing::info!("FlowFabric server shutdown complete");
    }
}

/// RFC-017 Stage B: lift the wire string (`"-"`, `"+"`, or a concrete
/// entry id) used by the REST boundary into the typed
/// [`ff_core::contracts::StreamCursor`] the trait method expects.
/// Keeps the `read_attempt_stream` / `tail_attempt_stream`
/// public-function signatures byte-identical while dispatching
/// through the backend.
fn wire_str_to_stream_cursor(s: &str) -> ff_core::contracts::StreamCursor {
    match s {
        "-" => ff_core::contracts::StreamCursor::Start,
        "+" => ff_core::contracts::StreamCursor::End,
        other => ff_core::contracts::StreamCursor::At(other.to_owned()),
    }
}


/// Result of a waitpoint HMAC secret rotation across all execution partitions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RotateWaitpointSecretResult {
    /// Count of partitions that accepted the rotation.
    pub rotated: u16,
    /// Partition indices that failed — operator should investigate (Valkey
    /// outage, auth failure, cluster split). Rotation is idempotent, so a
    /// re-run after the underlying fault clears converges to the correct
    /// state.
    pub failed: Vec<u16>,
    /// New kid installed as current.
    pub new_kid: String,
}

impl Server {
    /// Rotate the waitpoint HMAC secret. Promotes the current kid to previous
    /// (accepted within `FF_WAITPOINT_HMAC_GRACE_MS`), installs `new_secret_hex`
    /// as the new current kid. Idempotent: re-running with the same `new_kid`
    /// and `new_secret_hex` converges partitions to the same state.
    ///
    /// Returns a structured result so operators can see which partitions failed.
    /// HTTP layer returns 200 if any partition succeeded, 500 only if all fail.
    pub async fn rotate_waitpoint_secret(
        &self,
        new_kid: &str,
        new_secret_hex: &str,
    ) -> Result<RotateWaitpointSecretResult, ServerError> {
        if new_kid.is_empty() || new_kid.contains(':') {
            return Err(ServerError::OperationFailed(
                "new_kid must be non-empty and must not contain ':'".into(),
            ));
        }
        if new_secret_hex.is_empty()
            || !new_secret_hex.len().is_multiple_of(2)
            || !new_secret_hex.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(ServerError::OperationFailed(
                "new_secret_hex must be a non-empty even-length hex string".into(),
            ));
        }

        // Single-writer gate — admin semaphore + audit log stay on
        // Server per RFC-017 §4 row 11. The per-partition fan-out
        // moved inside `ValkeyBackend::rotate_waitpoint_hmac_secret_all`.
        let _permit = match self.admin_rotate_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                return Err(ServerError::ConcurrencyLimitExceeded("admin_rotate", 1));
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(ServerError::OperationFailed(
                    "admin rotate semaphore closed (server shutting down)".into(),
                ));
            }
        };

        let n = self.config.partition_config.num_flow_partitions;
        let grace_ms = self.config.waitpoint_hmac_grace_ms;

        // RFC-017 Stage B row 11: delegate the per-partition fan-out
        // to the backend. The trait method returns one entry per
        // partition with an inner `Result` so partial success is
        // observable — matching the pre-migration Server body.
        let args = ff_core::contracts::RotateWaitpointHmacSecretAllArgs::new(
            new_kid.to_owned(),
            new_secret_hex.to_owned(),
            grace_ms,
        );
        let result = self
            .backend
            .rotate_waitpoint_hmac_secret_all(args)
            .await?;

        let mut rotated = 0u16;
        let mut failed: Vec<u16> = Vec::new();
        for entry in &result.entries {
            match &entry.result {
                Ok(_) => {
                    rotated += 1;
                    tracing::debug!(
                        partition = entry.partition,
                        new_kid = %new_kid,
                        "waitpoint_hmac_rotated"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        target: "audit",
                        partition = entry.partition,
                        err = %e,
                        "waitpoint_hmac_rotation_failed"
                    );
                    failed.push(entry.partition);
                }
            }
        }

        // Single aggregated audit event (RFC-017 row 11: audit emit
        // stays on Server).
        tracing::info!(
            target: "audit",
            new_kid = %new_kid,
            total_partitions = n,
            rotated,
            failed_count = failed.len(),
            "waitpoint_hmac_rotation_complete"
        );

        Ok(RotateWaitpointSecretResult {
            rotated,
            failed,
            new_kid: new_kid.to_owned(),
        })
    }
}

// ── FCALL result parsing ──





// ── Flow FCALL result parsing ──




/// Extract a string from an FCALL result array at the given index.
/// Convert a `ScriptError` into a `ServerError` preserving `ferriskey::ErrorKind`
/// for transport-level variants. Business-logic variants keep their code as
/// `ServerError::Script(String)` so HTTP clients see a stable message.
///
/// Why this exists: before R2, the stream handlers did
/// `ScriptError → format!() → ServerError::Script(String)`, which erased
/// the ErrorKind and made `ServerError::is_retryable()` always return
/// false. Retry-capable clients (cairn-fabric) would not retry a legit
/// transient error like `IoError`.
#[allow(dead_code)] // retained for non-stream FCALL paths that still route via raw ScriptError; stream handlers moved to trait in RFC-017 Stage B
fn script_error_to_server(e: ff_script::error::ScriptError) -> ServerError {
    match e {
        ff_script::error::ScriptError::Valkey(valkey_err) => {
            crate::server::backend_context(valkey_err, "stream FCALL transport")
        }
        other => ServerError::Script(other.to_string()),
    }
}

/// Acknowledge that a member cancel has committed. Delegates to
/// [`EngineBackend::ack_cancel_member`] (Valkey: `ff_ack_cancel_member`
/// FCALL on `{fp:N}` — SREM the execution from the flow's
/// `pending_cancels` set and, if empty, ZREM the flow from the
/// partition-level `cancel_backlog`). Best-effort — failures are
/// logged but not propagated, since the reconciler drains any
/// leftovers on its next pass.
async fn ack_cancel_member_via_backend(
    backend: &dyn EngineBackend,
    flow_id: &FlowId,
    execution_id: &ExecutionId,
    eid_str: &str,
) {
    if let Err(e) = backend.ack_cancel_member(flow_id, execution_id).await {
        tracing::warn!(
            flow_id = %flow_id,
            execution_id = %eid_str,
            error = %e,
            "ack_cancel_member failed; reconciler will drain on next pass"
        );
    }
}

/// Engine-error variant: inspects an
/// `EngineError` returned from `self.backend.cancel_execution(...)`
/// and returns `true` when the member is already terminal. Matches the
/// Lua-code semantics of `execution_not_active` / `execution_not_found`
/// via the typed `State` / `Validation` classifications the backend
/// trait impl maps them to.
fn is_terminal_ack_engine_error(err: &EngineError) -> bool {
    match err {
        // Already terminal (Lua's `execution_not_active`) or missing
        // entirely (`execution_not_found`) — both treated as ack-worthy
        // so the cancel-backlog doesn't poison on a member already in
        // the intended terminal state.
        EngineError::State(kind) => matches!(
            kind,
            ff_core::engine_error::StateKind::Terminal
        ),
        EngineError::NotFound { .. } => true,
        EngineError::Contextual { source, .. } => is_terminal_ack_engine_error(source),
        _ => false,
    }
}


/// Single cancel attempt — pre-read + FCALL + parse. Factored out so the
/// retry loop in [`cancel_member_execution`] can invoke it cleanly.


#[cfg(test)]
mod tests {
    use super::*;
    use ferriskey::ErrorKind;

    fn mk_fk_err(kind: ErrorKind) -> ferriskey::Error {
        ferriskey::Error::from((kind, "synthetic"))
    }

    #[test]
    fn is_retryable_backend_variant_uses_kind_table() {
        // Transport-bucketed: retryable.
        assert!(ServerError::from(mk_fk_err(ErrorKind::IoError)).is_retryable());
        assert!(ServerError::from(mk_fk_err(ErrorKind::FatalSendError)).is_retryable());
        assert!(ServerError::from(mk_fk_err(ErrorKind::FatalReceiveError)).is_retryable());
        // Cluster-bucketed (Moved / Ask / TryAgain / ClusterDown): retryable
        // after topology settles — the #88 BackendErrorKind classifier
        // treats these as transient cluster-churn, a semantic refinement
        // over the previous ff-script retry table.
        assert!(ServerError::from(mk_fk_err(ErrorKind::TryAgain)).is_retryable());
        assert!(ServerError::from(mk_fk_err(ErrorKind::ClusterDown)).is_retryable());
        assert!(ServerError::from(mk_fk_err(ErrorKind::Moved)).is_retryable());
        assert!(ServerError::from(mk_fk_err(ErrorKind::Ask)).is_retryable());
        // BusyLoading: retryable.
        assert!(ServerError::from(mk_fk_err(ErrorKind::BusyLoadingError)).is_retryable());

        // Auth / Protocol / ScriptNotLoaded: terminal.
        assert!(!ServerError::from(mk_fk_err(ErrorKind::AuthenticationFailed)).is_retryable());
        assert!(!ServerError::from(mk_fk_err(ErrorKind::NoScriptError)).is_retryable());
        assert!(!ServerError::from(mk_fk_err(ErrorKind::ReadOnly)).is_retryable());
    }

    #[test]
    fn is_retryable_backend_context_uses_kind_table() {
        let err = crate::server::backend_context(mk_fk_err(ErrorKind::IoError), "HGET test");
        assert!(err.is_retryable());

        let err =
            crate::server::backend_context(mk_fk_err(ErrorKind::AuthenticationFailed), "auth");
        assert!(!err.is_retryable());
    }

    #[test]
    fn is_retryable_library_load_delegates_to_inner_kind() {
        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::Valkey(
            mk_fk_err(ErrorKind::IoError),
        ));
        assert!(err.is_retryable());

        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::Valkey(
            mk_fk_err(ErrorKind::AuthenticationFailed),
        ));
        assert!(!err.is_retryable());

        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::VersionMismatch {
            expected: "1".into(),
            got: "2".into(),
        });
        assert!(!err.is_retryable());
    }

    #[test]
    fn is_retryable_business_logic_variants_are_false() {
        assert!(!ServerError::NotFound("x".into()).is_retryable());
        assert!(!ServerError::InvalidInput("x".into()).is_retryable());
        assert!(!ServerError::OperationFailed("x".into()).is_retryable());
        assert!(!ServerError::Script("x".into()).is_retryable());
        assert!(!ServerError::PartitionMismatch("x".into()).is_retryable());
    }

    #[test]
    fn backend_kind_delegates_through_library_load() {
        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::Valkey(
            mk_fk_err(ErrorKind::ClusterDown),
        ));
        assert_eq!(err.backend_kind(), Some(ff_core::BackendErrorKind::Cluster));

        let err = ServerError::LibraryLoad(ff_script::loader::LoadError::VersionMismatch {
            expected: "1".into(),
            got: "2".into(),
        });
        assert_eq!(err.backend_kind(), None);
    }


    #[test]
    fn valkey_version_too_low_is_not_retryable() {
        let err = ServerError::ValkeyVersionTooLow {
            detected: "7.0".into(),
            required: "7.2".into(),
        };
        assert!(!err.is_retryable());
        assert_eq!(err.backend_kind(), None);
    }

    #[test]
    fn valkey_version_too_low_error_message_includes_both_versions() {
        let err = ServerError::ValkeyVersionTooLow {
            detected: "7.0".into(),
            required: "7.2".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("7.0"), "detected version in message: {msg}");
        assert!(msg.contains("7.2"), "required version in message: {msg}");
        assert!(msg.contains("RFC-011"), "RFC pointer in message: {msg}");
    }
}
