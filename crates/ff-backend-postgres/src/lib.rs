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
    PendingWaitpoint, ReclaimToken, ResumeSignal, SummaryDocument, TailVisibility,
    UsageDimensions,
};
#[cfg(feature = "core")]
use ff_core::contracts::{
    ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs, DeliverSignalResult,
    EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ListExecutionsPage, ListFlowsPage,
    ListLanesPage, ListSuspendedPage, SetEdgeGroupPolicyResult,
};
use ff_core::contracts::{
    CancelFlowResult, ExecutionSnapshot, FlowSnapshot, ReportUsageResult, SuspendArgs,
    SuspendOutcome,
};
#[cfg(feature = "streaming")]
use ff_core::contracts::{StreamCursor, StreamFrames};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
#[cfg(feature = "core")]
use ff_core::partition::PartitionKey;
use ff_core::partition::PartitionConfig;
#[cfg(feature = "streaming")]
use ff_core::types::AttemptIndex;
#[cfg(feature = "core")]
use ff_core::types::EdgeId;
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};
use sqlx::PgPool;

pub mod error;
pub mod listener;
pub mod migrate;
pub mod pool;
pub mod version;

pub use error::{map_sqlx_error, PostgresTransportError};
pub use listener::StreamNotifier;
pub use migrate::{apply_migrations, MigrationError};
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
pub struct PostgresBackend {
    #[allow(dead_code)] // filled in across waves 2-7
    pool: PgPool,
    #[allow(dead_code)]
    partition_config: PartitionConfig,
    #[allow(dead_code)]
    metrics: Option<Arc<ff_observability::Metrics>>,
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
        let backend = Self {
            pool,
            partition_config: PartitionConfig::default(),
            metrics: None,
        };
        Ok(Arc::new(backend))
    }

    /// Test / advanced constructor: build a `PostgresBackend` from an
    /// already-constructed `PgPool` + explicit partition config. No
    /// network I/O. Useful for integration tests against a shared
    /// pool and for a future migration CLI that wants to reuse a pool
    /// across migrate-run + smoke-check.
    pub fn from_pool(pool: PgPool, partition_config: PartitionConfig) -> Arc<Self> {
        Arc::new(Self {
            pool,
            partition_config,
            metrics: None,
        })
    }
}

/// Short helper: every stub method returns this. Kept as a function
/// (rather than a macro) so rust-analyzer / IDE jumps show a single
/// definition site for the Wave 0 stub pattern.
#[inline]
fn unavailable<T>(op: &'static str) -> Result<T, EngineError> {
    Err(EngineError::Unavailable { op })
}

#[async_trait]
impl EngineBackend for PostgresBackend {
    // ── Claim + lifecycle ──

    #[tracing::instrument(name = "pg.claim", skip_all)]
    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        unavailable("pg.claim")
    }

    #[tracing::instrument(name = "pg.renew", skip_all)]
    async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        unavailable("pg.renew")
    }

    #[tracing::instrument(name = "pg.progress", skip_all)]
    async fn progress(
        &self,
        _handle: &Handle,
        _percent: Option<u8>,
        _message: Option<String>,
    ) -> Result<(), EngineError> {
        unavailable("pg.progress")
    }

    #[tracing::instrument(name = "pg.append_frame", skip_all)]
    async fn append_frame(
        &self,
        _handle: &Handle,
        _frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        unavailable("pg.append_frame")
    }

    #[tracing::instrument(name = "pg.complete", skip_all)]
    async fn complete(
        &self,
        _handle: &Handle,
        _payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        unavailable("pg.complete")
    }

    #[tracing::instrument(name = "pg.fail", skip_all)]
    async fn fail(
        &self,
        _handle: &Handle,
        _reason: FailureReason,
        _classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        unavailable("pg.fail")
    }

    #[tracing::instrument(name = "pg.cancel", skip_all)]
    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        unavailable("pg.cancel")
    }

    #[tracing::instrument(name = "pg.suspend", skip_all)]
    async fn suspend(
        &self,
        _handle: &Handle,
        _args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        unavailable("pg.suspend")
    }

    #[tracing::instrument(name = "pg.create_waitpoint", skip_all)]
    async fn create_waitpoint(
        &self,
        _handle: &Handle,
        _waitpoint_key: &str,
        _expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        unavailable("pg.create_waitpoint")
    }

    #[tracing::instrument(name = "pg.observe_signals", skip_all)]
    async fn observe_signals(
        &self,
        _handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        unavailable("pg.observe_signals")
    }

    #[tracing::instrument(name = "pg.claim_from_reclaim", skip_all)]
    async fn claim_from_reclaim(
        &self,
        _token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        unavailable("pg.claim_from_reclaim")
    }

    #[tracing::instrument(name = "pg.delay", skip_all)]
    async fn delay(
        &self,
        _handle: &Handle,
        _delay_until: TimestampMs,
    ) -> Result<(), EngineError> {
        unavailable("pg.delay")
    }

    #[tracing::instrument(name = "pg.wait_children", skip_all)]
    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        unavailable("pg.wait_children")
    }

    // ── Read / admin ──

    #[tracing::instrument(name = "pg.describe_execution", skip_all)]
    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        unavailable("pg.describe_execution")
    }

    #[tracing::instrument(name = "pg.describe_flow", skip_all)]
    async fn describe_flow(
        &self,
        _id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        unavailable("pg.describe_flow")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_edges", skip_all)]
    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        unavailable("pg.list_edges")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.describe_edge", skip_all)]
    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        unavailable("pg.describe_edge")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.resolve_execution_flow_id", skip_all)]
    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        unavailable("pg.resolve_execution_flow_id")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_flows", skip_all)]
    async fn list_flows(
        &self,
        _partition: PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        unavailable("pg.list_flows")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_lanes", skip_all)]
    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        unavailable("pg.list_lanes")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_suspended", skip_all)]
    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        unavailable("pg.list_suspended")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.list_executions", skip_all)]
    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        unavailable("pg.list_executions")
    }

    // ── Trigger ops (issue #150) ──

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.deliver_signal", skip_all)]
    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        unavailable("pg.deliver_signal")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.claim_resumed_execution", skip_all)]
    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        unavailable("pg.claim_resumed_execution")
    }

    #[tracing::instrument(name = "pg.cancel_flow", skip_all)]
    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        unavailable("pg.cancel_flow")
    }

    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.set_edge_group_policy", skip_all)]
    async fn set_edge_group_policy(
        &self,
        _flow_id: &FlowId,
        _downstream_execution_id: &ExecutionId,
        _policy: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        unavailable("pg.set_edge_group_policy")
    }

    // ── Budget ──

    #[tracing::instrument(name = "pg.report_usage", skip_all)]
    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        unavailable("pg.report_usage")
    }

    // ── Stream reads (streaming feature) ──

    #[cfg(feature = "streaming")]
    #[tracing::instrument(name = "pg.read_stream", skip_all)]
    async fn read_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("pg.read_stream")
    }

    #[cfg(feature = "streaming")]
    #[tracing::instrument(name = "pg.tail_stream", skip_all)]
    async fn tail_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _after: StreamCursor,
        _block_ms: u64,
        _count_limit: u64,
        _visibility: TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        unavailable("pg.tail_stream")
    }

    #[cfg(feature = "streaming")]
    #[tracing::instrument(name = "pg.read_summary", skip_all)]
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        unavailable("pg.read_summary")
    }
}
