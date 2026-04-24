//! RFC-017 Stage A backfill — trait-surface default-impl parity.
//!
//! Per RFC-017 §14.1 "parity matrix test per new trait method" with
//! the Stage A minimum floor: every newly-added trait method must be
//! invokable via `Arc<dyn EngineBackend>` and surface
//! `EngineError::Unavailable { op }` when the backend does not
//! override the default. These tests confirm the trait expansion is
//! dyn-safe + the defaults propagate correctly — the substantive
//! per-backend parity tests (Valkey FCALL parity, Postgres SQL parity)
//! live in the per-backend test suites alongside the method
//! implementations.
//!
//! Mock: [`DefaultsOnlyBackend`] overrides nothing beyond the base
//! trait methods so every RFC-017 addition falls through to its
//! default.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint,
    ReclaimToken, ResumeSignal, SummaryDocument, TailVisibility, UsageDimensions,
};
use ff_core::contracts::{
    AddExecutionToFlowArgs, ApplyDependencyToChildArgs, CancelExecutionArgs, CancelFlowResult,
    ChangePriorityArgs, ClaimForWorkerArgs, ClaimResumedExecutionArgs,
    ClaimResumedExecutionResult, CreateBudgetArgs, CreateExecutionArgs, CreateFlowArgs,
    CreateQuotaPolicyArgs, DeliverSignalArgs, DeliverSignalResult, EdgeDependencyPolicy,
    EdgeDirection, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot, ListExecutionsPage,
    ListFlowsPage, ListLanesPage, ListPendingWaitpointsArgs, ListSuspendedPage,
    ReplayExecutionArgs, ReportUsageAdminArgs, ReportUsageResult, ResetBudgetArgs, RevokeLeaseArgs,
    
    SetEdgeGroupPolicyResult, StageDependencyEdgeArgs, StreamCursor, StreamFrames, SuspendArgs,
    SuspendOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::types::{
    AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs,
    WorkerId, WorkerInstanceId,
};

/// Bare-bones backend whose only overrides are the RFC-012 base
/// surface (methods without defaults). Every Stage A addition is
/// inherited verbatim from the trait default.
struct DefaultsOnlyBackend;

#[async_trait]
impl EngineBackend for DefaultsOnlyBackend {
    async fn claim(
        &self,
        _lane: &LaneId,
        _caps: &CapabilitySet,
        _p: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        Ok(None)
    }
    async fn renew(&self, _h: &Handle) -> Result<LeaseRenewal, EngineError> {
        Err(EngineError::Unavailable { op: "renew" })
    }
    async fn progress(
        &self,
        _h: &Handle,
        _p: Option<u8>,
        _m: Option<String>,
    ) -> Result<(), EngineError> {
        Ok(())
    }
    async fn append_frame(
        &self,
        _h: &Handle,
        _f: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        Err(EngineError::Unavailable { op: "append_frame" })
    }
    async fn complete(&self, _h: &Handle, _p: Option<Vec<u8>>) -> Result<(), EngineError> {
        Ok(())
    }
    async fn fail(
        &self,
        _h: &Handle,
        _r: FailureReason,
        _c: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        Err(EngineError::Unavailable { op: "fail" })
    }
    async fn cancel(&self, _h: &Handle, _r: &str) -> Result<(), EngineError> {
        Ok(())
    }
    async fn suspend(
        &self,
        _h: &Handle,
        _a: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        Err(EngineError::Unavailable { op: "suspend" })
    }
    async fn create_waitpoint(
        &self,
        _h: &Handle,
        _k: &str,
        _e: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        Err(EngineError::Unavailable { op: "create_waitpoint" })
    }
    async fn observe_signals(
        &self,
        _h: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        Ok(vec![])
    }
    async fn claim_from_reclaim(
        &self,
        _t: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        Ok(None)
    }
    async fn delay(&self, _h: &Handle, _u: TimestampMs) -> Result<(), EngineError> {
        Ok(())
    }
    async fn wait_children(&self, _h: &Handle) -> Result<(), EngineError> {
        Ok(())
    }
    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        Ok(None)
    }
    async fn describe_flow(
        &self,
        _id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        Ok(None)
    }
    async fn list_edges(
        &self,
        _f: &FlowId,
        _d: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        Ok(vec![])
    }
    async fn describe_edge(
        &self,
        _f: &FlowId,
        _e: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        Ok(None)
    }
    async fn resolve_execution_flow_id(
        &self,
        _e: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        Ok(None)
    }
    async fn list_flows(
        &self,
        _p: PartitionKey,
        _c: Option<FlowId>,
        _l: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        Err(EngineError::Unavailable { op: "list_flows" })
    }
    async fn list_lanes(
        &self,
        _c: Option<LaneId>,
        _l: usize,
    ) -> Result<ListLanesPage, EngineError> {
        Err(EngineError::Unavailable { op: "list_lanes" })
    }
    async fn list_suspended(
        &self,
        _p: PartitionKey,
        _c: Option<ExecutionId>,
        _l: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        Err(EngineError::Unavailable { op: "list_suspended" })
    }
    async fn list_executions(
        &self,
        _p: PartitionKey,
        _c: Option<ExecutionId>,
        _l: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        Err(EngineError::Unavailable { op: "list_executions" })
    }
    async fn deliver_signal(
        &self,
        _a: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        Err(EngineError::Unavailable { op: "deliver_signal" })
    }
    async fn claim_resumed_execution(
        &self,
        _a: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        Err(EngineError::Unavailable { op: "claim_resumed_execution" })
    }
    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _p: CancelFlowPolicy,
        _w: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        Err(EngineError::Unavailable { op: "cancel_flow" })
    }
    async fn set_edge_group_policy(
        &self,
        _f: &FlowId,
        _e: &ExecutionId,
        _p: EdgeDependencyPolicy,
    ) -> Result<SetEdgeGroupPolicyResult, EngineError> {
        Err(EngineError::Unavailable { op: "set_edge_group_policy" })
    }
    async fn report_usage(
        &self,
        _h: &Handle,
        _b: &BudgetId,
        _d: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        Err(EngineError::Unavailable { op: "report_usage" })
    }
    async fn read_stream(
        &self,
        _e: &ExecutionId,
        _a: AttemptIndex,
        _f: StreamCursor,
        _t: StreamCursor,
        _c: u64,
    ) -> Result<StreamFrames, EngineError> {
        Err(EngineError::Unavailable { op: "read_stream" })
    }
    async fn tail_stream(
        &self,
        _e: &ExecutionId,
        _a: AttemptIndex,
        _after: StreamCursor,
        _b: u64,
        _c: u64,
        _v: TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        Err(EngineError::Unavailable { op: "tail_stream" })
    }
    async fn read_summary(
        &self,
        _e: &ExecutionId,
        _a: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        Ok(None)
    }
    // Every RFC-017 Stage A method is deliberately NOT overridden so
    // the default `Unavailable` body is what the test observes.
}

fn backend() -> Arc<dyn EngineBackend> {
    Arc::new(DefaultsOnlyBackend)
}

fn expect_unavailable<T: std::fmt::Debug>(
    res: Result<T, EngineError>,
    expected_op: &str,
) {
    match res {
        Err(EngineError::Unavailable { op }) => {
            assert_eq!(op, expected_op, "wrong op label for default Unavailable");
        }
        other => panic!(
            "expected EngineError::Unavailable {{ op: \"{expected_op}\" }}, got {other:?}"
        ),
    }
}

// ─── Ingress (5) ─────────────────────────────────────────────────

#[tokio::test]
async fn default_create_execution_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &cfg);
    let args = CreateExecutionArgs {
        execution_id: eid,
        namespace: Namespace::new("ns"),
        lane_id: LaneId::new("l"),
        execution_kind: "k".into(),
        input_payload: vec![],
        payload_encoding: None,
        priority: 0,
        creator_identity: "creator".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(backend().create_execution(args).await, "create_execution");
}

#[tokio::test]
async fn default_create_flow_is_unavailable() {
    let args = CreateFlowArgs {
        flow_id: FlowId::new(),
        flow_kind: "k".into(),
        namespace: Namespace::new("ns"),
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(backend().create_flow(args).await, "create_flow");
}

#[tokio::test]
async fn default_add_execution_to_flow_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let args = AddExecutionToFlowArgs {
        flow_id: fid.clone(),
        execution_id: ExecutionId::for_flow(&fid, &cfg),
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(
        backend().add_execution_to_flow(args).await,
        "add_execution_to_flow",
    );
}

#[tokio::test]
async fn default_stage_dependency_edge_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let args = StageDependencyEdgeArgs {
        flow_id: fid.clone(),
        edge_id: EdgeId::new(),
        upstream_execution_id: ExecutionId::for_flow(&fid, &cfg),
        downstream_execution_id: ExecutionId::for_flow(&fid, &cfg),
        dependency_kind: "success_only".into(),
        data_passing_ref: None,
        expected_graph_revision: 0,
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(
        backend().stage_dependency_edge(args).await,
        "stage_dependency_edge",
    );
}

#[tokio::test]
async fn default_apply_dependency_to_child_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let args = ApplyDependencyToChildArgs {
        flow_id: fid.clone(),
        edge_id: EdgeId::new(),
        downstream_execution_id: ExecutionId::for_flow(&fid, &cfg),
        upstream_execution_id: ExecutionId::for_flow(&fid, &cfg),
        graph_revision: 0,
        dependency_kind: "success_only".into(),
        data_passing_ref: None,
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(
        backend().apply_dependency_to_child(args).await,
        "apply_dependency_to_child",
    );
}

// ─── Operator control (4) ────────────────────────────────────────

#[tokio::test]
async fn default_cancel_execution_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let args = CancelExecutionArgs {
        execution_id: ExecutionId::for_flow(&fid, &cfg),
        reason: "".into(),
        source: Default::default(),
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(
        backend().cancel_execution(args).await,
        "cancel_execution",
    );
}

#[tokio::test]
async fn default_change_priority_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let args = ChangePriorityArgs {
        execution_id: ExecutionId::for_flow(&fid, &cfg),
        new_priority: 0,
        lane_id: LaneId::new("l"),
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(
        backend().change_priority(args).await,
        "change_priority",
    );
}

#[tokio::test]
async fn default_replay_execution_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let args = ReplayExecutionArgs {
        execution_id: ExecutionId::for_flow(&fid, &cfg),
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(
        backend().replay_execution(args).await,
        "replay_execution",
    );
}

#[tokio::test]
async fn default_revoke_lease_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let args = RevokeLeaseArgs {
        execution_id: ExecutionId::for_flow(&fid, &cfg),
        expected_lease_id: None,
        worker_instance_id: WorkerInstanceId::new(""),
        reason: "".into(),
    };
    expect_unavailable(backend().revoke_lease(args).await, "revoke_lease");
}

// ─── Budget + quota admin (5) ────────────────────────────────────

#[tokio::test]
async fn default_create_budget_is_unavailable() {
    let args = CreateBudgetArgs {
        budget_id: BudgetId::new(),
        scope_type: "global".into(),
        scope_id: "g".into(),
        enforcement_mode: "hard".into(),
        on_hard_limit: "reject".into(),
        on_soft_limit: "allow".into(),
        reset_interval_ms: 0,
        dimensions: vec!["d".into()],
        hard_limits: vec![1],
        soft_limits: vec![0],
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(backend().create_budget(args).await, "create_budget");
}

#[tokio::test]
async fn default_reset_budget_is_unavailable() {
    let args = ResetBudgetArgs {
        budget_id: BudgetId::new(),
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(backend().reset_budget(args).await, "reset_budget");
}

#[tokio::test]
async fn default_create_quota_policy_is_unavailable() {
    let args = CreateQuotaPolicyArgs {
        quota_policy_id: ff_core::types::QuotaPolicyId::new(),
        window_seconds: 60,
        max_requests_per_window: 100,
        max_concurrent: 10,
        now: TimestampMs::from_millis(0),
    };
    expect_unavailable(
        backend().create_quota_policy(args).await,
        "create_quota_policy",
    );
}

#[tokio::test]
async fn default_get_budget_status_is_unavailable() {
    let bid = BudgetId::new();
    expect_unavailable(
        backend().get_budget_status(&bid).await,
        "get_budget_status",
    );
}

#[tokio::test]
async fn default_report_usage_admin_is_unavailable() {
    let bid = BudgetId::new();
    let args = ReportUsageAdminArgs::new(
        vec!["d".into()],
        vec![1],
        TimestampMs::from_millis(0),
    );
    expect_unavailable(
        backend().report_usage_admin(&bid, args).await,
        "report_usage_admin",
    );
}

// ─── Read + diagnostics (3) ──────────────────────────────────────

#[tokio::test]
async fn default_get_execution_result_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &cfg);
    expect_unavailable(
        backend().get_execution_result(&eid).await,
        "get_execution_result",
    );
}

#[tokio::test]
async fn default_list_pending_waitpoints_is_unavailable() {
    let cfg = PartitionConfig::default();
    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &cfg);
    let args = ListPendingWaitpointsArgs::new(eid);
    expect_unavailable(
        backend().list_pending_waitpoints(args).await,
        "list_pending_waitpoints",
    );
}

#[tokio::test]
async fn default_ping_is_unavailable() {
    expect_unavailable(backend().ping().await, "ping");
}

// ─── Scheduling (1) ──────────────────────────────────────────────

#[tokio::test]
async fn default_claim_for_worker_is_unavailable() {
    let args = ClaimForWorkerArgs::new(
        LaneId::new("l"),
        WorkerId::new("w"),
        WorkerInstanceId::new("wi"),
        std::collections::BTreeSet::new(),
        30_000,
    );
    expect_unavailable(
        backend().claim_for_worker(args).await,
        "claim_for_worker",
    );
}

// ─── Cross-cutting sanity ────────────────────────────────────────

#[test]
fn defaults_only_backend_is_dyn_compatible() {
    // Compile-time guard: if a new non-default trait method lands
    // without covering the base impl on `DefaultsOnlyBackend`, this
    // test file stops compiling, catching it before CI.
    let _: Arc<dyn EngineBackend> = backend();
}

#[tokio::test]
async fn defaults_only_backend_shutdown_prepare_no_op() {
    // `shutdown_prepare` has a default-impl returning Ok(()); assert
    // the defaults-only backend inherits it correctly.
    backend()
        .shutdown_prepare(Duration::from_millis(10))
        .await
        .expect("default shutdown_prepare returns Ok(())");
}

#[test]
fn defaults_only_backend_label_is_unknown() {
    // Default `backend_label` returns "unknown" per the trait body.
    // Non-default in-tree backends override to "valkey" / "postgres".
    assert_eq!(DefaultsOnlyBackend.backend_label(), "unknown");
}
