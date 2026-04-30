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
    AppendFrameOutcome, CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy,
    FailOutcome, FailureClass, FailureReason, Frame, Handle, LeaseRenewal, PendingWaitpoint,
    PrepareOutcome, ResumeSignal, ResumeToken,
};
// `SummaryDocument` and `TailVisibility` are referenced only inside
// `#[cfg(feature = "streaming")]` trait methods below, so the imports
// must be gated to avoid an unused-imports warning on the non-streaming
// build.
#[cfg(feature = "streaming")]
use crate::backend::{SummaryDocument, TailVisibility};
use crate::contracts::{
    CancelFlowResult, ExecutionContext, ExecutionSnapshot, FlowSnapshot, IssueReclaimGrantArgs,
    IssueReclaimGrantOutcome, ReclaimExecutionArgs, ReclaimExecutionOutcome, ReportUsageResult,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllResult, SeedOutcome,
    SeedWaitpointHmacSecretArgs, SuspendArgs, SuspendOutcome,
};
#[cfg(feature = "core")]
use crate::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, ApplyDependencyToChildArgs,
    ApplyDependencyToChildResult, BudgetStatus, CancelExecutionArgs, CancelExecutionResult,
    CancelFlowArgs, ChangePriorityArgs, ChangePriorityResult, ClaimExecutionArgs,
    ClaimExecutionResult, ClaimForWorkerArgs, ClaimForWorkerOutcome, ClaimResumedExecutionArgs,
    ClaimResumedExecutionResult,
    BlockRouteArgs, BlockRouteOutcome, CheckAdmissionArgs, CheckAdmissionResult,
    CompleteExecutionArgs, CompleteExecutionResult, CreateBudgetArgs, CreateBudgetResult,
    CreateExecutionArgs, CreateExecutionResult, CreateFlowArgs, CreateFlowResult,
    CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    DeliverSignalArgs, DeliverSignalResult, EdgeDirection, EdgeSnapshot,
    EvaluateFlowEligibilityArgs, EvaluateFlowEligibilityResult, ExecutionInfo,
    FailExecutionArgs, FailExecutionResult,
    IssueClaimGrantArgs, IssueClaimGrantOutcome, ScanEligibleArgs,
    ListExecutionsPage, ListFlowsPage, ListLanesPage, ListPendingWaitpointsArgs,
    ListPendingWaitpointsResult, ListSuspendedPage, RenewLeaseArgs, RenewLeaseResult,
    ReplayExecutionArgs, ReplayExecutionResult,
    ReportUsageAdminArgs, ResetBudgetArgs, ResetBudgetResult, ResumeExecutionArgs,
    ResumeExecutionResult, RevokeLeaseArgs, RevokeLeaseResult,
    StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
#[cfg(feature = "core")]
use crate::state::PublicState;
#[cfg(feature = "core")]
use crate::partition::PartitionKey;
#[cfg(feature = "streaming")]
use crate::contracts::{StreamCursor, StreamFrames};
use crate::engine_error::EngineError;
#[cfg(feature = "core")]
use crate::types::EdgeId;
#[cfg(feature = "core")]
use crate::types::WaitpointId;
use crate::types::{AttemptIndex, BudgetId, ExecutionId, FlowId, LaneId, LeaseFence, TimestampMs};

/// The engine write surface — a single trait a backend implementation
/// honours to serve a `FlowFabricWorker`.
///
/// See RFC-012 §3.1 for the inventory rationale and §3.3 for the
/// type-level shape. 16 methods (Round-7 added `create_waitpoint`;
/// `append_frame` return widened; `report_usage` return replaced —
/// RFC-012 §R7). Issue #150 added the two trigger-surface methods
/// (`deliver_signal` / `claim_resumed_execution`).
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
    ///
    /// Writes scalar `progress_percent` / `progress_message` fields on
    /// `exec_core`; each call overwrites the previous value. This does
    /// NOT append to the output stream — stream-frame producers must use
    /// [`append_frame`](Self::append_frame) instead.
    async fn progress(
        &self,
        handle: &Handle,
        percent: Option<u8>,
        message: Option<String>,
    ) -> Result<(), EngineError>;

    /// Append one stream frame. Distinct from [`progress`](Self::progress)
    /// per RFC-012 §3.1.1 K#6. Returns the backend-assigned stream entry
    /// id and post-append frame count (RFC-012 §R7.2.1).
    ///
    /// Stream-frame producers (arbitrary `frame_type` + payload, consumed
    /// via the read/tail surfaces) MUST use this method rather than
    /// [`progress`](Self::progress); the latter updates scalar fields on
    /// `exec_core` and is invisible to stream consumers.
    async fn append_frame(
        &self,
        handle: &Handle,
        frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError>;

    /// Terminal success. Borrows `handle` (round-4 M-D2) so callers
    /// can retry under `EngineError::Transport` without losing the
    /// cookie. Payload is `Option<Vec<u8>>` per the note above.
    async fn complete(&self, handle: &Handle, payload: Option<Vec<u8>>) -> Result<(), EngineError>;

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

    /// Suspend the execution awaiting a typed resume condition
    /// (RFC-013 Stage 1d).
    ///
    /// Borrows `handle` (round-4 M-D2). Terminal-looking behaviour is
    /// expressed through [`SuspendOutcome`]:
    ///
    /// * [`SuspendOutcome::Suspended`] — the pre-suspend handle is
    ///   logically invalidated; the fresh `HandleKind::Suspended`
    ///   handle inside the variant supersedes it. Runtime enforcement
    ///   via the fence triple: subsequent ops against the stale handle
    ///   surface as `Contention(LeaseConflict)`.
    /// * [`SuspendOutcome::AlreadySatisfied`] — buffered signals on a
    ///   pending waitpoint already matched the resume condition at
    ///   suspension time. The lease is NOT released; the caller's
    ///   pre-suspend handle remains valid.
    ///
    /// See RFC-013 §2 for the type shapes, §3 for the replay /
    /// idempotency contract, §4 for the error taxonomy.
    async fn suspend(
        &self,
        handle: &Handle,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError>;

    /// Suspend by execution id + lease fence triple, for service-layer
    /// callers that hold a run record / lease-claim descriptor but no
    /// worker [`Handle`] (cairn issue #322).
    ///
    /// Semantics mirror [`Self::suspend`] exactly — the same
    /// [`SuspendArgs`] validation, the same [`SuspendOutcome`]
    /// lifecycle, the same RFC-013 §3 dedup / replay contract. The
    /// only difference is the fencing source: instead of the
    /// `(lease_id, lease_epoch, attempt_id)` fields embedded in a
    /// `Handle`, the backend fences against the triple passed directly.
    /// Attempt-index, lane, and worker-instance metadata that
    /// [`Self::suspend`] reads from the handle payload are recovered
    /// from the backend's authoritative execution record (Valkey:
    /// `exec_core` HGETs; Postgres: `ff_attempt` row lookup).
    ///
    /// The default impl returns [`EngineError::Unavailable`] so
    /// existing backend impls remain non-breaking. Production backends
    /// (Valkey, Postgres) override.
    async fn suspend_by_triple(
        &self,
        exec_id: ExecutionId,
        triple: LeaseFence,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        let _ = (exec_id, triple, args);
        Err(EngineError::Unavailable {
            op: "suspend_by_triple",
        })
    }

    /// Issue a pending waitpoint for future signal delivery.
    ///
    /// Waitpoints have two states in the Valkey wire contract:
    /// **pending** (token issued, not yet backing a suspension) and
    /// **active** (bound to a suspension). This method creates a
    /// waitpoint in the **pending** state. A later `suspend` call
    /// transitions a pending waitpoint to active (see Lua
    /// `use_pending_waitpoint` ARGV flag at
    /// `flowfabric.lua:3603,3641,3690`) — or, if buffered signals
    /// already satisfy its condition, the suspend call returns
    /// `SuspendOutcome::AlreadySatisfied` and the waitpoint activates
    /// without ever releasing the lease.
    ///
    /// Pending-waitpoint expiry is a first-class terminal error on
    /// the wire (`PendingWaitpointExpired` at
    /// `ff-script/src/error.rs:170,403-408`). The attempt retains its
    /// lease while the waitpoint is pending; signals delivered to
    /// this waitpoint are buffered server-side (RFC-012 §R7.2.2).
    async fn create_waitpoint(
        &self,
        handle: &Handle,
        waitpoint_key: &str,
        expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError>;

    /// Read the HMAC token stored on a waitpoint record, keyed by
    /// `(partition, waitpoint_id)`.
    ///
    /// Returns `Ok(Some(token))` when the waitpoint exists and carries
    /// a token, `Ok(None)` when the waitpoint does not exist or no
    /// token field is present. A missing waitpoint is not an error —
    /// signals can legitimately arrive after a waitpoint has been
    /// consumed or expired, and the signal-bridge authenticates on the
    /// presence of a matching token, not on the waitpoint's liveness.
    ///
    /// # Use case
    ///
    /// Control-plane signal delivery (cairn signal-bridge): at
    /// signal-resume time the bridge reads the token off the
    /// waitpoint hash / row to authenticate the resume request it
    /// subsequently issues. Previously implemented as direct
    /// `ferriskey::Client::hget(waitpoint_key, "waitpoint_token")` —
    /// Valkey-only. This method routes the read through the trait so
    /// the pattern works on Postgres + SQLite as well.
    ///
    /// # Per-backend shape
    ///
    /// * **Valkey** — `HGET ff:wp:<tag>:<waitpoint_id> waitpoint_token`
    ///   on the waitpoint's partition. Empty string / missing field
    ///   maps to `None`.
    /// * **Postgres** — `SELECT token FROM ff_waitpoint_pending
    ///   WHERE partition_key = $1 AND waitpoint_id = $2 LIMIT 1`.
    ///   Row-absent → `None`; empty `token` → `None`.
    /// * **SQLite** — same shape as Postgres.
    ///
    /// The `partition` argument is the opaque [`PartitionKey`]
    /// produced by FlowFabric (typically extracted from the
    /// `Handle` / `ResumeToken` the waitpoint was minted against).
    ///
    /// # Default impl
    ///
    /// Returns [`EngineError::Unavailable`] with
    /// `op = "read_waitpoint_token"` so out-of-tree backends and
    /// in-tree backends not yet overriding this method continue to
    /// compile. Mirrors the precedent used by
    /// [`Self::issue_reclaim_grant`] / [`Self::reclaim_execution`].
    #[cfg(feature = "core")]
    async fn read_waitpoint_token(
        &self,
        _partition: PartitionKey,
        _waitpoint_id: &WaitpointId,
    ) -> Result<Option<String>, EngineError> {
        Err(EngineError::Unavailable {
            op: "read_waitpoint_token",
        })
    }

    /// Non-mutating observation of signals that satisfied the handle's
    /// resume condition.
    async fn observe_signals(&self, handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError>;

    /// Consume a resume grant (via [`ResumeToken`]) to mint a
    /// resumed-kind handle. Routes to `ff_claim_resumed_execution` on
    /// Valkey / the epoch-bump reconciler on PG/SQLite. Returns
    /// `Ok(None)` when the grant's target execution is no longer
    /// resumable (already reclaimed, terminal, etc.).
    ///
    /// **Renamed from `claim_from_reclaim` (RFC-024 PR-B+C).** The
    /// pre-rename name advertised "reclaim" but the semantic has
    /// always been resume-after-suspend. The new lease-reclaim path
    /// lives on [`Self::reclaim_execution`].
    async fn claim_from_resume_grant(
        &self,
        token: ResumeToken,
    ) -> Result<Option<Handle>, EngineError>;

    /// Issue a lease-reclaim grant (RFC-024 §3.2). Admits executions
    /// in `lease_expired_reclaimable` or `lease_revoked` state to the
    /// reclaim path; the returned [`IssueReclaimGrantOutcome::Granted`]
    /// carries a [`crate::contracts::ReclaimGrant`] which is then fed
    /// to [`Self::reclaim_execution`] to mint a fresh attempt.
    ///
    /// Default impl returns [`EngineError::Unavailable`] — PR-D (PG),
    /// PR-E (SQLite), and PR-F (Valkey) override with real bodies.
    async fn issue_reclaim_grant(
        &self,
        _args: IssueReclaimGrantArgs,
    ) -> Result<IssueReclaimGrantOutcome, EngineError> {
        Err(EngineError::Unavailable {
            op: "issue_reclaim_grant",
        })
    }

    /// Consume a [`crate::contracts::ReclaimGrant`] to mint a fresh
    /// attempt for a previously lease-expired / lease-revoked
    /// execution (RFC-024 §3.2). Creates a new attempt row, bumps the
    /// execution's `lease_reclaim_count`, and mints a
    /// [`crate::backend::HandleKind::Reclaimed`] handle.
    ///
    /// Default impl returns [`EngineError::Unavailable`] — PR-D (PG),
    /// PR-E (SQLite), and PR-F (Valkey) override with real bodies.
    async fn reclaim_execution(
        &self,
        _args: ReclaimExecutionArgs,
    ) -> Result<ReclaimExecutionOutcome, EngineError> {
        Err(EngineError::Unavailable {
            op: "reclaim_execution",
        })
    }

    // Round-5 amendment: lease-releasing peers of `suspend`.

    /// Park the execution until `delay_until`, releasing the lease.
    async fn delay(&self, handle: &Handle, delay_until: TimestampMs) -> Result<(), EngineError>;

    /// Mark the execution as waiting for its child flow to complete,
    /// releasing the lease.
    async fn wait_children(&self, handle: &Handle) -> Result<(), EngineError>;

    // ── Read / admin ──

    /// Snapshot an execution by id. `Ok(None)` ⇒ no such execution.
    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError>;

    /// Point-read of the execution-scoped `(input_payload,
    /// execution_kind, tags)` bundle used by the SDK worker when
    /// assembling a `ClaimedTask` (see `ff_sdk::ClaimedTask`) after a
    /// successful claim.
    ///
    /// No default impl — every `EngineBackend` must answer this
    /// explicitly. Distinct from [`Self::describe_execution`]
    /// (read-model projection) because the SDK needs the raw payload
    /// bytes + kind + tags immediately post-claim, and the snapshot
    /// projection deliberately omits the payload bytes.
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — pipelined `GET :payload` + `HGETALL :core`
    ///   + `HGETALL :tags` on the execution's partition (same pattern
    ///   as [`Self::describe_execution`]).
    /// * **Postgres** — single `SELECT payload, raw_fields` on
    ///   `ff_exec_core` keyed by `(partition_key, execution_id)`;
    ///   `execution_kind` + `tags` live in `raw_fields` JSONB.
    /// * **SQLite** — identical shape to Postgres.
    ///
    /// Returns [`EngineError::Validation { kind: ValidationKind::InvalidInput, .. }`](crate::engine_error::EngineError::Validation)
    /// when the execution does not exist — the SDK worker only calls
    /// this after a successful claim, so a missing row is a loud
    /// storage-tier invariant violation rather than a routine `Ok(None)`.
    async fn read_execution_context(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ExecutionContext, EngineError>;

    /// Point-read of the execution's current attempt-index **pointer**
    /// — the index of the currently-leased attempt row.
    ///
    /// Distinct from [`Self::read_total_attempt_count`]: this method
    /// names the attempt that *already exists* (pointer), whereas
    /// `read_total_attempt_count` is the monotonic claim counter used
    /// to compute the next fresh attempt index. See the sibling's
    /// rustdoc for the retry-path scenario that motivates the split.
    ///
    /// Used on the SDK worker's `claim_from_resume_grant` path —
    /// specifically the private `claim_resumed_execution` helper —
    /// immediately before dispatching [`Self::claim_resumed_execution`].
    /// The returned index is fed into
    /// [`ClaimResumedExecutionArgs::current_attempt_index`](crate::contracts::ClaimResumedExecutionArgs)
    /// so the backend's script / transaction targets the correct
    /// existing attempt row (KEYS[6] on Valkey; `ff_attempt` PK tuple
    /// on PG/SQLite).
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — `HGET {exec}:core current_attempt_index` on the
    ///   execution's partition. Single command. Both the
    ///   **missing-field** case (`exec_core` present but
    ///   `current_attempt_index` absent or empty-string, i.e. pre-claim
    ///   state) **and** the **missing-row** case (no `exec_core` hash
    ///   at all) read back as `AttemptIndex(0)`. This preserves the
    ///   pre-PR-3 inline-`HGET` semantic and is safe because Valkey's
    ///   happy path requires `exec_core` to exist before this method
    ///   is reached — the SDK only calls `read_current_attempt_index`
    ///   post-grant, and grant issuance is gated on `exec_core`
    ///   presence. A genuinely absent row would surface as the proper
    ///   business-logic error (`NotAResumedExecution` /
    ///   `ExecutionNotLeaseable`) on the downstream FCALL.
    /// * **Postgres** — `SELECT attempt_index FROM ff_exec_core
    ///   WHERE partition_key = $1 AND execution_id = $2`. The column
    ///   is `NOT NULL DEFAULT 0` so a pre-claim row reads back as `0`
    ///   (matching Valkey's missing-field case). **Missing row**
    ///   surfaces as [`EngineError::Validation { kind:
    ///   ValidationKind::InvalidInput, .. }`](crate::engine_error::EngineError::Validation)
    ///   — diverges from Valkey's missing-row `→ 0` mapping.
    /// * **SQLite** — `SELECT attempt_index FROM ff_exec_core
    ///   WHERE partition_key = ? AND execution_id = ?`; identical
    ///   semantics to Postgres (missing-row → `InvalidInput`).
    ///
    /// **Cross-backend asymmetry on missing row is intentional.** The
    /// SDK happy path never observes it (grant issuance on Valkey
    /// requires `exec_core`, and PG/SQLite currently return
    /// `Unavailable` from `claim_from_grant` per
    /// `project_claim_from_grant_pg_sqlite_gap.md`). Consumers writing
    /// backend-agnostic tooling against this method directly must
    /// treat the missing-row case as backend-dependent — match on
    /// `InvalidInput` for PG/SQLite, and treat an unexpected `0` as
    /// the Valkey equivalent signal.
    ///
    /// The default impl returns [`EngineError::Unavailable`] so the
    /// trait addition is non-breaking for out-of-tree backends (same
    /// precedent as [`Self::read_execution_context`] landing in v0.12
    /// PR-1).
    async fn read_current_attempt_index(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<AttemptIndex, EngineError> {
        Err(EngineError::Unavailable {
            op: "read_current_attempt_index",
        })
    }

    /// Point-read of the execution's **total attempt counter** — the
    /// monotonic count of claims that have ever fired against this
    /// execution (including the in-flight one once claimed).
    ///
    /// Used on the SDK worker's `claim_from_grant` / `claim_execution`
    /// path — the next attempt-index for a fresh claim is this
    /// counter's current value (so `1` on the second retry after the
    /// first attempt failed terminally). This is semantically distinct
    /// from [`Self::read_current_attempt_index`], which is a *pointer*
    /// at the currently-leased attempt row and is only meaningful on
    /// the `claim_from_resume_grant` path (where a live attempt already
    /// exists and we want to re-seat its lease rather than mint a new
    /// attempt row).
    ///
    /// Reading the pointer on the `claim_from_grant` path was a live
    /// bug: on the retry-of-a-retry scenario the pointer still named
    /// the *previous* terminal-failed attempt, so the newly-minted
    /// attempt collided with it (Valkey KEYS[6]) or mis-targeted the
    /// PG/SQLite `ff_attempt` PK tuple. This method fixes that by
    /// reading the counter that Lua 5920 / PG `ff_claim_execution` /
    /// SQLite `claim_impl` all already consult when computing the
    /// next attempt index.
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — `HGET {exec}:core total_attempt_count` on the
    ///   execution's partition. Single command; pre-claim read (field
    ///   absent or empty) maps to `0`.
    /// * **Postgres** — `SELECT raw_fields->>'total_attempt_count'
    ///   FROM ff_exec_core WHERE (partition_key, execution_id) = ...`.
    ///   The field lives in the JSONB `raw_fields` bag rather than a
    ///   dedicated column (mirrors how `create_execution_impl` seeds
    ///   it on row creation). Missing row → `InvalidInput`; missing
    ///   field → `0`.
    /// * **SQLite** — `SELECT CAST(json_extract(raw_fields,
    ///   '$.total_attempt_count') AS INTEGER) FROM ff_exec_core
    ///   WHERE ...`. Same JSON-in-`raw_fields` shape as PG; uses the
    ///   same `json_extract` idiom already employed in
    ///   `ff-backend-sqlite/src/queries/operator.rs` for replay_count.
    ///
    /// The default impl returns [`EngineError::Unavailable`] so the
    /// trait addition is non-breaking for out-of-tree backends (same
    /// precedent as [`Self::read_current_attempt_index`] landing in
    /// v0.12 PR-3).
    async fn read_total_attempt_count(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<AttemptIndex, EngineError> {
        Err(EngineError::Unavailable {
            op: "read_total_attempt_count",
        })
    }

    /// Snapshot a flow by id. `Ok(None)` ⇒ no such flow.
    async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError>;

    // ── Namespaced tag point-writes / reads (issue #433) ──

    /// Set a single namespaced tag on an execution. Tag `key` MUST match
    /// the reserved caller-namespace pattern `^[a-z][a-z0-9_]*\.[a-z0-9_][a-z0-9_.]*$` —
    /// i.e. `<caller>.<field>` — or the call returns
    /// [`EngineError::Validation { kind: ValidationKind::InvalidInput, .. }`](crate::engine_error::EngineError::Validation)
    /// with the offending key in `detail`. `value` is arbitrary UTF-8.
    ///
    /// The namespace prefix is carried inline in `key` (e.g.
    /// `"cairn.session_id"`) — there is no separate `namespace` arg.
    /// This matches the existing `ff_set_execution_tags` wire shape and
    /// the flow-tag projection in [`ExecutionSnapshot::tags`].
    ///
    /// Validation is performed by each overriding backend impl via
    /// [`validate_tag_key`] **before** the wire hop so PG / SQLite /
    /// Valkey reject the same set of keys. The default trait impl
    /// returns [`EngineError::Unavailable`] without running validation
    /// — there is no meaningful storage to validate against on an
    /// unsupported backend, and surfacing `Unavailable` before
    /// `Validation` matches the precedence used elsewhere on the trait.
    /// Backends MAY additionally validate on the storage tier (Valkey's
    /// Lua path does, with a more permissive prefix-only check).
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — `ff_set_execution_tags` FCALL with a single
    ///   `{key → value}` pair. Routes through the existing Lua
    ///   contract (no new wire format).
    /// * **Postgres** — `UPDATE ff_exec_core SET raw_fields = jsonb_set(
    ///   coalesce(raw_fields, '{}'::jsonb), '{tags,<key>}', to_jsonb($value))
    ///   WHERE (partition_key, execution_id) = ...`. Same storage shape
    ///   read by [`Self::describe_execution`] / [`Self::read_execution_context`].
    /// * **SQLite** — `UPDATE ff_exec_core SET raw_fields = json_set(
    ///   coalesce(raw_fields, '{}'), '$.tags."<key>"', $value) WHERE ...`.
    ///   The key is quoted in the JSON path so dots inside the
    ///   namespaced key (e.g. `cairn.session_id`) are treated as a
    ///   single literal member name rather than JSON-path separators —
    ///   yielding the same flat `raw_fields.tags` shape as PG.
    ///
    /// Missing execution surfaces as
    /// [`EngineError::NotFound { entity: "execution" }`](crate::engine_error::EngineError::NotFound)
    /// — matches the Valkey FCALL's `execution_not_found` mapping and
    /// the existing `ScriptError::ExecutionNotFound` → `EngineError`
    /// conversion (`ff_script::engine_error_ext`).
    ///
    /// The default impl returns [`EngineError::Unavailable`] so the
    /// trait addition is non-breaking for out-of-tree backends.
    async fn set_execution_tag(
        &self,
        _execution_id: &ExecutionId,
        _key: &str,
        _value: &str,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "set_execution_tag",
        })
    }

    /// Set a single namespaced tag on a flow. Same namespace rule as
    /// [`Self::set_execution_tag`]: `key` MUST match
    /// `^[a-z][a-z0-9_]*\.[a-z0-9_][a-z0-9_.]*$`.
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — `ff_set_flow_tags` FCALL with a single pair.
    ///   Tags land on the dedicated `ff:flow:{fp:N}:<flow_id>:tags`
    ///   hash, not on the `flow_core` hash (diverges from the
    ///   execution shape — execution tags live on `ff:exec:...:tags`
    ///   by the same split). **Lazy migration on first write**: the
    ///   Lua (`ff_script::flowfabric.lua`, `ff_set_flow_tags`) scans
    ///   `flow_core` once per flow for pre-58.4 inline namespaced
    ///   fields (anything matching `^[a-z][a-z0-9_]*\.`), HSETs them
    ///   onto `:tags`, HDELs them from `flow_core`, and stamps
    ///   `tags_migrated=1` on `flow_core` so subsequent calls
    ///   short-circuit to O(1). This heals flows created before
    ///   RFC-058.4 landed; well-formed flows pay the migration cost
    ///   only on their very first tag write. Callers MUST read tags
    ///   via [`Self::get_flow_tag`] (`HGET :tags <key>`) — direct
    ///   `HGETALL` against `flow_core` will not see post-migration
    ///   values.
    ///
    ///   **Cross-backend parity caveat on `describe_flow`**: the
    ///   pre-existing `ValkeyBackend::describe_flow` /
    ///   `FlowSnapshot::tags` read path snapshots `flow_core` fields
    ///   only and does NOT today merge the `:tags` sub-hash, whereas
    ///   Postgres `describe_flow` DOES surface flow tags via
    ///   `ff_backend_postgres::flow::extract_tags` (which reads them
    ///   off `raw_fields` — the same store `set_flow_tag` writes on
    ///   PG). Trait consumers MUST NOT assume a tag written here
    ///   will be visible via `describe_flow` on every backend: on
    ///   Valkey, callers that need the full tag set should
    ///   complement the snapshot with per-key [`Self::get_flow_tag`]
    ///   reads. Extending Valkey `describe_flow` to merge `:tags`
    ///   is additive and out of scope for this trait addition.
    /// * **Postgres** — `UPDATE ff_flow_core SET raw_fields =
    ///   jsonb_set(..., '{<key>}', ...)` — flow tags are stored as
    ///   top-level `raw_fields` keys (matches
    ///   `ff_backend_postgres::flow::extract_tags`). No `tags` nesting
    ///   on flows, which diverges from the execution shape.
    /// * **SQLite** — mirrors PG: `UPDATE ff_flow_core SET raw_fields =
    ///   json_set(..., '$."<key>"', $value) WHERE ...`. The key is
    ///   quoted so the dotted namespaced key lands as a single flat
    ///   top-level member of `raw_fields`.
    ///
    /// Missing flow surfaces as
    /// [`EngineError::NotFound { entity: "flow" }`](crate::engine_error::EngineError::NotFound)
    /// (matches the Valkey FCALL's `flow_not_found` mapping).
    ///
    /// The default impl returns [`EngineError::Unavailable`].
    async fn set_flow_tag(
        &self,
        _flow_id: &FlowId,
        _key: &str,
        _value: &str,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "set_flow_tag",
        })
    }

    /// Read a single namespaced execution tag. Returns `Ok(None)` when
    /// the tag is absent **or** the execution row does not exist —
    /// the two cases are not distinguished on the read path. Callers
    /// that need to distinguish should call [`Self::describe_execution`]
    /// first (an `Ok(None)` from that method proves the execution is
    /// absent). This matches Valkey's native `HGET` semantics and
    /// keeps the read path at a single round-trip on every backend.
    ///
    /// `key` must pass [`validate_tag_key`] — a malformed key can
    /// never be present in storage so the call short-circuits with
    /// [`EngineError::Validation { kind: ValidationKind::InvalidInput, .. }`](crate::engine_error::EngineError::Validation)
    /// rather than round-tripping.
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — `HGET :tags <key>` on the execution's partition.
    /// * **Postgres** — `SELECT raw_fields->'tags'->><key> FROM ff_exec_core
    ///   WHERE ...` with `fetch_optional` → missing row collapses to `None`.
    /// * **SQLite** — `SELECT json_extract(raw_fields, '$.tags."<key>"')
    ///   FROM ff_exec_core WHERE ...` with the same collapse. The key is
    ///   quoted in the JSON path so dotted namespaced keys resolve to
    ///   the flat literal member written by `set_execution_tag`.
    ///
    /// The default impl returns [`EngineError::Unavailable`].
    async fn get_execution_tag(
        &self,
        _execution_id: &ExecutionId,
        _key: &str,
    ) -> Result<Option<String>, EngineError> {
        Err(EngineError::Unavailable {
            op: "get_execution_tag",
        })
    }

    /// Read an execution's `namespace` scalar. Returns `Ok(None)` when
    /// the row is absent or the field is unset. Dedicated point-read
    /// used by the scanner per-candidate filter (`should_skip_candidate`)
    /// to preserve the 1-HGET cost contract documented in
    /// `ff_engine::scanner::should_skip_candidate` — `describe_execution`
    /// is heavier (HGETALL / full snapshot) and unnecessary when only
    /// the namespace scalar is needed.
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — `HGET :core namespace` on the execution's partition
    ///   (single field read on the already-hot exec_core hash).
    /// * **Postgres** — `SELECT raw_fields->>'namespace' FROM ff_exec_core
    ///   WHERE partition_key = $1 AND execution_id = $2`.
    /// * **SQLite** — `SELECT json_extract(raw_fields, '$.namespace')
    ///   FROM ff_exec_core WHERE ...`.
    ///
    /// The default impl returns [`EngineError::Unavailable`].
    async fn get_execution_namespace(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<Option<String>, EngineError> {
        Err(EngineError::Unavailable {
            op: "get_execution_namespace",
        })
    }

    /// Read a single namespaced flow tag. Returns `Ok(None)` when
    /// the tag is absent **or** the flow row does not exist (same
    /// collapse semantics as [`Self::get_execution_tag`]). Symmetry
    /// partner — consumers like cairn read `cairn.session_id` off
    /// flows for archival.
    ///
    /// `key` must pass [`validate_tag_key`].
    ///
    /// Per-backend shape:
    ///
    /// * **Valkey** — `HGET :tags <key>` on the flow's partition.
    /// * **Postgres** — `SELECT raw_fields->><key> FROM ff_flow_core
    ///   WHERE ...` (top-level `raw_fields` key, matches the flow-tag
    ///   storage shape).
    /// * **SQLite** — `SELECT json_extract(raw_fields, '$."<key>"')
    ///   FROM ff_flow_core WHERE ...` (quoted key — see
    ///   `set_flow_tag`).
    ///
    /// The default impl returns [`EngineError::Unavailable`].
    async fn get_flow_tag(
        &self,
        _flow_id: &FlowId,
        _key: &str,
    ) -> Result<Option<String>, EngineError> {
        Err(EngineError::Unavailable {
            op: "get_flow_tag",
        })
    }

    /// List dependency edges adjacent to an execution. Read-only; the
    /// backend resolves the subject execution's flow, reads the
    /// direction-specific adjacency SET, and decodes each member's
    /// flow-scoped `edge:<edge_id>` hash.
    ///
    /// Returns an empty `Vec` when the subject has no edges on the
    /// requested side — including standalone executions (no owning
    /// flow). Ordering is unspecified: the underlying adjacency SET
    /// is an unordered SMEMBERS read. Callers that need deterministic
    /// order should sort by [`EdgeSnapshot::edge_id`] /
    /// [`EdgeSnapshot::created_at`] themselves.
    ///
    /// Parse failures on the edge hash surface as
    /// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`]
    /// — unknown fields, missing required fields, endpoint mismatches
    /// against the adjacency SET all fail loud rather than silently
    /// returning partial results.
    ///
    /// Gated on the `core` feature — edge reads are part of the
    /// minimal engine surface a Postgres-style backend must honour.
    ///
    /// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`]: crate::engine_error::EngineError::Validation
    #[cfg(feature = "core")]
    async fn list_edges(
        &self,
        _flow_id: &FlowId,
        _direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        Err(EngineError::Unavailable { op: "list_edges" })
    }

    /// Snapshot a single dependency edge by its owning flow + edge id.
    ///
    /// `Ok(None)` when the edge hash is absent (never staged, or
    /// staged under a different flow than `flow_id`). Parse failures
    /// on a present edge hash surface as
    /// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`]
    /// — the stored `flow_id` field is cross-checked against the
    /// caller's expected `flow_id` so a wrong-key read fails loud
    /// rather than returning an unrelated edge.
    ///
    /// Gated on the `core` feature — single-edge reads are part of
    /// the minimal snapshot surface an alternate backend must honour
    /// alongside [`Self::describe_execution`] / [`Self::describe_flow`]
    /// / [`Self::list_edges`].
    ///
    /// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`]: crate::engine_error::EngineError::Validation
    #[cfg(feature = "core")]
    async fn describe_edge(
        &self,
        _flow_id: &FlowId,
        _edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        Err(EngineError::Unavailable {
            op: "describe_edge",
        })
    }

    /// Resolve an execution's owning flow id, if any.
    ///
    /// `Ok(None)` when the execution's core record is absent or has
    /// no associated flow (standalone execution). A present-but-
    /// malformed `flow_id` field surfaces as
    /// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`].
    ///
    /// Gated on the `core` feature. Used by ff-sdk's
    /// `list_outgoing_edges` / `list_incoming_edges` to pivot from a
    /// consumer-supplied `ExecutionId` to the `FlowId` required by
    /// [`Self::list_edges`]. A Valkey backend serves this with a
    /// single `HGET exec_core flow_id`; a Postgres backend serves it
    /// with the equivalent single-column row lookup.
    ///
    /// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`]: crate::engine_error::EngineError::Validation
    #[cfg(feature = "core")]
    async fn resolve_execution_flow_id(
        &self,
        _eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        Err(EngineError::Unavailable {
            op: "resolve_execution_flow_id",
        })
    }

    /// List flows on a partition with cursor-based pagination (issue
    /// #185).
    ///
    /// Returns a [`ListFlowsPage`] of [`FlowSummary`](crate::contracts::FlowSummary)
    /// rows ordered by `flow_id` (UUID byte-lexicographic). `cursor`
    /// is `None` for the first page; callers forward the returned
    /// `next_cursor` verbatim to continue iteration, and the listing
    /// is exhausted when `next_cursor` is `None`. `limit` is the
    /// maximum number of rows to return on this page — implementations
    /// MAY return fewer (end of partition) but MUST NOT exceed it.
    ///
    /// Ordering rationale: flow ids are UUIDs, and both Valkey
    /// (sort after-the-fact) and Postgres (`ORDER BY flow_id`) can
    /// agree on byte-lexicographic order — the same order
    /// `FlowId::to_string()` produces for canonical hyphenated UUIDs.
    /// Mapping to `cursor > flow_id` keeps the contract backend-
    /// independent.
    ///
    /// # Postgres implementation pattern
    ///
    /// A Postgres-backed implementation serves this directly with
    ///
    /// ```sql
    /// SELECT flow_id, created_at_ms, public_flow_state
    ///   FROM ff_flow
    ///  WHERE partition_key = $1
    ///    AND ($2::uuid IS NULL OR flow_id > $2)
    ///  ORDER BY flow_id
    ///  LIMIT $3 + 1;
    /// ```
    ///
    /// — reading one extra row to decide whether `next_cursor` should
    /// be set to the last row's `flow_id`. The Valkey implementation
    /// maintains the `ff:idx:{fp:N}:flow_index` SET and performs the
    /// sort + slice client-side (SMEMBERS then sort-by-UUID-bytes),
    /// pipelining `HGETALL flow_core` for each row on the page.
    ///
    /// Gated on the `core` feature — flow listing is part of the
    /// minimal engine surface a Postgres-style backend must honour.
    #[cfg(feature = "core")]
    async fn list_flows(
        &self,
        _partition: PartitionKey,
        _cursor: Option<FlowId>,
        _limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        Err(EngineError::Unavailable { op: "list_flows" })
    }

    /// Enumerate registered lanes with cursor-based pagination.
    ///
    /// Lanes are global (not partition-scoped) — the backend serves
    /// this from its lane registry and does NOT accept a
    /// [`crate::partition::Partition`] argument. Results are sorted
    /// by [`LaneId`] name so the ordering is stable across calls and
    /// cursors address a deterministic position in the sort.
    ///
    /// * `cursor` — exclusive lower bound. `None` starts from the
    ///   first lane. To continue a walk, pass the previous page's
    ///   [`ListLanesPage::next_cursor`].
    /// * `limit` — hard cap on the number of lanes returned in the
    ///   page. Backends MAY round this down when the registry size
    ///   is smaller; they MUST NOT return more than `limit`.
    ///
    /// [`ListLanesPage::next_cursor`] is `Some(last_lane_in_page)`
    /// iff at least one more lane exists after the returned page,
    /// and `None` on the final page. Callers loop until `next_cursor`
    /// is `None` to read the full registry.
    ///
    /// Gated on the `core` feature — lane enumeration is part of the
    /// minimal snapshot surface an alternate backend must honour
    /// alongside [`Self::describe_flow`] / [`Self::list_edges`].
    #[cfg(feature = "core")]
    async fn list_lanes(
        &self,
        _cursor: Option<LaneId>,
        _limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        Err(EngineError::Unavailable { op: "list_lanes" })
    }

    /// List suspended executions in one partition, cursor-paginated,
    /// with each entry's suspension `reason_code` populated (issue
    /// #183).
    ///
    /// Consumer-facing "what's blocked on what?" panels (ff-board's
    /// suspended-executions view, operator CLIs) need the reason in
    /// the list response so the UI does not round-trip per row to
    /// `describe_execution` for a field it knows it needs. `reason`
    /// on [`SuspendedExecutionEntry`] carries the free-form
    /// `suspension:current.reason_code` field — see the type rustdoc
    /// for the String-not-enum rationale.
    ///
    /// `cursor` is opaque to callers; pass `None` to start a fresh
    /// scan and feed the returned [`ListSuspendedPage::next_cursor`]
    /// back in on subsequent pages until it comes back `None`.
    /// `limit` bounds the `entries` count; backends MAY return fewer
    /// when the partition is exhausted.
    ///
    /// Ordering is by ascending `suspended_at_ms` (the per-lane
    /// suspended ZSET score == `timeout_at` or the no-timeout
    /// sentinel) with execution id as a lex tiebreak, so cursor
    /// continuation is deterministic across calls.
    ///
    /// Gated on the `core` feature — suspended-list enumeration is
    /// part of the minimal engine surface a Postgres-style backend
    /// must honour.
    #[cfg(feature = "core")]
    async fn list_suspended(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        Err(EngineError::Unavailable {
            op: "list_suspended",
        })
    }

    /// Forward-only paginated listing of the executions indexed under
    /// one partition.
    ///
    /// Reads the partition-wide `ff:idx:{p:N}:all_executions` set,
    /// sorts lexicographically on `ExecutionId`, and returns the page
    /// of ids strictly greater than `cursor` (or starting from the
    /// smallest id when `cursor = None`). The returned
    /// [`ListExecutionsPage::next_cursor`] is the last id on the page
    /// iff at least one more id exists past it; `None` signals
    /// end-of-stream.
    ///
    /// `limit` is the maximum number of ids returned on this page. A
    /// `limit` of `0` returns an empty page with `next_cursor = None`.
    /// Backends MAY cap `limit` internally (Valkey: 1000) and return
    /// fewer ids than requested; callers continue paginating until
    /// `next_cursor == None`.
    ///
    /// Ordering is stable under concurrent inserts for already-emitted
    /// ids (an id less-than-or-equal-to the caller's cursor is never
    /// re-emitted in later pages) but new inserts past the cursor WILL
    /// appear in subsequent pages — consistent with forward-only
    /// cursor semantics.
    ///
    /// Gated on the `core` feature — partition-scoped listing is part
    /// of the minimal engine surface every backend must honour.
    #[cfg(feature = "core")]
    async fn list_executions(
        &self,
        _partition: PartitionKey,
        _cursor: Option<ExecutionId>,
        _limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        Err(EngineError::Unavailable {
            op: "list_executions",
        })
    }

    // ── Trigger ops (issue #150) ──

    /// Deliver an external signal to a suspended execution's waitpoint.
    ///
    /// The backend atomically records the signal, evaluates the resume
    /// condition, and — when satisfied — transitions the execution
    /// from `suspended` to `runnable` (or buffers the signal when the
    /// waitpoint is still `pending`). Duplicate delivery — same
    /// `idempotency_key` + waitpoint — surfaces as
    /// [`DeliverSignalResult::Duplicate`] with the pre-existing
    /// `signal_id` rather than mutating state twice.
    ///
    /// Input validation (HMAC token presence, payload size limits,
    /// signal-name shape) is the backend's responsibility; callers
    /// pass a fully populated [`DeliverSignalArgs`] and receive typed
    /// outcomes or typed errors (`ScriptError::invalid_token`,
    /// `ScriptError::token_expired`, `ScriptError::ExecutionNotFound`
    /// surfaced via [`EngineError::Transport`] on the Valkey backend).
    ///
    /// Gated on the `core` feature — signal delivery is part of the
    /// minimal trigger surface every backend must honour so ff-server
    /// / REST handlers can dispatch against `Arc<dyn EngineBackend>`
    /// without knowing which backend is running underneath.
    #[cfg(feature = "core")]
    async fn deliver_signal(
        &self,
        _args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "deliver_signal",
        })
    }

    /// Claim a resumed execution — a previously-suspended attempt that
    /// has cleared its resume condition (e.g. via
    /// [`Self::deliver_signal`]) and now needs a worker to pick up the
    /// same attempt index.
    ///
    /// Distinct from [`Self::claim`] (fresh work) and
    /// [`Self::claim_from_resume_grant`] (grant-based ownership transfer
    /// after a crash): the resumed-claim path re-binds an existing
    /// attempt rather than minting a new one. The backend issues a
    /// fresh `lease_id` + bumps the `lease_epoch`, preserving
    /// `attempt_id` / `attempt_index` so stream frames and progress
    /// updates continue on the same attempt.
    ///
    /// Typed failures surface via `ScriptError` → `EngineError`:
    /// `NotAResumedExecution` when the attempt state is not
    /// `attempt_interrupted`, `ExecutionNotLeaseable` when the
    /// lifecycle phase is not `runnable`, and `InvalidClaimGrant`
    /// when the grant key is missing or was already consumed.
    ///
    /// Gated on the `core` feature — resumed-claim is part of the
    /// minimal trigger surface every backend must honour.
    #[cfg(feature = "core")]
    async fn claim_resumed_execution(
        &self,
        _args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "claim_resumed_execution",
        })
    }

    /// Scan a lane's eligible ZSET on one partition for
    /// highest-priority executions awaiting a worker (v0.12 PR-5).
    ///
    /// Lifted from the SDK-side `ZRANGEBYSCORE` inline on
    /// `FlowFabricWorker::claim_next` — the scheduler-bypass scanner
    /// gated behind `direct-valkey-claim`. The trait method itself is
    /// backend-agnostic; consumers that drive the scanner loop
    /// (bench harnesses, single-tenant dev) compose it with
    /// [`Self::issue_claim_grant`] + [`Self::claim_execution`] to
    /// replicate the pre-PR-5 `claim_next` body.
    ///
    /// # Backend coverage
    ///
    /// * **Valkey** — `ZRANGEBYSCORE eligible_zset -inf +inf LIMIT 0 <limit>`
    ///   on the lane's partition-scoped eligible key. Single
    ///   command; no script round-trip. Wire shape is byte-for-byte
    ///   identical to the pre-PR SDK inline call so bench traces
    ///   match pre-PR without new `#[tracing::instrument]` span names.
    /// * **Postgres / SQLite** — use the `Err(Unavailable)` default.
    ///   PG/SQLite consumers drive work through the scheduler-routed
    ///   [`Self::claim_for_worker`] path instead of the scanner
    ///   primitives exposed here; lifting the scheduler itself onto
    ///   the trait is RFC-024 follow-up scope. See
    ///   `project_claim_from_grant_pg_sqlite_gap.md` for motivation.
    ///
    /// Default impl returns [`EngineError::Unavailable`] so the trait
    /// addition is non-breaking for out-of-tree backends. Same
    /// precedent as [`Self::claim_execution`] landing in v0.12 PR-4.
    #[cfg(feature = "core")]
    async fn scan_eligible_executions(
        &self,
        _args: ScanEligibleArgs,
    ) -> Result<Vec<ExecutionId>, EngineError> {
        Err(EngineError::Unavailable {
            op: "scan_eligible_executions",
        })
    }

    /// Issue a claim grant — the scheduler's admission write — for a
    /// single execution on a single lane (v0.12 PR-5).
    ///
    /// Lifted from the SDK-side `ff_issue_claim_grant` inline helper
    /// on `FlowFabricWorker::claim_next`. The backend atomically
    /// writes the grant hash, appends to the per-worker grant index,
    /// and removes the execution from the lane's eligible ZSET.
    ///
    /// Typed rejects surface via [`EngineError::Validation`]:
    /// `CapabilityMismatch` when the worker's capabilities do not
    /// cover the execution's `required_capabilities`, `InvalidInput`
    /// for malformed args. Transport faults surface via
    /// [`EngineError::Transport`].
    ///
    /// # Backend coverage
    ///
    /// * **Valkey** — one `ff_issue_claim_grant` FCALL. KEYS/ARGV
    ///   shape is byte-for-byte identical to the pre-PR SDK inline
    ///   call; bench traces match pre-PR.
    /// * **Postgres / SQLite** — `Err(Unavailable)` default; use
    ///   [`Self::claim_for_worker`] instead. See
    ///   [`Self::scan_eligible_executions`] for the cross-link
    ///   rationale.
    #[cfg(feature = "core")]
    async fn issue_claim_grant(
        &self,
        _args: IssueClaimGrantArgs,
    ) -> Result<IssueClaimGrantOutcome, EngineError> {
        Err(EngineError::Unavailable {
            op: "issue_claim_grant",
        })
    }

    /// Move an execution from a lane's eligible ZSET into its
    /// blocked_route ZSET (v0.12 PR-5).
    ///
    /// Lifted from the SDK-side `ff_block_execution_for_admission`
    /// inline helper on `FlowFabricWorker::claim_next`. Called after
    /// a [`Self::issue_claim_grant`] `CapabilityMismatch` reject —
    /// without a block step, the inline scanner would re-pick the
    /// same top-of-ZSET every tick (parity with
    /// `ff-scheduler::Scheduler::block_candidate`).
    ///
    /// The engine's unblock scanner periodically promotes
    /// blocked_route back to eligible once a worker with matching
    /// caps registers.
    ///
    /// # Backend coverage
    ///
    /// * **Valkey** — one `ff_block_execution_for_admission` FCALL.
    /// * **Postgres / SQLite** — `Err(Unavailable)` default; the
    ///   scheduler-routed [`Self::claim_for_worker`] path handles
    ///   admission rejects server-side.
    #[cfg(feature = "core")]
    async fn block_route(
        &self,
        _args: BlockRouteArgs,
    ) -> Result<BlockRouteOutcome, EngineError> {
        Err(EngineError::Unavailable { op: "block_route" })
    }

    /// Consume a scheduler-issued claim grant to mint a fresh attempt.
    ///
    /// The SDK's grant-consumer path — paired with `FlowFabricWorker::claim_from_grant`
    /// in `ff-sdk` — routes through this method. The scheduler has
    /// already validated budget / quota / capabilities and written a
    /// grant (Valkey `claim_grant` hash); this call atomically
    /// consumes that grant and creates the attempt row, mints
    /// `lease_id` + `lease_epoch`, and returns a
    /// [`ClaimExecutionResult::Claimed`] carrying the minted lease
    /// triple.
    ///
    /// Distinct from [`Self::claim`] (the scheduler-bypass scanner
    /// used by the `direct-valkey-claim` feature) — this method
    /// assumes the grant already exists and skips capability / ZSET
    /// scanning. The Valkey impl fires exactly one `ff_claim_execution`
    /// FCALL.
    ///
    /// Typed failures surface via `ScriptError` → `EngineError`:
    /// `UseClaimResumedExecution` when the attempt is actually
    /// `attempt_interrupted` (caller should retry via
    /// [`Self::claim_resumed_execution`] — see `ContentionKind` at
    /// `ff_core::engine_error`), `InvalidClaimGrant` when the grant is
    /// missing / consumed / worker-mismatched, `CapabilityMismatch`
    /// when the execution's `required_capabilities` drifted after
    /// grant issuance.
    ///
    /// # Backend coverage
    ///
    /// * **Valkey** — implemented in `ff-backend-valkey` (one
    ///   `ff_claim_execution` FCALL).
    /// * **Postgres / SQLite** — use the `Err(Unavailable)` default in
    ///   this PR. Grants on PG / SQLite today flow through
    ///   `PostgresScheduler::claim_for_worker` (a sibling struct, not
    ///   an `EngineBackend` method); wiring the default-over-trait
    ///   behaviour into a PG / SQLite `claim_execution` impl lands
    ///   with a future RFC-024 grant-consumer extension.
    ///
    /// The default impl returns [`EngineError::Unavailable`] so the
    /// trait addition is non-breaking for out-of-tree backends. Same
    /// precedent as [`Self::read_current_attempt_index`] landing in
    /// v0.12 PR-3.
    #[cfg(feature = "core")]
    async fn claim_execution(
        &self,
        _args: ClaimExecutionArgs,
    ) -> Result<ClaimExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "claim_execution",
        })
    }

    /// Operator-initiated cancellation of a flow and (optionally) its
    /// member executions. See RFC-012 §3.1.1 for the policy /wait
    /// matrix.
    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError>;

    /// RFC-016 Stage A: set the inbound-edge-group policy for a
    /// downstream execution. Must be called before the first
    /// `add_dependency(... -> downstream_execution_id)` — the backend
    /// rejects with [`EngineError::Conflict`] if edges have already
    /// been staged for this group.
    ///
    /// Stage A honours only
    /// [`EdgeDependencyPolicy::AllOf`](crate::contracts::EdgeDependencyPolicy::AllOf);
    /// the `AnyOf` / `Quorum` variants return
    /// [`EngineError::Validation`] with
    /// `detail = "stage A supports AllOf only; AnyOf/Quorum land in stage B"`
    /// until Stage B's resolver lands.
    #[cfg(feature = "core")]
    async fn set_edge_group_policy(
        &self,
        _flow_id: &FlowId,
        _downstream_execution_id: &ExecutionId,
        _policy: crate::contracts::EdgeDependencyPolicy,
    ) -> Result<crate::contracts::SetEdgeGroupPolicyResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "set_edge_group_policy",
        })
    }

    // ── HMAC secret rotation (v0.7 migration-master Q4) ──

    /// Rotate the waitpoint HMAC signing kid **cluster-wide**.
    ///
    /// **v0.7 migration-master Q4 (adjudicated 2026-04-24).**
    /// Additive trait surface so Valkey and Postgres backends can
    /// both expose the "rotate everywhere" semantic under one name.
    ///
    /// * Valkey impl fans out an `ff_rotate_waitpoint_hmac_secret`
    ///   FCALL per execution partition. `entries.len() == num_flow_partitions`
    ///   and per-partition failures are surfaced as inner `Err`
    ///   entries — the call as a whole does not fail when one
    ///   partition's FCALL fails, matching
    ///   [`ff_sdk::admin::rotate_waitpoint_hmac_secret_all_partitions`]'s
    ///   partial-success contract.
    /// * Postgres impl (Wave 4) writes one row to
    ///   `ff_waitpoint_hmac(kid, secret, rotated_at)` and returns a
    ///   single-entry vec with `partition = 0`.
    ///
    /// The default impl returns
    /// [`EngineError::Unavailable`] with
    /// `op = "rotate_waitpoint_hmac_secret_all"` so backends that
    /// haven't implemented the method surface the miss loudly rather
    /// than silently no-op'ing. Both concrete backends override.
    async fn rotate_waitpoint_hmac_secret_all(
        &self,
        _args: RotateWaitpointHmacSecretAllArgs,
    ) -> Result<RotateWaitpointHmacSecretAllResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "rotate_waitpoint_hmac_secret_all",
        })
    }

    /// Seed the initial waitpoint HMAC secret for a fresh deployment
    /// (issue #280).
    ///
    /// **Idempotent.** If a `current_kid` (Valkey per-partition) or
    /// an active kid row (Postgres) already exists with the given
    /// `kid`, the method returns
    /// [`SeedOutcome::AlreadySeeded`] without overwriting, reporting
    /// whether the stored secret matches the caller-supplied one via
    /// `same_secret`. Callers (cairn boot, operator tooling) invoke
    /// this on every boot and let the backend decide whether to
    /// install — removing the client-side "check then HSET" race that
    /// cairn's raw-HSET boot path silently tolerated.
    ///
    /// For rotation of an already-seeded secret, use
    /// [`Self::rotate_waitpoint_hmac_secret_all`] instead; seed is
    /// install-only.
    ///
    /// The default impl returns [`EngineError::Unavailable`] with
    /// `op = "seed_waitpoint_hmac_secret"` so backends that haven't
    /// implemented the method surface the miss loudly.
    async fn seed_waitpoint_hmac_secret(
        &self,
        _args: SeedWaitpointHmacSecretArgs,
    ) -> Result<SeedOutcome, EngineError> {
        Err(EngineError::Unavailable {
            op: "seed_waitpoint_hmac_secret",
        })
    }

    // ── Budget ──

    /// Report usage against a budget and check limits. Returns the
    /// typed [`ReportUsageResult`] variant; backends enforce
    /// idempotency via the caller-supplied
    /// [`UsageDimensions::dedup_key`] (RFC-012 §R7.2.3 — replaces
    /// the pre-Round-7 `AdmissionDecision` return).
    async fn report_usage(
        &self,
        handle: &Handle,
        budget: &BudgetId,
        dimensions: crate::backend::UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError>;

    // ── Stream reads (RFC-012 Stage 1c tranche-4; issue #87) ──

    /// Read frames from a completed or in-flight attempt's stream.
    ///
    /// `from` / `to` are [`StreamCursor`] values — `StreamCursor::Start`
    /// / `StreamCursor::End` are equivalent to XRANGE `-` / `+`, and
    /// `StreamCursor::At("<id>")` reads from a concrete entry id.
    ///
    /// Input validation (count_limit bounds, cursor shape) is the
    /// caller's responsibility — SDK-side wrappers in
    /// [`ff-sdk`](https://docs.rs/ff-sdk) enforce bounds before
    /// forwarding. Backends MAY additionally reject out-of-range
    /// input via [`EngineError::Validation`].
    ///
    /// Gated on the `streaming` feature — stream reads are part of
    /// the stream-subset surface a backend without XREAD-like
    /// primitives may omit.
    #[cfg(feature = "streaming")]
    async fn read_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _from: StreamCursor,
        _to: StreamCursor,
        _count_limit: u64,
    ) -> Result<StreamFrames, EngineError> {
        Err(EngineError::Unavailable { op: "read_stream" })
    }

    /// Tail a live attempt's stream.
    ///
    /// `after` is an exclusive [`StreamCursor`] — entries with id
    /// strictly greater than `after` are returned. `StreamCursor::Start`
    /// / `StreamCursor::End` are NOT accepted here; callers MUST pass
    /// a concrete id (or `StreamCursor::from_beginning()`). The SDK
    /// wrapper rejects the open markers before reaching the backend.
    ///
    /// `block_ms == 0` → non-blocking peek. `block_ms > 0` → blocks up
    /// to that many ms for a new entry.
    ///
    /// `visibility` (RFC-015 §6.1) filters the returned entries by
    /// their stored [`StreamMode`](crate::backend::StreamMode)
    /// `mode` field. Default
    /// [`TailVisibility::All`](crate::backend::TailVisibility::All)
    /// preserves v1 behaviour.
    ///
    /// Gated on the `streaming` feature — see [`read_stream`](Self::read_stream).
    #[cfg(feature = "streaming")]
    async fn tail_stream(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
        _after: StreamCursor,
        _block_ms: u64,
        _count_limit: u64,
        _visibility: TailVisibility,
    ) -> Result<StreamFrames, EngineError> {
        Err(EngineError::Unavailable { op: "tail_stream" })
    }

    /// Read the rolling summary document for an attempt (RFC-015 §6.3).
    ///
    /// Returns `Ok(None)` when no [`StreamMode::DurableSummary`](crate::backend::StreamMode::DurableSummary)
    /// frame has ever been appended for the attempt. Non-blocking Hash
    /// read; safe to call from any consumer without holding the lease.
    ///
    /// Gated on the `streaming` feature — summary reads are part of
    /// the stream-subset surface.
    #[cfg(feature = "streaming")]
    async fn read_summary(
        &self,
        _execution_id: &ExecutionId,
        _attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        Err(EngineError::Unavailable {
            op: "read_summary",
        })
    }

    // ── RFC-017 Stage A — Ingress (5) ──────────────────────────
    //
    // Every method in this block has a default impl returning
    // `EngineError::Unavailable { op }` per RFC-017 §5.3. Concrete
    // backends override each method with a real body. A missing
    // override surfaces as a loud typed error at the call site rather
    // than a silent no-op.

    /// Create an execution. Ingress row 6 (RFC-017 §4). Wraps
    /// `ff_create_execution` on Valkey; `INSERT INTO ff_execution ...`
    /// on Postgres. The `idempotency_key` + backend-side default
    /// `dedup_ttl_ms = 86400000` make duplicate submissions idempotent.
    #[cfg(feature = "core")]
    async fn create_execution(
        &self,
        _args: CreateExecutionArgs,
    ) -> Result<CreateExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "create_execution",
        })
    }

    /// Create a flow header. Ingress row 5.
    #[cfg(feature = "core")]
    async fn create_flow(
        &self,
        _args: CreateFlowArgs,
    ) -> Result<CreateFlowResult, EngineError> {
        Err(EngineError::Unavailable { op: "create_flow" })
    }

    /// Atomically add an execution to a flow (single-FCALL co-located
    /// commit on Valkey; single-transaction UPSERT on Postgres).
    #[cfg(feature = "core")]
    async fn add_execution_to_flow(
        &self,
        _args: AddExecutionToFlowArgs,
    ) -> Result<AddExecutionToFlowResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "add_execution_to_flow",
        })
    }

    /// Stage a dependency edge between flow members. CAS-guarded on
    /// `graph_revision` — stale rev returns `Contention(StaleGraphRevision)`.
    #[cfg(feature = "core")]
    async fn stage_dependency_edge(
        &self,
        _args: StageDependencyEdgeArgs,
    ) -> Result<StageDependencyEdgeResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "stage_dependency_edge",
        })
    }

    /// Apply a staged dependency edge to its downstream child.
    #[cfg(feature = "core")]
    async fn apply_dependency_to_child(
        &self,
        _args: ApplyDependencyToChildArgs,
    ) -> Result<ApplyDependencyToChildResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "apply_dependency_to_child",
        })
    }

    /// Resolve one dependency edge after its upstream reached a
    /// terminal outcome — satisfy on "success", mark impossible
    /// otherwise. Idempotent (`AlreadyResolved` on replay).
    ///
    /// PR-7b Step 0 overlap-resolver: lifted here so cluster 2
    /// (`scanner/dependency_reconciler`) and cluster 4
    /// (`completion_listener::spawn_dispatch_loop`) can both trait-
    /// route through `Arc<dyn EngineBackend>` without a merge
    /// conflict. Both Valkey call sites today build identical
    /// KEYS[14]+ARGV[5] and invoke the `ff_resolve_dependency`
    /// FCALL — this method is that FCALL behind the trait.
    ///
    /// # Backend status
    ///
    /// - **Valkey:** wraps `ff_resolve_dependency` (RFC-016 Stage C
    ///   signature). Atomic single-slot FCALL.
    /// - **Postgres:** `Unavailable`. PG's post-completion cascade is
    ///   not per-edge; it runs via
    ///   `ff_backend_postgres::dispatch::dispatch_completion(event_id)`
    ///   keyed on the `ff_completion_event` outbox row. The Valkey-
    ///   shaped per-edge resolve does not map cleanly to that model;
    ///   PG's `dependency_reconciler` already calls `dispatch_completion`
    ///   directly. The engine's PR-7b/final integration test expects
    ///   `Unsupported` logs from Valkey-shaped scanners on a PG
    ///   deployment — this surface honours that contract.
    /// - **SQLite:** `Unavailable` for the same reason (mirrors PG).
    ///
    /// The default impl returns [`EngineError::Unavailable`] so a
    /// backend that has not been migrated surfaces a typed
    /// `Unsupported`-grade error rather than a panic.
    #[cfg(feature = "core")]
    async fn resolve_dependency(
        &self,
        _args: crate::contracts::ResolveDependencyArgs,
    ) -> Result<crate::contracts::ResolveDependencyOutcome, EngineError> {
        Err(EngineError::Unavailable {
            op: "resolve_dependency",
        })
    }

    // ── RFC-017 Stage A — Operator control (4) ─────────────────

    /// Operator-initiated execution cancel (row 2).
    #[cfg(feature = "core")]
    async fn cancel_execution(
        &self,
        _args: CancelExecutionArgs,
    ) -> Result<CancelExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "cancel_execution",
        })
    }

    /// Re-score an execution's eligibility priority (row 17).
    #[cfg(feature = "core")]
    async fn change_priority(
        &self,
        _args: ChangePriorityArgs,
    ) -> Result<ChangePriorityResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "change_priority",
        })
    }

    /// Replay a terminal execution (row 22). Variadic KEYS handling
    /// (inbound-edge pre-read) is hidden inside the Valkey impl per
    /// RFC-017 §4 row 3.
    #[cfg(feature = "core")]
    async fn replay_execution(
        &self,
        _args: ReplayExecutionArgs,
    ) -> Result<ReplayExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "replay_execution",
        })
    }

    /// Operator-initiated lease revoke (row 19).
    #[cfg(feature = "core")]
    async fn revoke_lease(
        &self,
        _args: RevokeLeaseArgs,
    ) -> Result<RevokeLeaseResult, EngineError> {
        Err(EngineError::Unavailable { op: "revoke_lease" })
    }

    // ── cairn #389 — service-layer typed FCALL surface ────────────
    //
    // These methods mirror `complete`/`fail`/`renew` (which take a
    // worker [`Handle`]) but dispatch against `(execution_id, fence)`
    // tuples supplied directly. Service-layer callers (cairn's
    // `valkey_control_plane_impl.rs`, future consumers that hold a
    // run/lease descriptor without a `Handle`) use these to avoid
    // going through the raw `ferriskey::Value` FCALL escape hatch.
    //
    // Same shape / same precedent as `suspend_by_triple` (cairn #322).
    //
    // Default impl returns [`EngineError::Unavailable`] so the trait
    // addition is non-breaking for out-of-tree backends. The in-tree
    // Valkey backend overrides; Postgres + SQLite keep the default
    // until follow-up parity work lands (consistent with
    // `issue_reclaim_grant` / `reclaim_execution` precedent).

    /// Service-layer `complete_execution` — peer of [`Self::complete`]
    /// that takes a fence triple instead of a worker [`Handle`]. See
    /// the group preamble above for cairn-migration context.
    #[cfg(feature = "core")]
    async fn complete_execution(
        &self,
        _args: CompleteExecutionArgs,
    ) -> Result<CompleteExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "complete_execution",
        })
    }

    /// Service-layer `fail_execution` — peer of [`Self::fail`] that
    /// takes a fence triple instead of a worker [`Handle`].
    #[cfg(feature = "core")]
    async fn fail_execution(
        &self,
        _args: FailExecutionArgs,
    ) -> Result<FailExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "fail_execution",
        })
    }

    /// Service-layer `renew_lease` — peer of [`Self::renew`] that
    /// takes a fence triple instead of a worker [`Handle`].
    #[cfg(feature = "core")]
    async fn renew_lease(
        &self,
        _args: RenewLeaseArgs,
    ) -> Result<RenewLeaseResult, EngineError> {
        Err(EngineError::Unavailable { op: "renew_lease" })
    }

    /// Service-layer `resume_execution` — transitions a suspended
    /// execution back to runnable. Distinct from
    /// [`Self::claim_from_resume_grant`] (which mints a worker handle
    /// against an already-eligible resumed execution): this method is
    /// the lifecycle transition primitive the control plane calls
    /// when an operator / auto-resume policy moves a suspended
    /// execution forward.
    ///
    /// The Valkey impl pre-reads `current_waitpoint_id` + `lane_id`
    /// from `exec_core` so callers only need the execution id + the
    /// trigger type — same ergonomics as `revoke_lease` reading
    /// `current_worker_instance_id` when callers omit it.
    #[cfg(feature = "core")]
    async fn resume_execution(
        &self,
        _args: ResumeExecutionArgs,
    ) -> Result<ResumeExecutionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "resume_execution",
        })
    }

    /// Service-layer `check_admission_and_record` — atomic admission
    /// check against a quota policy. Callers supply the policy id +
    /// dimension (quota keys live on their own `{q:<policy>}`
    /// partition that cannot be derived from `execution_id`, so these
    /// travel outside [`CheckAdmissionArgs`]). `dimension` defaults
    /// to `"default"` inside the Valkey body when the caller passes
    /// an empty string — matches cairn's pre-migration default.
    #[cfg(feature = "core")]
    async fn check_admission(
        &self,
        _quota_policy_id: &crate::types::QuotaPolicyId,
        _dimension: &str,
        _args: CheckAdmissionArgs,
    ) -> Result<CheckAdmissionResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "check_admission",
        })
    }

    /// Service-layer `evaluate_flow_eligibility` — read-only check
    /// that returns the execution's current eligibility state
    /// (`eligible`, `blocked_by_dependencies`, or a backend-specific
    /// status string). Called by cairn's dependency-resolution path
    /// to decide whether a downstream execution can proceed.
    #[cfg(feature = "core")]
    async fn evaluate_flow_eligibility(
        &self,
        _args: EvaluateFlowEligibilityArgs,
    ) -> Result<EvaluateFlowEligibilityResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "evaluate_flow_eligibility",
        })
    }

    // ── RFC-017 Stage A — Budget + quota admin (5) ─────────────

    /// Create a budget definition (row 6).
    #[cfg(feature = "core")]
    async fn create_budget(
        &self,
        _args: CreateBudgetArgs,
    ) -> Result<CreateBudgetResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "create_budget",
        })
    }

    /// Reset a budget's usage counters (row 10).
    #[cfg(feature = "core")]
    async fn reset_budget(
        &self,
        _args: ResetBudgetArgs,
    ) -> Result<ResetBudgetResult, EngineError> {
        Err(EngineError::Unavailable { op: "reset_budget" })
    }

    /// Create a quota policy (row 7).
    #[cfg(feature = "core")]
    async fn create_quota_policy(
        &self,
        _args: CreateQuotaPolicyArgs,
    ) -> Result<CreateQuotaPolicyResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "create_quota_policy",
        })
    }

    /// Read-only budget status for operator visibility (row 8).
    #[cfg(feature = "core")]
    async fn get_budget_status(
        &self,
        _id: &BudgetId,
    ) -> Result<BudgetStatus, EngineError> {
        Err(EngineError::Unavailable {
            op: "get_budget_status",
        })
    }

    /// Admin-path `report_usage` (row 9 + RFC-017 §5 round-1 F4).
    /// Distinct from the existing [`Self::report_usage`] which takes
    /// a worker handle — the admin path has no lease context.
    #[cfg(feature = "core")]
    async fn report_usage_admin(
        &self,
        _budget: &BudgetId,
        _args: ReportUsageAdminArgs,
    ) -> Result<ReportUsageResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "report_usage_admin",
        })
    }

    // ── RFC-017 Stage A — Read + diagnostics (3) ───────────────

    /// Fetch the stored result payload for a completed execution
    /// (row 4). Returns `Ok(None)` when the execution is missing, not
    /// yet complete, or its payload was trimmed by retention policy.
    async fn get_execution_result(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        Err(EngineError::Unavailable {
            op: "get_execution_result",
        })
    }

    /// List the pending-or-active waitpoints for an execution, cursor
    /// paginated (row 5 / §8). Stage A preserves the existing
    /// `PendingWaitpointInfo` shape; Stage D ships the §8 HMAC
    /// sanitisation + `(token_kid, token_fingerprint)` schema.
    #[cfg(feature = "core")]
    async fn list_pending_waitpoints(
        &self,
        _args: ListPendingWaitpointsArgs,
    ) -> Result<ListPendingWaitpointsResult, EngineError> {
        Err(EngineError::Unavailable {
            op: "list_pending_waitpoints",
        })
    }

    /// Backend-level reachability probe (row 1). Valkey: `PING`;
    /// Postgres: `SELECT 1`.
    async fn ping(&self) -> Result<(), EngineError> {
        Err(EngineError::Unavailable { op: "ping" })
    }

    // ── RFC-017 Stage A — Scheduling (1) ───────────────────────

    /// Scheduler-routed claim entrypoint (row 18, RFC-017 §7). Valkey
    /// forwards to its `ff_scheduler::Scheduler` cursor; Postgres
    /// forwards to `PostgresScheduler`'s `FOR UPDATE SKIP LOCKED`
    /// path.
    ///
    /// Backends that carry an embedded scheduler (e.g. `ValkeyBackend`
    /// constructed via `with_embedded_scheduler`, or `PostgresBackend`
    /// with its `with_scanners` sibling) route the claim through it.
    /// Backends without a wired scheduler return
    /// [`EngineError::Unavailable`]. HTTP consumers use
    /// `FlowFabricWorker::claim_via_server` instead.
    #[cfg(feature = "core")]
    async fn claim_for_worker(
        &self,
        _args: ClaimForWorkerArgs,
    ) -> Result<ClaimForWorkerOutcome, EngineError> {
        Err(EngineError::Unavailable {
            op: "claim_for_worker",
        })
    }

    // ── Cross-cutting (RFC-017 Stage B trait-lift) ──────────────

    /// Static observability label identifying the backend family in
    /// logs + metrics (RFC-017 §5.4 + §9 Stage B). Default impl
    /// returns `"unknown"` so legacy `impl EngineBackend` blocks that
    /// have not upgraded keep compiling; every in-tree backend
    /// overrides — `ValkeyBackend` → `"valkey"`, `PostgresBackend` →
    /// `"postgres"`.
    fn backend_label(&self) -> &'static str {
        "unknown"
    }

    /// Backend downcast escape hatch (v0.12 PR-7a transitional).
    ///
    /// Scanner supervisors in `ff-engine` still dispatch through a
    /// concrete `ferriskey::Client`; to keep the engine's public
    /// boundary backend-agnostic (`Arc<dyn EngineBackend>`) while the
    /// scanner internals remain Valkey-shaped, the engine downcasts
    /// via this method and reaches in for the embedded client. Every
    /// backend that wants to be consumed by `Engine::start_with_completions`
    /// overrides this to return `self` as `&dyn Any`; the default
    /// returns a placeholder so a stray `downcast_ref` fails cleanly
    /// rather than risking unsound behaviour.
    ///
    /// v0.13 (PR-7b) will trait-ify individual scanners onto
    /// `EngineBackend` and retire `ff-engine`'s dependence on this
    /// downcast path. The method itself will remain on the trait
    /// (likely deprecated) rather than be removed — removing a
    /// public trait method is a breaking change for external
    /// `impl EngineBackend` blocks.
    fn as_any(&self) -> &(dyn std::any::Any + 'static) {
        // Placeholder so the default does not expose `Self` for
        // downcast. Backends override to return `self`.
        &()
    }

    /// RFC-018 Stage A: snapshot of this backend's identity + the
    /// flat `Supports` surface it can actually service. Consumers use
    /// this at startup to gate UI features / choose between alternative
    /// code paths before dispatching. See
    /// `rfcs/RFC-018-backend-capability-discovery.md` for the full
    /// discovery contract and the four owner-adjudicated open
    /// questions (granularity: coarse; version: struct; sync; no
    /// event stream).
    ///
    /// Default: returns a value tagged `family = "unknown"` with every
    /// `supports.*` bool `false`, so pre-RFC-018 out-of-tree backends
    /// keep compiling and consumers treat "all false" as "dispatch
    /// and catch [`EngineError::Unavailable`]" (pre-RFC-018 behaviour).
    /// Concrete in-tree backends (`ValkeyBackend`, `PostgresBackend`)
    /// override to populate a real value.
    ///
    /// Sync (no `.await`): backend-static info should not require a
    /// probe on every query. Dynamic probes happen once at
    /// `connect*` time and cache the result.
    fn capabilities(&self) -> crate::capability::Capabilities {
        crate::capability::Capabilities::new(
            crate::capability::BackendIdentity::new(
                "unknown",
                crate::capability::Version::new(0, 0, 0),
                "unknown",
            ),
            crate::capability::Supports::none(),
        )
    }

    /// Issue #281: run one-time backend-specific boot preparation.
    ///
    /// Intended to run ONCE per deployment startup — NOT per request.
    /// Idempotent and safe for consumers to call on every application
    /// boot; backends that have nothing to do return
    /// [`PrepareOutcome::NoOp`] without side effects.
    ///
    /// Per-backend behaviour:
    ///
    /// * **Valkey** — issues `FUNCTION LOAD REPLACE` for the
    ///   `flowfabric` Lua library (with bounded retry on transient
    ///   transport faults; permanent compile errors surface as
    ///   [`EngineError::Transport`] without retry). Returns
    ///   [`PrepareOutcome::Applied`] carrying
    ///   `"FUNCTION LOAD (flowfabric lib v<N>)"`.
    /// * **Postgres** — returns [`PrepareOutcome::NoOp`]. Schema
    ///   migrations are applied out-of-band per
    ///   `rfcs/drafts/v0.7-migration-master.md §Q12`; the backend
    ///   runs a schema-version check at connect time and refuses to
    ///   start on mismatch, so no boot-side prepare work remains.
    /// * **Default impl** — returns [`PrepareOutcome::NoOp`] so
    ///   out-of-tree backends without preparation work compile
    ///   without boilerplate.
    ///
    /// # Relationship to the in-tree boot path
    ///
    /// `ValkeyBackend::initialize_deployment` (called from
    /// `Server::start_with_metrics`) already invokes
    /// [`ensure_library`](ff_script::loader::ensure_library) inline as
    /// its step 4; that path is unchanged. `prepare()` exists as a
    /// **trait-surface entry point** so consumers that construct an
    /// `Arc<dyn EngineBackend>` outside of `Server` (e.g.
    /// cairn-fabric's boot path at `cairn-fabric/src/boot.rs`) can
    /// run the same preparation without reaching into
    /// backend-specific modules. The overlap is intentional: calling
    /// both `prepare()` and `initialize_deployment` is safe because
    /// `FUNCTION LOAD REPLACE` is idempotent under the version
    /// check.
    ///
    /// # Layer forwarding
    ///
    /// Layer impls (`HookedBackend`, ff-sdk layers) do NOT forward
    /// `prepare` today — consistent with `backend_label` / `ping` /
    /// `shutdown_prepare`. Consumers that wrap a backend in layers
    /// MUST call `prepare()` on the raw backend before wrapping, or
    /// accept the default [`PrepareOutcome::NoOp`].
    async fn prepare(&self) -> Result<PrepareOutcome, EngineError> {
        Ok(PrepareOutcome::NoOp)
    }

    /// Drain-before-shutdown hook (RFC-017 §5.4). The server calls
    /// this before draining its own background tasks so backend-
    /// scoped primitives (Valkey stream semaphore, Postgres sqlx
    /// pool, …) can close their gates and await in-flight work up to
    /// `grace`.
    ///
    /// Default impl returns `Ok(())` — a no-op backend has nothing
    /// backend-scoped to drain. Concrete backends whose data plane
    /// owns resources (connection pools, semaphores, listeners)
    /// override with a real body.
    async fn shutdown_prepare(&self, _grace: Duration) -> Result<(), EngineError> {
        Ok(())
    }

    // ── RFC-017 Stage E2 — `Server::client` removal (header + reads) ───

    /// RFC-017 Stage E2: the "header" portion of `cancel_flow` — run the
    /// atomic flow-state flip (Valkey: `ff_cancel_flow` FCALL; Postgres:
    /// `cancel_flow_once` tx), decode policy + membership, and surface
    /// the `flow_already_terminal` idempotency branch as a first-class
    /// [`CancelFlowHeader::AlreadyTerminal`] so the Server can build
    /// the wire [`CancelFlowResult`] without reaching for a raw
    /// `Client`. Separate from the existing
    /// [`EngineBackend::cancel_flow`] entry point (which takes the
    /// enum-typed `(policy, wait)` split and returns the wait-collapsed
    /// `CancelFlowResult`) because the Server owns its own
    /// wait-dispatch + member-cancel machinery via
    /// [`EngineBackend::cancel_execution`] + backlog ack.
    ///
    /// Default impl returns [`EngineError::Unavailable`] so un-migrated
    /// backends surface the miss loudly.
    #[cfg(feature = "core")]
    async fn cancel_flow_header(
        &self,
        _args: CancelFlowArgs,
    ) -> Result<crate::contracts::CancelFlowHeader, EngineError> {
        Err(EngineError::Unavailable {
            op: "cancel_flow_header",
        })
    }

    /// RFC-017 Stage E2: best-effort acknowledgement that one member of
    /// a `cancel_all` flow has completed its per-member cancel. Drains
    /// the member from the flow's `pending_cancels` set and, if empty,
    /// removes the flow from the partition-level `cancel_backlog`
    /// (Valkey: `ff_ack_cancel_member` FCALL; Postgres: table write —
    /// default `Unavailable` until Wave 9).
    ///
    /// Failures are swallowed by the caller — the cancel-backlog
    /// reconciler is the authoritative drain — but a typed error here
    /// lets the caller log a backend-scoped context string.
    #[cfg(feature = "core")]
    async fn ack_cancel_member(
        &self,
        _flow_id: &FlowId,
        _execution_id: &ExecutionId,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "ack_cancel_member",
        })
    }

    /// RFC-017 Stage E2: full-shape execution read used by the
    /// `GET /v1/executions/{id}` HTTP route. Returns the legacy
    /// [`ExecutionInfo`] wire shape (not the decoupled
    /// [`ExecutionSnapshot`]) so the existing HTTP response bytes stay
    /// identical across the migration.
    ///
    /// `Ok(None)` ⇒ no such execution. Default `Unavailable` because
    /// the Valkey HGETALL-and-parse is backend-specific.
    #[cfg(feature = "core")]
    async fn read_execution_info(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionInfo>, EngineError> {
        Err(EngineError::Unavailable {
            op: "read_execution_info",
        })
    }

    /// RFC-017 Stage E2: narrow `public_state` read used by the
    /// `GET /v1/executions/{id}/state` HTTP route. Returns `Ok(None)`
    /// when the execution is missing. Default `Unavailable`.
    #[cfg(feature = "core")]
    async fn read_execution_state(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<PublicState>, EngineError> {
        Err(EngineError::Unavailable {
            op: "read_execution_state",
        })
    }

    // ── RFC-019 Stage A/B/C — Stream-cursor subscriptions ─────────
    //
    // Four owner-adjudicated families (RFC-019 §Open Questions #5):
    // `lease_history`, `completion`, `signal_delivery`,
    // `instance_tags`. Stage C (this crate) promotes each family to
    // a typed event enum; consumers `match` on variants instead of
    // parsing a backend-shaped byte blob.
    //
    // Each method returns a family-specific subscription alias (see
    // [`crate::stream_events`]). All defaults return
    // `EngineError::Unavailable` per RFC-017 trait-growth conventions.

    /// Subscribe to lease lifecycle events (acquired / renewed /
    /// expired / reclaimed / revoked) for the partition this backend
    /// is configured with.
    ///
    /// Cross-partition fan-out is consumer-side merge: subscribe
    /// per-partition backend instance and interleave on the read
    /// side. Yields
    /// `Err(EngineError::StreamDisconnected { cursor })` on backend
    /// disconnect; resume by calling this method again with the
    /// returned cursor.
    ///
    /// `filter` (#282): when `filter.instance_tag` is `Some((k, v))`,
    /// only events whose execution carries tag `k = v` are yielded
    /// (matching the [`crate::backend::ScannerFilter`] surface from
    /// #122). Pass `&ScannerFilter::default()` for unfiltered
    /// behaviour. Filtering happens inside the backend stream; the
    /// [`crate::stream_events::LeaseHistorySubscription`] return type
    /// is unchanged.
    async fn subscribe_lease_history(
        &self,
        _cursor: crate::stream_subscribe::StreamCursor,
        _filter: &crate::backend::ScannerFilter,
    ) -> Result<crate::stream_events::LeaseHistorySubscription, EngineError> {
        Err(EngineError::Unavailable {
            op: "subscribe_lease_history",
        })
    }

    /// Subscribe to completion events (terminal state transitions).
    ///
    /// - **Postgres**: wraps the `ff_completion_event` outbox +
    ///   LISTEN/NOTIFY machinery. Durable via event-id cursor.
    /// - **Valkey**: wraps the RESP3 `ff:dag:completions` pubsub
    ///   subscriber. Pubsub is at-most-once over the live
    ///   subscription window; the cursor is always the empty
    ///   sentinel. If you need at-least-once replay with durable
    ///   cursor resume, use the Postgres backend (see
    ///   `docs/POSTGRES_PARITY_MATRIX.md` row `subscribe_completion`).
    ///
    /// `filter` (#282): see [`Self::subscribe_lease_history`]. Valkey
    /// reuses the `subscribe_completions_filtered` per-event HGET
    /// gate; Postgres filters inline against the outbox's denormalised
    /// `instance_tag` column.
    async fn subscribe_completion(
        &self,
        _cursor: crate::stream_subscribe::StreamCursor,
        _filter: &crate::backend::ScannerFilter,
    ) -> Result<crate::stream_events::CompletionSubscription, EngineError> {
        Err(EngineError::Unavailable {
            op: "subscribe_completion",
        })
    }

    /// Subscribe to signal-delivery events (satisfied / buffered /
    /// deduped).
    ///
    /// `filter` (#282): see [`Self::subscribe_lease_history`].
    async fn subscribe_signal_delivery(
        &self,
        _cursor: crate::stream_subscribe::StreamCursor,
        _filter: &crate::backend::ScannerFilter,
    ) -> Result<crate::stream_events::SignalDeliverySubscription, EngineError> {
        Err(EngineError::Unavailable {
            op: "subscribe_signal_delivery",
        })
    }

    /// Subscribe to instance-tag events (tag attached / cleared).
    ///
    /// Producer wiring is deferred per #311 audit ("no concrete
    /// demand"); the trait method exists for API uniformity across
    /// the four families. Backends currently return
    /// `EngineError::Unavailable`.
    async fn subscribe_instance_tags(
        &self,
        _cursor: crate::stream_subscribe::StreamCursor,
    ) -> Result<crate::stream_events::InstanceTagSubscription, EngineError> {
        Err(EngineError::Unavailable {
            op: "subscribe_instance_tags",
        })
    }

    // ── PR-7b Cluster 1 — Foundation scanner operations ─────────
    //
    // Per-execution write hooks invoked by the engine scanner loop.
    // Scanner bodies discover candidate executions via partition
    // indices (Valkey ZSETs / PG index scans) and, for each candidate,
    // call through to one of the methods below to perform the atomic
    // state-flip. Defaults return `EngineError::Unavailable { op }`;
    // Valkey impls wrap the corresponding `ff_*` FCALL; Postgres impls
    // run a single-row tx mirroring the Lua semantic.

    /// Lease-expiry scanner hook — mark an expired lease as reclaimable
    /// so another worker can redeem the execution. Atomic per call.
    ///
    /// Valkey: `FCALL ff_mark_lease_expired_if_due`.
    /// Postgres: single-row tx on `ff_attempt` + `ff_exec_core` (see
    /// `ff_backend_postgres::reconcilers::lease_expiry`).
    async fn mark_lease_expired_if_due(
        &self,
        _partition: crate::partition::Partition,
        _execution_id: &ExecutionId,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "mark_lease_expired_if_due",
        })
    }

    /// Delayed-promoter scanner hook — promote a delayed execution to
    /// `eligible_now` once its `delay_until` has passed.
    ///
    /// Valkey: `FCALL ff_promote_delayed`.
    /// Postgres: Wave 9 schema scope (no current impl).
    async fn promote_delayed(
        &self,
        _partition: crate::partition::Partition,
        _lane: &LaneId,
        _execution_id: &ExecutionId,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "promote_delayed",
        })
    }

    /// Pending-waitpoint-expiry scanner hook — close a pending
    /// waitpoint whose deadline has passed (wake the suspended
    /// execution with a timeout signal).
    ///
    /// Valkey: `FCALL ff_close_waitpoint`.
    /// Postgres: Wave 9 schema scope (no current impl).
    async fn close_waitpoint(
        &self,
        _partition: crate::partition::Partition,
        _execution_id: &ExecutionId,
        _waitpoint_id: &str,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "close_waitpoint",
        })
    }

    /// Shared hook for the attempt-timeout and execution-deadline
    /// scanners — terminate or retry an execution whose wall-clock
    /// budget has elapsed. `phase` discriminates which of the two
    /// scanner paths is calling so the backend can preserve diagnostic
    /// breadcrumbs without forking the surface.
    ///
    /// Valkey: `FCALL ff_expire_execution` (with `phase` passed through
    /// as an ARGV discriminator).
    /// Postgres: single-row tx mirror of the Lua semantic for
    /// `AttemptTimeout`; `ExecutionDeadline` is Wave 9 schema scope.
    async fn expire_execution(
        &self,
        _partition: crate::partition::Partition,
        _execution_id: &ExecutionId,
        _phase: ExpirePhase,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "expire_execution",
        })
    }

    /// Suspension-timeout scanner hook — expire a suspended execution
    /// whose suspension deadline has passed (wake with timeout).
    ///
    /// Valkey: `FCALL ff_expire_suspension`.
    /// Postgres: single-row tx on `ff_suspend` + `ff_exec_core`.
    async fn expire_suspension(
        &self,
        _partition: crate::partition::Partition,
        _execution_id: &ExecutionId,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "expire_suspension",
        })
    }

    // ── PR-7b Cluster 2 — Reconciler scanner operations ─────────
    //
    // Per-execution write hooks invoked by the `unblock` scanner. The
    // other two cluster-2 scanners (`budget_reset`, `dependency_reconciler`)
    // route through `reset_budget` + `resolve_dependency`, which are
    // already part of the RFC-017 service-layer surface and live above.

    /// Unblock-scanner hook — move an execution from a blocked ZSET back
    /// to `eligible_now` once its blocking condition has cleared (budget
    /// under limit, quota window drained, or a capable worker has come
    /// online). `expected_blocking_reason` discriminates which of the
    /// `blocked:{budget,quota,route}` sets the execution is leaving and
    /// also fences against a stale unblock (Lua rejects if the core's
    /// `blocking_reason` no longer matches).
    ///
    /// # Backend status
    ///
    /// - **Valkey:** wraps `ff_unblock_execution` (RFC-010 §6). Atomic
    ///   single-slot FCALL on `{p:N}`.
    /// - **Postgres:** `Unavailable` (structural). PG does not persist a
    ///   per-reason `blocked:{budget,quota,route}` index — scheduler
    ///   eligibility is re-evaluated live via SQL predicates on
    ///   `ff_exec_core` + budget / quota tables (see
    ///   `ff_backend_postgres::scheduler`). Nothing to reconcile, so the
    ///   engine's PG scanner loop does not run this path; callers on
    ///   a PG deployment receive the typed `Unavailable` per PR-7b/final
    ///   contract.
    /// - **SQLite:** `Unavailable` for the same reason as Postgres.
    async fn unblock_execution(
        &self,
        _partition: crate::partition::Partition,
        _lane_id: &LaneId,
        _execution_id: &ExecutionId,
        _expected_blocking_reason: &str,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "unblock_execution",
        })
    }

    /// RFC-016 Stage C — sibling-cancel group drain.
    ///
    /// After `edge_cancel_dispatcher` has issued
    /// [`EngineBackend::cancel_execution`] against every listed
    /// sibling of a `(flow_id, downstream_eid)` group, this trait
    /// method atomically removes the tuple from the partition-local
    /// pending-cancel-groups index and clears the per-group flag +
    /// members state.
    ///
    /// Valkey: `FCALL ff_drain_sibling_cancel_group` (SREM +
    /// HDEL in one Lua unit).
    /// Postgres: `Unavailable` — PG's Wave-6b
    /// `reconcilers::edge_cancel_dispatcher::dispatcher_tick`
    /// owns the equivalent row drain inside its own per-group
    /// transaction; the Valkey-shaped per-tuple call is not used on
    /// the PG scanner path.
    /// SQLite: `Unavailable` (mirrors PG; RFC-023 scope).
    #[cfg(feature = "core")]
    async fn drain_sibling_cancel_group(
        &self,
        _flow_partition: crate::partition::Partition,
        _flow_id: &FlowId,
        _downstream_eid: &ExecutionId,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "drain_sibling_cancel_group",
        })
    }

    /// RFC-016 Stage D — sibling-cancel group reconcile (Invariant
    /// Q6 safety net).
    ///
    /// Re-examines a `(flow_id, downstream_eid)` tuple in the
    /// partition-local pending-cancel-groups index and returns one
    /// of three atomic dispositions — see
    /// [`SiblingCancelReconcileAction`] — so the crash-recovery
    /// reconciler can drain stale or completed tuples without
    /// fighting the Stage-C dispatcher.
    ///
    /// Valkey: `FCALL ff_reconcile_sibling_cancel_group`.
    /// Postgres: `Unavailable` — PG's
    /// `reconcilers::edge_cancel_reconciler::reconciler_tick`
    /// owns the row-level reconcile inside its own batched tx.
    /// SQLite: `Unavailable` (mirrors PG).
    #[cfg(feature = "core")]
    async fn reconcile_sibling_cancel_group(
        &self,
        _flow_partition: crate::partition::Partition,
        _flow_id: &FlowId,
        _downstream_eid: &ExecutionId,
    ) -> Result<SiblingCancelReconcileAction, EngineError> {
        Err(EngineError::Unavailable {
            op: "reconcile_sibling_cancel_group",
        })
    }
}

/// RFC-016 Stage D outcome of a single
/// [`EngineBackend::reconcile_sibling_cancel_group`] call.
///
/// Cardinality is intentionally bounded to three so the
/// `ff_sibling_cancel_reconcile_total{action}` metric label stays
/// closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiblingCancelReconcileAction {
    /// Pending tuple was stale (flag cleared or edgegroup absent);
    /// SREM'd / DELETE'd without touching the group.
    SremmedStale,
    /// Flag still set but every listed sibling is already terminal —
    /// an interrupted drain. Flag cleared + tuple drained.
    CompletedDrain,
    /// Flag set and at least one sibling non-terminal — dispatcher
    /// owns this tuple; left untouched.
    NoOp,
}

impl SiblingCancelReconcileAction {
    /// Short label for observability (matches the Lua + PG action
    /// strings exactly).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SremmedStale => "sremmed_stale",
            Self::CompletedDrain => "completed_drain",
            Self::NoOp => "no_op",
        }
    }
}

/// Which scanner invoked [`EngineBackend::expire_execution`].
///
/// The attempt-timeout and execution-deadline scanners share a single
/// trait method — their underlying state flip is identical (terminate
/// or retry per retry policy); the distinction is purely diagnostic
/// (which deadline elapsed) and carried through to the backend so the
/// same Lua / SQL path can log or tag appropriately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpirePhase {
    /// Invoked by `attempt_timeout` scanner — the per-attempt
    /// wall-clock budget elapsed.
    AttemptTimeout,
    /// Invoked by `execution_deadline` scanner — the whole-execution
    /// wall-clock deadline elapsed.
    ExecutionDeadline,
}

impl ExpirePhase {
    /// Short string tag suitable for Lua ARGV or Postgres breadcrumbs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AttemptTimeout => "attempt_timeout",
            Self::ExecutionDeadline => "execution_deadline",
        }
    }
}

/// Object-safety assertion: `dyn EngineBackend` compiles iff every
/// method is dyn-compatible. Kept as a compile-time guard so a future
/// trait change that accidentally breaks dyn-safety fails the build
/// at this site rather than at every downstream `Arc<dyn
/// EngineBackend>` use.
#[allow(dead_code)]
fn _assert_dyn_compatible(_: &dyn EngineBackend) {}

/// Polling interval for [`wait_for_flow_cancellation`]. Tight enough
/// that a local single-node cancel cascade observes `cancelled` within
/// one or two polls; slack enough that a `WaitIndefinite` caller does
/// not hammer `describe_flow` on a live cluster.
const CANCEL_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Defensive ceiling for [`CancelFlowWait::WaitIndefinite`] — if the
/// reconciler cascade has not converged in five minutes, something is
/// wedged and returning `Timeout` is strictly more useful than blocking
/// forever. RFC-012 §3.1.1 expects real-world cascades to finish within
/// `reconciler_interval + grace`, which is orders of magnitude below
/// this.
const CANCEL_WAIT_INDEFINITE_CEILING: Duration = Duration::from_secs(300);

/// Poll `backend.describe_flow(flow_id)` until `public_flow_state` is
/// `"cancelled"` or `deadline` elapses.
///
/// Shared by every backend's `cancel_flow` trait impl that honours
/// [`CancelFlowWait::WaitTimeout`] / [`CancelFlowWait::WaitIndefinite`].
/// The underlying `cancel_flow` FCALL / SQL transaction flips the
/// flow-level state synchronously; member cancellations dispatch
/// asynchronously via the reconciler, which also flips
/// `public_flow_state` to `cancelled` once the cascade completes. This
/// helper waits for that terminal flip.
///
/// Returns:
/// * `Ok(())` once `public_flow_state = "cancelled"` is observed.
/// * `Err(EngineError::Timeout { op: "cancel_flow", elapsed })` when
///   `deadline` elapses first. `elapsed` is the wait budget (the
///   requested timeout), not wall-clock precision.
/// * `Err(e)` if `describe_flow` itself errors (propagated).
pub async fn wait_for_flow_cancellation<B: EngineBackend + ?Sized>(
    backend: &B,
    flow_id: &crate::types::FlowId,
    deadline: Duration,
) -> Result<(), EngineError> {
    let start = std::time::Instant::now();
    loop {
        match backend.describe_flow(flow_id).await? {
            Some(snap) if snap.public_flow_state == "cancelled" => return Ok(()),
            // `None` (flow removed) is also terminal from the caller's
            // perspective — nothing left to wait on.
            None => return Ok(()),
            Some(_) => {}
        }
        if start.elapsed() >= deadline {
            return Err(EngineError::Timeout {
                op: "cancel_flow",
                elapsed: deadline,
            });
        }
        tokio::time::sleep(CANCEL_WAIT_POLL_INTERVAL).await;
    }
}

/// Convert a [`CancelFlowWait`] into the deadline passed to
/// [`wait_for_flow_cancellation`]. `NoWait` returns `None` — the caller
/// must skip the wait entirely.
pub fn cancel_flow_wait_deadline(wait: CancelFlowWait) -> Option<Duration> {
    // `CancelFlowWait` is `#[non_exhaustive]`; this match lives in the
    // defining crate so the exhaustiveness check keeps the compiler
    // honest. Future variants must be wired here explicitly.
    match wait {
        CancelFlowWait::NoWait => None,
        CancelFlowWait::WaitTimeout(d) => Some(d),
        CancelFlowWait::WaitIndefinite => Some(CANCEL_WAIT_INDEFINITE_CEILING),
    }
}

/// Validate a caller-namespaced tag key against the regex
/// `^[a-z][a-z0-9_]*\.[a-z0-9_][a-z0-9_.]*$`.
///
/// The Rust trait-side check is **stricter than the Valkey Lua
/// contracts** (`ff_set_execution_tags` / `ff_set_flow_tags`), which
/// only check `^[a-z][a-z0-9_]*%.[^.]` — namespace prefix + first
/// suffix char, with the rest of the suffix unvalidated. Every
/// backend-side impl (`ff-backend-{valkey,postgres,sqlite}`) calls
/// this helper **before** the wire hop so the effective parity
/// contract is this full-key regex; Valkey's Lua is an additional,
/// more permissive server-side guard, not the parity-of-record.
///
/// A key passes iff:
///
/// * it begins with an ASCII lowercase letter;
/// * all characters up to the first `.` are lowercase alnum or `_`;
/// * the first `.` is followed by at least one non-`.` character
///   (so `cairn.` and `cairn..x` fail — the `<field>` must be non-empty).
///
/// Shared entry point for [`EngineBackend::set_execution_tag`] /
/// [`EngineBackend::set_flow_tag`] / [`EngineBackend::get_execution_tag`] /
/// [`EngineBackend::get_flow_tag`] so every backend rejects the same
/// keyspace. On rejection, returns
/// [`EngineError::Validation { kind: ValidationKind::InvalidInput, .. }`](crate::engine_error::EngineError::Validation)
/// with the offending key in `detail`.
///
/// `#[allow(clippy::result_large_err)]` — `EngineError` is the uniform
/// error across the whole `EngineBackend` trait surface (see trait method
/// signatures above); boxing it here alone would introduce a
/// gratuitous signature deviation. Clippy 1.95 flags free functions but
/// not trait methods; this function mirrors the trait-method convention.
#[allow(clippy::result_large_err)]
pub fn validate_tag_key(key: &str) -> Result<(), EngineError> {
    use crate::engine_error::ValidationKind;

    let bad = || EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!(
            "invalid tag key: {key:?} (must match ^[a-z][a-z0-9_]*\\.[a-z0-9_][a-z0-9_.]*$)"
        ),
    };

    let mut chars = key.chars();
    let first = chars.next().ok_or_else(bad)?;
    if !first.is_ascii_lowercase() {
        return Err(bad());
    }
    // Phase 1: consume the namespace prefix up to (but not including) the first `.`.
    // Chars must be `[a-z0-9_]`.
    let mut saw_dot = false;
    for c in chars.by_ref() {
        if c == '.' {
            saw_dot = true;
            break;
        }
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(bad());
        }
    }
    if !saw_dot {
        return Err(bad());
    }
    // Phase 2: the first char after the first `.` must exist and be a
    // non-dot member char `[a-z0-9_]` (matches Valkey's Lua regex
    // `^[a-z][a-z0-9_]*%.[^.]` — a second consecutive dot is rejected).
    let second = chars.next().ok_or_else(bad)?;
    if !(second.is_ascii_lowercase() || second.is_ascii_digit() || second == '_') {
        return Err(bad());
    }
    // Phase 3 (Finding 2 tightening): every remaining char MUST be
    // `[a-z0-9_.]`. Before this, the suffix was unvalidated beyond
    // its first char — so `cairn.foo bar`, `cairn.Foo`,
    // `cairn.foo"bar`, `cairn.foo-bar` all passed trait-side validation
    // even though they would break SQLite JSON-path quoting, the Lua
    // HSET field-name conventions, or consumer grep patterns. Valkey's
    // Lua prefix regex is identically permissive on the suffix; this
    // tightens both layers from the Rust side. Dots in the suffix
    // remain legal (`app.sub.field` is valid) to preserve the existing
    // accepted shape.
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.') {
            return Err(bad());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zero-state backend stub used to exercise the default
    /// `capabilities()` impl without pulling in a real
    /// transport. Only the default method is under test here; every
    /// other method is unreachable on this type.
    struct DefaultBackend;

    #[async_trait]
    impl EngineBackend for DefaultBackend {
        async fn claim(
            &self,
            _lane: &LaneId,
            _capabilities: &CapabilitySet,
            _policy: ClaimPolicy,
        ) -> Result<Option<Handle>, EngineError> {
            unreachable!()
        }
        async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
            unreachable!()
        }
        async fn progress(
            &self,
            _handle: &Handle,
            _percent: Option<u8>,
            _message: Option<String>,
        ) -> Result<(), EngineError> {
            unreachable!()
        }
        async fn append_frame(
            &self,
            _handle: &Handle,
            _frame: Frame,
        ) -> Result<AppendFrameOutcome, EngineError> {
            unreachable!()
        }
        async fn complete(
            &self,
            _handle: &Handle,
            _payload: Option<Vec<u8>>,
        ) -> Result<(), EngineError> {
            unreachable!()
        }
        async fn fail(
            &self,
            _handle: &Handle,
            _reason: FailureReason,
            _classification: FailureClass,
        ) -> Result<FailOutcome, EngineError> {
            unreachable!()
        }
        async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
            unreachable!()
        }
        async fn suspend(
            &self,
            _handle: &Handle,
            _args: SuspendArgs,
        ) -> Result<SuspendOutcome, EngineError> {
            unreachable!()
        }
        async fn create_waitpoint(
            &self,
            _handle: &Handle,
            _waitpoint_key: &str,
            _expires_in: Duration,
        ) -> Result<PendingWaitpoint, EngineError> {
            unreachable!()
        }
        async fn observe_signals(
            &self,
            _handle: &Handle,
        ) -> Result<Vec<ResumeSignal>, EngineError> {
            unreachable!()
        }
        async fn claim_from_resume_grant(
            &self,
            _token: ResumeToken,
        ) -> Result<Option<Handle>, EngineError> {
            unreachable!()
        }
        async fn delay(
            &self,
            _handle: &Handle,
            _delay_until: TimestampMs,
        ) -> Result<(), EngineError> {
            unreachable!()
        }
        async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
            unreachable!()
        }
        async fn describe_execution(
            &self,
            _id: &ExecutionId,
        ) -> Result<Option<ExecutionSnapshot>, EngineError> {
            unreachable!()
        }
        async fn read_execution_context(
            &self,
            _execution_id: &ExecutionId,
        ) -> Result<ExecutionContext, EngineError> {
            Ok(ExecutionContext::new(
                Vec::new(),
                String::new(),
                std::collections::HashMap::new(),
            ))
        }
        async fn read_current_attempt_index(
            &self,
            _execution_id: &ExecutionId,
        ) -> Result<AttemptIndex, EngineError> {
            Ok(AttemptIndex::new(0))
        }
        async fn read_total_attempt_count(
            &self,
            _execution_id: &ExecutionId,
        ) -> Result<AttemptIndex, EngineError> {
            Ok(AttemptIndex::new(0))
        }
        async fn describe_flow(
            &self,
            _id: &FlowId,
        ) -> Result<Option<FlowSnapshot>, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn list_edges(
            &self,
            _flow_id: &FlowId,
            _direction: EdgeDirection,
        ) -> Result<Vec<EdgeSnapshot>, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn describe_edge(
            &self,
            _flow_id: &FlowId,
            _edge_id: &EdgeId,
        ) -> Result<Option<EdgeSnapshot>, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn resolve_execution_flow_id(
            &self,
            _eid: &ExecutionId,
        ) -> Result<Option<FlowId>, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn list_flows(
            &self,
            _partition: PartitionKey,
            _cursor: Option<FlowId>,
            _limit: usize,
        ) -> Result<ListFlowsPage, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn list_lanes(
            &self,
            _cursor: Option<LaneId>,
            _limit: usize,
        ) -> Result<ListLanesPage, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn list_suspended(
            &self,
            _partition: PartitionKey,
            _cursor: Option<ExecutionId>,
            _limit: usize,
        ) -> Result<ListSuspendedPage, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn list_executions(
            &self,
            _partition: PartitionKey,
            _cursor: Option<ExecutionId>,
            _limit: usize,
        ) -> Result<ListExecutionsPage, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn deliver_signal(
            &self,
            _args: DeliverSignalArgs,
        ) -> Result<DeliverSignalResult, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn claim_resumed_execution(
            &self,
            _args: ClaimResumedExecutionArgs,
        ) -> Result<ClaimResumedExecutionResult, EngineError> {
            unreachable!()
        }
        async fn cancel_flow(
            &self,
            _id: &FlowId,
            _policy: CancelFlowPolicy,
            _wait: CancelFlowWait,
        ) -> Result<CancelFlowResult, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "core")]
        async fn set_edge_group_policy(
            &self,
            _flow_id: &FlowId,
            _downstream_execution_id: &ExecutionId,
            _policy: crate::contracts::EdgeDependencyPolicy,
        ) -> Result<crate::contracts::SetEdgeGroupPolicyResult, EngineError> {
            unreachable!()
        }
        async fn report_usage(
            &self,
            _handle: &Handle,
            _budget: &BudgetId,
            _dimensions: crate::backend::UsageDimensions,
        ) -> Result<ReportUsageResult, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "streaming")]
        async fn read_stream(
            &self,
            _execution_id: &ExecutionId,
            _attempt_index: AttemptIndex,
            _from: StreamCursor,
            _to: StreamCursor,
            _count_limit: u64,
        ) -> Result<StreamFrames, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "streaming")]
        async fn tail_stream(
            &self,
            _execution_id: &ExecutionId,
            _attempt_index: AttemptIndex,
            _after: StreamCursor,
            _block_ms: u64,
            _count_limit: u64,
            _visibility: TailVisibility,
        ) -> Result<StreamFrames, EngineError> {
            unreachable!()
        }
        #[cfg(feature = "streaming")]
        async fn read_summary(
            &self,
            _execution_id: &ExecutionId,
            _attempt_index: AttemptIndex,
        ) -> Result<Option<SummaryDocument>, EngineError> {
            unreachable!()
        }
    }

    /// The default `capabilities()` impl returns a value tagged
    /// `family = "unknown"` with every `supports.*` bool false, so
    /// pre-RFC-018 out-of-tree backends keep compiling and consumers
    /// can distinguish "backend predates RFC-018" from "backend
    /// reports concrete bools." Every concrete in-tree backend
    /// overrides.
    #[test]
    fn default_capabilities_is_unknown_family_all_false() {
        let b = DefaultBackend;
        let caps = b.capabilities();
        assert_eq!(caps.identity.family, "unknown");
        assert_eq!(
            caps.identity.version,
            crate::capability::Version::new(0, 0, 0)
        );
        assert_eq!(caps.identity.rfc017_stage, "unknown");
        // Every field false on the default (matches `Supports::none()`).
        assert_eq!(caps.supports, crate::capability::Supports::none());
    }

    // ── resolve_dependency (PR-7b Step 0) ──

    /// Default impl returns `Unavailable { op: "resolve_dependency" }`
    /// so pre-migration backends surface a typed Unsupported-grade
    /// error rather than panic. Both cluster 2's dependency_reconciler
    /// and cluster 4's completion_listener route through this method;
    /// the default is the safety net for non-Valkey deployments.
    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_resolve_dependency_is_unavailable() {
        use crate::contracts::ResolveDependencyArgs;
        use crate::partition::{Partition, PartitionFamily};
        use crate::types::{AttemptIndex, EdgeId, FlowId, LaneId};

        let b = DefaultBackend;
        let partition = Partition {
            family: PartitionFamily::Flow,
            index: 0,
        };
        let args = ResolveDependencyArgs::new(
            partition,
            FlowId::parse("11111111-1111-1111-1111-111111111111").unwrap(),
            ExecutionId::parse("{fp:0}:22222222-2222-2222-2222-222222222222").unwrap(),
            ExecutionId::parse("{fp:0}:33333333-3333-3333-3333-333333333333").unwrap(),
            EdgeId::parse("44444444-4444-4444-4444-444444444444").unwrap(),
            LaneId::new("default"),
            AttemptIndex::new(0),
            "success".to_owned(),
            TimestampMs::now(),
        );
        match b.resolve_dependency(args).await {
            Err(EngineError::Unavailable { op }) => {
                assert_eq!(op, "resolve_dependency");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ── unblock_execution (PR-7b Cluster 2) ──

    /// Default impl returns `Unavailable { op: "unblock_execution" }`
    /// so non-Valkey deployments get a typed `Unavailable` rather than
    /// a panic. The engine scanner loop on PG/SQLite skips this path
    /// entirely (scheduler eligibility is re-evaluated live via SQL
    /// predicates), but a stray direct call must still fail gracefully.
    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_unblock_execution_is_unavailable() {
        use crate::partition::{Partition, PartitionFamily};

        let b = DefaultBackend;
        let partition = Partition {
            family: PartitionFamily::Execution,
            index: 0,
        };
        let eid = ExecutionId::parse(
            "{fp:0}:55555555-5555-5555-5555-555555555555",
        )
        .unwrap();
        let lane = LaneId::new("default");
        match b
            .unblock_execution(
                partition,
                &lane,
                &eid,
                "waiting_for_budget",
                TimestampMs::from_millis(0),
            )
            .await
        {
            Err(EngineError::Unavailable { op }) => {
                assert_eq!(op, "unblock_execution");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    // ── validate_tag_key (issue #433) ──

    #[test]
    fn validate_tag_key_accepts_valid() {
        for k in [
            "cairn.session_id",
            "cairn.project",
            "a.b",
            "a1_2.x",
            "app.sub.field",
            "x.y_z",
        ] {
            validate_tag_key(k).unwrap_or_else(|e| panic!("{k:?} should pass: {e:?}"));
        }
    }

    #[test]
    fn validate_tag_key_rejects_invalid() {
        for k in [
            "",                // empty
            "Cairn.x",         // uppercase first
            "1cairn.x",        // leading digit
            "cairn",           // no dot
            "cairn.",          // empty suffix
            "cairn..x",        // dot immediately after first dot
            ".cairn",          // leading dot
            "cair n.x",        // space before dot
            "ca-irn.x",        // hyphen in prefix
            // Finding 2 tightening — suffix now fully validated.
            "cairn.Foo",       // uppercase in suffix
            "cairn.foo bar",   // space in suffix
            "cairn.foo\"bar",  // double-quote in suffix (would break SQLite JSON-path quoting)
            "cairn.foo-bar",   // hyphen in suffix
            "cairn.foo\\bar",  // backslash in suffix
        ] {
            let err = validate_tag_key(k)
                .err()
                .unwrap_or_else(|| panic!("{k:?} should fail"));
            match err {
                EngineError::Validation {
                    kind: crate::engine_error::ValidationKind::InvalidInput,
                    ..
                } => {}
                other => panic!("{k:?}: unexpected err {other:?}"),
            }
        }
    }

    // ── cairn #389: service-layer FCALL trait-method defaults ──
    //
    // Each method MUST return `EngineError::Unavailable { op: "<name>" }`
    // on backends that haven't overridden it, so out-of-tree backends
    // keep compiling and consumers get a terminal-classified error
    // rather than a panic. Mirrors the precedent used by
    // `issue_reclaim_grant` / `reclaim_execution` / `suspend_by_triple`.
    //
    // Feature-gated on `core` because the methods under test only
    // exist on the trait under that feature — matches the precedent
    // of `default_resolve_dependency_is_unavailable` above.

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_complete_execution_is_unavailable() {
        use crate::contracts::CompleteExecutionArgs;
        use crate::types::{ExecutionId, FlowId};
        let b = DefaultBackend;
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let args = CompleteExecutionArgs {
            execution_id: eid,
            fence: None,
            attempt_index: AttemptIndex::new(0),
            result_payload: None,
            result_encoding: None,
            source: crate::types::CancelSource::default(),
            now: TimestampMs::from_millis(0),
        };
        match b.complete_execution(args).await.unwrap_err() {
            EngineError::Unavailable { op } => assert_eq!(op, "complete_execution"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_fail_execution_is_unavailable() {
        use crate::contracts::FailExecutionArgs;
        use crate::types::{ExecutionId, FlowId};
        let b = DefaultBackend;
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let args = FailExecutionArgs {
            execution_id: eid,
            fence: None,
            attempt_index: AttemptIndex::new(0),
            failure_reason: String::new(),
            failure_category: String::new(),
            retry_policy_json: String::new(),
            next_attempt_policy_json: String::new(),
            source: crate::types::CancelSource::default(),
        };
        match b.fail_execution(args).await.unwrap_err() {
            EngineError::Unavailable { op } => assert_eq!(op, "fail_execution"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_renew_lease_is_unavailable() {
        use crate::contracts::RenewLeaseArgs;
        use crate::types::{ExecutionId, FlowId};
        let b = DefaultBackend;
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let args = RenewLeaseArgs {
            execution_id: eid,
            attempt_index: AttemptIndex::new(0),
            fence: None,
            lease_ttl_ms: 1_000,
            lease_history_grace_ms: 60_000,
        };
        match b.renew_lease(args).await.unwrap_err() {
            EngineError::Unavailable { op } => assert_eq!(op, "renew_lease"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_resume_execution_is_unavailable() {
        use crate::contracts::ResumeExecutionArgs;
        use crate::types::{ExecutionId, FlowId};
        let b = DefaultBackend;
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let args = ResumeExecutionArgs {
            execution_id: eid,
            trigger_type: "signal".to_owned(),
            resume_delay_ms: 0,
        };
        match b.resume_execution(args).await.unwrap_err() {
            EngineError::Unavailable { op } => assert_eq!(op, "resume_execution"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_check_admission_is_unavailable() {
        use crate::contracts::CheckAdmissionArgs;
        use crate::types::{ExecutionId, FlowId, QuotaPolicyId};
        let b = DefaultBackend;
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let args = CheckAdmissionArgs {
            execution_id: eid,
            now: TimestampMs::from_millis(0),
            window_seconds: 60,
            rate_limit: 10,
            concurrency_cap: 1,
            jitter_ms: None,
        };
        let qid = QuotaPolicyId::new();
        match b
            .check_admission(&qid, "default", args)
            .await
            .unwrap_err()
        {
            EngineError::Unavailable { op } => assert_eq!(op, "check_admission"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn default_evaluate_flow_eligibility_is_unavailable() {
        use crate::contracts::EvaluateFlowEligibilityArgs;
        use crate::types::{ExecutionId, FlowId};
        let b = DefaultBackend;
        let config = crate::partition::PartitionConfig::default();
        let eid = ExecutionId::for_flow(&FlowId::new(), &config);
        let args = EvaluateFlowEligibilityArgs { execution_id: eid };
        match b.evaluate_flow_eligibility(args).await.unwrap_err() {
            EngineError::Unavailable { op } => assert_eq!(op, "evaluate_flow_eligibility"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }
}
