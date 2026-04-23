//! In-memory passthrough [`EngineBackend`] used by layer unit tests.
//!
//! Not public — the only consumers are the layer unit tests, which
//! need to verify a layer's observe / short-circuit behaviour
//! without spinning up a real Valkey. Methods return synthetic
//! success values (or a configurable error) and count calls per
//! method name.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{
    AppendFrameOutcome, BackendTag, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, HandleKind, HandleOpaque,
    LeaseRenewal, PendingWaitpoint, ReclaimToken, ResumeSignal, UsageDimensions, WaitpointHmac,
    WaitpointSpec,
};
use ff_core::contracts::{
    CancelFlowResult, ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs,
    DeliverSignalResult, EdgeDirection, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot,
    ListExecutionsPage, ListFlowsPage, ListLanesPage, ListSuspendedPage, ReportUsageResult,
};
use ff_core::partition::PartitionKey;
#[cfg(feature = "valkey-default")]
use ff_core::contracts::{StreamCursor, StreamFrames};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{BugKind, EngineError};
#[cfg(feature = "valkey-default")]
use ff_core::types::AttemptIndex;
use ff_core::types::{BudgetId, EdgeId, ExecutionId, FlowId, LaneId, TimestampMs, WaitpointId};

#[derive(Default)]
pub(crate) struct PassthroughBackend {
    calls: Mutex<HashMap<&'static str, usize>>,
    /// When set, methods return `EngineError::Transport { backend:
    /// "test", source: "forced" }` so the circuit-breaker tests can
    /// drive the breaker open.
    fail_transport: AtomicBool,
}

impl PassthroughBackend {
    pub(crate) fn calls(&self, method: &'static str) -> usize {
        self.calls.lock().unwrap().get(method).copied().unwrap_or(0)
    }

    pub(crate) fn set_fail_transport(&self, on: bool) {
        self.fail_transport.store(on, Ordering::SeqCst);
    }

    fn record(&self, method: &'static str) -> Result<(), EngineError> {
        *self.calls.lock().unwrap().entry(method).or_insert(0) += 1;
        if self.fail_transport.load(Ordering::SeqCst) {
            return Err(EngineError::Transport {
                backend: "test",
                source: "forced".into(),
            });
        }
        Ok(())
    }

    fn handle() -> Handle {
        Handle::new(
            BackendTag::Valkey,
            HandleKind::Fresh,
            HandleOpaque::new(Box::new([])),
        )
    }
}

#[async_trait]
impl EngineBackend for PassthroughBackend {
    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        self.record("claim")?;
        Ok(None)
    }

    async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        self.record("renew")?;
        Ok(LeaseRenewal::new(0, 0))
    }

    async fn progress(
        &self,
        _handle: &Handle,
        _percent: Option<u8>,
        _message: Option<String>,
    ) -> Result<(), EngineError> {
        self.record("progress")
    }

    async fn append_frame(
        &self,
        _handle: &Handle,
        _frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        self.record("append_frame")?;
        Ok(AppendFrameOutcome::new(String::new(), 0))
    }

    async fn complete(
        &self,
        _handle: &Handle,
        _payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        self.record("complete")
    }

    async fn fail(
        &self,
        _handle: &Handle,
        _reason: FailureReason,
        _classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        self.record("fail")?;
        // Use a synthetic Bug err to materialise a FailOutcome shape —
        // we don't actually touch the variants, so any default-like is
        // fine. Instead, return a plausible outcome via a synthetic
        // path: since FailOutcome is non-exhaustive with internal
        // fields, ask the test to use an alternate method if it needs
        // a specific shape. Default case: return Engine bug to force
        // the test to declare fail()-path assertions explicitly.
        Err(EngineError::Bug(BugKind::AttemptNotInCreatedState))
    }

    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        self.record("cancel")
    }

    async fn suspend(
        &self,
        _handle: &Handle,
        _waitpoints: Vec<WaitpointSpec>,
        _timeout: Option<Duration>,
    ) -> Result<Handle, EngineError> {
        self.record("suspend")?;
        Ok(Self::handle())
    }

    async fn create_waitpoint(
        &self,
        _handle: &Handle,
        _waitpoint_key: &str,
        _expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        self.record("create_waitpoint")?;
        Ok(PendingWaitpoint::new(
            WaitpointId::new(),
            WaitpointHmac::new("tok:0"),
        ))
    }

    async fn observe_signals(&self, _handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError> {
        self.record("observe_signals")?;
        Ok(Vec::new())
    }

    async fn claim_from_reclaim(
        &self,
        _token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        self.record("claim_from_reclaim")?;
        Ok(None)
    }

    async fn delay(&self, _handle: &Handle, _delay_until: TimestampMs) -> Result<(), EngineError> {
        self.record("delay")
    }

    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        self.record("wait_children")
    }

    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        self.record("describe_execution")?;
        Ok(None)
    }

    async fn describe_flow(&self, _id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError> {
        self.record("describe_flow")?;
        Ok(None)
    }

    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        self.record("list_edges")?;
        Ok(Vec::new())
    }

    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        self.record("describe_edge")?;
        Ok(None)
    }

    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        self.record("resolve_execution_flow_id")?;
        Ok(None)
    }

    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        self.record("list_lanes")?;
        Ok(ListLanesPage::new(Vec::new(), None))
    }

    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        self.record("list_suspended")?;
        Ok(ListSuspendedPage::new(Vec::new(), None))
    }

    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        self.record("cancel_flow")?;
        Err(EngineError::Unavailable { op: "cancel_flow" })
    }

    async fn list_flows(
        &self,
        _partition: ff_core::partition::PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        self.record("list_flows")?;
        Ok(ListFlowsPage::new(Vec::new(), None))
    }

    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        self.record("report_usage")?;
        Err(EngineError::Unavailable { op: "report_usage" })
    }

    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        self.record("list_executions")?;
        Ok(ListExecutionsPage::new(Vec::new(), None))
    }

    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        self.record("deliver_signal")?;
        Err(EngineError::Unavailable {
            op: "deliver_signal",
        })
    }

    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        self.record("claim_resumed_execution")?;
        Err(EngineError::Unavailable {
            op: "claim_resumed_execution",
        })
    }

    #[cfg(feature = "valkey-default")]
    async fn read_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        self.record("read_stream")?;
        Err(EngineError::Unavailable { op: "read_stream" })
    }

    #[cfg(feature = "valkey-default")]
    async fn tail_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _after: StreamCursor,
        _block_ms: u64,
        _count_limit: u64,
        _visibility: ff_core::backend::TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        self.record("tail_stream")?;
        Err(EngineError::Unavailable { op: "tail_stream" })
    }

    #[cfg(feature = "valkey-default")]
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<ff_core::backend::SummaryDocument>, EngineError> {
        self.record("read_summary")?;
        Err(EngineError::Unavailable { op: "read_summary" })
    }
}
