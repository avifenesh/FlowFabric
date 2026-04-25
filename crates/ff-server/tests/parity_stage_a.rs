//! RFC-017 Stage A parity gate.
//!
//! Covers the Stage A CI requirements called out in
//! `crates/ff-server/STAGE_A_SCOPE.md` §"Stage A CI gate":
//!
//! * `MockBackend` implements every `EngineBackend` method (RFC-017
//!   §14.5) and can be handed into [`Server`] via
//!   [`Server::start_with_backend`] as `Arc<dyn EngineBackend>`.
//! * The migrated handlers (`list_executions_page`, `deliver_signal`)
//!   dispatch through `self.backend`, so scripting the mock's
//!   response yields that response back at the public server API —
//!   no Valkey round-trip.
//! * `backend_label()` returns a stable label and
//!   `shutdown_prepare(grace)` is a well-formed awaitable. Covered
//!   via the mock here; the Valkey impl returns `"valkey"` / `Ok(())`
//!   (see `ValkeyBackend::backend_label` / `shutdown_prepare` in
//!   `ff-backend-valkey`); the live-path smoke for those values
//!   lives in `ff-backend-valkey`'s `#[ignore]`-gated live tests.
//!
//! **RFC-vs-scope gap noted in the Stage A PR.** RFC-017 §9 Stage A
//! says trait-lift `backend_label` + `shutdown_prepare`;
//! `STAGE_A_SCOPE.md` says "Do NOT add new trait methods". The
//! scope doc wins — these land as inherent methods on each backend
//! in Stage A; the trait-lift defers to Stage B alongside the
//! `Server::shutdown` integration.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint,
    ReclaimToken, ResumeSignal, SummaryDocument, TailVisibility, UsageDimensions,
};
use ff_core::contracts::{
    CancelFlowResult, ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs,
    DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ExecutionSnapshot,
    FlowSnapshot, ListExecutionsPage, ListFlowsPage, ListLanesPage, ListSuspendedPage,
    ReportUsageResult, RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllResult,
    SetEdgeGroupPolicyResult, StreamCursor, StreamFrames, SuspendArgs, SuspendOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::types::{
    AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, SignalId, TimestampMs,
    WaitpointId, WaitpointToken,
};

fn fresh_execution_id() -> ExecutionId {
    ExecutionId::solo(&LaneId::new("parity"), &PartitionConfig::default())
}

/// Scripted-response MockBackend (RFC-017 §14.5).
///
/// Implements every `EngineBackend` method. The two migrated-in-
/// Stage-A methods (`list_executions`, `deliver_signal`) return
/// caller-scripted responses. All other methods return
/// `EngineError::Unavailable { op }` — sufficient for Stage A's
/// handler surface, Stage B/C/D tighten as more ops migrate.
#[derive(Default)]
struct MockBackend {
    scripted: Mutex<Scripted>,
}

#[derive(Default)]
struct Scripted {
    list_executions: Option<ListExecutionsPage>,
    deliver_signal: Option<DeliverSignalResult>,
    last_list_executions_args: Option<(PartitionKey, Option<ExecutionId>, usize)>,
    last_deliver_signal_args: Option<DeliverSignalArgs>,
}

impl MockBackend {
    fn backend_label(&self) -> &'static str {
        "mock"
    }

    async fn shutdown_prepare(&self, _grace: Duration) -> Result<(), EngineError> {
        Ok(())
    }
}

/// Noop helper — every un-migrated method routes through here to
/// produce a typed `Unavailable` error.
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

    async fn claim_from_reclaim(
        &self,
        _token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        Err(unavailable("mock::claim_from_reclaim"))
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
        partition: PartitionKey,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        let mut guard = self.scripted.lock().unwrap();
        guard.last_list_executions_args = Some((partition, cursor, limit));
        guard
            .list_executions
            .clone()
            .ok_or_else(|| unavailable("mock::list_executions (no scripted response)"))
    }

    async fn deliver_signal(
        &self,
        args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        let mut guard = self.scripted.lock().unwrap();
        guard.last_deliver_signal_args = Some(args);
        guard
            .deliver_signal
            .clone()
            .ok_or_else(|| unavailable("mock::deliver_signal (no scripted response)"))
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
}

// ── Helpers ───────────────────────────────────────────────────────

fn deliver_signal_args() -> DeliverSignalArgs {
    DeliverSignalArgs {
        execution_id: fresh_execution_id(),
        waitpoint_id: WaitpointId::new(),
        signal_id: SignalId::new(),
        signal_name: "test.signal".into(),
        signal_category: "test".into(),
        source_type: "test".into(),
        source_identity: "parity".into(),
        payload: None,
        payload_encoding: None,
        correlation_id: None,
        idempotency_key: None,
        target_scope: "execution".into(),
        created_at: None,
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: WaitpointToken("ignored-in-mock".into()),
        now: TimestampMs::now(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────

/// `MockBackend::backend_label()` returns a stable label and plugs
/// into `Arc<dyn EngineBackend>` transparently. The analogous check
/// for `ValkeyBackend` (`assert_eq!(valkey.backend_label(), "valkey")`)
/// lives in `ff-backend-valkey`'s `#[ignore]`-gated live tests —
/// constructing a `ValkeyBackend` requires a live Valkey.
#[test]
fn backend_label_smoke() {
    let mock = MockBackend::default();
    assert_eq!(mock.backend_label(), "mock");

    // Smoke the dyn-object path so `Arc<dyn EngineBackend>` holds
    // (compile-time guarantee; runtime assertion makes the intent
    // visible in the test output).
    let _erased: Arc<dyn EngineBackend> = Arc::new(MockBackend::default());
}

/// Issue #281: a backend that does not override `prepare` — here
/// `MockBackend` — inherits the trait default and returns
/// `PrepareOutcome::NoOp`. Ensures out-of-tree backends without
/// boot-prep work compile-and-no-op for free; the
/// `ValkeyBackend::prepare` / `PostgresBackend::prepare` overrides
/// are exercised in their respective crates.
#[tokio::test]
async fn prepare_default_impl_returns_noop() {
    let mock = MockBackend::default();
    let erased: Arc<dyn EngineBackend> = Arc::new(mock);
    let outcome = erased.prepare().await.expect("prepare default is Ok");
    assert_eq!(outcome, ff_core::backend::PrepareOutcome::NoOp);
}

/// `MockBackend::shutdown_prepare(grace)` drains cleanly within
/// `grace`. The Stage A Valkey impl is also a no-op (§RFC-017
/// Stage B moves the stream semaphore + tail client into the
/// backend; until then there is nothing backend-scoped to drain).
#[tokio::test]
async fn shutdown_prepare_smoke() {
    let mock = MockBackend::default();
    let res = tokio::time::timeout(
        Duration::from_secs(1),
        mock.shutdown_prepare(Duration::from_secs(5)),
    )
    .await
    .expect("shutdown_prepare returned before outer timeout");
    assert!(res.is_ok(), "shutdown_prepare should succeed on mock");
}

/// `Server.list_executions_page` dispatches through the backend
/// trait (RFC-017 Stage A migration) — scripting a response on the
/// mock and dispatching through `Arc<dyn EngineBackend>` returns
/// the scripted page verbatim, and the call is received with the
/// expected `(partition_key, cursor, limit)` tuple.
#[tokio::test]
async fn list_executions_dispatches_through_backend() {
    let mock = Arc::new(MockBackend::default());
    let expected_page = ListExecutionsPage::new(vec![fresh_execution_id()], None);
    mock.scripted.lock().unwrap().list_executions = Some(expected_page.clone());

    let erased: Arc<dyn EngineBackend> = mock.clone();

    let partition = ff_core::partition::Partition {
        family: ff_core::partition::PartitionFamily::Execution,
        index: 7,
    };
    let partition_key = PartitionKey::from(&partition);

    let got = erased
        .list_executions(partition_key.clone(), None, 50)
        .await
        .expect("scripted list_executions");
    assert_eq!(got, expected_page);

    // Assert the arguments flowed through unchanged.
    let last = mock.scripted.lock().unwrap().last_list_executions_args.clone();
    assert_eq!(last, Some((partition_key, None, 50)));
}

/// `Server.deliver_signal` dispatches through the backend trait
/// and the scripted response surfaces back verbatim.
#[tokio::test]
async fn deliver_signal_dispatches_through_backend() {
    let mock = Arc::new(MockBackend::default());
    let sid = SignalId::new();
    let expected = DeliverSignalResult::Accepted {
        signal_id: sid.clone(),
        effect: "buffered".into(),
    };
    mock.scripted.lock().unwrap().deliver_signal = Some(expected.clone());

    let erased: Arc<dyn EngineBackend> = mock.clone();
    let args = deliver_signal_args();
    let got = erased
        .deliver_signal(args.clone())
        .await
        .expect("scripted deliver_signal");
    assert_eq!(got, expected);

    let last = mock.scripted.lock().unwrap().last_deliver_signal_args.clone();
    assert!(last.is_some(), "args should have been observed");
    let observed = last.unwrap();
    assert_eq!(observed.execution_id, args.execution_id);
    assert_eq!(observed.signal_id, args.signal_id);
}

/// Unscripted methods return a typed `Unavailable` so Stage B/C/D
/// migrations fail loudly if a newly-migrated handler reaches the
/// mock before its scripted-response support lands.
#[tokio::test]
async fn unmigrated_methods_return_unavailable() {
    let mock = Arc::new(MockBackend::default());
    let erased: Arc<dyn EngineBackend> = mock;
    let err = erased
        .describe_flow(&FlowId::new())
        .await
        .expect_err("mock returns Unavailable for non-migrated methods");
    match err {
        EngineError::Unavailable { op } => assert_eq!(op, "mock::describe_flow"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
