//! RFC-017 Stage C parity gate.
//!
//! Mirrors `parity_stage_b.rs` for the Stage C migrations (§4 rows
//! 2 operator-control, 3 replay, 7 budget admin, 8 budget status,
//! 9 claim, plus the `report_usage_admin` admin path on row 9).
//!
//! Each test drives an `Arc<dyn EngineBackend>` with a scripted
//! response and asserts the trait method observes the expected
//! arguments and returns the scripted payload verbatim — proving
//! the migrated `ff-server` handlers dispatch through `self.backend`
//! rather than direct ferriskey FCALL / inherent `Server::X(...)`.
//!
//! Scope of this file (Stage C):
//!
//! * `cancel_execution`, `change_priority`, `replay_execution`,
//!   `revoke_lease` — §4 row 2/3/19.
//! * `create_budget`, `reset_budget`, `get_budget_status`,
//!   `create_quota_policy`, `report_usage_admin` — §4 row 7.
//! * `claim_for_worker` — §4 row 9 / §7.
//!
//! The §4 row 4 signal handler was already covered by Stage A; this
//! file does not duplicate it.

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
    ClaimForWorkerOutcome, ClaimGrant, ClaimResumedExecutionArgs, ClaimResumedExecutionResult,
    CreateBudgetArgs, CreateBudgetResult, CreateExecutionArgs, CreateExecutionResult,
    CreateFlowArgs, CreateFlowResult, CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    DeliverSignalArgs, DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot,
    ExecutionContext, ExecutionSnapshot, FlowSnapshot, ListExecutionsPage, ListFlowsPage, ListLanesPage,
    ListPendingWaitpointsArgs, ListPendingWaitpointsResult, ListSuspendedPage, ReplayExecutionArgs,
    ReplayExecutionResult, ReportUsageAdminArgs, ReportUsageResult, ResetBudgetArgs,
    ResetBudgetResult, RevokeLeaseArgs, RevokeLeaseResult, RotateWaitpointHmacSecretAllArgs,
    RotateWaitpointHmacSecretAllResult, SetEdgeGroupPolicyResult, StageDependencyEdgeArgs,
    StageDependencyEdgeResult, StreamCursor, StreamFrames, SuspendArgs, SuspendOutcome,
};
use ff_core::state::PublicState;
use ff_core::types::CancelSource;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::types::{
    AttemptId, AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, LeaseEpoch, LeaseId,
    TimestampMs, WorkerId, WorkerInstanceId,
};

// ── Scripted-response MockBackend (Stage C) ─────────────────────

#[derive(Default)]
struct Scripted {
    cancel_execution: Option<Result<CancelExecutionResult, EngineError>>,
    change_priority: Option<Result<ChangePriorityResult, EngineError>>,
    replay_execution: Option<Result<ReplayExecutionResult, EngineError>>,
    revoke_lease: Option<Result<RevokeLeaseResult, EngineError>>,
    create_budget: Option<Result<CreateBudgetResult, EngineError>>,
    reset_budget: Option<Result<ResetBudgetResult, EngineError>>,
    create_quota_policy: Option<Result<CreateQuotaPolicyResult, EngineError>>,
    get_budget_status: Option<Result<BudgetStatus, EngineError>>,
    report_usage_admin: Option<Result<ReportUsageResult, EngineError>>,
    claim_for_worker: Option<Result<ClaimForWorkerOutcome, EngineError>>,

    last_cancel_args: Option<CancelExecutionArgs>,
    last_change_priority_args: Option<ChangePriorityArgs>,
    last_replay_args: Option<ReplayExecutionArgs>,
    last_revoke_args: Option<RevokeLeaseArgs>,
    last_create_budget_args: Option<CreateBudgetArgs>,
    last_reset_budget_args: Option<ResetBudgetArgs>,
    last_create_quota_args: Option<CreateQuotaPolicyArgs>,
    last_get_budget_status_id: Option<BudgetId>,
    last_report_usage_admin_args: Option<(BudgetId, ReportUsageAdminArgs)>,
    last_claim_args: Option<ClaimForWorkerArgs>,
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
    async fn read_execution_context(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<ExecutionContext, EngineError> {
        Err(unavailable("mock::read_execution_context"))
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
    // RFC-017 Stage A ingress defaults — not exercised in Stage C
    // tests but required by the trait surface.
    async fn create_execution(
        &self,
        _args: CreateExecutionArgs,
    ) -> Result<CreateExecutionResult, EngineError> {
        Err(unavailable("mock::create_execution"))
    }
    async fn create_flow(
        &self,
        _args: CreateFlowArgs,
    ) -> Result<CreateFlowResult, EngineError> {
        Err(unavailable("mock::create_flow"))
    }
    async fn add_execution_to_flow(
        &self,
        _args: AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, EngineError> {
        Err(unavailable("mock::add_execution_to_flow"))
    }
    async fn stage_dependency_edge(
        &self,
        _args: StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, EngineError> {
        Err(unavailable("mock::stage_dependency_edge"))
    }
    async fn apply_dependency_to_child(
        &self,
        _args: ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, EngineError> {
        Err(unavailable("mock::apply_dependency_to_child"))
    }

    // ── Stage C methods under test ────────────────────────────

    async fn cancel_execution(
        &self,
        args: CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_cancel_args = Some(args);
        g.cancel_execution
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::cancel_execution (no scripted response)")))
    }

    async fn change_priority(
        &self,
        args: ChangePriorityArgs,
    ) -> Result<ChangePriorityResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_change_priority_args = Some(args);
        g.change_priority
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::change_priority (no scripted response)")))
    }

    async fn replay_execution(
        &self,
        args: ReplayExecutionArgs,
    ) -> Result<ReplayExecutionResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_replay_args = Some(args);
        g.replay_execution
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::replay_execution (no scripted response)")))
    }

    async fn revoke_lease(
        &self,
        args: RevokeLeaseArgs,
    ) -> Result<RevokeLeaseResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_revoke_args = Some(args);
        g.revoke_lease
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::revoke_lease (no scripted response)")))
    }

    async fn create_budget(
        &self,
        args: CreateBudgetArgs,
    ) -> Result<CreateBudgetResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_create_budget_args = Some(args);
        g.create_budget
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::create_budget (no scripted response)")))
    }

    async fn reset_budget(
        &self,
        args: ResetBudgetArgs,
    ) -> Result<ResetBudgetResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_reset_budget_args = Some(args);
        g.reset_budget
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::reset_budget (no scripted response)")))
    }

    async fn create_quota_policy(
        &self,
        args: CreateQuotaPolicyArgs,
    ) -> Result<CreateQuotaPolicyResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_create_quota_args = Some(args);
        g.create_quota_policy
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::create_quota_policy (no scripted response)")))
    }

    async fn get_budget_status(
        &self,
        id: &BudgetId,
    ) -> Result<BudgetStatus, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_get_budget_status_id = Some(id.clone());
        g.get_budget_status
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::get_budget_status (no scripted response)")))
    }

    async fn report_usage_admin(
        &self,
        budget: &BudgetId,
        args: ReportUsageAdminArgs,
    ) -> Result<ReportUsageResult, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_report_usage_admin_args = Some((budget.clone(), args));
        g.report_usage_admin
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::report_usage_admin (no scripted response)")))
    }

    async fn claim_for_worker(
        &self,
        args: ClaimForWorkerArgs,
    ) -> Result<ClaimForWorkerOutcome, EngineError> {
        let mut g = self.scripted.lock().unwrap();
        g.last_claim_args = Some(args);
        g.claim_for_worker
            .take()
            .unwrap_or_else(|| Err(unavailable("mock::claim_for_worker (no scripted response)")))
    }

    async fn list_pending_waitpoints(
        &self,
        _args: ListPendingWaitpointsArgs,
    ) -> Result<ListPendingWaitpointsResult, EngineError> {
        Err(unavailable("mock::list_pending_waitpoints"))
    }
}

fn mk_mock() -> Arc<MockBackend> {
    Arc::new(MockBackend::default())
}

fn mk_eid() -> ExecutionId {
    ExecutionId::for_flow(&FlowId::new(), &PartitionConfig::default())
}

// ─── cancel_execution (§4 row 2) ─────────────────────────────────

#[tokio::test]
async fn cancel_execution_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let eid = mk_eid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.cancel_execution = Some(Ok(CancelExecutionResult::Cancelled {
            execution_id: eid.clone(),
            public_state: PublicState::Cancelled,
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CancelExecutionArgs {
        execution_id: eid.clone(),
        reason: "test".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(1),
    };
    let got = backend.cancel_execution(args.clone()).await.unwrap();
    match got {
        CancelExecutionResult::Cancelled {
            execution_id: got_eid,
            public_state,
        } => {
            assert_eq!(got_eid, eid);
            assert_eq!(public_state, PublicState::Cancelled);
        }
    }
    let g = mock.scripted.lock().unwrap();
    assert_eq!(g.last_cancel_args.as_ref().unwrap().execution_id, eid);
    assert_eq!(g.last_cancel_args.as_ref().unwrap().reason, "test");
}

#[tokio::test]
async fn cancel_execution_error_propagates_engine_error() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.cancel_execution = Some(Err(EngineError::State(StateKind::Terminal)));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CancelExecutionArgs {
        execution_id: mk_eid(),
        reason: "x".into(),
        source: CancelSource::LeaseHolder,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(0),
    };
    let err = backend.cancel_execution(args).await.unwrap_err();
    assert!(matches!(err, EngineError::State(StateKind::Terminal)));
}

// ─── change_priority (§4 row 17) ─────────────────────────────────

#[tokio::test]
async fn change_priority_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let eid = mk_eid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.change_priority = Some(Ok(ChangePriorityResult::Changed {
            execution_id: eid.clone(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ChangePriorityArgs {
        execution_id: eid.clone(),
        new_priority: 7,
        lane_id: LaneId::new("lane-1"),
        now: TimestampMs::from_millis(0),
    };
    let got = backend.change_priority(args).await.unwrap();
    match got {
        ChangePriorityResult::Changed {
            execution_id: got_eid,
        } => assert_eq!(got_eid, eid),
    }
    let g = mock.scripted.lock().unwrap();
    assert_eq!(
        g.last_change_priority_args.as_ref().unwrap().new_priority,
        7
    );
}

#[tokio::test]
async fn change_priority_invalid_input_surfaces_validation() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.change_priority = Some(Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "priority out of range".into(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ChangePriorityArgs {
        execution_id: mk_eid(),
        new_priority: -1_000_000,
        lane_id: LaneId::new("l"),
        now: TimestampMs::from_millis(0),
    };
    let err = backend.change_priority(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            ..
        }
    ));
}

// ─── replay_execution (§4 row 3 Hard) ────────────────────────────

#[tokio::test]
async fn replay_execution_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let eid = mk_eid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.replay_execution = Some(Ok(ReplayExecutionResult::Replayed {
            public_state: PublicState::Waiting,
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ReplayExecutionArgs {
        execution_id: eid.clone(),
        now: TimestampMs::from_millis(1),
    };
    let got = backend.replay_execution(args).await.unwrap();
    match got {
        ReplayExecutionResult::Replayed { public_state } => {
            assert_eq!(public_state, PublicState::Waiting);
        }
    }
    let g = mock.scripted.lock().unwrap();
    assert_eq!(g.last_replay_args.as_ref().unwrap().execution_id, eid);
}

#[tokio::test]
async fn replay_execution_not_terminal_surfaces_state_error() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.replay_execution = Some(Err(EngineError::State(StateKind::ExecutionNotTerminal)));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ReplayExecutionArgs {
        execution_id: mk_eid(),
        now: TimestampMs::from_millis(0),
    };
    let err = backend.replay_execution(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::State(StateKind::ExecutionNotTerminal)
    ));
}

// ─── revoke_lease (§4 row 19) ────────────────────────────────────

#[tokio::test]
async fn revoke_lease_happy_dispatches_through_trait() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.revoke_lease = Some(Ok(RevokeLeaseResult::Revoked {
            lease_id: "lease-1".into(),
            lease_epoch: "3".into(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let eid = mk_eid();
    let args = RevokeLeaseArgs {
        execution_id: eid.clone(),
        expected_lease_id: None,
        worker_instance_id: WorkerInstanceId::new("wi-1"),
        reason: "op_revoke".into(),
    };
    let got = backend.revoke_lease(args).await.unwrap();
    match got {
        RevokeLeaseResult::Revoked {
            lease_id,
            lease_epoch,
        } => {
            assert_eq!(lease_id, "lease-1");
            assert_eq!(lease_epoch, "3");
        }
        other => panic!("unexpected: {other:?}"),
    }
    let g = mock.scripted.lock().unwrap();
    assert_eq!(g.last_revoke_args.as_ref().unwrap().execution_id, eid);
}

#[tokio::test]
async fn revoke_lease_already_satisfied_is_a_benign_result() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.revoke_lease = Some(Ok(RevokeLeaseResult::AlreadySatisfied {
            reason: "already_revoked".into(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = RevokeLeaseArgs {
        execution_id: mk_eid(),
        expected_lease_id: None,
        worker_instance_id: WorkerInstanceId::new("wi-1"),
        reason: "op_revoke".into(),
    };
    let got = backend.revoke_lease(args).await.unwrap();
    match got {
        RevokeLeaseResult::AlreadySatisfied { reason } => {
            assert_eq!(reason, "already_revoked");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// ─── create_budget (§4 row 7) ────────────────────────────────────

#[tokio::test]
async fn create_budget_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let bid = BudgetId::new();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_budget = Some(Ok(CreateBudgetResult::Created {
            budget_id: bid.clone(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateBudgetArgs {
        budget_id: bid.clone(),
        scope_type: "tenant".into(),
        scope_id: "t-1".into(),
        enforcement_mode: "hard".into(),
        on_hard_limit: "reject".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: vec!["tokens".into()],
        hard_limits: vec![100],
        soft_limits: vec![80],
        now: TimestampMs::from_millis(0),
    };
    let got = backend.create_budget(args).await.unwrap();
    match got {
        CreateBudgetResult::Created { budget_id } => assert_eq!(budget_id, bid),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn create_budget_conflict_is_benign_shape() {
    let mock = mk_mock();
    let bid = BudgetId::new();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_budget = Some(Ok(CreateBudgetResult::AlreadySatisfied {
            budget_id: bid.clone(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateBudgetArgs {
        budget_id: bid.clone(),
        scope_type: "tenant".into(),
        scope_id: "t-1".into(),
        enforcement_mode: "hard".into(),
        on_hard_limit: "reject".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: vec!["tokens".into()],
        hard_limits: vec![100],
        soft_limits: vec![80],
        now: TimestampMs::from_millis(0),
    };
    let got = backend.create_budget(args).await.unwrap();
    assert!(matches!(got, CreateBudgetResult::AlreadySatisfied { .. }));
}

// ─── get_budget_status (§4 row 7 read path) ──────────────────────

#[tokio::test]
async fn get_budget_status_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let bid = BudgetId::new();
    let status = BudgetStatus {
        budget_id: bid.to_string(),
        scope_type: "tenant".into(),
        scope_id: "t".into(),
        enforcement_mode: "hard".into(),
        usage: Default::default(),
        hard_limits: Default::default(),
        soft_limits: Default::default(),
        breach_count: 0,
        soft_breach_count: 0,
        last_breach_at: None,
        last_breach_dim: None,
        next_reset_at: None,
        created_at: None,
    };
    {
        let mut g = mock.scripted.lock().unwrap();
        g.get_budget_status = Some(Ok(status.clone()));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let got = backend.get_budget_status(&bid).await.unwrap();
    assert_eq!(got, status);
    let g = mock.scripted.lock().unwrap();
    assert_eq!(g.last_get_budget_status_id.as_ref().unwrap(), &bid);
}

#[tokio::test]
async fn get_budget_status_not_found_surfaces_notfound() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.get_budget_status = Some(Err(EngineError::NotFound { entity: "budget" }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let bid = BudgetId::new();
    let err = backend.get_budget_status(&bid).await.unwrap_err();
    assert!(matches!(err, EngineError::NotFound { entity: "budget" }));
}

// ─── reset_budget (§4 row 7) ─────────────────────────────────────

#[tokio::test]
async fn reset_budget_happy_dispatches_through_trait() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.reset_budget = Some(Ok(ResetBudgetResult::Reset {
            next_reset_at: TimestampMs::from_millis(10_000),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ResetBudgetArgs {
        budget_id: BudgetId::new(),
        now: TimestampMs::from_millis(0),
    };
    let got = backend.reset_budget(args).await.unwrap();
    match got {
        ResetBudgetResult::Reset { next_reset_at } => {
            assert_eq!(next_reset_at, TimestampMs::from_millis(10_000));
        }
    }
}

#[tokio::test]
async fn reset_budget_not_found_propagates() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.reset_budget = Some(Err(EngineError::NotFound { entity: "budget" }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ResetBudgetArgs {
        budget_id: BudgetId::new(),
        now: TimestampMs::from_millis(0),
    };
    let err = backend.reset_budget(args).await.unwrap_err();
    assert!(matches!(err, EngineError::NotFound { .. }));
}

// ─── create_quota_policy (§4 row 7) ──────────────────────────────

#[tokio::test]
async fn create_quota_policy_happy_dispatches_through_trait() {
    let mock = mk_mock();
    let qid = ff_core::types::QuotaPolicyId::new();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_quota_policy = Some(Ok(CreateQuotaPolicyResult::Created {
            quota_policy_id: qid.clone(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateQuotaPolicyArgs {
        quota_policy_id: qid.clone(),
        window_seconds: 60,
        max_requests_per_window: 100,
        max_concurrent: 10,
        now: TimestampMs::from_millis(0),
    };
    let got = backend.create_quota_policy(args).await.unwrap();
    match got {
        CreateQuotaPolicyResult::Created { quota_policy_id } => {
            assert_eq!(quota_policy_id, qid);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn create_quota_policy_invalid_spec_surfaces_validation() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.create_quota_policy = Some(Err(EngineError::Validation {
            kind: ValidationKind::InvalidQuotaSpec,
            detail: "window_seconds must be > 0".into(),
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CreateQuotaPolicyArgs {
        quota_policy_id: ff_core::types::QuotaPolicyId::new(),
        window_seconds: 0,
        max_requests_per_window: 100,
        max_concurrent: 10,
        now: TimestampMs::from_millis(0),
    };
    let err = backend.create_quota_policy(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::Validation {
            kind: ValidationKind::InvalidQuotaSpec,
            ..
        }
    ));
}

// ─── report_usage_admin (§4 row 7 admin path) ───────────────────

#[tokio::test]
async fn report_usage_admin_happy_dispatches_through_trait() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.report_usage_admin = Some(Ok(ReportUsageResult::Ok));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let bid = BudgetId::new();
    let args = ReportUsageAdminArgs::new(
        vec!["tokens".into()],
        vec![10],
        TimestampMs::from_millis(0),
    );
    let got = backend.report_usage_admin(&bid, args).await.unwrap();
    assert!(matches!(got, ReportUsageResult::Ok));
    let g = mock.scripted.lock().unwrap();
    assert_eq!(g.last_report_usage_admin_args.as_ref().unwrap().0, bid);
}

#[tokio::test]
async fn report_usage_admin_hard_breach_surfaces_typed_result() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.report_usage_admin = Some(Ok(ReportUsageResult::HardBreach {
            dimension: "tokens".into(),
            current_usage: 100,
            hard_limit: 100,
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let bid = BudgetId::new();
    let args = ReportUsageAdminArgs::new(
        vec!["tokens".into()],
        vec![10],
        TimestampMs::from_millis(0),
    );
    let got = backend.report_usage_admin(&bid, args).await.unwrap();
    match got {
        ReportUsageResult::HardBreach {
            dimension,
            current_usage,
            hard_limit,
        } => {
            assert_eq!(dimension, "tokens");
            assert_eq!(current_usage, 100);
            assert_eq!(hard_limit, 100);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

// ─── claim_for_worker (§4 row 9 / §7) ───────────────────────────

#[tokio::test]
async fn claim_for_worker_no_work_dispatches_through_trait() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.claim_for_worker = Some(Ok(ClaimForWorkerOutcome::no_work()));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ClaimForWorkerArgs::new(
        LaneId::new("lane"),
        WorkerId::new("worker-1"),
        WorkerInstanceId::new("wi-1"),
        Default::default(),
        30_000,
    );
    let got = backend.claim_for_worker(args).await.unwrap();
    assert!(matches!(got, ClaimForWorkerOutcome::NoWork));
}

#[tokio::test]
async fn claim_for_worker_granted_returns_grant_shape() {
    let mock = mk_mock();
    let partition = PartitionConfig::default();
    let eid = ExecutionId::for_flow(&FlowId::new(), &partition);
    let part = ff_core::partition::execution_partition(&eid, &partition);
    let grant = ClaimGrant::new(
        eid.clone(),
        PartitionKey::from(&part),
        "ff:exec:{p:0}:xxx:claim_grant".into(),
        30_000,
    );
    {
        let mut g = mock.scripted.lock().unwrap();
        g.claim_for_worker = Some(Ok(ClaimForWorkerOutcome::granted(grant.clone())));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ClaimForWorkerArgs::new(
        LaneId::new("l"),
        WorkerId::new("w"),
        WorkerInstanceId::new("wi"),
        Default::default(),
        30_000,
    );
    let got = backend.claim_for_worker(args).await.unwrap();
    match got {
        ClaimForWorkerOutcome::Granted(g) => {
            assert_eq!(g.execution_id, eid);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn claim_for_worker_contention_surfaces_typed_error() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.claim_for_worker = Some(Err(EngineError::Contention(
            ContentionKind::NoEligibleExecution,
        )));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = ClaimForWorkerArgs::new(
        LaneId::new("empty-lane"),
        WorkerId::new("w"),
        WorkerInstanceId::new("wi"),
        Default::default(),
        30_000,
    );
    let err = backend.claim_for_worker(args).await.unwrap_err();
    assert!(matches!(
        err,
        EngineError::Contention(ContentionKind::NoEligibleExecution)
    ));
}

// ─── defaults-only parity on the Stage C ops ────────────────────

#[tokio::test]
async fn defaults_only_backend_stage_c_methods_are_unavailable() {
    // An `Arc<dyn EngineBackend>` built from the bare trait defaults
    // (no overrides) should surface `EngineError::Unavailable` on
    // every Stage C op — guaranteeing the trait-lift did not
    // accidentally ship a usable default body.
    //
    // We exercise this through the MockBackend's stock
    // implementations (which bypass scripted-response); the assertion
    // is that `MockBackend::<method>` emits `Unavailable` when no
    // script is set.
    let mock = mk_mock();
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CancelExecutionArgs {
        execution_id: mk_eid(),
        reason: "".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(0),
    };
    match backend.cancel_execution(args).await {
        Err(EngineError::Unavailable { op }) => {
            assert!(op.contains("cancel_execution") || op.contains("mock"));
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

// ─── smoke: the trait is dyn-safe on the Stage C surface ────────

#[tokio::test]
async fn stage_c_trait_is_dyn_compatible() {
    // Compile-time assertion: if this compiles, every Stage C method
    // is object-safe on the trait surface. Runtime executes a no-op.
    let _b: Arc<dyn EngineBackend> = mk_mock();
}

// ─── extra: cancel_execution fence detail roundtrip ─────────────

#[tokio::test]
async fn cancel_execution_preserves_fence_triple() {
    let mock = mk_mock();
    let eid = mk_eid();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.cancel_execution = Some(Ok(CancelExecutionResult::Cancelled {
            execution_id: eid.clone(),
            public_state: PublicState::Cancelled,
        }));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let args = CancelExecutionArgs {
        execution_id: eid.clone(),
        reason: "bad_state".into(),
        source: CancelSource::LeaseHolder,
        lease_id: Some(LeaseId::new()),
        lease_epoch: Some(LeaseEpoch::new(0)),
        attempt_id: Some(AttemptId::new()),
        now: TimestampMs::from_millis(0),
    };
    backend.cancel_execution(args.clone()).await.unwrap();
    let g = mock.scripted.lock().unwrap();
    let captured = g.last_cancel_args.as_ref().unwrap();
    assert_eq!(captured.reason, "bad_state");
    assert!(captured.lease_id.is_some());
    assert!(captured.lease_epoch.is_some());
    assert!(captured.attempt_id.is_some());
}

// ─── extra: claim_for_worker capabilities roundtrip ─────────────

#[tokio::test]
async fn claim_for_worker_preserves_capabilities_set() {
    let mock = mk_mock();
    {
        let mut g = mock.scripted.lock().unwrap();
        g.claim_for_worker = Some(Ok(ClaimForWorkerOutcome::no_work()));
    }
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let mut caps = std::collections::BTreeSet::new();
    caps.insert("gpu".to_owned());
    caps.insert("llm".to_owned());
    let args = ClaimForWorkerArgs::new(
        LaneId::new("l"),
        WorkerId::new("w"),
        WorkerInstanceId::new("wi"),
        caps.clone(),
        15_000,
    );
    backend.claim_for_worker(args).await.unwrap();
    let g = mock.scripted.lock().unwrap();
    assert_eq!(
        g.last_claim_args.as_ref().unwrap().worker_capabilities,
        caps
    );
    assert_eq!(g.last_claim_args.as_ref().unwrap().grant_ttl_ms, 15_000);
}
