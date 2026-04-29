//! Regression guard for the scanner per-candidate filter cost
//! contract (issue #122 / PR #443 Issue-2 fix).
//!
//! `scanner::should_skip_candidate` must route the namespace check
//! through `EngineBackend::get_execution_namespace` — a single-field
//! point read — **not** `describe_execution`, which issues an
//! N-field HGETALL / full-snapshot read on every backend.
//!
//! This test exercises the helper against a mock `EngineBackend`
//! that:
//!
//! * Implements `get_execution_namespace` (returns a scripted value).
//! * Implements every **other** required trait method as a `todo!()`
//!   or `EngineError::Unavailable { ... }` stub (the trait has
//!   required-without-default methods that any `impl` must provide).
//! * Inherits the **default** `Err(Unavailable)` body for every
//!   default-provided method — including `describe_execution`. If
//!   `should_skip_candidate` ever regresses to calling
//!   `describe_execution` for the namespace check, that default body
//!   fires, the filter rejects conservatively, and the
//!   "matching-namespace passes" assertion below fails.
//!
//! The trait-surface stub is bulky but mechanical; it's the price of
//! having a pure-Rust regression guard that doesn't need a live
//! Valkey. The `ff-test/scanner_filter_isolation` integration test
//! remains the end-to-end cover.

#![allow(clippy::too_many_arguments)]

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy, FailOutcome,
    FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint, ResumeSignal,
    ResumeToken, ScannerFilter, SummaryDocument, SuspendArgs, SuspendOutcome, TailVisibility,
    UsageDimensions,
};
use ff_core::contracts::{
    CancelFlowResult, ClaimResumedExecutionArgs, ClaimResumedExecutionResult, DeliverSignalArgs,
    DeliverSignalResult, EdgeDependencyPolicy, EdgeDirection, EdgeSnapshot, ExecutionContext,
    ExecutionSnapshot, FlowSnapshot, ListExecutionsPage, ListFlowsPage, ListLanesPage,
    ListSuspendedPage, ReportUsageResult, RotateWaitpointHmacSecretAllArgs,
    RotateWaitpointHmacSecretAllResult, SetEdgeGroupPolicyResult, StreamCursor, StreamFrames,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::partition::PartitionKey;
use ff_core::types::{
    AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, Namespace, SignalId, TimestampMs,
    WaitpointId,
};
use ff_engine::scanner::should_skip_candidate;

fn unavailable(op: &'static str) -> EngineError {
    EngineError::Unavailable { op }
}

/// Mock that only implements `get_execution_namespace`. Everything
/// else returns `Unavailable`; the default trait body for
/// `describe_execution` also returns `Unavailable`.
#[derive(Default)]
struct NamespaceOnlyBackend {
    stored_namespace: Option<String>,
    get_ns_calls: AtomicU32,
}

#[async_trait]
impl EngineBackend for NamespaceOnlyBackend {
    async fn get_execution_namespace(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<Option<String>, EngineError> {
        self.get_ns_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.stored_namespace.clone())
    }

    // ─── Required (no default) methods — all stub Unavailable ───

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
        // Intentionally Unavailable — the point of this test is to
        // prove should_skip_candidate does not take this path for
        // the namespace check.
        Err(unavailable("mock::describe_execution"))
    }
    async fn read_execution_context(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<ExecutionContext, EngineError> {
        Err(unavailable("mock::read_execution_context"))
    }
    async fn describe_flow(&self, _id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError> {
        Err(unavailable("mock::describe_flow"))
    }
    async fn cancel_flow(
        &self,
        _id: &FlowId,
        _policy: CancelFlowPolicy,
        _wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        Err(unavailable("mock::cancel_flow"))
    }
    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        Err(unavailable("mock::report_usage"))
    }
}

fn test_eid_string() -> String {
    // Any valid `{fp:N}:<uuid>` string; the mock ignores it.
    "{fp:0}:00000000-0000-0000-0000-000000000001".to_owned()
}

#[tokio::test]
async fn namespace_filter_passes_on_match_via_point_read() {
    let mock = Arc::new(NamespaceOnlyBackend {
        stored_namespace: Some("alpha".to_owned()),
        ..Default::default()
    });
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let filter = ScannerFilter::new().with_namespace(Namespace::new("alpha"));

    let skip = should_skip_candidate(Some(&backend), &filter, 0, &test_eid_string()).await;
    assert!(!skip, "matching-namespace candidate must pass the filter");
    assert_eq!(
        mock.get_ns_calls.load(Ordering::Relaxed),
        1,
        "exactly one get_execution_namespace call (1-point-read contract)",
    );
}

#[tokio::test]
async fn namespace_filter_rejects_on_mismatch() {
    let mock = Arc::new(NamespaceOnlyBackend {
        stored_namespace: Some("beta".to_owned()),
        ..Default::default()
    });
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let filter = ScannerFilter::new().with_namespace(Namespace::new("alpha"));

    let skip = should_skip_candidate(Some(&backend), &filter, 0, &test_eid_string()).await;
    assert!(skip, "mismatched-namespace candidate must be skipped");
}

#[tokio::test]
async fn namespace_filter_rejects_on_missing_row() {
    let mock = Arc::new(NamespaceOnlyBackend {
        stored_namespace: None,
        ..Default::default()
    });
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let filter = ScannerFilter::new().with_namespace(Namespace::new("alpha"));

    let skip = should_skip_candidate(Some(&backend), &filter, 0, &test_eid_string()).await;
    assert!(skip, "absent-row candidate must be skipped conservatively");
}

#[tokio::test]
async fn noop_filter_skips_backend_entirely() {
    let mock = Arc::new(NamespaceOnlyBackend::default());
    let backend: Arc<dyn EngineBackend> = mock.clone();
    let filter = ScannerFilter::new();

    let skip = should_skip_candidate(Some(&backend), &filter, 0, &test_eid_string()).await;
    assert!(!skip, "noop filter must pass without touching the backend");
    assert_eq!(
        mock.get_ns_calls.load(Ordering::Relaxed),
        0,
        "noop filter must not invoke get_execution_namespace",
    );
}

// Silence unused-import lints on the big surface list — the stub
// impls above reference every import, but the compiler counts them
// per-attribute.
#[allow(dead_code)]
fn _unused_refs(
    _: ListExecutionsPage,
    _: ListFlowsPage,
    _: ListLanesPage,
    _: ListSuspendedPage,
    _: DeliverSignalArgs,
    _: DeliverSignalResult,
    _: ClaimResumedExecutionArgs,
    _: ClaimResumedExecutionResult,
    _: EdgeDependencyPolicy,
    _: EdgeDirection,
    _: EdgeSnapshot,
    _: SetEdgeGroupPolicyResult,
    _: RotateWaitpointHmacSecretAllArgs,
    _: RotateWaitpointHmacSecretAllResult,
    _: StreamCursor,
    _: StreamFrames,
    _: SummaryDocument,
    _: TailVisibility,
    _: AttemptIndex,
    _: EdgeId,
    _: SignalId,
    _: WaitpointId,
    _: PartitionKey,
) {
}
