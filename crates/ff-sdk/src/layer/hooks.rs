//! Shared plumbing for built-in layers.
//!
//! Each built-in layer (tracing, ratelimit, metrics, circuit-breaker)
//! plugs into an `EngineBackend` impl via a small `LayerHooks`
//! trait: one synchronous `before(method)` and one
//! `after(method, duration, outcome)`. The generated
//! `HookedBackend<H>` impl below handles the 17-method dispatch so
//! layers don't re-implement it.
//!
//! Layers that need more than before/after (e.g. the circuit breaker
//! needs to *replace* the call with a synthetic error when open)
//! return `Admit::Reject(err)` from `before` — the dispatch arm
//! returns the err and skips the delegate call.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy, FailOutcome,
    FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint, ReclaimToken,
    ResumeSignal, WaitpointSpec,
};
use ff_core::contracts::{
    CancelFlowResult, EdgeDirection, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot, ListFlowsPage,
    ListLanesPage, ReportUsageResult,
};
#[cfg(feature = "valkey-default")]
use ff_core::contracts::{StreamCursor, StreamFrames};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
#[cfg(feature = "valkey-default")]
use ff_core::types::AttemptIndex;
use ff_core::types::{BudgetId, EdgeId, ExecutionId, FlowId, LaneId, TimestampMs};

/// Outcome of a delegated call, as a borrowed view for hook
/// consumption. Kept allocation-free: error category is a
/// `&'static str` (see `BackendErrorKind::as_stable_str` for the
/// shape the built-in metrics sink uses).
#[derive(Clone, Copy, Debug)]
pub enum HookOutcome<'a> {
    Ok,
    Err(&'a EngineError),
}

/// Admission decision from `LayerHooks::before`. Default is
/// `Admit::Proceed`. Layers that need to short-circuit (rate-limit
/// reject, breaker open) return `Reject(err)`.
pub enum Admit {
    Proceed,
    /// Short-circuit with an error. Boxed — `EngineError`'s richest
    /// variant is ~200 bytes; boxing keeps the enum small per the
    /// `SdkError::Engine` convention elsewhere in this crate.
    Reject(Box<EngineError>),
}

impl Admit {
    /// Construct a `Reject` variant from an `EngineError`. Convenience
    /// so layers don't have to spell `Box::new` at every call site.
    pub fn reject(err: EngineError) -> Self {
        Self::Reject(Box::new(err))
    }
}

/// Per-layer plumbing. All hooks are synchronous and must not block
/// the executor meaningfully — they run on every backend call.
pub trait LayerHooks: Send + Sync + 'static {
    /// Called before the delegate. `method_name` is a `&'static str`
    /// matching the trait method name exactly. Return `Admit::Reject`
    /// to short-circuit.
    fn before(&self, method_name: &'static str) -> Admit;

    /// Called after the delegate (or after a `Reject`). `elapsed`
    /// measures from just after `before`'s admission to now.
    fn after(&self, method_name: &'static str, elapsed: Duration, outcome: HookOutcome<'_>);
}

/// `EngineBackend` wrapper that applies `LayerHooks` to every method
/// call. Construct via the per-layer `.layer(..)` impl, which picks
/// the right `H`.
pub struct HookedBackend<H: LayerHooks> {
    pub(crate) inner: Arc<dyn EngineBackend>,
    pub(crate) hooks: H,
}

impl<H: LayerHooks> HookedBackend<H> {
    pub(crate) fn new(inner: Arc<dyn EngineBackend>, hooks: H) -> Self {
        Self { inner, hooks }
    }
}

/// Helper: run the before hook, and if it admits, run the delegate
/// closure and observe the outcome; otherwise return the rejection.
/// Encapsulates the timing + outcome-reporting boilerplate.
macro_rules! with_hooks {
    ($self:ident, $method_name:literal, $delegate:expr) => {{
        match $self.hooks.before($method_name) {
            Admit::Proceed => {
                let start = Instant::now();
                let result = $delegate;
                let elapsed = start.elapsed();
                let hook_outcome = match &result {
                    Ok(_) => HookOutcome::Ok,
                    Err(e) => HookOutcome::Err(e),
                };
                $self.hooks.after($method_name, elapsed, hook_outcome);
                result
            }
            Admit::Reject(err) => {
                let elapsed = Duration::ZERO;
                $self
                    .hooks
                    .after($method_name, elapsed, HookOutcome::Err(&err));
                Err(*err)
            }
        }
    }};
}

#[async_trait]
impl<H: LayerHooks> EngineBackend for HookedBackend<H> {
    async fn claim(
        &self,
        lane: &LaneId,
        capabilities: &CapabilitySet,
        policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        with_hooks!(
            self,
            "claim",
            self.inner.claim(lane, capabilities, policy).await
        )
    }

    async fn renew(&self, handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        with_hooks!(self, "renew", self.inner.renew(handle).await)
    }

    async fn progress(
        &self,
        handle: &Handle,
        percent: Option<u8>,
        message: Option<String>,
    ) -> Result<(), EngineError> {
        with_hooks!(
            self,
            "progress",
            self.inner.progress(handle, percent, message).await
        )
    }

    async fn append_frame(
        &self,
        handle: &Handle,
        frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        with_hooks!(
            self,
            "append_frame",
            self.inner.append_frame(handle, frame).await
        )
    }

    async fn complete(&self, handle: &Handle, payload: Option<Vec<u8>>) -> Result<(), EngineError> {
        with_hooks!(self, "complete", self.inner.complete(handle, payload).await)
    }

    async fn fail(
        &self,
        handle: &Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        with_hooks!(
            self,
            "fail",
            self.inner.fail(handle, reason, classification).await
        )
    }

    async fn cancel(&self, handle: &Handle, reason: &str) -> Result<(), EngineError> {
        with_hooks!(self, "cancel", self.inner.cancel(handle, reason).await)
    }

    async fn suspend(
        &self,
        handle: &Handle,
        waitpoints: Vec<WaitpointSpec>,
        timeout: Option<Duration>,
    ) -> Result<Handle, EngineError> {
        with_hooks!(
            self,
            "suspend",
            self.inner.suspend(handle, waitpoints, timeout).await
        )
    }

    async fn create_waitpoint(
        &self,
        handle: &Handle,
        waitpoint_key: &str,
        expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        with_hooks!(
            self,
            "create_waitpoint",
            self.inner
                .create_waitpoint(handle, waitpoint_key, expires_in)
                .await
        )
    }

    async fn observe_signals(&self, handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError> {
        with_hooks!(
            self,
            "observe_signals",
            self.inner.observe_signals(handle).await
        )
    }

    async fn claim_from_reclaim(&self, token: ReclaimToken) -> Result<Option<Handle>, EngineError> {
        with_hooks!(
            self,
            "claim_from_reclaim",
            self.inner.claim_from_reclaim(token).await
        )
    }

    async fn delay(&self, handle: &Handle, delay_until: TimestampMs) -> Result<(), EngineError> {
        with_hooks!(self, "delay", self.inner.delay(handle, delay_until).await)
    }

    async fn wait_children(&self, handle: &Handle) -> Result<(), EngineError> {
        with_hooks!(
            self,
            "wait_children",
            self.inner.wait_children(handle).await
        )
    }

    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        with_hooks!(
            self,
            "describe_execution",
            self.inner.describe_execution(id).await
        )
    }

    async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError> {
        with_hooks!(self, "describe_flow", self.inner.describe_flow(id).await)
    }

    async fn list_edges(
        &self,
        flow_id: &FlowId,
        direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        with_hooks!(
            self,
            "list_edges",
            self.inner.list_edges(flow_id, direction).await
        )
    }

    async fn describe_edge(
        &self,
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        with_hooks!(
            self,
            "describe_edge",
            self.inner.describe_edge(flow_id, edge_id).await
        )
    }

    async fn resolve_execution_flow_id(
        &self,
        eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        with_hooks!(
            self,
            "resolve_execution_flow_id",
            self.inner.resolve_execution_flow_id(eid).await
        )
    }

    async fn list_flows(
        &self,
        partition: ff_core::partition::PartitionKey,
        cursor: Option<FlowId>,
        limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        with_hooks!(
            self,
            "list_flows",
            self.inner.list_flows(partition, cursor, limit).await
        )
    }

    async fn list_lanes(
        &self,
        cursor: Option<LaneId>,
        limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        with_hooks!(
            self,
            "list_lanes",
            self.inner.list_lanes(cursor, limit).await
        )
    }

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        with_hooks!(
            self,
            "cancel_flow",
            self.inner.cancel_flow(id, policy, wait).await
        )
    }

    async fn report_usage(
        &self,
        handle: &Handle,
        budget: &BudgetId,
        dimensions: ff_core::backend::UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        with_hooks!(
            self,
            "report_usage",
            self.inner.report_usage(handle, budget, dimensions).await
        )
    }

    #[cfg(feature = "core")]
    async fn list_lanes(
        &self,
        cursor: Option<LaneId>,
        limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        with_hooks!(
            self,
            "list_lanes",
            self.inner.list_lanes(cursor, limit).await
        )
    }

    #[cfg(feature = "valkey-default")]
    async fn read_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from: StreamCursor,
        to: StreamCursor,
        count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        with_hooks!(
            self,
            "read_stream",
            self.inner
                .read_stream(execution_id, attempt_index, from, to, count_limit)
                .await
        )
    }

    #[cfg(feature = "valkey-default")]
    async fn tail_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        after: StreamCursor,
        block_ms: u64,
        count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        with_hooks!(
            self,
            "tail_stream",
            self.inner
                .tail_stream(execution_id, attempt_index, after, block_ms, count_limit)
                .await
        )
    }
}
