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
    PrepareOutcome, ReclaimToken, ResumeSignal, SummaryDocument, TailVisibility,
};
use crate::contracts::{
    CancelFlowResult, ExecutionSnapshot, FlowSnapshot, ReportUsageResult,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllResult, SeedOutcome,
    SeedWaitpointHmacSecretArgs, SuspendArgs, SuspendOutcome,
};
#[cfg(feature = "core")]
use crate::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, ApplyDependencyToChildArgs,
    ApplyDependencyToChildResult, BudgetStatus, CancelExecutionArgs, CancelExecutionResult,
    CancelFlowArgs, ChangePriorityArgs, ChangePriorityResult, ClaimForWorkerArgs,
    ClaimForWorkerOutcome, ClaimResumedExecutionArgs, ClaimResumedExecutionResult,
    CreateBudgetArgs, CreateBudgetResult, CreateExecutionArgs, CreateExecutionResult,
    CreateFlowArgs, CreateFlowResult, CreateQuotaPolicyArgs, CreateQuotaPolicyResult,
    DeliverSignalArgs, DeliverSignalResult, EdgeDirection, EdgeSnapshot, ExecutionInfo,
    ListExecutionsPage, ListFlowsPage, ListLanesPage, ListPendingWaitpointsArgs,
    ListPendingWaitpointsResult, ListSuspendedPage, ReplayExecutionArgs, ReplayExecutionResult,
    ReportUsageAdminArgs, ResetBudgetArgs, ResetBudgetResult, RevokeLeaseArgs, RevokeLeaseResult,
    StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
#[cfg(feature = "core")]
use crate::state::PublicState;
#[cfg(feature = "core")]
use crate::partition::PartitionKey;
#[cfg(feature = "streaming")]
use crate::contracts::{StreamCursor, StreamFrames};
use crate::engine_error::EngineError;
#[cfg(feature = "streaming")]
use crate::types::AttemptIndex;
#[cfg(feature = "core")]
use crate::types::EdgeId;
use crate::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};

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

    /// Non-mutating observation of signals that satisfied the handle's
    /// resume condition.
    async fn observe_signals(&self, handle: &Handle) -> Result<Vec<ResumeSignal>, EngineError>;

    /// Consume a reclaim grant to mint a resumed-kind handle. Returns
    /// `Ok(None)` when the grant's target execution is no longer
    /// resumable (already reclaimed, terminal, etc.).
    async fn claim_from_reclaim(&self, token: ReclaimToken) -> Result<Option<Handle>, EngineError>;

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

    /// Snapshot a flow by id. `Ok(None)` ⇒ no such flow.
    async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError>;

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
        flow_id: &FlowId,
        direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError>;

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
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError>;

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
        eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError>;

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
        partition: PartitionKey,
        cursor: Option<FlowId>,
        limit: usize,
    ) -> Result<ListFlowsPage, EngineError>;

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
        cursor: Option<LaneId>,
        limit: usize,
    ) -> Result<ListLanesPage, EngineError>;

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
        partition: PartitionKey,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListSuspendedPage, EngineError>;

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
        partition: PartitionKey,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListExecutionsPage, EngineError>;

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
        args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError>;

    /// Claim a resumed execution — a previously-suspended attempt that
    /// has cleared its resume condition (e.g. via
    /// [`Self::deliver_signal`]) and now needs a worker to pick up the
    /// same attempt index.
    ///
    /// Distinct from [`Self::claim`] (fresh work) and
    /// [`Self::claim_from_reclaim`] (grant-based ownership transfer
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
        args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError>;

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
        flow_id: &FlowId,
        downstream_execution_id: &ExecutionId,
        policy: crate::contracts::EdgeDependencyPolicy,
    ) -> Result<crate::contracts::SetEdgeGroupPolicyResult, EngineError>;

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
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from: StreamCursor,
        to: StreamCursor,
        count_limit: u64,
    ) -> Result<StreamFrames, EngineError>;

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
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        after: StreamCursor,
        block_ms: u64,
        count_limit: u64,
        visibility: TailVisibility,
    ) -> Result<StreamFrames, EngineError>;

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
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError>;

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

}

/// Object-safety assertion: `dyn EngineBackend` compiles iff every
/// method is dyn-compatible. Kept as a compile-time guard so a future
/// trait change that accidentally breaks dyn-safety fails the build
/// at this site rather than at every downstream `Arc<dyn
/// EngineBackend>` use.
#[allow(dead_code)]
fn _assert_dyn_compatible(_: &dyn EngineBackend) {}
