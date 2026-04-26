//! `SqliteBackend` — RFC-023 dev-only SQLite [`EngineBackend`] impl.
//!
//! Phase 1a lands the scaffolding: construction guard, registry
//! dedup, pool setup, WARN banner, and Unavailable stubs for every
//! required trait method. Phase 2+ progressively replaces the stubs
//! with real bodies paralleling `ff-backend-postgres`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint,
    ReclaimToken, ResumeSignal, UsageDimensions,
};
use ff_core::capability::{BackendIdentity, Capabilities, Supports, Version};
#[cfg(feature = "core")]
use ff_core::contracts::{
    ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs,
    DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ListExecutionsPage,
    ListFlowsPage, ListLanesPage, ListSuspendedPage, SetEdgeGroupPolicyResult,
};
use ff_core::contracts::{
    CancelFlowResult, ExecutionSnapshot, FlowSnapshot, ReportUsageResult, SuspendArgs,
    SuspendOutcome,
};
#[cfg(feature = "streaming")]
use ff_core::contracts::{StreamCursor, StreamFrames};
use ff_core::backend::PrepareOutcome;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{BackendError, EngineError};
#[cfg(feature = "core")]
use ff_core::partition::PartitionKey;
#[cfg(feature = "streaming")]
use ff_core::types::AttemptIndex;
#[cfg(feature = "core")]
use ff_core::types::EdgeId;
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};

use crate::pubsub::PubSub;
use crate::registry;

/// Phase-1a-wide `Unavailable` helper. Each stubbed method names
/// itself here so call-site errors carry a stable identifier.
#[inline]
fn unavailable<T>(op: &'static str) -> Result<T, EngineError> {
    Err(EngineError::Unavailable { op })
}

/// Internal shared state. `Arc<SqliteBackendInner>` is what the
/// registry stores weak references to and what `SqliteBackend`
/// wraps.
pub(crate) struct SqliteBackendInner {
    /// Connection pool. Held live even when the trait-object surface
    /// isn't exercising it so Phase 2+ can migrate bodies without
    /// re-plumbing construction.
    #[allow(dead_code)]
    pub(crate) pool: SqlitePool,
    /// Per-backend wakeup channels (Phase 3 wiring).
    #[allow(dead_code)]
    pub(crate) pubsub: PubSub,
    /// Registry key (canonical path or verbatim `:memory:` URI).
    /// Held for Drop-time cleanup if we need it in a future phase;
    /// today the `Weak` entries decay naturally.
    #[allow(dead_code)]
    pub(crate) key: PathBuf,
}

/// RFC-023 SQLite dev-only backend.
///
/// Construction demands `FF_DEV_MODE=1` (§4.5). Identical paths
/// within a process return the same handle via the §4.2 B6
/// registry.
#[derive(Clone)]
pub struct SqliteBackend {
    inner: Arc<SqliteBackendInner>,
}

impl std::fmt::Debug for SqliteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteBackend")
            .field("key", &self.inner.key)
            .finish()
    }
}

impl SqliteBackend {
    /// RFC-023 Phase 1a entry point. `path` accepts a filesystem
    /// path, `:memory:`, or a `file:...?mode=memory&cache=shared`
    /// URI.
    ///
    /// # Errors
    ///
    /// * [`BackendError::RequiresDevMode`] when `FF_DEV_MODE` is
    ///   unset or not `"1"`.
    /// * [`BackendError::Valkey`] (historical name — the classifier
    ///   is backend-agnostic despite the variant name) when the
    ///   pool cannot be constructed.
    pub async fn new(path: &str) -> Result<Arc<Self>, BackendError> {
        // §4.5 production guard — TYPE-level emission point (§3.3 A3).
        if std::env::var("FF_DEV_MODE").as_deref() != Ok("1") {
            return Err(BackendError::RequiresDevMode);
        }

        // §3.3 WARN banner — emits on every successful construction.
        tracing::warn!(
            "FlowFabric SQLite backend active (FF_DEV_MODE=1). \
             This backend is dev-only; single-writer, single-process, \
             not supported in production. See RFC-023."
        );

        // §4.2 B6: canonicalize the key. `:memory:` and
        // `file::memory:...` pass through verbatim (distinct per-URI
        // entries via embedded UUIDs). Filesystem paths resolve via
        // `fs::canonicalize` when the file exists; absent files fall
        // back to the raw path so two concurrent constructions before
        // file creation still dedup.
        let key = if path == ":memory:" || path.starts_with("file::memory:") {
            PathBuf::from(path)
        } else {
            std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
        };

        if let Some(existing) = registry::lookup(&key) {
            return Ok(Arc::new(Self { inner: existing }));
        }

        // Build the pool. sqlx's SqliteConnectOptions parses the full
        // URI form as well as plain paths. `create_if_missing` is
        // what embedded-test consumers expect.
        let opts: SqliteConnectOptions = path
            .parse::<SqliteConnectOptions>()
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Protocol,
                message: format!("sqlite connect-opts parse for {path:?}: {e}"),
            })?
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .map_err(|e| BackendError::Valkey {
                kind: ff_core::engine_error::BackendErrorKind::Transport,
                message: format!("sqlite pool connect for {path:?}: {e}"),
            })?;

        // Phase 1a: no migrations to apply — the migrations directory
        // is an empty placeholder with a `.sqlite-skip` tombstone.
        // Phase 1b lands `sqlx::migrate!("./migrations").run(&pool)`
        // alongside the first real migration file.

        let inner = Arc::new(SqliteBackendInner {
            pool,
            pubsub: PubSub::new(),
            key: key.clone(),
        });
        let inner = registry::insert(key, inner);
        Ok(Arc::new(Self { inner }))
    }

    /// Accessor for Phase 2+ code that needs direct pool access
    /// without re-routing through the trait surface.
    #[allow(dead_code)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.inner.pool
    }
}

#[async_trait]
impl EngineBackend for SqliteBackend {
    // ── Claim + lifecycle ──

    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        unavailable("sqlite.claim")
    }

    async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        unavailable("sqlite.renew")
    }

    async fn progress(
        &self,
        _handle: &Handle,
        _percent: Option<u8>,
        _message: Option<String>,
    ) -> Result<(), EngineError> {
        unavailable("sqlite.progress")
    }

    async fn append_frame(
        &self,
        _handle: &Handle,
        _frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        unavailable("sqlite.append_frame")
    }

    async fn complete(
        &self,
        _handle: &Handle,
        _payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        unavailable("sqlite.complete")
    }

    async fn fail(
        &self,
        _handle: &Handle,
        _reason: FailureReason,
        _classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        unavailable("sqlite.fail")
    }

    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        unavailable("sqlite.cancel")
    }

    async fn suspend(
        &self,
        _handle: &Handle,
        _args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        unavailable("sqlite.suspend")
    }

    async fn create_waitpoint(
        &self,
        _handle: &Handle,
        _waitpoint_key: &str,
        _expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        unavailable("sqlite.create_waitpoint")
    }

    async fn observe_signals(&self, _handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError> {
        unavailable("sqlite.observe_signals")
    }

    async fn claim_from_reclaim(
        &self,
        _token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        unavailable("sqlite.claim_from_reclaim")
    }

    async fn delay(
        &self,
        _handle: &Handle,
        _delay_until: TimestampMs,
    ) -> Result<(), EngineError> {
        unavailable("sqlite.delay")
    }

    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        unavailable("sqlite.wait_children")
    }

    // ── Read / admin ──

    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        unavailable("sqlite.describe_execution")
    }

    async fn describe_flow(
        &self,
        _id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        unavailable("sqlite.describe_flow")
    }

    #[cfg(feature = "core")]
    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        unavailable("sqlite.list_edges")
    }

    #[cfg(feature = "core")]
    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        unavailable("sqlite.describe_edge")
    }

    #[cfg(feature = "core")]
    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        unavailable("sqlite.resolve_execution_flow_id")
    }

    #[cfg(feature = "core")]
    async fn list_flows(
        &self,
        _partition: PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        unavailable("sqlite.list_flows")
    }

    #[cfg(feature = "core")]
    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        unavailable("sqlite.list_lanes")
    }

    #[cfg(feature = "core")]
    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        unavailable("sqlite.list_suspended")
    }

    #[cfg(feature = "core")]
    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        unavailable("sqlite.list_executions")
    }

    #[cfg(feature = "core")]
    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        unavailable("sqlite.deliver_signal")
    }

    #[cfg(feature = "core")]
    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        unavailable("sqlite.claim_resumed_execution")
    }

    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        unavailable("sqlite.cancel_flow")
    }

    #[cfg(feature = "core")]
    async fn set_edge_group_policy(
        &self,
        _flow_id: &FlowId,
        _downstream_execution_id: &ExecutionId,
        _policy: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        unavailable("sqlite.set_edge_group_policy")
    }

    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        unavailable("sqlite.report_usage")
    }

    #[cfg(feature = "streaming")]
    async fn read_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("sqlite.read_stream")
    }

    #[cfg(feature = "streaming")]
    async fn tail_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _after: StreamCursor,
        _block_ms: u64,
        _count_limit: u64,
        _visibility: ff_core::backend::TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("sqlite.tail_stream")
    }

    #[cfg(feature = "streaming")]
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<ff_core::backend::SummaryDocument>, EngineError> {
        unavailable("sqlite.read_summary")
    }

    // ── RFC-018 capability discovery ──

    fn backend_label(&self) -> &'static str {
        "sqlite"
    }

    fn capabilities(&self) -> Capabilities {
        // RFC-023 §4.3: Phase 1a exposes only the identity tuple; real
        // `Supports::*` flags flip at the Phase 4 release PR when trait
        // bodies ship. Consumers seeing `supports.*` all-false under
        // "sqlite" know to expect `Unavailable` on every data-plane
        // call until Phase 2+ lands.
        Capabilities::new(
            BackendIdentity::new(
                "sqlite",
                Version::new(
                    env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or(0),
                    env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or(0),
                    env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or(0),
                ),
                "Phase-1a",
            ),
            Supports::none(),
        )
    }

    async fn prepare(&self) -> Result<PrepareOutcome, EngineError> {
        // Phase 1a: no boot-time prep (no migrations yet). Phase 1b
        // applies the migrations inside `SqliteBackend::new` itself
        // rather than here, matching the PG posture.
        Ok(PrepareOutcome::NoOp)
    }
}
