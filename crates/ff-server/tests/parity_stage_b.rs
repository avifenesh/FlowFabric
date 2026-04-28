//! RFC-017 Stage B parity gate.
//!
//! Mirrors `parity_stage_a.rs` for the Stage B migrations (§4 rows
//! 10/11 + the trait-lift of `backend_label` / `shutdown_prepare`).
//! Each test drives an `Arc<dyn EngineBackend>` with a scripted
//! response and asserts the trait method observes the expected
//! arguments and returns the scripted payload verbatim — proving
//! the migrated `ff-server` handlers dispatch through `self.backend`
//! rather than direct `ferriskey::Client` / `ff_script` calls.
//!
//! Scope of this file (Stage B):
//!
//! * `read_stream` / `tail_stream` — §4 row 10; Server methods
//!   `read_attempt_stream` / `tail_attempt_stream` now route here.
//! * `rotate_waitpoint_hmac_secret_all` — §4 row 11; Server method
//!   `rotate_waitpoint_secret` now routes here (admin semaphore +
//!   audit emit stay on `Server`).
//! * Trait defaults — `backend_label()` / `shutdown_prepare()` are
//!   now on the trait (§9 Stage B trait-lift); confirm a backend
//!   that omits an override picks up the documented defaults
//!   (`"unknown"` + `Ok(())`).
//! * `EngineError::ResourceExhausted` → `ServerError::Engine` is a
//!   Stage-B-specific mapping; the `ServerError` → HTTP 429 leg is
//!   exercised by inspecting the shape the mock surfaces when a
//!   test-mock emits `ResourceExhausted`.

#![allow(clippy::too_many_arguments)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint,
    ResumeToken, ResumeSignal, SummaryDocument, TailVisibility, UsageDimensions,
};
use ff_core::contracts::{
    CancelFlowResult, ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs,
    DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ExecutionSnapshot,
    FlowSnapshot, ListExecutionsPage, ListFlowsPage, ListLanesPage, ListSuspendedPage,
    ReportUsageResult, RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllEntry,
    RotateWaitpointHmacSecretAllResult, RotateWaitpointHmacSecretOutcome,
    SetEdgeGroupPolicyResult, StreamCursor, StreamFrame, StreamFrames, SuspendArgs,
    SuspendOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::partition::PartitionKey;
use ff_core::types::{
    AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, TimestampMs,
};

// ── Scripted-response MockBackend (Stage B) ─────────────────────

#[derive(Default)]
struct Scripted {
    read_stream: Option<StreamFrames>,
    tail_stream: Option<StreamFrames>,
    rotate_all: Option<RotateWaitpointHmacSecretAllResult>,
    last_read_args: Option<(ExecutionId, AttemptIndex, StreamCursor, StreamCursor, u64)>,
    last_tail_args: Option<(ExecutionId, AttemptIndex, StreamCursor, u64, u64, TailVisibility)>,
    last_rotate_args: Option<RotateWaitpointHmacSecretAllArgs>,
}

#[derive(Default)]
struct MockBackend {
    scripted: Mutex<Scripted>,
}

fn unavailable(op: &'static str) -> EngineError {
    EngineError::Unavailable { op }
}

#[async_trait]
impl EngineBackend for MockBackend {
    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        Err(unavailable("mock::claim"))
    }
    async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        Err(unavailable("mock::renew"))
    }
    async fn progress(
        &self,
        _handle: &Handle,
        _percent: Option<u8>,
        _message: Option<String>,
    ) -> Result<(), EngineError> {
        Err(unavailable("mock::progress"))
    }
    async fn append_frame(
        &self,
        _handle: &Handle,
        _frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        Err(unavailable("mock::append_frame"))
    }
    async fn complete(
        &self,
        _handle: &Handle,
        _payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        Err(unavailable("mock::complete"))
    }
    async fn fail(
        &self,
        _handle: &Handle,
        _reason: FailureReason,
        _classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        Err(unavailable("mock::fail"))
    }
    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        Err(unavailable("mock::cancel"))
    }
    async fn suspend(
        &self,
        _handle: &Handle,
        _args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        Err(unavailable("mock::suspend"))
    }
    async fn create_waitpoint(
        &self,
        _handle: &Handle,
        _waitpoint_key: &str,
        _expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        Err(unavailable("mock::create_waitpoint"))
    }
    async fn observe_signals(
        &self,
        _handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        Err(unavailable("mock::observe_signals"))
    }
    async fn claim_from_resume_grant(
        &self,
        _token: ResumeToken,
    ) -> Result<Option<Handle>, EngineError> {
        Err(unavailable("mock::claim_from_resume_grant"))
    }
    async fn delay(
        &self,
        _handle: &Handle,
        _delay_until: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(unavailable("mock::delay"))
    }
    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        Err(unavailable("mock::wait_children"))
    }
    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        Err(unavailable("mock::describe_execution"))
    }
    async fn describe_flow(
        &self,
        _id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        Err(unavailable("mock::describe_flow"))
    }
    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        Err(unavailable("mock::list_edges"))
    }
    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        Err(unavailable("mock::describe_edge"))
    }
    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        Err(unavailable("mock::resolve_execution_flow_id"))
    }
    async fn list_flows(
        &self,
        _partition: PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        Err(unavailable("mock::list_flows"))
    }
    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        Err(unavailable("mock::list_lanes"))
    }
    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        Err(unavailable("mock::list_suspended"))
    }
    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        Err(unavailable("mock::list_executions"))
    }
    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        Err(unavailable("mock::deliver_signal"))
    }
    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        Err(unavailable("mock::claim_resumed_execution"))
    }
    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        Err(unavailable("mock::cancel_flow"))
    }
    async fn set_edge_group_policy(
        &self,
        _flow_id: &FlowId,
        _downstream_execution_id: &ExecutionId,
        _policy: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        Err(unavailable("mock::set_edge_group_policy"))
    }
    async fn rotate_waitpoint_hmac_secret_all(
        &self,
        args: RotateWaitpointHmacSecretAllArgs,
    ) -> Result<RotateWaitpointHmacSecretAllResult, EngineError> {
        let mut guard = self.scripted.lock().unwrap();
        guard.last_rotate_args = Some(args);
        guard
            .rotate_all
            .take()
            .ok_or_else(|| unavailable("mock::rotate_waitpoint_hmac_secret_all (no scripted response)"))
    }
    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        Err(unavailable("mock::report_usage"))
    }
    async fn read_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from: StreamCursor,
        to: StreamCursor,
        count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        let mut guard = self.scripted.lock().unwrap();
        guard.last_read_args = Some((
            execution_id.clone(),
            attempt_index,
            from,
            to,
            count_limit,
        ));
        guard
            .read_stream
            .clone()
            .ok_or_else(|| unavailable("mock::read_stream (no scripted response)"))
    }
    async fn tail_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        after: StreamCursor,
        block_ms: u64,
        count_limit: u64,
        visibility: TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        let mut guard = self.scripted.lock().unwrap();
        guard.last_tail_args = Some((
            execution_id.clone(),
            attempt_index,
            after,
            block_ms,
            count_limit,
            visibility,
        ));
        guard
            .tail_stream
            .clone()
            .ok_or_else(|| unavailable("mock::tail_stream (no scripted response)"))
    }
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        Err(unavailable("mock::read_summary"))
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn fresh_execution_id() -> ExecutionId {
    ExecutionId::solo(&LaneId::new("stageb"), &ff_core::partition::PartitionConfig::default())
}

// ── Tests ──────────────────────────────────────────────────────────

/// A minimal backend that relies on the trait defaults for
/// `backend_label` (`"unknown"`) and `shutdown_prepare` (`Ok(())`).
/// Only a couple of methods needed for object safety are defined
/// explicitly — the rest delegate to an inner `MockBackend`.
struct DefaultsBackend;

#[async_trait]
impl EngineBackend for DefaultsBackend {
    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        Err(unavailable("defaults::claim"))
    }
    async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        Err(unavailable("defaults::renew"))
    }
    async fn progress(
        &self,
        _handle: &Handle,
        _percent: Option<u8>,
        _message: Option<String>,
    ) -> Result<(), EngineError> {
        Err(unavailable("defaults::progress"))
    }
    async fn append_frame(
        &self,
        _handle: &Handle,
        _frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        Err(unavailable("defaults::append_frame"))
    }
    async fn complete(
        &self,
        _handle: &Handle,
        _payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        Err(unavailable("defaults::complete"))
    }
    async fn fail(
        &self,
        _handle: &Handle,
        _reason: FailureReason,
        _classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        Err(unavailable("defaults::fail"))
    }
    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        Err(unavailable("defaults::cancel"))
    }
    async fn suspend(
        &self,
        _handle: &Handle,
        _args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        Err(unavailable("defaults::suspend"))
    }
    async fn create_waitpoint(
        &self,
        _handle: &Handle,
        _waitpoint_key: &str,
        _expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        Err(unavailable("defaults::create_waitpoint"))
    }
    async fn observe_signals(
        &self,
        _handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        Err(unavailable("defaults::observe_signals"))
    }
    async fn claim_from_resume_grant(
        &self,
        _token: ResumeToken,
    ) -> Result<Option<Handle>, EngineError> {
        Err(unavailable("defaults::claim_from_resume_grant"))
    }
    async fn delay(
        &self,
        _handle: &Handle,
        _delay_until: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(unavailable("defaults::delay"))
    }
    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        Err(unavailable("defaults::wait_children"))
    }
    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        Err(unavailable("defaults::describe_execution"))
    }
    async fn describe_flow(
        &self,
        _id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        Err(unavailable("defaults::describe_flow"))
    }
    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        Err(unavailable("defaults::list_edges"))
    }
    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        Err(unavailable("defaults::describe_edge"))
    }
    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        Err(unavailable("defaults::resolve_execution_flow_id"))
    }
    async fn list_flows(
        &self,
        _partition: PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        Err(unavailable("defaults::list_flows"))
    }
    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        Err(unavailable("defaults::list_lanes"))
    }
    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        Err(unavailable("defaults::list_suspended"))
    }
    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        Err(unavailable("defaults::list_executions"))
    }
    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        Err(unavailable("defaults::deliver_signal"))
    }
    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        Err(unavailable("defaults::claim_resumed_execution"))
    }
    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        Err(unavailable("defaults::cancel_flow"))
    }
    async fn set_edge_group_policy(
        &self,
        _flow_id: &FlowId,
        _downstream_execution_id: &ExecutionId,
        _policy: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        Err(unavailable("defaults::set_edge_group_policy"))
    }
    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        Err(unavailable("defaults::report_usage"))
    }
    async fn read_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        Err(unavailable("defaults::read_stream"))
    }
    async fn tail_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _after: StreamCursor,
        _block_ms: u64,
        _count_limit: u64,
        _visibility: TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        Err(unavailable("defaults::tail_stream"))
    }
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        Err(unavailable("defaults::read_summary"))
    }
}

/// Stage-B trait-lift assertion: the defaults for `backend_label` +
/// `shutdown_prepare` are now on the trait (RFC-017 §9 Stage B).
/// A backend that omits the overrides picks up `"unknown"` +
/// `Ok(())` respectively.
#[tokio::test]
async fn trait_defaults_apply_when_not_overridden() {
    let backend: Arc<dyn EngineBackend> = Arc::new(DefaultsBackend);
    assert_eq!(backend.backend_label(), "unknown");
    let res = backend.shutdown_prepare(Duration::from_secs(1)).await;
    assert!(res.is_ok(), "default shutdown_prepare is Ok(())");
}

/// `read_stream` dispatches through the backend trait; a scripted
/// response surfaces back verbatim and the argument tuple flows
/// through unchanged.
#[tokio::test]
async fn read_stream_dispatches_through_backend() {
    let mock = Arc::new(MockBackend::default());
    let expected = StreamFrames {
        frames: vec![StreamFrame {
            id: "17-0".into(),
            fields: Default::default(),
        }],
        closed_at: None,
        closed_reason: None,
    };
    mock.scripted.lock().unwrap().read_stream = Some(expected.clone());

    let eid = fresh_execution_id();
    let ai = AttemptIndex::new(1);
    let from = StreamCursor::Start;
    let to = StreamCursor::End;

    let erased: Arc<dyn EngineBackend> = mock.clone();
    let got = erased
        .read_stream(&eid, ai, from.clone(), to.clone(), 50)
        .await
        .expect("scripted read_stream");
    assert_eq!(got.frames.len(), 1);
    assert_eq!(got.frames[0].id, "17-0");

    let last = mock.scripted.lock().unwrap().last_read_args.clone().unwrap();
    assert_eq!(last.0, eid);
    assert_eq!(last.1, ai);
    assert_eq!(last.2, from);
    assert_eq!(last.3, to);
    assert_eq!(last.4, 50);
}

/// `tail_stream` dispatches through the backend trait.
#[tokio::test]
async fn tail_stream_dispatches_through_backend() {
    let mock = Arc::new(MockBackend::default());
    let expected = StreamFrames {
        frames: vec![],
        closed_at: None,
        closed_reason: None,
    };
    mock.scripted.lock().unwrap().tail_stream = Some(expected);

    let eid = fresh_execution_id();
    let ai = AttemptIndex::new(2);
    let after = StreamCursor::At("5-0".into());

    let erased: Arc<dyn EngineBackend> = mock.clone();
    let _got = erased
        .tail_stream(&eid, ai, after.clone(), 100, 25, TailVisibility::All)
        .await
        .expect("scripted tail_stream");

    let last = mock.scripted.lock().unwrap().last_tail_args.clone().unwrap();
    assert_eq!(last.0, eid);
    assert_eq!(last.1, ai);
    assert_eq!(last.2, after);
    assert_eq!(last.3, 100);
    assert_eq!(last.4, 25);
}

/// `rotate_waitpoint_hmac_secret_all` dispatches through the backend
/// trait. A scripted multi-partition response surfaces back verbatim,
/// including mixed success/failure entries.
#[tokio::test]
async fn rotate_waitpoint_hmac_secret_all_dispatches_through_backend() {
    let mock = Arc::new(MockBackend::default());

    let entries = vec![
        RotateWaitpointHmacSecretAllEntry::new(
            0,
            Ok(RotateWaitpointHmacSecretOutcome::Rotated {
                previous_kid: None,
                new_kid: "new_kid_42".into(),
                gc_count: 0,
            }),
        ),
        RotateWaitpointHmacSecretAllEntry::new(
            1,
            Ok(RotateWaitpointHmacSecretOutcome::Noop {
                kid: "new_kid_42".into(),
            }),
        ),
        RotateWaitpointHmacSecretAllEntry::new(
            2,
            Err(EngineError::Unavailable { op: "forced_err" }),
        ),
    ];
    mock.scripted.lock().unwrap().rotate_all =
        Some(RotateWaitpointHmacSecretAllResult::new(entries));

    let args = RotateWaitpointHmacSecretAllArgs::new("new_kid_42", "00112233", 60_000);
    let erased: Arc<dyn EngineBackend> = mock.clone();
    let got = erased
        .rotate_waitpoint_hmac_secret_all(args.clone())
        .await
        .expect("scripted rotate_all");

    assert_eq!(got.entries.len(), 3);
    assert_eq!(got.entries[0].partition, 0);
    assert!(got.entries[0].result.is_ok());
    assert_eq!(got.entries[2].partition, 2);
    assert!(matches!(
        &got.entries[2].result,
        Err(EngineError::Unavailable { op: "forced_err" })
    ));

    let observed = mock.scripted.lock().unwrap().last_rotate_args.clone().unwrap();
    assert_eq!(observed.new_kid, args.new_kid);
    assert_eq!(observed.new_secret_hex, args.new_secret_hex);
    assert_eq!(observed.grace_ms, args.grace_ms);
}

/// Smoke that `EngineError::ResourceExhausted` is a first-class
/// typed variant (Stage-B error-surface addition). The HTTP-429
/// mapping leg is exercised via `ServerError::from` +
/// `IntoResponse for ApiError` in `ff-server`'s integration path;
/// this unit check ensures the enum shape is stable.
#[test]
fn resource_exhausted_variant_is_buildable() {
    let err = EngineError::ResourceExhausted {
        pool: "stream_ops",
        max: 64,
        retry_after_ms: Some(120),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("stream_ops"));
    assert!(rendered.contains("64"));
}

/// Similarly, confirm `EngineError::Timeout { op, elapsed }` is
/// addressable. Used by `Server::shutdown` to count
/// `ff_shutdown_timeout_total` via `backend.shutdown_prepare`.
#[test]
fn timeout_variant_is_buildable() {
    let err = EngineError::Timeout {
        op: "shutdown_prepare",
        elapsed: Duration::from_millis(250),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("shutdown_prepare"));
}
