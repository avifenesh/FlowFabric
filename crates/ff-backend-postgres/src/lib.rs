//! `EngineBackend` implementation backed by Postgres.
//!
//! **RFC-v0.7 Wave 0 — scaffold.** This crate lands the
//! [`PostgresBackend`] struct + the `impl EngineBackend` block with
//! every method stubbed to return `EngineError::Unavailable`.
//! Subsequent waves fill in bodies:
//!
//! * Wave 1: cross-cutting error/helpers.
//! * Wave 2: describe / list / resolve (read surface).
//! * Wave 3: schema migrations (replaces the Wave 0 placeholder).
//! * Wave 4: LISTEN/NOTIFY wiring + stream reads/tails.
//! * Wave 5-7: hot-path write methods.
//! * Wave 8: ff-server wire-up + dual-backend running.
//!
//! The trait stays object-safe; consumers can hold
//! `Arc<dyn EngineBackend>`. No ferriskey dep — this crate's
//! transport is `sqlx`.

#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use ff_core::backend::{
    AppendFrameOutcome, BackendConfig, CancelFlowPolicy, CancelFlowWait, CapabilitySet,
    ClaimPolicy, FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal,
    PendingWaitpoint, ResumeToken, ResumeSignal, SummaryDocument, TailVisibility,
    UsageDimensions,
};
#[cfg(feature = "core")]
use ff_core::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, ApplyDependencyToChildArgs,
    ApplyDependencyToChildResult, ClaimResumedExecutionArgs, ClaimResumedExecutionResult,
    CreateExecutionArgs, CreateExecutionResult, CreateFlowArgs, CreateFlowResult,
    DeliverSignalArgs, DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot,
    ListExecutionsPage, ListFlowsPage, ListLanesPage, ListPendingWaitpointsArgs,
    ListPendingWaitpointsResult, ListSuspendedPage, SetEdgeGroupPolicyResult,
    StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
#[cfg(feature = "core")]
use ff_core::state::PublicState;
use ff_core::contracts::{
    CancelFlowResult, ExecutionContext, ExecutionSnapshot, FlowSnapshot, IssueReclaimGrantArgs,
    IssueReclaimGrantOutcome, ReclaimExecutionArgs, ReclaimExecutionOutcome, ReportUsageResult,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllResult, SeedOutcome,
    SeedWaitpointHmacSecretArgs, SuspendArgs, SuspendOutcome,
};
#[cfg(feature = "core")]
use ff_core::contracts::ExecutionInfo;
// RFC-020 Wave 9 Spine-A pt.1 — operator-control mutating surfaces.
#[cfg(feature = "core")]
use ff_core::contracts::{
    CancelExecutionArgs, CancelExecutionResult, RevokeLeaseArgs, RevokeLeaseResult,
};
// RFC-020 Wave 9 Spine-A pt.2 — operator-control + flow-cancel mutating surfaces.
#[cfg(feature = "core")]
use ff_core::contracts::{
    CancelFlowArgs, CancelFlowHeader, ChangePriorityArgs, ChangePriorityResult,
    ReplayExecutionArgs, ReplayExecutionResult,
};
// RFC-020 Wave 9 Standalone-1 — budget/quota admin surfaces.
#[cfg(feature = "core")]
use ff_core::contracts::{
    BudgetStatus, CreateBudgetArgs, CreateBudgetResult, CreateQuotaPolicyArgs,
    CreateQuotaPolicyResult, ReportUsageAdminArgs, ResetBudgetArgs, ResetBudgetResult,
};
#[cfg(feature = "streaming")]
use ff_core::contracts::{StreamCursor, StreamFrames};
use ff_core::engine_backend::{EngineBackend, ExpirePhase};
use ff_core::engine_error::EngineError;
#[cfg(feature = "core")]
use ff_core::partition::PartitionKey;
use ff_core::partition::{Partition, PartitionConfig};
#[cfg(feature = "streaming")]
use ff_core::types::AttemptIndex;
#[cfg(feature = "core")]
use ff_core::types::EdgeId;
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, LeaseFence, TimestampMs};
// Wave 5a — re-export `PgPool` so crates that depend on
// `ff-backend-postgres` (and not `sqlx` directly) can name the pool
// type in their own APIs (e.g. `ff-engine::dispatch_via_postgres`).
pub use sqlx::PgPool;

#[cfg(feature = "core")]
mod admin;
pub mod attempt;
pub mod budget;
pub mod claim_grant;
pub mod completion;
#[cfg(feature = "core")]
pub mod dispatch;
pub mod error;
pub mod exec_core;
pub mod flow;
#[cfg(feature = "core")]
pub mod flow_staging;
pub mod handle_codec;
mod lease_event;
mod lease_event_subscribe;
pub mod listener;
pub mod migrate;
#[cfg(feature = "core")]
pub mod operator;
#[cfg(feature = "core")]
mod operator_event;
pub mod pool;
#[cfg(feature = "core")]
pub mod reconcilers;
#[cfg(feature = "core")]
pub mod scanner_supervisor;
#[cfg(feature = "core")]
pub mod scheduler;
pub mod signal;
mod signal_delivery_subscribe;
mod signal_event;
#[cfg(feature = "streaming")]
pub mod stream;
pub mod suspend;
pub mod suspend_ops;
#[cfg(feature = "core")]
pub(crate) mod typed_ops;
pub mod version;

pub use completion::{PostgresCompletionStream, COMPLETION_CHANNEL};
pub use error::{map_sqlx_error, PostgresTransportError};
pub use listener::StreamNotifier;
pub use migrate::{apply_migrations, MigrationError};
#[cfg(feature = "core")]
pub use scanner_supervisor::{PostgresScannerConfig, PostgresScannerHandle};
pub use version::check_schema_version;

// Re-export the new `PostgresConnection` shape so consumers can name
// it from this crate directly without dipping into `ff_core::backend`.
// `BackendConfig` is already imported above and is part of the
// `connect()` signature, so it re-exports transparently via
// rustdoc — no explicit `pub use` needed.
pub use ff_core::backend::PostgresConnection;

/// Postgres-backed `EngineBackend`.
///
/// Wave 0 shape: holds a `sqlx::PgPool`, the deployment's
/// [`PartitionConfig`] (Q5 — partition column survives on Postgres
/// with hash partitioning across the same 256 slots Valkey uses),
/// and an optional `ff_observability::Metrics` handle mirroring
/// [`ff_backend_valkey::ValkeyBackend`]. Future waves add the
/// [`StreamNotifier`] handle once Wave 4 wires up LISTEN/NOTIFY.
/// RFC-018 Stage A: build a [`ff_core::capability::Supports`]
/// snapshot for the Postgres backend at v0.9. `true` fields correspond
/// to trait methods `PostgresBackend` overrides with a real body
/// (ingress, scheduler, seed + rotate HMAC, flow cancel bulk path,
/// stream reads, RFC-019 subscriptions, cross-cutting). `false` fields
/// correspond to trait methods that still return
/// `EngineError::Unavailable` on Postgres today — Wave 9 follow-up
/// scope. See `docs/POSTGRES_PARITY_MATRIX.md` for the authoritative
/// per-row status.
///
/// `prepare` is `true` on Postgres even though `prepare()` returns
/// `PrepareOutcome::NoOp` (schema migrations are applied out-of-band).
/// `Supports.prepare` means "can the consumer call `backend.prepare()`
/// without getting `EngineError::Unavailable`?" — for Postgres the
/// answer is yes; NoOp is a successful well-defined outcome. Gating
/// the call off in consumer UIs based on a `false` bool would hide
/// a callable + correct method.
///
/// `Supports` is `#[non_exhaustive]` so struct-literal construction
/// from this crate is forbidden; we start from
/// [`ff_core::capability::Supports::none`] and mutate named fields.
fn postgres_supports_base() -> ff_core::capability::Supports {
    let mut s = ff_core::capability::Supports::none();

    // ── Flow bulk cancel (impl) ──
    s.cancel_flow_wait_timeout = true;
    s.cancel_flow_wait_indefinite = true;

    // ── Admin seed + rotate HMAC (impl) ──
    s.rotate_waitpoint_hmac_secret_all = true;
    s.seed_waitpoint_hmac_secret = true;

    // ── Scheduler ──
    s.claim_for_worker = true;

    // ── RFC-019 subscriptions ──
    s.subscribe_lease_history = true;
    s.subscribe_completion = true;
    s.subscribe_signal_delivery = true;
    s.subscribe_instance_tags = false;

    // ── Streaming (RFC-015) ──
    s.stream_durable_summary = true;
    s.stream_best_effort_live = true;

    // ── Boot (Postgres returns NoOp but call is callable + correct) ──
    s.prepare = true;

    // ── Wave 9 (v0.11) — operator control + read model + budget/quota
    //    admin + list_pending_waitpoints + cancel_flow_header +
    //    ack_cancel_member all ship concretely on Postgres via
    //    RFC-020 Rev 7. subscribe_instance_tags remains `false` per
    //    #311 (speculative demand, served by list_executions +
    //    ScannerFilter::with_instance_tag today).
    s.cancel_execution = true;
    s.change_priority = true;
    s.replay_execution = true;
    s.revoke_lease = true;
    s.read_execution_state = true;
    s.read_execution_info = true;
    s.get_execution_result = true;
    s.budget_admin = true;
    s.quota_admin = true;
    s.list_pending_waitpoints = true;
    s.cancel_flow_header = true;
    s.ack_cancel_member = true;

    s
}

pub struct PostgresBackend {
    #[allow(dead_code)] // filled in across waves 2-7
    pool: PgPool,
    #[allow(dead_code)]
    partition_config: PartitionConfig,
    #[allow(dead_code)]
    metrics: Option<Arc<ff_observability::Metrics>>,
    /// Wave 4: shared LISTEN notifier. Present on `connect()`-built
    /// backends; `None` on bare `from_pool` constructions that skip
    /// LISTEN wiring (tests that only exercise the write path).
    #[allow(dead_code)]
    stream_notifier: Option<Arc<StreamNotifier>>,
    /// RFC-017 Wave 8 Stage E3: scanner supervisor handle. Spawned
    /// during [`Self::connect_with_metrics`] when the caller opts in
    /// via [`Self::spawn_scanners_during_connect`]; drained on
    /// [`EngineBackend::shutdown_prepare`]. `None` on `from_pool` /
    /// test builds that don't want background reconcilers.
    #[cfg(feature = "core")]
    scanner_handle: Option<Arc<scanner_supervisor::PostgresScannerHandle>>,
}

impl PostgresBackend {
    /// Dial Postgres with [`BackendConfig`] and return the backend as
    /// `Arc<dyn EngineBackend>`. Modeled on
    /// [`ff_backend_valkey::ValkeyBackend::connect`] so ff-server /
    /// SDK call sites can swap backends without changing the
    /// constructor shape.
    ///
    /// **Wave 0:** builds the pool and constructs the backend. Does
    /// NOT run migrations (Q12 — operator out-of-band). Does NOT run
    /// the schema-version check (Wave 3 adds the version const and
    /// wires [`check_schema_version`] in). Does NOT start the LISTEN
    /// task (Wave 4).
    ///
    /// Returns `EngineError::Unavailable` when the config's
    /// connection arm is not Postgres.
    pub async fn connect(config: BackendConfig) -> Result<Arc<dyn EngineBackend>, EngineError> {
        let pool = pool::build_pool(&config).await?;
        warn_if_max_locks_low(&pool).await;
        let stream_notifier = Some(StreamNotifier::spawn(pool.clone()));
        let backend = Self {
            pool,
            partition_config: PartitionConfig::default(),
            metrics: None,
            stream_notifier,
            #[cfg(feature = "core")]
            scanner_handle: None,
        };
        Ok(Arc::new(backend))
    }

    /// Test / advanced constructor: build a `PostgresBackend` from an
    /// already-constructed `PgPool` + explicit partition config. No
    /// network I/O. Useful for integration tests against a shared
    /// pool and for a future migration CLI that wants to reuse a pool
    /// across migrate-run + smoke-check.
    pub fn from_pool(pool: PgPool, partition_config: PartitionConfig) -> Arc<Self> {
        let stream_notifier = Some(StreamNotifier::spawn(pool.clone()));
        Arc::new(Self {
            pool,
            partition_config,
            metrics: None,
            stream_notifier,
            #[cfg(feature = "core")]
            scanner_handle: None,
        })
    }

    /// RFC-017 Wave 8 Stage E1: dial Postgres with an explicit
    /// [`PartitionConfig`] + shared [`ff_observability::Metrics`].
    /// Mirrors [`ff_backend_valkey::ValkeyBackend::connect_with_metrics`]
    /// so `ff-server::Server::start_with_metrics` can wire the Postgres
    /// branch without reaching into the pool builder directly.
    ///
    /// Returns a concrete `Arc<Self>` rather than `Arc<dyn EngineBackend>`
    /// so the caller can cast to the trait object after any additional
    /// field installs (parallel to the Valkey path which calls
    /// `with_scheduler` / `with_stream_semaphore_permits` before the
    /// cast). Stage E1 does NOT run `apply_migrations` — schema
    /// provisioning is an operator concern (matches the Wave 0 contract
    /// on [`Self::connect`]).
    pub async fn connect_with_metrics(
        config: BackendConfig,
        partition_config: PartitionConfig,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Arc<Self>, EngineError> {
        let pool = pool::build_pool(&config).await?;
        warn_if_max_locks_low(&pool).await;
        let stream_notifier = Some(StreamNotifier::spawn(pool.clone()));
        Ok(Arc::new(Self {
            pool,
            partition_config,
            metrics: Some(metrics),
            stream_notifier,
            #[cfg(feature = "core")]
            scanner_handle: None,
        }))
    }

    /// RFC-017 Wave 8 Stage E3: spawn the six Postgres reconcilers as
    /// background tick loops. Returns `true` if the scanner handle
    /// was installed; `false` if the `Arc<Self>` has outstanding
    /// clones (mirrors the Valkey `with_*` pattern). Callers must
    /// invoke this before publishing the `Arc<dyn EngineBackend>` so
    /// the underlying `Arc::get_mut` succeeds.
    #[cfg(feature = "core")]
    pub fn with_scanners(
        self: &mut Arc<Self>,
        cfg: scanner_supervisor::PostgresScannerConfig,
    ) -> bool {
        let Some(inner) = Arc::get_mut(self) else {
            return false;
        };
        let handle = scanner_supervisor::spawn_scanners(inner.pool.clone(), cfg);
        inner.scanner_handle = Some(Arc::new(handle));
        true
    }

    /// Accessor for the underlying `PgPool`. Stage E1 uses this so
    /// `ff-server::Server::start_with_metrics` can run
    /// [`apply_migrations`] on the same pool before handing the backend
    /// out as `Arc<dyn EngineBackend>`.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Create one execution row (+ seed the lane registry if new).
    ///
    /// **RFC-017 Stage A:** this inherent method is retained as a
    /// thin wrapper around the module-level impl so existing in-tree
    /// callers (ff-server request handlers, integration tests) keep
    /// compiling. The trait-lifted entry point is
    /// [`EngineBackend::create_execution`] below, which calls the
    /// same impl. Return shape differs — inherent returns
    /// `ExecutionId`, trait returns
    /// [`CreateExecutionResult`] per RFC-017 §5 — so we cannot simply
    /// replace the inherent method. A follow-up PR may deprecate
    /// this inherent alongside the broader ingress shape alignment.
    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.create_execution", skip_all)]
    pub async fn create_execution(
        &self,
        args: CreateExecutionArgs,
    ) -> Result<ExecutionId, EngineError> {
        exec_core::create_execution_impl(&self.pool, &self.partition_config, args).await
    }

    // ── RFC-017 Stage A: inherent ingress methods retained for
    // back-compat with in-tree test harnesses + ff-server direct
    // calls. The trait-lifted peers (`EngineBackend::create_flow`
    // etc.) delegate to the SAME module-level impls under the hood.
    // Follow-up PR may sunset these inherents once all in-tree
    // consumers route through `Arc<dyn EngineBackend>`.

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.create_flow.inherent", skip_all)]
    pub async fn create_flow(
        &self,
        args: &CreateFlowArgs,
    ) -> Result<CreateFlowResult, EngineError> {
        flow_staging::create_flow(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.add_execution_to_flow.inherent", skip_all)]
    pub async fn add_execution_to_flow(
        &self,
        args: &AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, EngineError> {
        flow_staging::add_execution_to_flow(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.stage_dependency_edge.inherent", skip_all)]
    pub async fn stage_dependency_edge(
        &self,
        args: &StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, EngineError> {
        flow_staging::stage_dependency_edge(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.apply_dependency_to_child.inherent", skip_all)]
    pub async fn apply_dependency_to_child(
        &self,
        args: &ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, EngineError> {
        flow_staging::apply_dependency_to_child(&self.pool, &self.partition_config, args).await
    }
}

/// Short helper: every stub method returns this. Kept as a function
/// (rather than a macro) so rust-analyzer / IDE jumps show a single
/// definition site for the Wave 0 stub pattern.
#[inline]
#[cfg_attr(feature = "streaming", allow(dead_code))]
fn unavailable<T>(op: &'static str) -> Result<T, EngineError> {
    Err(EngineError::Unavailable { op })
}

#[async_trait]
impl EngineBackend for PostgresBackend {
    // ── Claim + lifecycle ──

    #[tracing::instrument(name = "pg.claim", skip_all)]
    async fn claim(
        &self,
        lane: &LaneId,
        capabilities: &CapabilitySet,
        policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        attempt::claim(&self.pool, lane, capabilities, &policy).await
    }

    #[tracing::instrument(name = "pg.renew", skip_all)]
    async fn renew(&self, handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        attempt::renew(&self.pool, handle).await
    }

    #[tracing::instrument(name = "pg.progress", skip_all)]
    async fn progress(
        &self,
        handle: &Handle,
        percent: Option<u8>,
        message: Option<String>,
    ) -> Result<(), EngineError> {
        attempt::progress(&self.pool, handle, percent, message).await
    }

    #[tracing::instrument(name = "pg.append_frame", skip_all)]
    async fn append_frame(
        &self,
        handle: &Handle,
        frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        #[cfg(feature = "streaming")]
        {
            stream::append_frame(&self.pool, &self.partition_config, handle, frame).await
        }
        #[cfg(not(feature = "streaming"))]
        {
            let _ = (handle, frame);
            unavailable("pg.append_frame")
        }
    }

    #[tracing::instrument(name = "pg.complete", skip_all)]
    async fn complete(
        &self,
        handle: &Handle,
        payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        attempt::complete(&self.pool, handle, payload).await
    }

    #[tracing::instrument(name = "pg.fail", skip_all)]
    async fn fail(
        &self,
        handle: &Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        attempt::fail(&self.pool, handle, reason, classification).await
    }

    #[tracing::instrument(name = "pg.cancel", skip_all)]
    async fn cancel(&self, handle: &Handle, reason: &str) -> Result<(), EngineError> {
        let payload = handle_codec::decode_handle(handle)?;
        exec_core::cancel_impl(
            &self.pool,
            &self.partition_config,
            &payload.execution_id,
            reason,
        )
        .await
    }

    #[tracing::instrument(name = "pg.suspend", skip_all)]
    async fn suspend(
        &self,
        handle: &Handle,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        suspend_ops::suspend_impl(&self.pool, &self.partition_config, handle, args).await
    }

    #[tracing::instrument(name = "pg.suspend_by_triple", skip_all)]
    async fn suspend_by_triple(
        &self,
        exec_id: ExecutionId,
        triple: LeaseFence,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        suspend_ops::suspend_by_triple_impl(
            &self.pool,
            &self.partition_config,
            exec_id,
            triple,
            args,
        )
        .await
    }

    #[tracing::instrument(name = "pg.create_waitpoint", skip_all)]
    async fn create_waitpoint(
        &self,
        handle: &Handle,
        waitpoint_key: &str,
        expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        suspend_ops::create_waitpoint_impl(&self.pool, handle, waitpoint_key, expires_in).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.read_waitpoint_token", skip_all)]
    async fn read_waitpoint_token(
        &self,
        partition: PartitionKey,
        waitpoint_id: &ff_core::types::WaitpointId,
    ) -> Result<Option<String>, EngineError> {
        suspend_ops::read_waitpoint_token_impl(&self.pool, &partition, waitpoint_id).await
    }

    #[tracing::instrument(name = "pg.observe_signals", skip_all)]
    async fn observe_signals(
        &self,
        handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        suspend_ops::observe_signals_impl(&self.pool, handle).await
    }

    #[tracing::instrument(name = "pg.claim_from_resume_grant", skip_all)]
    async fn claim_from_resume_grant(
        &self,
        token: ResumeToken,
    ) -> Result<Option<Handle>, EngineError> {
        attempt::claim_from_resume_grant(&self.pool, token).await
    }

    // RFC-024 PR-D — lease-reclaim grant issuance + consumption.

    #[tracing::instrument(name = "pg.issue_reclaim_grant", skip_all)]
    async fn issue_reclaim_grant(
        &self,
        args: IssueReclaimGrantArgs,
    ) -> Result<IssueReclaimGrantOutcome, EngineError> {
        claim_grant::issue_reclaim_grant_impl(&self.pool, args).await
    }

    #[tracing::instrument(name = "pg.reclaim_execution", skip_all)]
    async fn reclaim_execution(
        &self,
        args: ReclaimExecutionArgs,
    ) -> Result<ReclaimExecutionOutcome, EngineError> {
        claim_grant::reclaim_execution_impl(&self.pool, args).await
    }

    #[tracing::instrument(name = "pg.delay", skip_all)]
    async fn delay(
        &self,
        handle: &Handle,
        delay_until: TimestampMs,
    ) -> Result<(), EngineError> {
        attempt::delay(&self.pool, handle, delay_until).await
    }

    #[tracing::instrument(name = "pg.wait_children", skip_all)]
    async fn wait_children(&self, handle: &Handle) -> Result<(), EngineError> {
        attempt::wait_children(&self.pool, handle).await
    }

    // ── Read / admin ──

    #[tracing::instrument(name = "pg.describe_execution", skip_all)]
    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        exec_core::describe_execution_impl(&self.pool, &self.partition_config, id).await
    }

    #[tracing::instrument(name = "pg.read_execution_context", skip_all)]
    async fn read_execution_context(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ExecutionContext, EngineError> {
        exec_core::read_execution_context_impl(&self.pool, &self.partition_config, execution_id)
            .await
    }

    #[tracing::instrument(name = "pg.read_current_attempt_index", skip_all)]
    async fn read_current_attempt_index(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ff_core::types::AttemptIndex, EngineError> {
        exec_core::read_current_attempt_index_impl(
            &self.pool,
            &self.partition_config,
            execution_id,
        )
        .await
    }

    #[tracing::instrument(name = "pg.read_total_attempt_count", skip_all)]
    async fn read_total_attempt_count(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ff_core::types::AttemptIndex, EngineError> {
        exec_core::read_total_attempt_count_impl(
            &self.pool,
            &self.partition_config,
            execution_id,
        )
        .await
    }

    #[tracing::instrument(name = "pg.describe_flow", skip_all)]
    async fn describe_flow(
        &self,
        id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        flow::describe_flow(&self.pool, &self.partition_config, id).await
    }

    #[tracing::instrument(name = "pg.set_execution_tag", skip_all)]
    async fn set_execution_tag(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: &str,
    ) -> Result<(), EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        exec_core::set_execution_tag_impl(&self.pool, execution_id, key, value).await
    }

    #[tracing::instrument(name = "pg.set_flow_tag", skip_all)]
    async fn set_flow_tag(
        &self,
        flow_id: &FlowId,
        key: &str,
        value: &str,
    ) -> Result<(), EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        flow::set_flow_tag_impl(&self.pool, &self.partition_config, flow_id, key, value).await
    }

    #[tracing::instrument(name = "pg.get_execution_tag", skip_all)]
    async fn get_execution_tag(
        &self,
        execution_id: &ExecutionId,
        key: &str,
    ) -> Result<Option<String>, EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        exec_core::get_execution_tag_impl(&self.pool, execution_id, key).await
    }

    #[tracing::instrument(name = "pg.get_flow_tag", skip_all)]
    async fn get_flow_tag(
        &self,
        flow_id: &FlowId,
        key: &str,
    ) -> Result<Option<String>, EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        flow::get_flow_tag_impl(&self.pool, &self.partition_config, flow_id, key).await
    }

    #[tracing::instrument(name = "pg.get_execution_namespace", skip_all)]
    async fn get_execution_namespace(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Option<String>, EngineError> {
        exec_core::get_execution_namespace_impl(&self.pool, execution_id).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_edges", skip_all)]
    async fn list_edges(
        &self,
        flow_id: &FlowId,
        direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        flow::list_edges(&self.pool, &self.partition_config, flow_id, direction).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.describe_edge", skip_all)]
    async fn describe_edge(
        &self,
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        flow::describe_edge(&self.pool, &self.partition_config, flow_id, edge_id).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.resolve_execution_flow_id", skip_all)]
    async fn resolve_execution_flow_id(
        &self,
        eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        exec_core::resolve_execution_flow_id_impl(&self.pool, &self.partition_config, eid).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_flows", skip_all)]
    async fn list_flows(
        &self,
        partition: PartitionKey,
        cursor: Option<FlowId>,
        limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        flow::list_flows(&self.pool, partition, cursor, limit).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_lanes", skip_all)]
    async fn list_lanes(
        &self,
        cursor: Option<LaneId>,
        limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        admin::list_lanes_impl(&self.pool, cursor, limit).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_suspended", skip_all)]
    async fn list_suspended(
        &self,
        partition: PartitionKey,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        admin::list_suspended_impl(&self.pool, partition, cursor, limit).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_executions", skip_all)]
    async fn list_executions(
        &self,
        partition: PartitionKey,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        exec_core::list_executions_impl(
            &self.pool,
            &self.partition_config,
            partition,
            cursor,
            limit,
        )
        .await
    }

    // ── Trigger ops (issue #150) ──

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.deliver_signal", skip_all)]
    async fn deliver_signal(
        &self,
        args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        suspend_ops::deliver_signal_impl(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.claim_resumed_execution", skip_all)]
    async fn claim_resumed_execution(
        &self,
        args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        suspend_ops::claim_resumed_execution_impl(&self.pool, &self.partition_config, args).await
    }

    // ── RFC-020 Wave 9 Spine-B — read model (3 methods, §4.1) ────────
    //
    // Partition-local single-row reads against `ff_exec_core` (+ LATERAL
    // join on `ff_attempt` for `read_execution_info`). READ COMMITTED
    // (no CAS; all three are read-only). `get_execution_result` returns
    // current-attempt semantics per §7.8 (matches Valkey's
    // `GET ctx.result()` primitive). Capability flips land at the Wave 9
    // release PR per RFC §6.3.

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.read_execution_state", skip_all)]
    async fn read_execution_state(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<PublicState>, EngineError> {
        exec_core::read_execution_state_impl(&self.pool, &self.partition_config, id).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.read_execution_info", skip_all)]
    async fn read_execution_info(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionInfo>, EngineError> {
        exec_core::read_execution_info_impl(&self.pool, &self.partition_config, id).await
    }

    #[tracing::instrument(name = "pg.get_execution_result", skip_all)]
    async fn get_execution_result(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        exec_core::get_execution_result_impl(&self.pool, &self.partition_config, id).await
    }

    // ── RFC-020 Wave 9 Standalone-2 — list_pending_waitpoints (§4.5) ─
    //
    // Read-only projection of `ff_waitpoint_pending` serving the 10-
    // field `PendingWaitpointInfo` contract. Producer-side writes of
    // the 3 new 0011 columns (`state`, `required_signal_names`,
    // `activated_at_ms`) land alongside this method in the same PR —
    // see `suspend_ops::suspend_core` INSERT site. Capability flip
    // deferred to Wave 9 release PR per RFC §6.3.
    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_pending_waitpoints", skip_all)]
    async fn list_pending_waitpoints(
        &self,
        args: ListPendingWaitpointsArgs,
    ) -> Result<ListPendingWaitpointsResult, EngineError> {
        suspend_ops::list_pending_waitpoints_impl(&self.pool, args).await
    }

    // ── RFC-020 Wave 9 Spine-A pt.1 — operator-control mutations (§4.2) ─
    //
    // Two methods landing behind `Supports.cancel_execution` +
    // `Supports.revoke_lease` (both stay `false` until the Wave 9
    // release PR flips them atomically, RFC §6.3). SERIALIZABLE + CAS +
    // `ff_lease_event` outbox emit on the same tx (§4.2.6 + §4.2.7).

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.cancel_execution", skip_all)]
    async fn cancel_execution(
        &self,
        args: CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, EngineError> {
        operator::cancel_execution_impl(&self.pool, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.revoke_lease", skip_all)]
    async fn revoke_lease(
        &self,
        args: RevokeLeaseArgs,
    ) -> Result<RevokeLeaseResult, EngineError> {
        operator::revoke_lease_impl(&self.pool, args).await
    }

    // ── RFC-020 Wave 9 Spine-A pt.2 — operator control + flow cancel (§4.2.3 + §4.2.4 + §4.2.5) ─
    //
    // Four methods landing behind `Supports.change_priority` +
    // `Supports.replay_execution` + `Supports.cancel_flow_header` +
    // `Supports.ack_cancel_member` (all stay `false` until the Wave 9
    // release PR flips them atomically, RFC §6.3). SERIALIZABLE + CAS +
    // `ff_operator_event` outbox emit on the same tx (§4.2.6 + §4.2.7).
    // `ack_cancel_member` is silent on the outbox (Valkey-parity).

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.change_priority", skip_all)]
    async fn change_priority(
        &self,
        args: ChangePriorityArgs,
    ) -> Result<ChangePriorityResult, EngineError> {
        operator::change_priority_impl(&self.pool, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.replay_execution", skip_all)]
    async fn replay_execution(
        &self,
        args: ReplayExecutionArgs,
    ) -> Result<ReplayExecutionResult, EngineError> {
        operator::replay_execution_impl(&self.pool, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.cancel_flow_header", skip_all)]
    async fn cancel_flow_header(
        &self,
        args: CancelFlowArgs,
    ) -> Result<CancelFlowHeader, EngineError> {
        operator::cancel_flow_header_impl(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.ack_cancel_member", skip_all)]
    async fn ack_cancel_member(
        &self,
        flow_id: &FlowId,
        execution_id: &ExecutionId,
    ) -> Result<(), EngineError> {
        operator::ack_cancel_member_impl(
            &self.pool,
            &self.partition_config,
            flow_id.clone(),
            execution_id.clone(),
        )
        .await
    }

    // ── RFC-017 Stage A — ingress (promoted from inherent) ────

    /// RFC-017 Wave 8 Stage E1: lift the inherent
    /// [`PostgresBackend::create_execution`] onto the trait so
    /// ff-server's migrated HTTP handler can dispatch to Postgres.
    /// Post-insert the row is idempotent; the Postgres impl does not
    /// distinguish `Created` from `Duplicate` at the helper level
    /// (both paths commit and return the execution id), so we always
    /// surface `Created { public_state: Waiting }` here. A follow-up
    /// may lift the distinction if a consumer relies on it.
    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.create_execution.trait", skip_all)]
    async fn create_execution(
        &self,
        args: CreateExecutionArgs,
    ) -> Result<CreateExecutionResult, EngineError> {
        let eid = args.execution_id.clone();
        exec_core::create_execution_impl(&self.pool, &self.partition_config, args).await?;
        Ok(CreateExecutionResult::Created {
            execution_id: eid,
            public_state: PublicState::Waiting,
        })
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.create_flow", skip_all)]
    async fn create_flow(
        &self,
        args: CreateFlowArgs,
    ) -> Result<CreateFlowResult, EngineError> {
        flow_staging::create_flow(&self.pool, &self.partition_config, &args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.add_execution_to_flow", skip_all)]
    async fn add_execution_to_flow(
        &self,
        args: AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, EngineError> {
        flow_staging::add_execution_to_flow(&self.pool, &self.partition_config, &args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.stage_dependency_edge", skip_all)]
    async fn stage_dependency_edge(
        &self,
        args: StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, EngineError> {
        flow_staging::stage_dependency_edge(&self.pool, &self.partition_config, &args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.apply_dependency_to_child", skip_all)]
    async fn apply_dependency_to_child(
        &self,
        args: ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, EngineError> {
        flow_staging::apply_dependency_to_child(&self.pool, &self.partition_config, &args).await
    }

    // PR-7b Cluster 4. Async-via-outbox cascade — see trait rustdoc
    // "Timing semantics" for the Valkey-sync vs PG-outbox divergence.
    // This resolves the payload to its `ff_completion_event.event_id`
    // and invokes the existing Wave-5a `dispatch_completion`; further
    // hops ride their own outbox events emitted by the per-hop tx.
    #[cfg(feature = "core")]
    async fn cascade_completion(
        &self,
        payload: &ff_core::backend::CompletionPayload,
    ) -> Result<ff_core::contracts::CascadeOutcome, EngineError> {
        let event_id = match resolve_event_id(&self.pool, payload).await {
            Some(id) => id,
            None => {
                // Event not materialised (outbox race or pre-subscribe
                // payload) — reconciler is the backstop.
                tracing::warn!(
                    execution_id = %payload.execution_id,
                    produced_at_ms = payload.produced_at_ms.0,
                    "pg.cascade_completion: could not resolve event_id; reconciler will claim"
                );
                return Ok(ff_core::contracts::CascadeOutcome::async_dispatched(0));
            }
        };
        let outcome = crate::dispatch::dispatch_completion(&self.pool, event_id).await?;
        let advanced = match outcome {
            crate::dispatch::DispatchOutcome::NoOp => 0,
            crate::dispatch::DispatchOutcome::Advanced(n) => n,
        };
        Ok(ff_core::contracts::CascadeOutcome::async_dispatched(advanced))
    }

    fn backend_label(&self) -> &'static str {
        "postgres"
    }

    /// RFC-018 Stage A: populate the `Capabilities` snapshot from the
    /// static [`postgres_supports_base`] shape. The Postgres backend
    /// landed through RFC-017 Stage E4 at v0.8.0; fields still `false`
    /// correspond to Wave-9 follow-up work (`cancel_flow_header`,
    /// `ack_cancel_member`, read-model, operator control, budget /
    /// quota, `list_pending_waitpoints`). See
    /// `docs/POSTGRES_PARITY_MATRIX.md` for the per-row breakdown.
    fn capabilities(&self) -> ff_core::capability::Capabilities {
        ff_core::capability::Capabilities::new(
            ff_core::capability::BackendIdentity::new(
                "postgres",
                ff_core::capability::Version::new(0, 11, 0),
                "E-shipped",
            ),
            postgres_supports_base(),
        )
    }

    /// Issue #281: no-op. Schema migrations are applied out-of-band
    /// per `rfcs/drafts/v0.7-migration-master.md §Q12` (operator runs
    /// `sqlx migrate run` or the future `ff-migrate` CLI). Boot runs a
    /// schema-version check at connect time
    /// ([`crate::version::check_schema_version`]) and refuses to
    /// start on mismatch, so by the time `prepare()` is callable
    /// there is nothing further to do.
    async fn prepare(
        &self,
    ) -> Result<ff_core::backend::PrepareOutcome, EngineError> {
        Ok(ff_core::backend::PrepareOutcome::NoOp)
    }

    /// RFC-017 Wave 8 Stage E3 (§4 row 9, §7): forward the claim to the
    /// Postgres-native admission pipeline. Returns `NoWork` when no
    /// eligible execution is admissible this scan cycle. Budget
    /// breaches surface as `NoWork` (leaving the row eligible for a
    /// retry by another worker); validation-class rejections
    /// (malformed partition, unknown kid) surface as typed
    /// [`EngineError`] variants mapped to the Server's 400/503 arms.
    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.claim_for_worker", skip_all)]
    async fn claim_for_worker(
        &self,
        args: ff_core::contracts::ClaimForWorkerArgs,
    ) -> Result<ff_core::contracts::ClaimForWorkerOutcome, EngineError> {
        let sched = scheduler::PostgresScheduler::new(self.pool.clone());
        let grant_opt = sched
            .claim_for_worker(
                &args.lane_id,
                &args.worker_id,
                &args.worker_instance_id,
                &args.worker_capabilities,
                args.grant_ttl_ms,
            )
            .await?;
        Ok(match grant_opt {
            Some(g) => ff_core::contracts::ClaimForWorkerOutcome::granted(g),
            None => ff_core::contracts::ClaimForWorkerOutcome::no_work(),
        })
    }

    async fn ping(&self) -> Result<(), EngineError> {
        // Postgres analogue to Valkey PING — single-round-trip pool
        // liveness. Errors propagate as transport-class EngineError via
        // the existing sqlx→EngineError map.
        let _ = sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(error::map_sqlx_error)?;
        Ok(())
    }

    /// RFC-017 Wave 8 Stage E3: drain the scanner supervisor's
    /// reconciler tasks up to `grace`, then close the sqlx pool.
    /// Matches the Valkey backend's shutdown_prepare contract —
    /// bounded best-effort drain, never returns an error.
    async fn shutdown_prepare(&self, grace: Duration) -> Result<(), EngineError> {
        #[cfg(feature = "core")]
        if let Some(handle) = self.scanner_handle.as_ref() {
            let timed_out = handle.shutdown(grace).await;
            if timed_out > 0 {
                tracing::warn!(
                    timed_out,
                    ?grace,
                    "postgres scanner supervisor exceeded grace on shutdown"
                );
            }
        }
        Ok(())
    }

    #[tracing::instrument(name = "pg.cancel_flow", skip_all)]
    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        let result = flow::cancel_flow(&self.pool, &self.partition_config, id, policy).await?;
        if let Some(deadline) = ff_core::engine_backend::cancel_flow_wait_deadline(wait) {
            ff_core::engine_backend::wait_for_flow_cancellation(self, id, deadline).await?;
        }
        Ok(result)
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.set_edge_group_policy", skip_all)]
    async fn set_edge_group_policy(
        &self,
        flow_id: &FlowId,
        downstream_execution_id: &ExecutionId,
        policy: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        flow::set_edge_group_policy(
            &self.pool,
            &self.partition_config,
            flow_id,
            downstream_execution_id,
            policy,
        )
        .await
    }

    // ── Budget ──

    #[tracing::instrument(name = "pg.report_usage", skip_all)]
    async fn report_usage(
        &self,
        _handle: &Handle,
        budget: &BudgetId,
        dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        budget::report_usage_impl(&self.pool, &self.partition_config, budget, dimensions).await
    }

    // ── RFC-020 Wave 9 Standalone-1 — budget/quota admin (§4.4) ─────
    //
    // Five methods landing behind capability flags that stay `false`
    // until the Wave 9 release PR flips them atomically (RFC §6.3).
    // Schema + trait impls land now; capability-surface flip is one
    // PR later.

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.create_budget", skip_all)]
    async fn create_budget(
        &self,
        args: CreateBudgetArgs,
    ) -> Result<CreateBudgetResult, EngineError> {
        budget::create_budget_impl(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.reset_budget", skip_all)]
    async fn reset_budget(
        &self,
        args: ResetBudgetArgs,
    ) -> Result<ResetBudgetResult, EngineError> {
        budget::reset_budget_impl(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.create_quota_policy", skip_all)]
    async fn create_quota_policy(
        &self,
        args: CreateQuotaPolicyArgs,
    ) -> Result<CreateQuotaPolicyResult, EngineError> {
        budget::create_quota_policy_impl(&self.pool, &self.partition_config, args).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.get_budget_status", skip_all)]
    async fn get_budget_status(
        &self,
        id: &BudgetId,
    ) -> Result<BudgetStatus, EngineError> {
        budget::get_budget_status_impl(&self.pool, &self.partition_config, id).await
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.report_usage_admin", skip_all)]
    async fn report_usage_admin(
        &self,
        budget_id: &BudgetId,
        args: ReportUsageAdminArgs,
    ) -> Result<ReportUsageResult, EngineError> {
        budget::report_usage_admin_impl(&self.pool, &self.partition_config, budget_id, args).await
    }

    // ── HMAC secret rotation (v0.7 migration-master Q4) ──
    //
    // Wave 4 replaces this stub with a single INSERT into
    // `ff_waitpoint_hmac(kid, secret, rotated_at)`. Wave 0/1 keep
    // the `Unavailable` shape so a running Postgres backend surfaces
    // the unimplemented op loudly rather than silently no-op'ing.
    #[tracing::instrument(name = "pg.rotate_waitpoint_hmac_secret_all", skip_all)]
    async fn rotate_waitpoint_hmac_secret_all(
        &self,
        args: RotateWaitpointHmacSecretAllArgs,
    ) -> Result<RotateWaitpointHmacSecretAllResult, EngineError> {
        // Wave 4 Agent D: Q4 single-global-row write against
        // `ff_waitpoint_hmac`. `now_ms` is captured here (not
        // inside the impl) so tests can inject a deterministic
        // clock via the pool layer in the future.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        signal::rotate_waitpoint_hmac_secret_all_impl(&self.pool, args, now_ms).await
    }

    #[tracing::instrument(name = "pg.seed_waitpoint_hmac_secret", skip_all)]
    async fn seed_waitpoint_hmac_secret(
        &self,
        args: SeedWaitpointHmacSecretArgs,
    ) -> Result<SeedOutcome, EngineError> {
        // Issue #280: install-only boot-time seed against the global
        // `ff_waitpoint_hmac` table. Idempotent — cairn calls this on
        // every boot and the backend decides whether to INSERT.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        signal::seed_waitpoint_hmac_secret_impl(&self.pool, args, now_ms).await
    }

    // ── Stream reads (streaming feature) ──

    #[cfg(feature = "streaming")]
    #[tracing::instrument(name = "pg.read_stream", skip_all)]
    async fn read_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from: StreamCursor,
        to: StreamCursor,
        count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        stream::read_stream(&self.pool, execution_id, attempt_index, from, to, count_limit).await
    }

    #[cfg(feature = "streaming")]
    #[tracing::instrument(name = "pg.tail_stream", skip_all)]
    async fn tail_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        after: StreamCursor,
        block_ms: u64,
        count_limit: u64,
        visibility: TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        let notifier = self
            .stream_notifier
            .as_ref()
            .ok_or(EngineError::Unavailable {
                op: "pg.tail_stream (notifier not initialised)",
            })?;
        stream::tail_stream(
            &self.pool,
            notifier,
            execution_id,
            attempt_index,
            after,
            block_ms,
            count_limit,
            visibility,
        )
        .await
    }

    #[cfg(feature = "streaming")]
    #[tracing::instrument(name = "pg.read_summary", skip_all)]
    async fn read_summary(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        stream::read_summary(&self.pool, execution_id, attempt_index).await
    }

    // ── RFC-019 Stage A — `subscribe_completion` ──────────────────
    //
    // Postgres real impl. Wraps the existing `ff_completion_event`
    // outbox + `LISTEN ff_completion` machinery
    // (see `completion::subscribe`) and adapts each completion
    // payload into a `stream_subscribe::StreamEvent`.
    //
    // Cursor encoding: `POSTGRES_CURSOR_PREFIX (0x02)` + `event_id`
    // (i64 BE). Stage A resume-from-cursor is not plumbed through the
    // adapter (the existing subscriber tails from `max(event_id)`);
    // Stage B threads the cursor into the replay path. The surface is
    // correct today for consumers that subscribe from tail and
    // persist cursors for future resume.
    #[tracing::instrument(name = "pg.subscribe_completion", skip_all)]
    async fn subscribe_completion(
        &self,
        _cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::CompletionSubscription, EngineError> {
        use ff_core::stream_events::{CompletionEvent, CompletionOutcome};
        use ff_core::stream_subscribe::encode_postgres_event_cursor;
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // Delegate to the existing CompletionBackend implementation so
        // the LISTEN/replay machinery is shared. When a non-noop
        // `ScannerFilter` (#282) is supplied, route through the
        // `_filtered` variant so the outbox-inline SQL filter applies.
        // Resume-from-cursor is still unwired (Stage A surface tails
        // from tail).
        let inner = if filter.is_noop() {
            ff_core::completion_backend::CompletionBackend::subscribe_completions(self).await?
        } else {
            ff_core::completion_backend::CompletionBackend::subscribe_completions_filtered(
                self, filter,
            )
            .await?
        };

        struct Adapter {
            inner: ff_core::completion_backend::CompletionStream,
        }

        impl Stream for Adapter {
            type Item = Result<CompletionEvent, EngineError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                match Pin::new(&mut self.inner).poll_next(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Ready(Some(payload)) => {
                        // Placeholder cursor (0-event_id) because
                        // `CompletionPayload` does not surface
                        // `event_id` today. Family prefix stays stable
                        // so persistence is forward-compatible.
                        let cursor = encode_postgres_event_cursor(0);
                        let event = CompletionEvent::new(
                            cursor,
                            payload.execution_id.clone(),
                            CompletionOutcome::from_wire(&payload.outcome),
                            payload.produced_at_ms,
                        );
                        Poll::Ready(Some(Ok(event)))
                    }
                }
            }
        }

        Ok(Box::pin(Adapter { inner }))
    }

    // ── RFC-019 Stage B — `subscribe_lease_history` ──────────────
    //
    // Real Postgres impl. Tails the `ff_lease_event` outbox (written
    // by producer sites in `attempt.rs`, `flow.rs`, `suspend_ops.rs`,
    // and the `attempt_timeout` / `lease_expiry` reconcilers) via
    // `LISTEN ff_lease_event` + catch-up SELECT. Cursor encoding
    // matches `subscribe_completion`: `0x02 ++ event_id(BE8)`.
    //
    // Partition scope: hardcoded to partition 0 — mirrors the Valkey
    // Stage A impl, which tails partition 0's aggregate stream key.
    // Cross-partition consumers instantiate one backend per
    // partition + merge streams consumer-side (RFC-019 §Backend
    // Semantics).
    #[tracing::instrument(name = "pg.subscribe_lease_history", skip_all)]
    async fn subscribe_lease_history(
        &self,
        cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::LeaseHistorySubscription, EngineError> {
        lease_event_subscribe::subscribe(&self.pool, 0, cursor, filter.clone()).await
    }

    // ── RFC-019 Stage B — `subscribe_signal_delivery` (#310) ─────
    //
    // Tails the `ff_signal_event` outbox (written by the producer
    // INSERT in `suspend_ops::deliver_signal_impl`) via
    // `LISTEN ff_signal_event` + catch-up SELECT. Cursor encoding
    // matches `subscribe_lease_history`: `0x02 ++ event_id(BE8)`.
    //
    // Partition scope: hardcoded to partition 0 — mirrors the Valkey
    // Stage B impl which tails partition 0's aggregate stream key.
    #[tracing::instrument(name = "pg.subscribe_signal_delivery", skip_all)]
    async fn subscribe_signal_delivery(
        &self,
        cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::SignalDeliverySubscription, EngineError> {
        signal_delivery_subscribe::subscribe(&self.pool, 0, cursor, filter.clone()).await
    }

    // ── PR-7b Cluster 1 — Foundation scanner operations ─────────
    //
    // Per-execution wrappers around the reconcilers in
    // `crate::reconcilers::*`. Each reconciler exposes a
    // `*_for_execution` / `*_for_*` helper mirroring the batch
    // `scan_tick` path's per-row tx logic so the engine-side scanner
    // trait-dispatch and the batch reconciler share one SQL code
    // path.
    //
    // All 5 scanner-op methods run real SQL on Postgres; both phases
    // of `expire_execution` (`AttemptTimeout` + `ExecutionDeadline`)
    // are covered. The Wave-9-minimal delivery (this commit) adds
    // `delayed_promoter`, `pending_wp_expiry`, and
    // `execution_deadline` reconcilers on top of the Wave 6c
    // reconcilers shipped with PR-7b/1.
    //
    // Gated on `core` because `reconcilers` (and its `dispatch` dep)
    // require `core`. Without `core` the trait defaults return
    // `EngineError::Unavailable`, preserving behavioural parity with
    // other feature-stripped callsites.

    #[cfg(feature = "core")]
    async fn mark_lease_expired_if_due(
        &self,
        partition: Partition,
        execution_id: &ExecutionId,
    ) -> Result<(), EngineError> {
        let (_pk_from_eid, exec_uuid) = attempt::split_exec_id(execution_id)?;
        let partition_key = partition_index_to_i16(partition)?;
        reconcilers::lease_expiry::release_for_execution(&self.pool, partition_key, exec_uuid)
            .await
    }

    #[cfg(feature = "core")]
    async fn promote_delayed(
        &self,
        partition: Partition,
        _lane: &LaneId,
        execution_id: &ExecutionId,
        now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let (_pk_from_eid, exec_uuid) = attempt::split_exec_id(execution_id)?;
        let partition_key = partition_index_to_i16(partition)?;
        // `lane` is ignored: lane is authoritative on `ff_exec_core`
        // already (it was used only to locate the Valkey ZSET). The
        // candidate selection in `promote_for_execution` re-checks
        // the (lifecycle_phase, eligibility_state, deadline_at_ms)
        // tuple that `attempt::delay()` writes.
        reconcilers::delayed_promoter::promote_for_execution(
            &self.pool,
            partition_key,
            exec_uuid,
            now_ms.0,
        )
        .await
    }

    #[cfg(feature = "core")]
    async fn close_waitpoint(
        &self,
        partition: Partition,
        _execution_id: &ExecutionId,
        waitpoint_id: &str,
        now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        // The scanner resolves (waitpoint_id → owning execution_id)
        // separately for filter application; the close action itself
        // only needs the waitpoint row (which carries `execution_id`
        // + `waitpoint_key`). Partition is authoritative from the
        // caller.
        let partition_key = partition_index_to_i16(partition)?;
        let waitpoint_uuid = uuid::Uuid::parse_str(waitpoint_id).map_err(|e| {
            EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::InvalidInput,
                detail: format!("waitpoint_id not a UUID: {e}"),
            }
        })?;
        reconcilers::pending_wp_expiry::close_for_execution(
            &self.pool,
            partition_key,
            waitpoint_uuid,
            now_ms.0,
        )
        .await
    }

    #[cfg(feature = "core")]
    async fn expire_execution(
        &self,
        partition: Partition,
        execution_id: &ExecutionId,
        phase: ExpirePhase,
        now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let (_pk_from_eid, exec_uuid) = attempt::split_exec_id(execution_id)?;
        let partition_key = partition_index_to_i16(partition)?;
        match phase {
            ExpirePhase::AttemptTimeout => {
                reconcilers::attempt_timeout::expire_for_execution(
                    &self.pool,
                    partition_key,
                    exec_uuid,
                )
                .await
            }
            ExpirePhase::ExecutionDeadline => {
                reconcilers::execution_deadline::expire_for_execution(
                    &self.pool,
                    partition_key,
                    exec_uuid,
                    now_ms.0,
                )
                .await
            }
        }
    }

    #[cfg(feature = "core")]
    async fn expire_suspension(
        &self,
        partition: Partition,
        execution_id: &ExecutionId,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let (_pk_from_eid, exec_uuid) = attempt::split_exec_id(execution_id)?;
        let partition_key = partition_index_to_i16(partition)?;
        reconcilers::suspension_timeout::expire_for_execution(
            &self.pool,
            partition_key,
            exec_uuid,
        )
        .await
    }

    // PR-7b Cluster 2b-B: flow summary projection on Postgres.
    // Aggregates member public_state from ff_exec_core and UPSERTs the
    // derived summary into ff_flow_summary (migration 0019). One SQL
    // round-trip for the aggregation + one for the upsert.
    #[cfg(feature = "core")]
    async fn project_flow_summary(
        &self,
        partition: Partition,
        flow_id: &FlowId,
        now_ms: TimestampMs,
    ) -> Result<bool, EngineError> {
        let partition_key = partition_index_to_i16(partition)?;
        let flow_uuid: sqlx::types::Uuid = flow_id.0;
        flow::project_flow_summary_impl(
            &self.pool,
            partition_key,
            flow_uuid,
            now_ms.0,
        )
        .await
    }

    // PR-7b Cluster 2b-B: retention trim on Postgres.
    // SELECTs a batch of terminal executions past the cutoff, then
    // per-execution DELETEs across every sibling table (no FK CASCADE
    // in the schema, so this is explicit). One transaction per batch.
    #[cfg(feature = "core")]
    async fn trim_retention(
        &self,
        partition: Partition,
        lane_id: &LaneId,
        retention_ms: u64,
        now_ms: TimestampMs,
        batch_size: u32,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<u32, EngineError> {
        let partition_key = partition_index_to_i16(partition)?;
        exec_core::trim_retention_impl(
            &self.pool,
            partition_key,
            lane_id.as_str(),
            retention_ms,
            now_ms.0,
            batch_size,
            filter,
        )
        .await
    }

    // ── PR-7b / #453: typed-FCALL bodies ──

    #[cfg(feature = "core")]
    async fn renew_lease(
        &self,
        args: ff_core::contracts::RenewLeaseArgs,
    ) -> Result<ff_core::contracts::RenewLeaseResult, EngineError> {
        crate::typed_ops::renew_lease(self.pool(), args).await
    }

    #[cfg(feature = "core")]
    async fn complete_execution(
        &self,
        args: ff_core::contracts::CompleteExecutionArgs,
    ) -> Result<ff_core::contracts::CompleteExecutionResult, EngineError> {
        crate::typed_ops::complete_execution(self.pool(), args).await
    }

    #[cfg(feature = "core")]
    async fn fail_execution(
        &self,
        args: ff_core::contracts::FailExecutionArgs,
    ) -> Result<ff_core::contracts::FailExecutionResult, EngineError> {
        crate::typed_ops::fail_execution(self.pool(), args).await
    }

    #[cfg(feature = "core")]
    async fn resume_execution(
        &self,
        args: ff_core::contracts::ResumeExecutionArgs,
    ) -> Result<ff_core::contracts::ResumeExecutionResult, EngineError> {
        crate::typed_ops::resume_execution(self.pool(), args).await
    }

    // ── PR-7b Wave 0a: exec_core field read ──

    async fn read_exec_core_fields(
        &self,
        partition: ff_core::partition::Partition,
        execution_id: &ff_core::types::ExecutionId,
        fields: &[&str],
    ) -> Result<std::collections::HashMap<String, Option<String>>, EngineError> {
        if fields.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        // Cross-check: `partition` and `execution_id.partition()` must
        // agree. A mismatch would silently read the wrong row (or miss)
        // on Valkey via the `{p:N}` key tag, so surface it explicitly
        // as a validation error here.
        let derived: u16 = execution_id.partition();
        if partition.index != derived {
            return Err(EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::InvalidInput,
                detail: format!(
                    "read_exec_core_fields: partition mismatch (arg={}, eid={})",
                    partition.index, derived
                ),
            });
        }
        let partition_key: i16 = partition.index as i16;
        let exec_uuid = crate::exec_core::eid_uuid(execution_id);

        // Build a single SELECT that projects each requested field to
        // a text value. Fields are classified:
        //  - canonical columns (lane_id, lifecycle_phase,
        //    ownership_state, eligibility_state, public_state,
        //    attempt_state, blocking_reason, cancellation_reason,
        //    cancelled_by, attempt_index, flow_id, priority,
        //    created_at_ms, terminal_at_ms, deadline_at_ms): CAST the
        //    column to text. Scanner-facing aliases (current_attempt_index
        //    → attempt_index, completed_at → terminal_at_ms,
        //    cancel_reason → cancellation_reason) project the canonical
        //    column.
        //  - `required_capabilities` is `text[]` on PG; projected as CSV
        //    via `array_to_string(..., ',')` to match Valkey's HMGET
        //    string shape.
        //  - `raw_fields` JSONB-resident names (current_waitpoint_id,
        //    current_worker_instance_id, budget_ids, quota_policy_id)
        //    project via `raw_fields ->> '<field>'`.
        //  - Unknown names project NULL (absent-field parity with
        //    Valkey HMGET).
        let mut projections: Vec<String> = Vec::with_capacity(fields.len());
        for field in fields {
            let expr = match *field {
                // Canonical exec_core columns.
                "lane_id" | "lifecycle_phase" | "ownership_state" | "eligibility_state"
                | "public_state" | "attempt_state" | "blocking_reason" | "cancellation_reason"
                | "cancelled_by" => format!("{f}::text", f = field),
                "attempt_index" => "attempt_index::text".to_string(),
                "flow_id" => "flow_id::text".to_string(),
                "priority" => "priority::text".to_string(),
                "created_at_ms" => "created_at_ms::text".to_string(),
                "terminal_at_ms" => "terminal_at_ms::text".to_string(),
                "deadline_at_ms" => "deadline_at_ms::text".to_string(),
                // Scanner-facing aliases that scan the same data from
                // different angles.
                "current_attempt_index" => "attempt_index::text".to_string(),
                "completed_at" => "terminal_at_ms::text".to_string(),
                "cancel_reason" => "cancellation_reason::text".to_string(),
                // required_capabilities is `text[]` on PG; scanner
                // callers expect a CSV string on Valkey. Convert.
                "required_capabilities" => {
                    "array_to_string(required_capabilities, ',')".to_string()
                }
                // Everything else lives under raw_fields JSONB.
                other => {
                    // Strict allowlist of raw_fields names the scanner
                    // code reads. Any other name returns NULL.
                    match other {
                        "current_waitpoint_id"
                        | "current_worker_instance_id"
                        | "budget_ids"
                        | "quota_policy_id" => format!("raw_fields ->> '{other}'"),
                        _ => "NULL".to_string(),
                    }
                }
            };
            projections.push(expr);
        }
        let projection_sql = projections.join(", ");
        let query = format!(
            "SELECT {projection_sql} FROM ff_exec_core \
             WHERE partition_key = $1 AND execution_id = $2"
        );
        let row_opt = sqlx::query(&query)
            .bind(partition_key)
            .bind(exec_uuid)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| EngineError::Transport {
                backend: "postgres",
                source: format!("read_exec_core_fields: {e}").into(),
            })?;

        let mut out = std::collections::HashMap::with_capacity(fields.len());
        if let Some(row) = row_opt {
            use sqlx::Row;
            for (idx, field) in fields.iter().enumerate() {
                let val: Option<String> =
                    row.try_get(idx).map_err(|e| EngineError::Transport {
                        backend: "postgres",
                        source: format!("read_exec_core_fields[{field}]: {e}").into(),
                    })?;
                out.insert((*field).to_string(), val);
            }
        } else {
            for field in fields {
                out.insert((*field).to_string(), None);
            }
        }
        Ok(out)
    }

    // ── PR-7b Wave 0a: clock primitive ──

    async fn server_time_ms(&self) -> Result<u64, EngineError> {
        // `clock_timestamp()` — `now()` returns the transaction start
        // timestamp and would be stale under any long-running tx.
        // Scanners use this to compute "due" windows, so the wall-
        // clock read must be fresh. Matches the `flow.rs` convention.
        let ms: i64 = sqlx::query_scalar(
            "SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint",
        )
            .fetch_one(self.pool())
            .await
            .map_err(|e| EngineError::Transport {
                backend: "postgres",
                source: format!("server_time_ms: {e}").into(),
            })?;
        if ms < 0 {
            return Err(EngineError::Transport {
                backend: "postgres",
                source: "server_time_ms: negative epoch".into(),
            });
        }
        Ok(ms as u64)
    }
}

/// Resolve a `CompletionPayload` to the matching
/// `ff_completion_event.event_id` for PR-7b cascade dispatch.
///
/// Mirrors the cursor-walk previously inlined in
/// `ff-engine::completion_listener::run_completion_listener_postgres` —
/// keyed on `(partition_key, execution_id_uuid, occurred_at_ms)`. The
/// `partition_key` scoping is what makes this hit the
/// `ff_completion_event_lookup_idx` composite instead of devolving into
/// a cross-partition seq-scan; it's recoverable from the textual
/// `ExecutionId` (`"<partition>:<uuid>"`).
///
/// Returns `None` if the outbox row isn't visible yet (race with the
/// producing tx), the payload's execution id can't be parsed, or the
/// partition prefix isn't an `i16` — all recoverable via the
/// dependency_reconciler backstop. Transient sqlx errors are logged at
/// `warn` with the query inputs and also fall back to `None`; the
/// listener retries on the next payload and the reconciler covers the
/// worst case.
async fn resolve_event_id(
    pool: &PgPool,
    payload: &ff_core::backend::CompletionPayload,
) -> Option<i64> {
    let eid_str = payload.execution_id.as_str();
    // ExecutionId is `{fp:N}:<uuid>`; split on the rightmost `:` so the
    // `{fp:N}` hash-tag prefix stays intact.
    let uuid_str = eid_str.rsplit_once(':').map(|(_, u)| u)?;
    let uuid = uuid::Uuid::parse_str(uuid_str).ok()?;
    // The payload's ExecutionId is already validated (construction
    // enforces the `{fp:N}:<uuid>` shape), so `.partition()` is
    // infallible. Narrow to `i16` for the partition_key column.
    let partition_key = i16::try_from(payload.execution_id.partition()).ok()?;
    let occurred_at_ms = payload.produced_at_ms.0;

    match sqlx::query_scalar::<_, i64>(
        "SELECT event_id FROM ff_completion_event \
         WHERE partition_key = $1 AND execution_id = $2 AND occurred_at_ms = $3 \
         ORDER BY event_id ASC LIMIT 1",
    )
    .bind(partition_key)
    .bind(uuid)
    .bind(occurred_at_ms)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(
                partition_key,
                execution_id = %uuid,
                occurred_at_ms,
                error = %err,
                "resolve_event_id: ff_completion_event lookup failed; falling back to \
                 dependency_reconciler backstop"
            );
            None
        }
    }
}

/// Narrow a `Partition`'s `u16` index into the `i16` the Postgres
/// schema uses as its partition-key column. FlowFabric partitions
/// max out well below `i16::MAX` in practice (production deployments
/// run a few hundred partitions per family), but the conversion is
/// still fallible at the type level. Surface overflow as
/// `ValidationKind::InvalidInput` rather than silently substituting a
/// fallback — a silently mis-routed reconciler would
/// corrupt-by-omission without diagnostics.
fn partition_index_to_i16(partition: Partition) -> Result<i16, EngineError> {
    i16::try_from(partition.index).map_err(|_| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: format!(
            "partition index {} exceeds i16 range (max {})",
            partition.index,
            i16::MAX
        ),
    })
}

/// Minimum recommended `max_locks_per_transaction`. Partition-heavy
/// schemas (256 hash partitions per logical table) can exceed the
/// Postgres default of `64` per tx under modest concurrent bench
/// load — the Wave 7c bench hit `out of shared memory` at 16 workers
/// × 10k tasks with the default and unblocked at `512`. We warn at
/// boot rather than hard-fail because operators may legitimately
/// run with a tuned value that still exceeds 64 but sits below our
/// threshold.
const MIN_MAX_LOCKS_PER_TRANSACTION: i64 = 256;

/// Probe `max_locks_per_transaction` at connect time + log a warning
/// when the current value is below the production-safe threshold.
/// Never fails the connect — probe errors are logged at debug and
/// swallowed (pg_show may be restricted on exotic deploys).
async fn warn_if_max_locks_low(pool: &PgPool) {
    let row: Result<(String,), sqlx::Error> =
        sqlx::query_as("SHOW max_locks_per_transaction")
            .fetch_one(pool)
            .await;
    match row {
        Ok((raw,)) => emit_max_locks_decision(&raw),
        Err(e) => {
            tracing::debug!("failed to probe max_locks_per_transaction: {e}");
        }
    }
}

/// Pure decision surface for the max-locks probe — extracted for
/// unit-testability (the live probe is gated by a running Postgres).
/// Returns the integer value when a warning SHOULD fire, `None`
/// otherwise (either the raw is valid + at/above threshold, or the
/// raw is unparseable — the latter is debug-only).
fn max_locks_warn_value(raw: &str) -> Option<i64> {
    match raw.parse::<i64>() {
        Ok(v) if v < MIN_MAX_LOCKS_PER_TRANSACTION => Some(v),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!(raw, "failed to parse max_locks_per_transaction: {e}");
            None
        }
    }
}

fn emit_max_locks_decision(raw: &str) {
    if let Some(v) = max_locks_warn_value(raw) {
        tracing::warn!(
            current = v,
            recommended = MIN_MAX_LOCKS_PER_TRANSACTION,
            "postgres max_locks_per_transaction={v} is below the recommended \
             minimum ({MIN_MAX_LOCKS_PER_TRANSACTION}); partition-heavy workloads \
             may hit 'out of shared memory' under concurrent load. \
             See docs/operator-guide-postgres.md."
        );
    }
}

#[cfg(test)]
mod max_locks_tests {
    use super::{max_locks_warn_value, MIN_MAX_LOCKS_PER_TRANSACTION};

    #[test]
    fn warns_when_below_threshold() {
        assert_eq!(max_locks_warn_value("64"), Some(64));
        assert_eq!(
            max_locks_warn_value(&(MIN_MAX_LOCKS_PER_TRANSACTION - 1).to_string()),
            Some(MIN_MAX_LOCKS_PER_TRANSACTION - 1)
        );
    }

    #[test]
    fn silent_at_or_above_threshold() {
        assert_eq!(
            max_locks_warn_value(&MIN_MAX_LOCKS_PER_TRANSACTION.to_string()),
            None
        );
        assert_eq!(max_locks_warn_value("1024"), None);
    }

    #[test]
    fn silent_for_unparseable_raw() {
        assert_eq!(max_locks_warn_value("not-a-number"), None);
    }
}

#[cfg(test)]
mod partition_index_tests {
    use super::partition_index_to_i16;
    use ff_core::engine_error::{EngineError, ValidationKind};
    use ff_core::partition::{Partition, PartitionFamily};

    #[test]
    fn accepts_values_within_i16_range() {
        let p = Partition { family: PartitionFamily::Flow, index: 0 };
        assert_eq!(partition_index_to_i16(p).unwrap(), 0);

        let p = Partition { family: PartitionFamily::Flow, index: 255 };
        assert_eq!(partition_index_to_i16(p).unwrap(), 255);

        let p = Partition { family: PartitionFamily::Budget, index: i16::MAX as u16 };
        assert_eq!(partition_index_to_i16(p).unwrap(), i16::MAX);
    }

    #[test]
    fn rejects_overflow_above_i16_max() {
        let p = Partition { family: PartitionFamily::Flow, index: (i16::MAX as u16) + 1 };
        let err = partition_index_to_i16(p).unwrap_err();
        match err {
            EngineError::Validation { kind, detail } => {
                assert_eq!(kind, ValidationKind::InvalidInput);
                assert!(detail.contains("exceeds i16 range"), "unexpected detail: {detail}");
            }
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_u16_max() {
        let p = Partition { family: PartitionFamily::Quota, index: u16::MAX };
        assert!(matches!(
            partition_index_to_i16(p),
            Err(EngineError::Validation { kind: ValidationKind::InvalidInput, .. })
        ));
    }
}
