//! RFC-017 Stage D1 parity gate.
//!
//! Mirrors `parity_stage_c.rs` for the Stage D1 migrations (§4 rows
//! 5 flow ingress, 6 execution ingress, 8 pending-waitpoints, plus
//! `get_execution_result` from row 2).
//!
//! Each test drives an `Arc<dyn EngineBackend>` with a scripted
//! response and asserts the trait method observes the expected
//! arguments and returns the scripted payload verbatim — proving
//! the migrated `ff-server` handlers dispatch through
//! `self.backend()` rather than the inherent `Server::X(...)`
//! implementation.
//!
//! Scope of this file (Stage D1):
//!
//! * `create_execution` — §4 row 6 (ingress).
//! * `create_flow`, `add_execution_to_flow`,
//!   `stage_dependency_edge`, `apply_dependency_to_child` — §4 row 5.
//! * `get_execution_result` — §4 row 2 (read).
//! * `list_pending_waitpoints` — §4 row 8 / §8 (schema rewrite).
//! * `cancel_flow` header-only dispatch — §4 row 5 (exercised by the
//!   already-migrated trait method; the async member dispatcher is
//!   deferred to D2).

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
    AddExecutionToFlowArgs, AddExecutionToFlowResult, ApplyDependencyToChildArgs,
    ApplyDependencyToChildResult, BudgetStatus, CancelExecutionArgs, CancelExecutionResult,
    CancelFlowResult, ChangePriorityArgs, ChangePriorityResult, ClaimForWorkerArgs,
    ClaimForWorkerOutcome, ClaimResumedExecutionArgs, ClaimResumedExecutionResult,
    CreateBudgetArgs, CreateBudgetResult, CreateExecutionArgs, CreateExecutionResult,
    CreateFlowArgs, CreateFlowResult, CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    DeliverSignalArgs, DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot,
    ExecutionSnapshot, FlowSnapshot, ListExecutionsPage, ListFlowsPage, ListLanesPage,
    ListPendingWaitpointsArgs, ListPendingWaitpointsResult, ListSuspendedPage,
    PendingWaitpointInfo, ReplayExecutionArgs, ReplayExecutionResult, ReportUsageAdminArgs,
    ReportUsageResult, ResetBudgetArgs, ResetBudgetResult, RevokeLeaseArgs, RevokeLeaseResult,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllResult,
    SetEdgeGroupPolicyResult, StageDependencyEdgeArgs, StageDependencyEdgeResult,
    StreamCursor, StreamFrames, SuspendArgs, SuspendOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs,
    WaitpointId,
};

// ── Scripted-response MockBackend (Stage D1) ────────────────────

#[derive(Default)]
struct Scripted {
    create_execution: Option<Result<CreateExecutionResult, EngineError>>,
    create_flow: Option<Result<CreateFlowResult, EngineError>>,
    add_execution_to_flow: Option<Result<AddExecutionToFlowResult, EngineError>>,
    stage_dependency_edge: Option<Result<StageDependencyEdgeResult, EngineError>>,
    apply_dependency_to_child: Option<Result<ApplyDependencyToChildResult, EngineError>>,
    cancel_flow: Option<Result<CancelFlowResult, EngineError>>,
    get_execution_result: Option<Result<Option<Vec<u8>>, EngineError>>,
    list_pending_waitpoints:
        Option<Result<ListPendingWaitpointsResult, EngineError>>,

    last_create_execution: Option<CreateExecutionArgs>,
    last_create_flow: Option<CreateFlowArgs>,
    last_add_execution_to_flow: Option<AddExecutionToFlowArgs>,
    last_stage_dependency_edge: Option<StageDependencyEdgeArgs>,
    last_apply_dependency_to_child: Option<ApplyDependencyToChildArgs>,
    last_cancel_flow: Option<(FlowId, CancelFlowPolicy, CancelFlowWait)>,
    last_get_execution_result: Option<ExecutionId>,
    last_list_pending_waitpoints: Option<ListPendingWaitpointsArgs>,
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
        _args: RotateWaitpointHmacSecretAllArgs,
    ) -> Result<RotateWaitpointHmacSecretAllResult, EngineError> {
        Err(unavailable("mock::rotate_waitpoint_hmac_secret_all"))
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
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        Err(unavailable("mock::read_stream"))
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
        Err(unavailable("mock::tail_stream"))
    }
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        Err(unavailable("mock::read_summary"))
    }
    async fn cancel_execution(
        &self,
        _args: CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, EngineError> {
        Err(unavailable("mock::cancel_execution"))
    }
    async fn change_priority(
        &self,
        _args: ChangePriorityArgs,
    ) -> Result<ChangePriorityResult, EngineError> {
        Err(unavailable("mock::change_priority"))
    }
    async fn replay_execution(
        &self,
        _args: ReplayExecutionArgs,
    ) -> Result<ReplayExecutionResult, EngineError> {
        Err(unavailable("mock::replay_execution"))
    }
    async fn revoke_lease(
        &self,
        _args: RevokeLeaseArgs,
    ) -> Result<RevokeLeaseResult, EngineError> {
        Err(unavailable("mock::revoke_lease"))
    }
    async fn create_budget(
        &self,
        _args: CreateBudgetArgs,
    ) -> Result<CreateBudgetResult, EngineError> {
        Err(unavailable("mock::create_budget"))
    }
    async fn reset_budget(
        &self,
        _args: ResetBudgetArgs,
    ) -> Result<ResetBudgetResult, EngineError> {
        Err(unavailable("mock::reset_budget"))
    }
    async fn create_quota_policy(
        &self,
        _args: CreateQuotaPolicyArgs,
    ) -> Result<CreateQuotaPolicyResult, EngineError> {
        Err(unavailable("mock::create_quota_policy"))
    }
    async fn get_budget_status(
        &self,
        _id: &BudgetId,
    ) -> Result<BudgetStatus, EngineError> {
        Err(unavailable("mock::get_budget_status"))
    }
    async fn report_usage_admin(
        &self,
        _budget: &BudgetId,
        _args: ReportUsageAdminArgs,
    ) -> Result<ReportUsageResult, EngineError> {
        Err(unavailable("mock::report_usage_admin"))
    }
    async fn claim_for_worker(
        &self,
        _args: ClaimForWorkerArgs,
    ) -> Result<ClaimForWorkerOutcome, EngineError> {
        Err(unavailable("mock::claim_for_worker"))
    }

    // ── Stage D1 methods under test ───────────────────────────

    async fn create_execution(
        &self,
        args: CreateExecutionArgs,
    ) -> Result<CreateExecutionResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_create_execution = Some(args);
        g.create_execution
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::create_execution (no scripted response)")))
    }

    async fn create_flow(
        &self,
        args: CreateFlowArgs,
    ) -> Result<CreateFlowResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_create_flow = Some(args);
        g.create_flow
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::create_flow (no scripted response)")))
    }

    async fn add_execution_to_flow(
        &self,
        args: AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_add_execution_to_flow = Some(args);
        g.add_execution_to_flow.take().unwrap_or_else(|| {
            Err(unavailable(
                "mock::add_execution_to_flow (no scripted response)",
            ))
        })
    }

    async fn stage_dependency_edge(
        &self,
        args: StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_stage_dependency_edge = Some(args);
        g.stage_dependency_edge.take().unwrap_or_else(|| {
            Err(unavailable(
                "mock::stage_dependency_edge (no scripted response)",
            ))
        })
    }

    async fn apply_dependency_to_child(
        &self,
        args: ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_apply_dependency_to_child = Some(args);
        g.apply_dependency_to_child.take().unwrap_or_else(|| {
            Err(unavailable(
                "mock::apply_dependency_to_child (no scripted response)",
            ))
        })
    }

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_cancel_flow = Some((id.clone(), policy, wait));
        g.cancel_flow
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::cancel_flow (no scripted response)")))
    }

    async fn get_execution_result(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_get_execution_result = Some(id.clone());
        g.get_execution_result.take().unwrap_or_else(|| {
            Err(unavailable(
                "mock::get_execution_result (no scripted response)",
            ))
        })
    }

    async fn list_pending_waitpoints(
        &self,
        args: ListPendingWaitpointsArgs,
    ) -> Result<ListPendingWaitpointsResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_list_pending_waitpoints = Some(args);
        g.list_pending_waitpoints.take().unwrap_or_else(|| {
            Err(unavailable(
                "mock::list_pending_waitpoints (no scripted response)",
            ))
        })
    }
}

fn mk_mock() -> Arc<MockBackend> {
    Arc::new(MockBackend::default())
}

fn mk_eid() -> ExecutionId {
    ExecutionId::for_flow(&FlowId::new(), &PartitionConfig::default())
}

fn mk_fid() -> FlowId {
    FlowId::new()
}

// ─── create_execution (§4 row 6) ────────────────────────────────

#[tokio::test]
async fn create_execution_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let eid = mk_eid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_execution = Some(Ok(CreateExecutionResult::Created {
            execution_id: eid.clone(),
            public_state: PublicState::Waiting,
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new("ns"),
        lane_id: LaneId::new("l"),
        execution_kind: "k".into(),
        input_payload: vec![],
        payload_encoding: None,
        priority: 0,
        creator_identity: "test".into(),
        idempotency_key: Some("idem-1".into()),
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::from_millis(0),
    };
    let got = backend.create_execution(args.clone()).await.unwrap();
    match got {
        CreateExecutionResult::Created {
            execution_id: got_eid,
            public_state,
        } => {
            assert_eq!(got_eid, eid);
            assert_eq!(public_state, PublicState::Waiting);
        }
        other => panic!("expected Created, got {other:?}"),
    }
    let g = mock.scripted.lock().unwrap();
    assert_eq!(
        g.last_create_execution
            .as_ref()
            .and_then(|a| a.idempotency_key.as_deref()),
        Some("idem-1")
    );
}

#[tokio::test]
async fn create_execution_error_propagates_engine_error() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_execution = Some(Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "bad lane".into(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateExecutionArgs {
        execution_id: mk_eid(),
        namespace: Namespace::new("ns"),
        lane_id: LaneId::new(""),
        execution_kind: "k".into(),
        input_payload: vec![],
        payload_encoding: None,
        priority: 0,
        creator_identity: "test".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::from_millis(0),
    };
    let err = backend.create_execution(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::Validation { kind: ValidationKind::InvalidInput, .. }
    ));
}

// ─── create_flow (§4 row 5) ─────────────────────────────────────

#[tokio::test]
async fn create_flow_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let fid = mk_fid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_flow = Some(Ok(CreateFlowResult::Created {
            flow_id: fid.clone(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateFlowArgs {
        flow_id: fid.clone(),
        flow_kind: "test-flow".into(),
        namespace: Namespace::new("ns"),
        now: TimestampMs::from_millis(0),
    };
    let got = backend.create_flow(args).await.unwrap();
    match got {
        CreateFlowResult::Created { flow_id } => assert_eq!(flow_id, fid),
        other => panic!("expected Created, got {other:?}"),
    }
    let g = mock.scripted.lock().unwrap();
    assert_eq!(
        g.last_create_flow.as_ref().unwrap().flow_id,
        fid
    );
}

#[tokio::test]
async fn create_flow_idempotent_duplicate_returns_already_satisfied() {
    let mock = mk_mock();
    let fid = mk_fid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_flow = Some(Ok(CreateFlowResult::AlreadySatisfied {
            flow_id: fid.clone(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateFlowArgs {
        flow_id: fid.clone(),
        flow_kind: "test-flow".into(),
        namespace: Namespace::new("ns"),
        now: TimestampMs::from_millis(0),
    };
    let got = backend.create_flow(args).await.unwrap();
    assert!(matches!(
        got,
        CreateFlowResult::AlreadySatisfied { .. }
    ));
}

// ─── add_execution_to_flow (§4 row 5) ───────────────────────────

#[tokio::test]
async fn add_execution_to_flow_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let fid = mk_fid();
    let eid = mk_eid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.add_execution_to_flow = Some(Ok(AddExecutionToFlowResult::Added {
            execution_id: eid.clone(),
            new_node_count: 1,
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = AddExecutionToFlowArgs {
        flow_id: fid.clone(),
        execution_id: eid.clone(),
        now: TimestampMs::from_millis(0),
    };
    let got = backend.add_execution_to_flow(args).await.unwrap();
    match got {
        AddExecutionToFlowResult::Added {
            execution_id,
            new_node_count,
        } => {
            assert_eq!(execution_id, eid);
            assert_eq!(new_node_count, 1);
        }
        other => panic!("expected Added, got {other:?}"),
    }
}

#[tokio::test]
async fn add_execution_to_flow_partition_mismatch_propagates() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.add_execution_to_flow = Some(Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "partition mismatch".into(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = AddExecutionToFlowArgs {
        flow_id: mk_fid(),
        execution_id: mk_eid(),
        now: TimestampMs::from_millis(0),
    };
    let err = backend.add_execution_to_flow(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::Validation { kind: ValidationKind::InvalidInput, .. }
    ));
}

// ─── stage_dependency_edge (§4 row 5) ───────────────────────────

#[tokio::test]
async fn stage_dependency_edge_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let edge_id = EdgeId::new();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.stage_dependency_edge = Some(Ok(StageDependencyEdgeResult::Staged {
            edge_id: edge_id.clone(),
            new_graph_revision: 42,
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = StageDependencyEdgeArgs {
        flow_id: mk_fid(),
        edge_id: edge_id.clone(),
        upstream_execution_id: mk_eid(),
        downstream_execution_id: mk_eid(),
        dependency_kind: "success_only".into(),
        data_passing_ref: None,
        expected_graph_revision: 41,
        now: TimestampMs::from_millis(0),
    };
    let got = backend.stage_dependency_edge(args).await.unwrap();
    match got {
        StageDependencyEdgeResult::Staged {
            edge_id: got_id,
            new_graph_revision,
        } => {
            assert_eq!(got_id, edge_id);
            assert_eq!(new_graph_revision, 42);
        }
    }
}

#[tokio::test]
async fn stage_dependency_edge_stale_revision_surfaces_contention() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.stage_dependency_edge = Some(Err(EngineError::Contention(
            ContentionKind::StaleGraphRevision,
        )));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = StageDependencyEdgeArgs {
        flow_id: mk_fid(),
        edge_id: EdgeId::new(),
        upstream_execution_id: mk_eid(),
        downstream_execution_id: mk_eid(),
        dependency_kind: "success_only".into(),
        data_passing_ref: None,
        expected_graph_revision: 0,
        now: TimestampMs::from_millis(0),
    };
    let err = backend.stage_dependency_edge(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::Contention(ContentionKind::StaleGraphRevision)
    ));
}

// ─── apply_dependency_to_child (§4 row 5) ───────────────────────

#[tokio::test]
async fn apply_dependency_to_child_happy_dispatches_through_trait() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.apply_dependency_to_child =
            Some(Ok(ApplyDependencyToChildResult::Applied {
                unsatisfied_count: 0,
            }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ApplyDependencyToChildArgs {
        flow_id: mk_fid(),
        edge_id: EdgeId::new(),
        downstream_execution_id: mk_eid(),
        upstream_execution_id: mk_eid(),
        graph_revision: 1,
        dependency_kind: "success_only".into(),
        data_passing_ref: None,
        now: TimestampMs::from_millis(0),
    };
    let got = backend.apply_dependency_to_child(args).await.unwrap();
    match got {
        ApplyDependencyToChildResult::Applied { unsatisfied_count } => {
            assert_eq!(unsatisfied_count, 0);
        }
        other => panic!("expected Applied, got {other:?}"),
    }
}

#[tokio::test]
async fn apply_dependency_to_child_idempotent_returns_already_applied() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.apply_dependency_to_child =
            Some(Ok(ApplyDependencyToChildResult::AlreadyApplied));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ApplyDependencyToChildArgs {
        flow_id: mk_fid(),
        edge_id: EdgeId::new(),
        downstream_execution_id: mk_eid(),
        upstream_execution_id: mk_eid(),
        graph_revision: 1,
        dependency_kind: "success_only".into(),
        data_passing_ref: None,
        now: TimestampMs::from_millis(0),
    };
    let got = backend.apply_dependency_to_child(args).await.unwrap();
    assert!(matches!(
        got,
        ApplyDependencyToChildResult::AlreadyApplied
    ));
}

// ─── cancel_flow (§4 row 5 — header-only) ────────────────────────

#[tokio::test]
async fn cancel_flow_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let fid = mk_fid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.cancel_flow = Some(Ok(CancelFlowResult::Cancelled {
            cancellation_policy: "cancel_all".into(),
            member_execution_ids: vec![],
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let got = backend
        .cancel_flow(&fid, CancelFlowPolicy::CancelAll, CancelFlowWait::NoWait)
        .await
        .unwrap();
    assert!(matches!(got, CancelFlowResult::Cancelled { .. }));
    let g = mock.scripted.lock().unwrap();
    assert_eq!(g.last_cancel_flow.as_ref().unwrap().0, fid);
}

#[tokio::test]
async fn cancel_flow_already_terminal_surfaces_state_error() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.cancel_flow = Some(Err(EngineError::State(StateKind::FlowAlreadyTerminal)));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let err = backend
        .cancel_flow(&mk_fid(), CancelFlowPolicy::CancelAll, CancelFlowWait::NoWait)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        EngineError::State(StateKind::FlowAlreadyTerminal)
    ));
}

// ─── get_execution_result (§4 row 2) ────────────────────────────

#[tokio::test]
async fn get_execution_result_happy_returns_bytes() {
    let mock = mk_mock();
    let eid = mk_eid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.get_execution_result = Some(Ok(Some(b"payload".to_vec())));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let got = backend.get_execution_result(&eid).await.unwrap();
    assert_eq!(got.as_deref(), Some(b"payload" as &[u8]));
    let g = mock.scripted.lock().unwrap();
    assert_eq!(g.last_get_execution_result.as_ref(), Some(&eid));
}

#[tokio::test]
async fn get_execution_result_missing_returns_none() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.get_execution_result = Some(Ok(None));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let got = backend.get_execution_result(&mk_eid()).await.unwrap();
    assert!(got.is_none());
}

// ─── list_pending_waitpoints (§4 row 8 / §8) ────────────────────

fn mk_pending_entry(
    eid: ExecutionId,
    wp_id: WaitpointId,
    token_kid: &str,
    token_fingerprint: &str,
) -> PendingWaitpointInfo {
    PendingWaitpointInfo::new(
        wp_id,
        "wp-key".into(),
        "pending".into(),
        TimestampMs::from_millis(1234),
        eid,
        token_kid.to_owned(),
        token_fingerprint.to_owned(),
    )
}

#[tokio::test]
async fn list_pending_waitpoints_happy_sanitised_fields_present() {
    let mock = mk_mock();
    let eid = mk_eid();
    let wp_id = WaitpointId::new();
    let entry = mk_pending_entry(eid.clone(), wp_id.clone(), "kid-1", "deadbeefcafef00d");
    {
        let mut g = mock.scripted.lock().unwrap();
        g.list_pending_waitpoints = Some(Ok(ListPendingWaitpointsResult::new(vec![
            entry.clone(),
        ])));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ListPendingWaitpointsArgs::new(eid.clone());
    let page = backend.list_pending_waitpoints(args).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    let got = &page.entries[0];
    assert_eq!(got.waitpoint_id, wp_id);
    assert_eq!(got.execution_id, eid);
    assert_eq!(got.token_kid, "kid-1");
    assert_eq!(got.token_fingerprint, "deadbeefcafef00d");
    assert_eq!(got.token_fingerprint.len(), 16);
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn list_pending_waitpoints_paginates_with_cursor() {
    let mock = mk_mock();
    let eid = mk_eid();
    let wp_id_1 = WaitpointId::new();
    let wp_id_2 = WaitpointId::new();
    let first_entry =
        mk_pending_entry(eid.clone(), wp_id_1.clone(), "kid-1", "aaaaaaaaaaaaaaaa");
    {
        let mut g = mock.scripted.lock().unwrap();
        g.list_pending_waitpoints =
            Some(Ok(ListPendingWaitpointsResult::new(vec![first_entry])
                .with_next_cursor(wp_id_2.clone())));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ListPendingWaitpointsArgs::new(eid.clone()).with_limit(1);
    let page = backend.list_pending_waitpoints(args.clone()).await.unwrap();
    assert_eq!(page.next_cursor.as_ref(), Some(&wp_id_2));
    let g = mock.scripted.lock().unwrap();
    assert_eq!(
        g.last_list_pending_waitpoints.as_ref().unwrap().limit,
        Some(1)
    );
}

#[tokio::test]
async fn list_pending_waitpoints_not_found_propagates() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.list_pending_waitpoints = Some(Err(EngineError::NotFound {
            entity: "execution",
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ListPendingWaitpointsArgs::new(mk_eid());
    let err = backend.list_pending_waitpoints(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::NotFound { entity: "execution" }
    ));
}

// ─── smoke: trait is dyn-safe on the Stage D1 surface ────────

#[tokio::test]
async fn stage_d1_trait_is_dyn_compatible() {
    let _b: Arc<dyn EngineBackend> = mk_mock();
}
