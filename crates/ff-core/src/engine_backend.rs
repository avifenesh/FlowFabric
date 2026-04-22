//! The `EngineBackend` trait — abstracting FlowFabric's write surface.
//!
//! **RFC-012 Stage 1a:** this is the trait landing. The
//! Valkey-backed impl lives in `ff-backend-valkey`; future backends
//! (Postgres) add a sibling crate with their own impl. ff-sdk's
//! `FlowFabricWorker` gains `connect_with(backend)` /
//! `backend(&self)` accessors so consumers that want to bring their
//! own backend (tests, future non-Valkey deployments) can hand one
//! in. The hot-path migration of `ClaimedTask` / `FlowFabricWorker`
//! to forward through the trait lands across Stages 1b-1d.
//!
//! # Object safety
//!
//! `EngineBackend` is object-safe: all methods are `async fn` behind
//! `#[async_trait]` and take `&self`. Consumers can hold
//! `Arc<dyn EngineBackend>` for heterogenous-backend deployments.
//! The trait is `Send + Sync + 'static` per RFC-012 §4.1; every impl
//! must honour that bound.
//!
//! # Error surface
//!
//! Every method returns [`Result<_, EngineError>`]. `EngineError`'s
//! `Transport` variant carries a boxed `dyn Error + Send + Sync`;
//! Valkey-backed transport faults box a
//! `ff_script::error::ScriptError` (downcast via
//! `ff_script::engine_error_ext::transport_script_ref`). Other
//! backends box their native error type and set the `backend` tag
//! accordingly.
//!
//! # Atomicity contract
//!
//! Per-op state transitions MUST be atomic (RFC-012 §3.4). On Valkey
//! this is the single-FCALL-per-op property; on Postgres it is the
//! per-transaction property. A backend that cannot honour atomicity
//! for a given op either MUST NOT implement `EngineBackend` or MUST
//! return `EngineError::Unavailable { op }` for the affected method.
//!
//! # Replay semantics
//!
//! `complete`, `fail`, `cancel`, `suspend`, `delay`, `wait_children`
//! are idempotent under replay — calling twice with the same handle
//! and args returns the same outcome (success on first call, typed
//! `State` / `Contention` on subsequent calls where the fence triple
//! no longer matches a live lease).

use std::time::Duration;

use async_trait::async_trait;

use crate::backend::{
    AdmissionDecision, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal, ReclaimToken,
    ResumeSignal, WaitpointSpec,
};
use crate::contracts::{CancelFlowResult, ExecutionSnapshot, FlowSnapshot};
use crate::engine_error::EngineError;
use crate::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};

/// The engine write surface — a single trait a backend implementation
/// honours to serve a `FlowFabricWorker`.
///
/// See RFC-012 §3.1 for the inventory rationale and §3.3 for the
/// type-level shape. 15 methods.
///
/// # Note on `complete` payload shape
///
/// The RFC §3.3 sketch uses `Option<Bytes>`; the Stage 1a trait uses
/// `Option<Vec<u8>>` to match the existing
/// `ff_sdk::ClaimedTask::complete` signature and avoid adding a
/// `bytes` public-type dep for zero consumer benefit. Round-4 §7.17
/// resolved the payload container debate to `Box<[u8]>` in the
/// public type (see `HandleOpaque`); `Option<Vec<u8>>` is the
/// zero-churn choice consistent with today's code. Consumers that
/// need `&[u8]` can borrow via `.as_deref()` on the Option.
#[async_trait]
pub trait EngineBackend: Send + Sync + 'static {
    // ── Claim + lifecycle ──

    /// Fresh-work claim. Returns `Ok(None)` when no work is currently
    /// available; `Err` only on transport or input-validation faults.
    async fn claim(
        &self,
        lane: &LaneId,
        capabilities: &CapabilitySet,
        policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError>;

    /// Renew a held lease. Returns the updated expiry + epoch on
    /// success; typed `State::StaleLease` / `State::LeaseExpired`
    /// when the lease has been stolen or timed out.
    async fn renew(&self, handle: &Handle) -> Result<LeaseRenewal, EngineError>;

    /// Numeric-progress heartbeat.
    async fn progress(
        &self,
        handle: &Handle,
        percent: Option<u8>,
        message: Option<String>,
    ) -> Result<(), EngineError>;

    /// Append one stream frame. Distinct from [`progress`](Self::progress)
    /// per RFC-012 §3.1.1 K#6.
    async fn append_frame(&self, handle: &Handle, frame: Frame) -> Result<(), EngineError>;

    /// Terminal success. Borrows `handle` (round-4 M-D2) so callers
    /// can retry under `EngineError::Transport` without losing the
    /// cookie. Payload is `Option<Vec<u8>>` per the note above.
    async fn complete(
        &self,
        handle: &Handle,
        payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError>;

    /// Terminal failure with classification. Returns [`FailOutcome`]
    /// so the caller learns whether a retry was scheduled.
    async fn fail(
        &self,
        handle: &Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError>;

    /// Cooperative cancel by the worker holding the lease.
    async fn cancel(&self, handle: &Handle, reason: &str) -> Result<(), EngineError>;

    /// Suspend the execution awaiting one or more waitpoints. Returns
    /// a fresh `Handle` whose `HandleKind::Suspended` supersedes the
    /// caller's pre-suspend handle.
    async fn suspend(
        &self,
        handle: &Handle,
        waitpoints: Vec<WaitpointSpec>,
        timeout: Option<Duration>,
    ) -> Result<Handle, EngineError>;

    /// Non-mutating observation of signals that satisfied the handle's
    /// resume condition.
    async fn observe_signals(&self, handle: &Handle)
        -> Result<Vec<ResumeSignal>, EngineError>;

    /// Consume a reclaim grant to mint a resumed-kind handle. Returns
    /// `Ok(None)` when the grant's target execution is no longer
    /// resumable (already reclaimed, terminal, etc.).
    async fn claim_from_reclaim(
        &self,
        token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError>;

    // Round-5 amendment: lease-releasing peers of `suspend`.

    /// Park the execution until `delay_until`, releasing the lease.
    async fn delay(
        &self,
        handle: &Handle,
        delay_until: TimestampMs,
    ) -> Result<(), EngineError>;

    /// Mark the execution as waiting for its child flow to complete,
    /// releasing the lease.
    async fn wait_children(&self, handle: &Handle) -> Result<(), EngineError>;

    // ── Read / admin ──

    /// Snapshot an execution by id. `Ok(None)` ⇒ no such execution.
    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError>;

    /// Snapshot a flow by id. `Ok(None)` ⇒ no such flow.
    async fn describe_flow(
        &self,
        id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError>;

    /// Operator-initiated cancellation of a flow and (optionally) its
    /// member executions. See RFC-012 §3.1.1 for the policy /wait
    /// matrix.
    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError>;

    // ── Budget ──

    /// Report usage and check admission. Returns the admission
    /// decision; backends enforce idempotency via the caller-supplied
    /// dedup key embedded in `UsageDimensions::custom` (today's
    /// `ff_report_usage_and_check` contract).
    async fn report_usage(
        &self,
        handle: &Handle,
        budget: &BudgetId,
        dimensions: crate::backend::UsageDimensions,
    ) -> Result<AdmissionDecision, EngineError>;
}

/// Object-safety assertion: `dyn EngineBackend` compiles iff every
/// method is dyn-compatible. Kept as a compile-time guard so a future
/// trait change that accidentally breaks dyn-safety fails the build
/// at this site rather than at every downstream `Arc<dyn
/// EngineBackend>` use.
#[allow(dead_code)]
fn _assert_dyn_compatible(_: &dyn EngineBackend) {}

