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
    ClaimResumedExecutionArgs, ClaimResumedExecutionResult, CreateExecutionArgs, DeliverSignalArgs,
    DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ListExecutionsPage,
    ListFlowsPage, ListLanesPage, ListSuspendedPage, SetEdgeGroupPolicyResult,
};
use ff_core::contracts::{
    CancelFlowResult, ExecutionSnapshot, FlowSnapshot, ReportUsageResult,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllResult, SuspendArgs,
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

#[cfg(feature = "core")]
mod admin;
pub mod attempt;
pub mod budget;
pub mod completion;
pub mod error;
pub mod exec_core;
pub mod flow;
pub mod handle_codec;
pub mod listener;
pub mod migrate;
pub mod pool;
pub mod signal;
#[cfg(feature = "streaming")]
pub mod stream;
pub mod suspend;
pub mod version;

pub use completion::{PostgresCompletionStream, COMPLETION_CHANNEL};
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
    /// Wave 4: shared LISTEN notifier. Present on `connect()`-built
    /// backends; `None` on bare `from_pool` constructions that skip
    /// LISTEN wiring (tests that only exercise the write path).
    #[allow(dead_code)]
    stream_notifier: Option<Arc<StreamNotifier>>,
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
        let stream_notifier = Some(StreamNotifier::spawn(pool.clone()));
        let backend = Self {
            pool,
            partition_config: PartitionConfig::default(),
            metrics: None,
            stream_notifier,
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
        })
    }

    /// Create one execution row (+ seed the lane registry if new).
    ///
    /// **RFC-v0.7 Wave 4a.** Inherent method (not on the `EngineBackend`
    /// trait) — the Valkey side drives creates via a direct
    /// `ff_create_execution` FCALL from ff-sdk / ff-server, and the
    /// Postgres side mirrors that pattern: create is an ingress-layer
    /// operation, not a worker-side op, so it sits outside the worker
    /// trait surface. ff-server's request handlers + the integration
    /// test harness call this entry point directly.
    ///
    /// Idempotent on primary-key conflict (`(partition_key,
    /// execution_id)`): a duplicate create returns the caller's
    /// `execution_id` unchanged, matching
    /// [`ff_core::contracts::CreateExecutionResult::Duplicate`].
    #[cfg(feature = "core")]
    #[tracing::instrument(name = "pg.create_execution", skip_all)]
    pub async fn create_execution(
        &self,
        args: CreateExecutionArgs,
    ) -> Result<ExecutionId, EngineError> {
        exec_core::create_execution_impl(&self.pool, &self.partition_config, args).await
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
        token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        attempt::claim_from_reclaim(&self.pool, token).await
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

    #[tracing::instrument(name = "pg.describe_flow", skip_all)]
    async fn describe_flow(
        &self,
        id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        flow::describe_flow(&self.pool, &self.partition_config, id).await
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
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        flow::cancel_flow(&self.pool, &self.partition_config, id, policy, wait).await
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
}
