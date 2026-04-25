use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferriskey::{Client, Value};
use ff_core::backend::{Handle, HandleKind, PendingWaitpoint};
use ff_core::contracts::ReportUsageResult;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::StateKind;
use ff_script::error::ScriptError;
use ff_core::partition::PartitionConfig;
use ff_core::types::*;
use tokio::sync::{Notify, OwnedSemaphorePermit};
use tokio::task::JoinHandle;

use crate::SdkError;

// ── Phase 3: Suspend/Signal types ──

// RFC-013 Stage 1d — `TimeoutBehavior`, `SuspendOutcome`,
// `ResumeCondition`, `SignalMatcher`, `ResumePolicy`,
// `SuspensionReasonCode`, `SuspensionRequester`, `WaitpointBinding`,
// `SuspendArgs`, `SuspendOutcomeDetails`, `IdempotencyKey`,
// `CompositeBody` all live in `ff_core::contracts`. The `SuspendOutcome`
// re-export preserves the `ff_sdk::SuspendOutcome` path; the other
// paths are new.
pub use ff_core::contracts::{
    CompositeBody, CountKind, IdempotencyKey, ResumeCondition, ResumePolicy, ResumeTarget,
    SignalMatcher, SuspendArgs, SuspendOutcome, SuspendOutcomeDetails, SuspensionReasonCode,
    SuspensionRequester, TimeoutBehavior, WaitpointBinding,
};

/// RFC-013 Stage 1d — cookie returned by the strict `suspend` wrapper.
///
/// The returned `handle` is a `HandleKind::Suspended` cookie the caller
/// uses for `observe_signals` and the eventual re-claim via
/// `claim_resumed_execution`. Consuming `ClaimedTask::suspend`
/// invalidates the pre-suspend `ClaimedTask` (already moved by the
/// `self: ClaimedTask` receiver); this handle is the only live cookie
/// left on the caller side.
#[derive(Debug)]
pub struct SuspendedHandle {
    pub handle: Handle,
    pub details: SuspendOutcomeDetails,
}

/// RFC-013 Stage 1d — outcome of the fallible `try_suspend` wrapper.
///
/// Unlike the strict wrapper, `AlreadySatisfied` hands the `ClaimedTask`
/// back to the caller so work continues on the retained lease.
#[allow(clippy::large_enum_variant)]
pub enum TrySuspendOutcome {
    Suspended(SuspendedHandle),
    AlreadySatisfied {
        task: ClaimedTask,
        details: SuspendOutcomeDetails,
    },
}

impl std::fmt::Debug for TrySuspendOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Suspended(h) => f.debug_tuple("Suspended").field(h).finish(),
            Self::AlreadySatisfied { details, .. } => f
                .debug_struct("AlreadySatisfied")
                .field("details", details)
                .finish_non_exhaustive(),
        }
    }
}

/// A signal to deliver to a suspended execution's waitpoint.
#[derive(Clone, Debug)]
pub struct Signal {
    pub signal_name: String,
    pub signal_category: String,
    pub payload: Option<Vec<u8>>,
    pub source_type: String,
    pub source_identity: String,
    pub idempotency_key: Option<String>,
    /// HMAC token issued when the waitpoint was created. Required for
    /// authenticated signal delivery (RFC-004 §Waitpoint Security).
    pub waitpoint_token: WaitpointToken,
}

/// Outcome of `deliver_signal()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalOutcome {
    /// Signal accepted, appended to waitpoint but condition not yet satisfied.
    Accepted { signal_id: SignalId, effect: String },
    /// Signal triggered resume — execution is now runnable.
    TriggeredResume { signal_id: SignalId },
    /// Duplicate signal (idempotency key matched).
    Duplicate { existing_signal_id: String },
}

impl SignalOutcome {
    /// Parse a raw `ff_deliver_signal` FCALL result into a `SignalOutcome`.
    ///
    /// Consuming packages that call `ff_deliver_signal` directly can use this
    /// to interpret the Lua return value without depending on SDK internals.
    pub fn from_fcall_value(raw: &Value) -> Result<Self, SdkError> {
        parse_signal_result(raw)
    }
}

/// A signal that triggered a resume, readable by a worker after re-claim.
///
/// **RFC-012 Stage 0:** the canonical definition has moved to
/// [`ff_core::backend::ResumeSignal`]. This `pub use` shim preserves the
/// `ff_sdk::task::ResumeSignal` path (and, via `ff_sdk::lib`'s re-export,
/// `ff_sdk::ResumeSignal`) through the 0.4.x window. The shim is
/// scheduled for removal in 0.5.0.
pub use ff_core::backend::ResumeSignal;

/// Outcome of `append_frame()`.
///
/// **RFC-012 §R7.2.1:** canonical definition moved to
/// [`ff_core::backend::AppendFrameOutcome`]; this `pub use` shim
/// preserves the `ff_sdk::task::AppendFrameOutcome` path through the
/// 0.4.x window. Existing consumers construct + match exhaustively
/// without change. The derive set widened from `Clone, Debug` to
/// `Clone, Debug, PartialEq, Eq` matching the `FailOutcome` precedent.
pub use ff_core::backend::AppendFrameOutcome;

/// Outcome of a `fail()` call.
///
/// **RFC-012 Stage 1a:** canonical definition moved to
/// [`ff_core::backend::FailOutcome`]; this `pub use` shim preserves
/// the `ff_sdk::task::FailOutcome` path (and the `ff_sdk::FailOutcome`
/// re-export in `lib.rs`) through the 0.4.x window. Existing
/// consumers construct + match exhaustively without change.
pub use ff_core::backend::FailOutcome;

/// A claimed execution with an active lease. The worker processes this task
/// and must call one of `complete()`, `fail()`, or `cancel()` when done.
///
/// The lease is automatically renewed in the background at `lease_ttl / 3`
/// intervals. Renewal stops when the task is consumed or dropped.
///
/// `complete`, `fail`, and `cancel` consume `self` — this prevents
/// double-complete bugs at the type level.
pub struct ClaimedTask {
    /// Shared Valkey client.
    #[allow(dead_code)]
    client: Client,
    /// `EngineBackend` the trait-migrated ops forward through.
    ///
    /// **RFC-012 Stage 1b + Round-7.** Today this is always a
    /// `ValkeyBackend` wrapping `client`. Most per-task ops
    /// (`complete`/`fail`/`cancel`/`delay_execution`/
    /// `move_to_waiting_children`/`update_progress`/`resume_signals`/
    /// `create_pending_waitpoint`/`append_frame`/`report_usage`) route
    /// through `backend.*()`. `suspend` still uses
    /// `client.fcall("ff_suspend_execution", ...)` directly — this is
    /// the deferred suspend per RFC-012 §R7.6.1, pending Stage 1d
    /// input-shape work. Lease renewal goes through
    /// `backend.renew(&handle)`. Round-7 (#135/#145) closed the four
    /// trait-shape gaps tracked by #117.
    ///
    /// Stage 1c migrates the FlowFabricWorker hot paths (claim,
    /// deliver_signal) through the same trait surface; Stage 1d
    /// refactors this struct to carry a single `Handle` rather than
    /// synthesising one per op via `synth_handle`.
    backend: Arc<dyn EngineBackend>,
    /// Partition config for key construction.
    #[allow(dead_code)]
    partition_config: PartitionConfig,
    /// Execution identity.
    execution_id: ExecutionId,
    /// Current attempt.
    attempt_index: AttemptIndex,
    attempt_id: AttemptId,
    /// Lease identity.
    lease_id: LeaseId,
    lease_epoch: LeaseEpoch,
    /// Lease timing.
    lease_ttl_ms: u64,
    /// Lane used at claim time.
    lane_id: LaneId,
    /// Worker instance that holds this lease (for index cleanup keys).
    worker_instance_id: WorkerInstanceId,
    /// Execution data.
    input_payload: Vec<u8>,
    execution_kind: String,
    tags: HashMap<String, String>,
    /// Background renewal task handle.
    renewal_handle: JoinHandle<()>,
    /// Signal to stop renewal (used before consuming self).
    renewal_stop: Arc<Notify>,
    /// Consecutive lease renewal failures. Shared with the background renewal
    /// task. Reset to 0 on each successful renewal. Workers should check
    /// `is_lease_healthy()` before committing expensive side effects.
    renewal_failures: Arc<AtomicU32>,
    /// Set to `true` by `stop_renewal()` after a terminal op's FCALL
    /// response is received, just before `self` is consumed into `Drop`.
    /// `Drop` reads this instead of `renewal_handle.is_finished()` to
    /// suppress the false-positive "dropped without terminal operation"
    /// warning: after `notify_one`, the renewal task has not yet been
    /// polled by the runtime, so `is_finished()` is still `false` on the
    /// happy path when self is being consumed.
    ///
    /// Note: the flag is set for any terminal-op path that reaches
    /// `stop_renewal()`, which includes Lua-level script errors (the
    /// FCALL returned a `{0, "error", ...}` payload). That is intentional:
    /// the caller already receives the `Err` via the op's return value,
    /// so an additional `Drop` warning would be noise. The warning is
    /// reserved for genuine drop-without-terminal-op cases (panic, early
    /// return, transport failure before stop_renewal ran).
    terminal_op_called: AtomicBool,
    /// Concurrency permit from the worker's semaphore. Held for the lifetime
    /// of the task; released on complete/fail/cancel/drop.
    _concurrency_permit: Option<OwnedSemaphorePermit>,
}

impl ClaimedTask {
    /// Construct a `ClaimedTask` from the results of a successful
    /// `ff_claim_execution` or `ff_claim_resumed_execution` FCALL.
    ///
    /// # Arguments
    ///
    /// * `client` — shared Valkey client used for subsequent lease
    ///   renewals, signal delivery, and the final
    ///   complete/fail/cancel FCALL.
    /// * `partition_config` — partition topology snapshot read at
    ///   `FlowFabricWorker::connect`. Used for key construction on
    ///   the lifetime of this task.
    /// * `execution_id` — the claimed execution's UUID.
    /// * `attempt_index` / `attempt_id` — current attempt identity.
    ///   `attempt_index` is 0 on a fresh claim, preserved on a
    ///   resumed claim.
    /// * `lease_id` / `lease_epoch` — lease identity. `lease_epoch`
    ///   is returned by the Lua FCALL and bumped on each resumed
    ///   claim.
    /// * `lease_ttl_ms` — lease TTL used to schedule the background
    ///   renewal task (renews at `lease_ttl_ms / 3`).
    /// * `lane_id` — the lane the task was claimed on. Used for
    ///   index key construction (`lane_active`, `lease_expiry`,
    ///   etc.).
    /// * `worker_instance_id` — this worker's identity. Used to
    ///   resolve `worker_leases` index entries during renewal and
    ///   completion.
    /// * `input_payload` / `execution_kind` / `tags` — pre-read
    ///   execution metadata, exposed via getters so the worker
    ///   doesn't round-trip to Valkey to inspect what it just
    ///   claimed.
    ///
    /// # Invariant: constructor is `pub(crate)` on purpose
    ///
    /// **External callers cannot construct a `ClaimedTask`** — only
    /// the in-crate claim entry points (`claim_next`,
    /// `claim_from_grant`, `claim_from_reclaim_grant`) may. This is
    /// load-bearing: those entry points are the ONLY sites that
    /// acquire a permit from the worker's concurrency semaphore
    /// (via `FlowFabricWorker::concurrency_semaphore`) and attach
    /// it through `ClaimedTask::set_concurrency_permit` before
    /// returning the task. Promoting `new` to `pub` would let
    /// consumers build tasks that bypass the concurrency contract
    /// the worker's `max_concurrent_tasks` config advertises — the
    /// returned task would run alongside other in-flight work
    /// without debiting the permit bank, and the
    /// complete/fail/cancel/drop path would have nothing to
    /// release.
    ///
    /// If an external callsite genuinely needs to rehydrate a
    /// task from saved FCALL results, the right answer is a new
    /// `FlowFabricWorker` entry point that wraps `new` + acquires a
    /// permit — not promoting this constructor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Client,
        backend: Arc<dyn EngineBackend>,
        partition_config: PartitionConfig,
        execution_id: ExecutionId,
        attempt_index: AttemptIndex,
        attempt_id: AttemptId,
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        lease_ttl_ms: u64,
        lane_id: LaneId,
        worker_instance_id: WorkerInstanceId,
        input_payload: Vec<u8>,
        execution_kind: String,
        tags: HashMap<String, String>,
    ) -> Self {
        let renewal_stop = Arc::new(Notify::new());
        let renewal_failures = Arc::new(AtomicU32::new(0));

        // Stage 1b: the renewal task forwards through `backend.renew(&handle)`.
        // Build the handle once at construction time and hand both Arc clones
        // into the spawned task.
        let renewal_handle_cookie = ff_backend_valkey::ValkeyBackend::encode_handle(
            execution_id.clone(),
            attempt_index,
            attempt_id.clone(),
            lease_id.clone(),
            lease_epoch,
            lease_ttl_ms,
            lane_id.clone(),
            worker_instance_id.clone(),
            HandleKind::Fresh,
        );
        let renewal_handle = spawn_renewal_task(
            backend.clone(),
            renewal_handle_cookie,
            execution_id.clone(),
            lease_ttl_ms,
            renewal_stop.clone(),
            renewal_failures.clone(),
        );

        Self {
            client,
            backend,
            partition_config,
            execution_id,
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            lease_ttl_ms,
            lane_id,
            worker_instance_id,
            input_payload,
            execution_kind,
            tags,
            renewal_handle,
            renewal_stop,
            renewal_failures,
            terminal_op_called: AtomicBool::new(false),
            _concurrency_permit: None,
        }
    }

    /// Attach a concurrency permit from the worker's semaphore.
    /// The permit is held for the task's lifetime and released on drop.
    #[allow(dead_code)]
    pub(crate) fn set_concurrency_permit(&mut self, permit: OwnedSemaphorePermit) {
        self._concurrency_permit = Some(permit);
    }

    // ── Accessors ──

    pub fn execution_id(&self) -> &ExecutionId {
        &self.execution_id
    }

    pub fn attempt_index(&self) -> AttemptIndex {
        self.attempt_index
    }

    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    pub fn lease_epoch(&self) -> LeaseEpoch {
        self.lease_epoch
    }

    pub fn input_payload(&self) -> &[u8] {
        &self.input_payload
    }

    pub fn execution_kind(&self) -> &str {
        &self.execution_kind
    }

    pub fn tags(&self) -> &HashMap<String, String> {
        &self.tags
    }

    pub fn lane_id(&self) -> &LaneId {
        &self.lane_id
    }

    /// Read frames from this task's attempt stream.
    ///
    /// Forwards through the task's `Arc<dyn EngineBackend>` — consumers
    /// no longer need the `ferriskey::Client` leak (#87). See
    /// [`EngineBackend::read_stream`](ff_core::engine_backend::EngineBackend::read_stream)
    /// for the backend-level contract.
    ///
    /// Validation (count_limit bounds) runs at this boundary — out-of-range
    /// input surfaces as [`SdkError::Config`] before reaching the backend.
    pub async fn read_stream(
        &self,
        from: StreamCursor,
        to: StreamCursor,
        count_limit: u64,
    ) -> Result<StreamFrames, SdkError> {
        validate_stream_read_count(count_limit)?;
        Ok(self
            .backend
            .read_stream(&self.execution_id, self.attempt_index, from, to, count_limit)
            .await?)
    }

    /// Tail this task's attempt stream for new frames.
    ///
    /// See [`EngineBackend::tail_stream`](ff_core::engine_backend::EngineBackend::tail_stream)
    /// for the head-of-line warning — the task's backend is shared
    /// with claim/complete/fail hot paths. Consumers that need
    /// isolation should build a dedicated `EngineBackend` for tail
    /// reads.
    pub async fn tail_stream(
        &self,
        after: StreamCursor,
        block_ms: u64,
        count_limit: u64,
    ) -> Result<StreamFrames, SdkError> {
        self.tail_stream_with_visibility(
            after,
            block_ms,
            count_limit,
            ff_core::backend::TailVisibility::All,
        )
        .await
    }

    /// Tail with an explicit [`TailVisibility`](ff_core::backend::TailVisibility)
    /// filter (RFC-015 §6). Use [`TailVisibility::ExcludeBestEffort`]
    /// (ff_core::backend::TailVisibility::ExcludeBestEffort) to drop
    /// [`StreamMode::BestEffortLive`](ff_core::backend::StreamMode::BestEffortLive)
    /// frames server-side.
    pub async fn tail_stream_with_visibility(
        &self,
        after: StreamCursor,
        block_ms: u64,
        count_limit: u64,
        visibility: ff_core::backend::TailVisibility,
    ) -> Result<StreamFrames, SdkError> {
        if block_ms > MAX_TAIL_BLOCK_MS {
            return Err(SdkError::Config {
                context: "tail_stream".into(),
                field: Some("block_ms".into()),
                message: format!("exceeds {MAX_TAIL_BLOCK_MS}ms ceiling"),
            });
        }
        validate_stream_read_count(count_limit)?;
        validate_tail_cursor(&after)?;
        Ok(self
            .backend
            .tail_stream(
                &self.execution_id,
                self.attempt_index,
                after,
                block_ms,
                count_limit,
                visibility,
            )
            .await?)
    }

    /// Synthesise a Valkey-backend `Handle` encoding this task's
    /// attempt-cookie fields. Private to the SDK — the trait
    /// forwarders call this per op to produce the `&Handle` argument
    /// the `EngineBackend` methods take.
    ///
    /// **RFC-012 Stage 1b option A.** Refactoring `ClaimedTask` to
    /// carry one cached `Handle` instead of synthesising per op is
    /// deferred to Stage 1d (issue #89 follow-up). The per-op cost is
    /// a single allocation of a small byte buffer — negligible
    /// compared to the FCALL round-trip that follows.
    ///
    /// Kind is always `HandleKind::Fresh` because today the 9
    /// Stage-1b ops all treat the backend tag + opaque bytes as
    /// authoritative; they do not dispatch on kind. Stage 1d routes
    /// resumed-claim callers through a `Resumed` synth.
    fn synth_handle(&self) -> Handle {
        ff_backend_valkey::ValkeyBackend::encode_handle(
            self.execution_id.clone(),
            self.attempt_index,
            self.attempt_id.clone(),
            self.lease_id.clone(),
            self.lease_epoch,
            self.lease_ttl_ms,
            self.lane_id.clone(),
            self.worker_instance_id.clone(),
            HandleKind::Fresh,
        )
    }

    /// Check if the lease is likely still valid based on renewal success.
    ///
    /// Returns `false` if 3 or more consecutive renewal attempts have failed.
    /// Workers should check this before committing expensive or irreversible
    /// side effects. A `false` return means Valkey may have already expired
    /// the lease and another worker could be processing this execution.
    pub fn is_lease_healthy(&self) -> bool {
        self.renewal_failures.load(Ordering::Relaxed) < 3
    }

    /// Number of consecutive lease renewal failures since the last success.
    ///
    /// Returns 0 when renewals are working normally. Useful for observability
    /// and custom health policies beyond the default threshold of 3.
    pub fn consecutive_renewal_failures(&self) -> u32 {
        self.renewal_failures.load(Ordering::Relaxed)
    }

    // ── Terminal operations (consume self) ──

    /// Delay the execution until `delay_until`.
    ///
    /// Releases the lease. The execution moves to `delayed` state.
    /// Consumes self — the task cannot be used after delay.
    pub async fn delay_execution(self, delay_until: TimestampMs) -> Result<(), SdkError> {
        // RFC-012 Stage 1b: forwards through `backend.delay`. Matches
        // the pre-migration `stop_renewal` contract: called only when
        // the FCALL round-tripped (Ok or a typed Lua/engine error);
        // raw transport errors bubble up without stopping renewal so
        // the `Drop` warning still fires if the caller later drops
        // `self` without retrying.
        let handle = self.synth_handle();
        let out = self.backend.delay(&handle, delay_until).await;
        if fcall_landed(&out) {
            self.stop_renewal();
        }
        out.map_err(SdkError::from)
    }

    /// Move execution to waiting_children state.
    ///
    /// Releases the lease. The execution waits for child dependencies to complete.
    /// Consumes self.
    pub async fn move_to_waiting_children(self) -> Result<(), SdkError> {
        // RFC-012 Stage 1b: forwards through `backend.wait_children`.
        // See `delay_execution` for the stop_renewal contract.
        let handle = self.synth_handle();
        let out = self.backend.wait_children(&handle).await;
        if fcall_landed(&out) {
            self.stop_renewal();
        }
        out.map_err(SdkError::from)
    }

    /// Complete the execution successfully.
    ///
    /// Calls `ff_complete_execution` via FCALL, then stops lease renewal.
    /// Renewal continues during the FCALL to prevent lease expiry under
    /// network latency — the Lua fences on lease_id+epoch atomically.
    /// Consumes self — the task cannot be used after completion.
    ///
    /// # Connection errors and replay
    ///
    /// If the Valkey connection drops after the Lua commit but before the
    /// client reads the response, retrying `complete()` with the same
    /// `ClaimedTask` (same `lease_epoch` + `attempt_id`) is safe: the SDK
    /// reconciles the "already terminal with matching outcome" response
    /// into `Ok(())`. A retry after a different terminal op has raced in
    /// (e.g. operator cancel) surfaces as `ExecutionNotActive` with the
    /// populated `terminal_outcome` so the caller can see what actually
    /// happened.
    pub async fn complete(self, result_payload: Option<Vec<u8>>) -> Result<(), SdkError> {
        // RFC-012 Stage 1b: forwards through `backend.complete`.
        // The FCALL body + the `ExecutionNotActive` replay reconciliation
        // (match same epoch + attempt_id + outcome=="success" → Ok)
        // moved to `ff_backend_valkey::complete_impl`.
        let handle = self.synth_handle();
        let out = self.backend.complete(&handle, result_payload).await;
        if fcall_landed(&out) {
            self.stop_renewal();
        }
        out.map_err(SdkError::from)
    }

    /// Fail the execution with a reason and error category.
    ///
    /// If the execution policy allows retries, the engine schedules a retry
    /// (delayed backoff). Otherwise, the execution becomes terminal failed.
    /// Returns [`FailOutcome`] so the caller knows what happened.
    ///
    /// # Connection errors and replay
    ///
    /// `fail()` is replay-safe under the same conditions as `complete()`:
    /// a retry by the same caller matching `lease_epoch` + `attempt_id` is
    /// reconciled into the outcome the server actually committed
    /// (`TerminalFailed` if no retries left; `RetryScheduled` with
    /// `delay_until = 0` if a retry was scheduled — the exact delay is
    /// not recovered on the replay path).
    pub async fn fail(
        self,
        reason: &str,
        error_category: &str,
    ) -> Result<FailOutcome, SdkError> {
        // RFC-012 Stage 1b: forwards through `backend.fail`.
        // The FCALL body + retry-policy read + the two-shape replay
        // reconciliation (terminal/failed → TerminalFailed, runnable
        // → RetryScheduled{0}) moved to
        // `ff_backend_valkey::fail_impl`.
        //
        // **Preserving the caller's raw category string.** The
        // pre-Stage-1b code passed `error_category` straight to the
        // Lua `failure_category` ARGV. Real callers use arbitrary
        // lower_snake_case strings (`"lease_stale"`, `"bad_signal"`,
        // `"inference_error"`, …) that do not correspond to the 5
        // named `FailureClass` variants. A lossy enum conversion
        // would rewrite every non-canonical category to
        // `Transient` — a real behavior change for
        // ff-readiness-tests + the examples crates.
        //
        // Instead: pack the raw category bytes into
        // `FailureReason.detail`; `fail_impl` reads them back and
        // threads them to Lua verbatim. The `classification` enum
        // is derived from the string for the few consumers who
        // match on the trait return today (none in-workspace).
        // Stage 1d (or issue #117) widens `FailureClass` with a
        // `Custom(String)` arm; this stash carrier retires then.
        let handle = self.synth_handle();
        let failure_reason = ff_core::backend::FailureReason::with_detail(
            reason.to_owned(),
            error_category.as_bytes().to_vec(),
        );
        let classification = error_category_to_class(error_category);
        let out = self.backend.fail(&handle, failure_reason, classification).await;
        if fcall_landed(&out) {
            self.stop_renewal();
        }
        out.map_err(SdkError::from)
    }

    /// Cancel the execution.
    ///
    /// Stops lease renewal, then calls `ff_cancel_execution` via FCALL.
    /// Consumes self.
    ///
    /// # Connection errors and replay
    ///
    /// `cancel()` is replay-safe under the same conditions as `complete()`:
    /// a retry by the same caller matching `lease_epoch` + `attempt_id`
    /// returns `Ok(())` if the server's stored `terminal_outcome` is
    /// `cancelled`. A retry that finds a different outcome (because a
    /// concurrent `complete()` or `fail()` won the race) surfaces as
    /// `ExecutionNotActive` with the populated `terminal_outcome` so the
    /// caller can see that the cancel intent was NOT honored.
    pub async fn cancel(self, reason: &str) -> Result<(), SdkError> {
        // RFC-012 Stage 1b: forwards through `backend.cancel`.
        // The 21-KEYS FCALL body, the `current_waitpoint_id` pre-read,
        // and the `ExecutionNotActive` → outcome=="cancelled" replay
        // reconciliation all moved to `ff_backend_valkey::cancel_impl`.
        let handle = self.synth_handle();
        let out = self.backend.cancel(&handle, reason).await;
        if fcall_landed(&out) {
            self.stop_renewal();
        }
        out.map_err(SdkError::from)
    }

    // ── Non-terminal operations ──

    /// Manually renew the lease.
    ///
    /// **RFC-012 Stage 1b.** Forwards through the `EngineBackend`
    /// trait. The background renewal task calls
    /// `backend.renew(&handle)` directly (see `spawn_renewal_task`);
    /// this method is the public entry point for workers that want
    /// to force a renew out-of-band.
    pub async fn renew_lease(&self) -> Result<(), SdkError> {
        let handle = self.synth_handle();
        self.backend
            .renew(&handle)
            .await
            .map(|_renewal| ())
            .map_err(SdkError::from)
    }

    /// Update progress (pct 0-100 and optional message).
    ///
    /// **RFC-012 Stage 1b.** Forwards through the `EngineBackend`
    /// trait (`backend.progress(&handle, Some(pct), Some(msg))`).
    ///
    /// # Not for stream frames
    ///
    /// `update_progress` writes the `progress_percent` / `progress_message`
    /// fields on `exec_core` (the execution's state hash). It is a
    /// scalar heartbeat — each call overwrites the previous value and
    /// nothing is appended to the output stream. Producers emitting
    /// stream frames (arbitrary `frame_type` + payload, consumed via
    /// `ClaimedTask::read_stream` / `tail_stream` or the HTTP
    /// stream-tail routes) MUST use [`Self::append_frame`] instead;
    /// `update_progress` is invisible to stream consumers.
    pub async fn update_progress(&self, pct: u8, message: &str) -> Result<(), SdkError> {
        let handle = self.synth_handle();
        self.backend
            .progress(&handle, Some(pct), Some(message.to_owned()))
            .await
            .map_err(SdkError::from)
    }

    /// Report usage against a budget and check limits.
    ///
    /// Non-consuming — the worker can report usage multiple times.
    /// `dimensions` is a slice of `(dimension_name, delta)` pairs.
    /// `dedup_key` prevents double-counting on retries (auto-prefixed with budget hash tag).
    ///
    /// **RFC-012 §R7.2.3:** forwards through
    /// [`EngineBackend::report_usage`](ff_core::engine_backend::EngineBackend::report_usage).
    /// The trait's `UsageDimensions.dedup_key` carries the raw key;
    /// the backend impl applies the `usage_dedup_key(hash_tag, k)`
    /// wrap so the dedup state co-locates with the budget partition
    /// (PR #108).
    pub async fn report_usage(
        &self,
        budget_id: &BudgetId,
        dimensions: &[(&str, u64)],
        dedup_key: Option<&str>,
    ) -> Result<ReportUsageResult, SdkError> {
        let handle = self.synth_handle();
        let mut dims = ff_core::backend::UsageDimensions::default();
        for (name, delta) in dimensions {
            dims.custom.insert((*name).to_owned(), *delta);
        }
        dims.dedup_key = dedup_key
            .filter(|k| !k.is_empty())
            .map(|k| k.to_owned());
        self.backend
            .report_usage(&handle, budget_id, dims)
            .await
            .map_err(SdkError::from)
    }

    /// Create a pending waitpoint for future signal delivery.
    ///
    /// Non-consuming — the worker keeps the lease. Signals delivered to the
    /// waitpoint are buffered. When the worker later calls `suspend()` with
    /// `use_pending_waitpoint`, buffered signals may immediately satisfy the
    /// resume condition.
    ///
    /// Returns both the waitpoint_id AND the HMAC token required by external
    /// callers to buffer signals against this pending waitpoint
    /// (RFC-004 §Waitpoint Security).
    ///
    /// **RFC-012 §R7.2.2:** forwards through
    /// [`EngineBackend::create_waitpoint`](ff_core::engine_backend::EngineBackend::create_waitpoint).
    /// The trait returns
    /// [`PendingWaitpoint`](ff_core::backend::PendingWaitpoint) whose
    /// `hmac_token` is the same wire HMAC this method has always
    /// produced; the SDK unwraps it back to the historical
    /// `(WaitpointId, WaitpointToken)` tuple for caller-shape parity.
    pub async fn create_pending_waitpoint(
        &self,
        waitpoint_key: &str,
        expires_in_ms: u64,
    ) -> Result<(WaitpointId, WaitpointToken), SdkError> {
        let handle = self.synth_handle();
        let expires_in = std::time::Duration::from_millis(expires_in_ms);
        let pending = self
            .backend
            .create_waitpoint(&handle, waitpoint_key, expires_in)
            .await
            .map_err(SdkError::from)?;
        // `WaitpointHmac` wraps the canonical `WaitpointToken`; the
        // `.token()` accessor borrows the underlying
        // `WaitpointToken`, which is the caller's historical return
        // shape.
        Ok((pending.waitpoint_id, pending.hmac_token.token().clone()))
    }

    // ── Phase 4: Streaming ──

    /// Append a frame to the current attempt's output stream.
    ///
    /// Non-consuming — the worker can append many frames during execution.
    /// The stream is created lazily on the first append.
    ///
    /// **RFC-012 §R7.2.1 / PR #146:** forwards through the
    /// `EngineBackend` trait. The free-form `frame_type` tag and
    /// optional `metadata` (wire `correlation_id`) travel on the
    /// extended [`ff_core::backend::Frame`] shape (`frame_type:
    /// String`, `correlation_id: Option<String>`), giving byte-for-byte
    /// wire parity with the pre-migration direct-FCALL path.
    ///
    /// # `append_frame` vs [`Self::update_progress`]
    ///
    /// Stream-frame producers (arbitrary `frame_type` + payload
    /// consumed via `ClaimedTask::read_stream` / `tail_stream` or the
    /// HTTP stream-tail routes) MUST use `append_frame`.
    /// `update_progress` writes scalar `progress_percent` /
    /// `progress_message` fields to `exec_core` and is invisible to
    /// stream consumers.
    pub async fn append_frame(
        &self,
        frame_type: &str,
        payload: &[u8],
        metadata: Option<&str>,
    ) -> Result<AppendFrameOutcome, SdkError> {
        self.append_frame_with_mode(
            frame_type,
            payload,
            metadata,
            ff_core::backend::StreamMode::Durable,
        )
        .await
    }

    /// Append a frame under an explicit RFC-015
    /// [`StreamMode`](ff_core::backend::StreamMode).
    /// Defaults via [`Self::append_frame`] preserve pre-015 behaviour
    /// ([`StreamMode::Durable`](ff_core::backend::StreamMode::Durable)).
    pub async fn append_frame_with_mode(
        &self,
        frame_type: &str,
        payload: &[u8],
        metadata: Option<&str>,
        mode: ff_core::backend::StreamMode,
    ) -> Result<AppendFrameOutcome, SdkError> {
        let handle = self.synth_handle();
        let mut frame = ff_core::backend::Frame::new(
            payload.to_vec(),
            ff_core::backend::FrameKind::Event,
        )
        .with_frame_type(frame_type)
        .with_mode(mode);
        if let Some(cid) = metadata {
            frame = frame.with_correlation_id(cid);
        }
        self.backend
            .append_frame(&handle, frame)
            .await
            .map_err(SdkError::from)
    }

    // ── RFC-013 Stage 1d: Suspend ──

    /// **Strict suspend.** Consumes `self` and yields a
    /// [`SuspendedHandle`] or errors. On the early-satisfied path
    /// (buffered signals already matched the condition), returns
    /// `EngineError::State(AlreadySatisfied)` — use `try_suspend` when
    /// that branch is part of the flow's happy path.
    ///
    /// RFC-013 §5.1 — this is the classic Rust `foo` / `try_foo`
    /// split; `suspend` is the unconditional form. Defaults
    /// `WaitpointBinding::fresh()` for the waitpoint. For pending
    /// waitpoints previously issued via `create_pending_waitpoint`,
    /// use `try_suspend_on_pending`.
    pub async fn suspend(
        self,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
        resume_policy: ResumePolicy,
    ) -> Result<SuspendedHandle, SdkError> {
        let outcome = self
            .try_suspend_inner(WaitpointBinding::fresh(), reason_code, resume_condition, timeout, resume_policy)
            .await?;
        match outcome {
            TrySuspendOutcome::Suspended(h) => Ok(h),
            TrySuspendOutcome::AlreadySatisfied { .. } => Err(SdkError::from(
                crate::EngineError::State(StateKind::AlreadySatisfied),
            )),
        }
    }

    /// **Fallible suspend.** On the early-satisfied path, the
    /// `ClaimedTask` is handed back unchanged so the worker can
    /// continue running against the retained lease.
    ///
    /// RFC-013 §5.1 — use this when consuming a pending waitpoint whose
    /// buffered signals may already match the condition.
    pub async fn try_suspend(
        self,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
        resume_policy: ResumePolicy,
    ) -> Result<TrySuspendOutcome, SdkError> {
        self.try_suspend_inner(WaitpointBinding::fresh(), reason_code, resume_condition, timeout, resume_policy)
            .await
    }

    /// Convenience: `try_suspend` against a pending waitpoint previously
    /// issued via `create_pending_waitpoint`.
    pub async fn try_suspend_on_pending(
        self,
        pending: &PendingWaitpoint,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
        resume_policy: ResumePolicy,
    ) -> Result<TrySuspendOutcome, SdkError> {
        self.try_suspend_inner(
            WaitpointBinding::use_pending(pending),
            reason_code,
            resume_condition,
            timeout,
            resume_policy,
        )
        .await
    }

    async fn try_suspend_inner(
        self,
        waitpoint: WaitpointBinding,
        reason_code: SuspensionReasonCode,
        resume_condition: ResumeCondition,
        timeout: Option<(TimestampMs, TimeoutBehavior)>,
        resume_policy: ResumePolicy,
    ) -> Result<TrySuspendOutcome, SdkError> {
        let handle = self.synth_handle();
        let (timeout_at, timeout_behavior) = match timeout {
            Some((at, b)) => (Some(at), b),
            None => (None, TimeoutBehavior::Fail),
        };
        // If the caller passed `ResumeCondition::Single { waitpoint_key }`
        // alongside the default `WaitpointBinding::fresh()` (which mints a
        // random internal key), rebind the Fresh binding to use the
        // condition's key so RFC-013 §2.4 "waitpoint_key cross-field
        // invariant" is honored. UsePending retains its own binding.
        // RFC-014: for `Composite`, rebind to the first waitpoint_key
        // observed in the composite tree (Single.waitpoint_key or
        // Count.waitpoints[0]); consumers using single-waitpoint
        // composites must keep all Single/Count waitpoint_keys equal
        // (see RFC-014 single-waitpoint scoping).
        let waitpoint = match (&waitpoint, &resume_condition) {
            (
                WaitpointBinding::Fresh { waitpoint_id, .. },
                ResumeCondition::Single { waitpoint_key, .. },
            ) => WaitpointBinding::Fresh {
                waitpoint_id: waitpoint_id.clone(),
                waitpoint_key: waitpoint_key.clone(),
            },
            (
                WaitpointBinding::Fresh { waitpoint_id, .. },
                ResumeCondition::Composite(body),
            ) => {
                if let Some(key) = composite_first_waitpoint_key(body) {
                    WaitpointBinding::Fresh {
                        waitpoint_id: waitpoint_id.clone(),
                        waitpoint_key: key,
                    }
                } else {
                    waitpoint
                }
            }
            _ => waitpoint,
        };
        let mut args = SuspendArgs::new(
            SuspensionId::new(),
            waitpoint,
            resume_condition,
            resume_policy,
            reason_code,
            TimestampMs::now(),
        )
        .with_requester(SuspensionRequester::Worker);
        if let Some(at) = timeout_at {
            args = args.with_timeout(at, timeout_behavior);
        }

        let outcome = self
            .backend
            .suspend(&handle, args)
            .await
            .map_err(SdkError::from)?;
        match outcome {
            SuspendOutcome::Suspended { details, handle: new_handle } => {
                // Renewal is stopped only AFTER a successful Suspended
                // outcome. The suspended-kind handle is what the caller
                // will later hand to `observe_signals` / `claim_from_reclaim`.
                self.stop_renewal();
                Ok(TrySuspendOutcome::Suspended(SuspendedHandle {
                    handle: new_handle,
                    details,
                }))
            }
            SuspendOutcome::AlreadySatisfied { details } => {
                // Lease is retained Lua-side; keep the ClaimedTask alive
                // so the worker continues against the existing lease.
                Ok(TrySuspendOutcome::AlreadySatisfied {
                    task: self,
                    details,
                })
            }
            _ => Err(SdkError::from(ScriptError::Parse {
                fcall: "try_suspend_inner".into(),
                execution_id: None,
                message: "unexpected SuspendOutcome variant".into(),
            })),
        }
    }

    /// Read the signals that satisfied the waitpoint and triggered this
    /// resume.
    ///
    /// Non-consuming. Intended to be called immediately after re-claim via
    /// [`crate::FlowFabricWorker::claim_from_reclaim_grant`], before any
    /// subsequent `suspend()` (which replaces `suspension:current`).
    ///
    /// Returns `Ok(vec![])` when this claim is NOT a signal-resume:
    ///
    /// - No prior suspension on this execution.
    /// - The prior suspension belonged to an earlier attempt (e.g. the
    ///   attempt was cancelled/failed and a retry is now claiming).
    /// - The prior suspension was closed by timeout / cancel / operator
    ///   override rather than by a matched signal.
    ///
    /// Reads `suspension:current` once, filters by `attempt_index` to
    /// guard against stale prior-attempt records, then fetches the matched
    /// `signal_id` set from `waitpoint_condition`'s `matcher:N:signal_id`
    /// fields and reads each signal's metadata + payload directly.
    pub async fn resume_signals(&self) -> Result<Vec<ResumeSignal>, SdkError> {
        // RFC-012 Stage 1b: forwards through `backend.observe_signals`.
        // Pre-migration body (HGETALL suspension_current + HMGET
        // matchers + pipelined HGETALL signal_hash / GET
        // signal_payload) lives in
        // `ff_backend_valkey::observe_signals_impl`.
        let handle = self.synth_handle();
        self.backend
            .observe_signals(&handle)
            .await
            .map_err(SdkError::from)
    }

    /// Signal the renewal task to stop. Called by every terminal op
    /// (`complete`/`fail`/`cancel`/`suspend`/`delay_execution`/
    /// `move_to_waiting_children`) after the FCALL returns. Also marks
    /// `terminal_op_called` so the `Drop` impl can distinguish happy-path
    /// consumption from a genuine drop-without-terminal-op.
    fn stop_renewal(&self) {
        self.terminal_op_called.store(true, Ordering::Release);
        self.renewal_stop.notify_one();
    }

}

/// True iff the backend's FCALL result represents a round-trip that
/// reached the Lua side. `Ok(_)` and typed engine errors (validation,
/// contention, conflict, state, bug) all count as "landed" — the
/// server either committed or rejected with a typed response. Only
/// raw `Transport` errors (connection drops, request timeouts, parse
/// failures) count as "did not land", which matches the pre-Stage-1b
/// SDK's `fcall(...).await.map_err(SdkError::from)?` short-circuit
/// — those errors returned before `stop_renewal()` ran, preserving
/// the `Drop` warning for genuine "lease will leak" cases.
///
/// Stage 1b terminal-op forwarders use this predicate to decide
/// whether to call `stop_renewal()`: yes for landed responses, no
/// for transport errors so the caller's retry path still sees a
/// running renewal task.
fn fcall_landed<T>(r: &Result<T, crate::EngineError>) -> bool {
    match r {
        Ok(_) => true,
        Err(crate::EngineError::Transport { .. }) => false,
        Err(_) => true,
    }
}

/// Map the SDK's free-form `error_category: &str` to the typed
/// `FailureClass` the trait's `fail` method takes. Unknown categories
/// fall through to `Transient` — the Lua side already tolerated
/// arbitrary strings, so the worst a category drift does under the
/// Stage 1b forwarder is reclassify a novel category as transient.
/// Stage 1d (or issue #117) widens `FailureClass` with a
/// `Custom(String)` arm for exact round-trip.
fn error_category_to_class(s: &str) -> ff_core::backend::FailureClass {
    use ff_core::backend::FailureClass;
    match s {
        "transient" => FailureClass::Transient,
        "permanent" => FailureClass::Permanent,
        "infra_crash" => FailureClass::InfraCrash,
        "timeout" => FailureClass::Timeout,
        "cancelled" => FailureClass::Cancelled,
        _ => FailureClass::Transient,
    }
}

impl Drop for ClaimedTask {
    fn drop(&mut self) {
        // Abort the background renewal task on drop.
        // This is a safety net — complete/fail/cancel already stop renewal
        // via notify before consuming self. But if the task is dropped
        // without being consumed (e.g., panic), abort prevents leaked renewals.
        //
        // Why check `terminal_op_called` instead of `renewal_handle.is_finished()`:
        // on the happy path, `stop_renewal()` fires `notify_one` synchronously
        // and then self is consumed into Drop immediately. The renewal task
        // has not yet been polled by the runtime, so `is_finished()` is still
        // `false` here — which previously fired the warning on every
        // complete/fail/cancel/suspend call. `terminal_op_called` is the
        // authoritative signal that a terminal-op path ran to the point of
        // stopping renewal; it does not by itself certify the Lua side
        // succeeded (see the field doc). The caller surfaces any error via
        // the op's return value, so a `Drop` warning is unneeded there.
        if !self.terminal_op_called.load(Ordering::Acquire) {
            tracing::warn!(
                execution_id = %self.execution_id,
                "ClaimedTask dropped without terminal operation — lease will expire"
            );
        }
        self.renewal_handle.abort();
    }
}

// ── Lease renewal ──

/// Per-tick renewal: single `backend.renew(&handle)` call wrapped in
/// the `renew_lease` tracing span so bench harnesses' on_enter / on_exit
/// hooks still see one span per renewal (restores PR #119 Cursor
/// Bugbot finding — the top-level `spawn_renewal_task` fires once at
/// construction time, not per-tick).
///
/// See `benches/harness/src/bin/long_running.rs` for the bench
/// consumer that depends on this span naming.
#[tracing::instrument(
    name = "renew_lease",
    skip_all,
    fields(execution_id = %execution_id)
)]
async fn renew_once(
    backend: &dyn EngineBackend,
    handle: &Handle,
    execution_id: &ExecutionId,
) -> Result<(), crate::EngineError> {
    backend.renew(handle).await.map(|_| ())
}

/// Spawn a background tokio task that renews the lease at `ttl / 3`
/// intervals.
///
/// **RFC-012 Stage 1b.** The renewal loop now forwards through the
/// `EngineBackend` trait (`backend.renew(&handle)`) instead of calling
/// `ff_renew_lease` via a direct FCALL. The Stage-1a `renew_lease_inner`
/// free function was deleted; this task holds an `Arc<dyn EngineBackend>`
/// + the encoded `Handle` instead. Per-tick tracing lives on
///   [`renew_once`]; this function itself is sync + one-shot.
///
/// Stops when:
/// - `stop_signal` is notified (complete/fail/cancel called)
/// - Renewal fails with a terminal error (stale_lease, lease_expired, etc.)
/// - The task handle is aborted (ClaimedTask dropped)
fn spawn_renewal_task(
    backend: Arc<dyn EngineBackend>,
    handle: Handle,
    execution_id: ExecutionId,
    lease_ttl_ms: u64,
    stop_signal: Arc<Notify>,
    failure_counter: Arc<AtomicU32>,
) -> JoinHandle<()> {
    // Clamp to ≥1ms so `tokio::time::interval(Duration::ZERO)` never
    // panics if a caller (or a misconfigured test) passes a
    // lease_ttl_ms < 3. The SDK config validator already enforces
    // `lease_ttl_ms >= 1_000` for healthy deployments, but the clamp
    // is a cheap belt-and-suspenders (Copilot review finding on
    // PR #119).
    let interval = Duration::from_millis((lease_ttl_ms / 3).max(1));

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first immediate tick — the lease was just acquired.
        tick.tick().await;

        loop {
            tokio::select! {
                _ = stop_signal.notified() => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        "lease renewal stopped by signal"
                    );
                    return;
                }
                _ = tick.tick() => {
                    match renew_once(backend.as_ref(), &handle, &execution_id).await {
                        Ok(_renewal) => {
                            failure_counter.store(0, Ordering::Relaxed);
                            tracing::trace!(
                                execution_id = %execution_id,
                                "lease renewed"
                            );
                        }
                        Err(e) if is_terminal_renewal_error(&e) => {
                            failure_counter.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %e,
                                "lease renewal failed with terminal error, stopping renewal"
                            );
                            return;
                        }
                        Err(e) => {
                            let count = failure_counter.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %e,
                                consecutive_failures = count,
                                "lease renewal failed (will retry next interval)"
                            );
                        }
                    }
                }
            }
        }
    })
}

/// Check if an engine error means renewal should stop permanently.
#[allow(dead_code)]
fn is_terminal_renewal_error(err: &crate::EngineError) -> bool {
    use crate::{ContentionKind, EngineError, StateKind};
    matches!(
        err,
        EngineError::State(
            StateKind::StaleLease | StateKind::LeaseExpired | StateKind::LeaseRevoked
        ) | EngineError::Contention(ContentionKind::ExecutionNotActive { .. })
            | EngineError::NotFound { entity: "execution" }
    )
}

// ── FCALL result parsing ──

/// Parse the wire-format result of the `ff_report_usage_and_check` Lua
/// function into a typed [`ReportUsageResult`].
///
/// Standard format: `{1, "OK"}`, `{1, "SOFT_BREACH", dim, current, limit}`,
///                  `{1, "HARD_BREACH", dim, current, limit}`, `{1, "ALREADY_APPLIED"}`.
/// Status code `!= 1` is parsed as a [`ScriptError`] via
/// [`ScriptError::from_code_with_detail`].
///
/// Exposed as `pub` so downstream SDKs that speak the same wire format
/// — notably cairn-fabric's `budget_service::parse_spend_result` — can
/// call this directly instead of re-implementing the parse. Keeping one
/// parser paired with the producer (the Lua function registered at
/// `lua/budget.lua:99`, `ff_report_usage_and_check`) is the defence
/// against silent format drift between producer and consumer.
pub fn parse_report_usage_result(raw: &Value) -> Result<ReportUsageResult, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_report_usage_result".into(),
                execution_id: None,
                message: "ff_report_usage_and_check: expected Array".into(),
            }));
        }
    };
    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_report_usage_result".into(),
                execution_id: None,
                message: "ff_report_usage_and_check: expected Int status code".into(),
            }));
        }
    };
    if status_code != 1 {
        let error_code = usage_field_str(arr, 1);
        let detail = usage_field_str(arr, 2);
        return Err(SdkError::from(
            ScriptError::from_code_with_detail(&error_code, &detail).unwrap_or_else(|| {
                ScriptError::Parse {
                    fcall: "parse_report_usage_result".into(),
                    execution_id: None,
                    message: format!("ff_report_usage_and_check: {error_code}"),
                }
            }),
        ));
    }
    let sub_status = usage_field_str(arr, 1);
    match sub_status.as_str() {
        "OK" => Ok(ReportUsageResult::Ok),
        "ALREADY_APPLIED" => Ok(ReportUsageResult::AlreadyApplied),
        "SOFT_BREACH" => {
            let dim = usage_field_str(arr, 2);
            let current = parse_usage_u64(arr, 3, "SOFT_BREACH", "current_usage")?;
            let limit = parse_usage_u64(arr, 4, "SOFT_BREACH", "soft_limit")?;
            Ok(ReportUsageResult::SoftBreach { dimension: dim, current_usage: current, soft_limit: limit })
        }
        "HARD_BREACH" => {
            let dim = usage_field_str(arr, 2);
            let current = parse_usage_u64(arr, 3, "HARD_BREACH", "current_usage")?;
            let limit = parse_usage_u64(arr, 4, "HARD_BREACH", "hard_limit")?;
            Ok(ReportUsageResult::HardBreach {
                dimension: dim,
                current_usage: current,
                hard_limit: limit,
            })
        }
        _ => Err(SdkError::from(ScriptError::Parse {
            fcall: "parse_report_usage_result".into(),
            execution_id: None,
            message: format!(
            "ff_report_usage_and_check: unknown sub-status: {sub_status}"
        ),
        })),
    }
}

fn usage_field_str(arr: &[Result<Value, ferriskey::Error>], index: usize) -> String {
    match arr.get(index) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => String::new(),
    }
}

/// Parse a required numeric usage field (u64) from the wire array at
/// `index`. Returns `Err(ScriptError::Parse)` if the slot is missing,
/// holds a non-string/non-int value, or contains a string that does
/// not parse as u64.
///
/// Rationale: the Lua producer (`lua/budget.lua:99`,
/// `ff_report_usage_and_check`) always emits
/// `tostring(current_usage)` / `tostring(soft_or_hard_limit)` for
/// SOFT_BREACH/HARD_BREACH, never an empty slot. A missing or
/// non-numeric value here means the Lua and Rust sides drifted;
/// silently coercing to `0` would surface drift as "zero-usage breach"
/// — arithmetically correct but semantically nonsense. Fail loudly
/// instead so drift shows up as a parse error at the first call site.
fn parse_usage_u64(
    arr: &[Result<Value, ferriskey::Error>],
    index: usize,
    sub_status: &str,
    field_name: &str,
) -> Result<u64, SdkError> {
    match arr.get(index) {
        Some(Ok(Value::Int(n))) => {
            u64::try_from(*n).map_err(|_| {
                SdkError::from(ScriptError::Parse {
                    fcall: "parse_usage_u64".into(),
                    execution_id: None,
                    message: format!(
                    "ff_report_usage_and_check {sub_status}: {field_name} \
                     (index {index}) negative int {n} cannot be u64"
                ),
                })
            })
        }
        Some(Ok(Value::BulkString(b))) => {
            let s = String::from_utf8_lossy(b);
            s.parse::<u64>().map_err(|_| {
                SdkError::from(ScriptError::Parse {
                    fcall: "parse_usage_u64".into(),
                    execution_id: None,
                    message: format!(
                    "ff_report_usage_and_check {sub_status}: {field_name} \
                     (index {index}) not a u64 string: {s:?}"
                ),
                })
            })
        }
        Some(Ok(Value::SimpleString(s))) => s.parse::<u64>().map_err(|_| {
            SdkError::from(ScriptError::Parse {
                fcall: "parse_usage_u64".into(),
                execution_id: None,
                message: format!(
                "ff_report_usage_and_check {sub_status}: {field_name} \
                 (index {index}) not a u64 string: {s:?}"
            ),
            })
        }),
        Some(_) => Err(SdkError::from(ScriptError::Parse {
            fcall: "parse_usage_u64".into(),
            execution_id: None,
            message: format!(
            "ff_report_usage_and_check {sub_status}: {field_name} \
             (index {index}) wrong wire type (expected Int or String)"
        ),
        })),
        None => Err(SdkError::from(ScriptError::Parse {
            fcall: "parse_usage_u64".into(),
            execution_id: None,
            message: format!(
            "ff_report_usage_and_check {sub_status}: {field_name} \
             (index {index}) missing from response"
        ),
        })),
    }
}

/// Pure helper: decide whether `suspension:current` represents a
/// signal-driven resume for the currently-claimed attempt, and extract
/// the waitpoint_id if so. Returns `Ok(None)` for every non-match case
/// (no record, stale prior-attempt, non-resumed close). Returns an error
/// only for a present-but-malformed waitpoint_id, which indicates a Lua
/// bug rather than a missing-data case.
///
/// RFC-012 Stage 1b: `ClaimedTask::resume_signals` now forwards through
/// `EngineBackend::observe_signals`, which re-implements this invariant
/// inside `ff_backend_valkey`. The SDK helper is retained with its unit
/// tests so the parsing contract stays exercised at the SDK layer —
/// Stage 1d will consolidate (either promote the helper into ff-core
/// or drop these tests once the backend-side tests cover equivalent
/// ground).
#[allow(dead_code)]
fn resume_waitpoint_id_from_suspension(
    susp: &HashMap<String, String>,
    claimed_attempt: AttemptIndex,
) -> Result<Option<WaitpointId>, SdkError> {
    if susp.is_empty() {
        return Ok(None);
    }
    let susp_att: u32 = susp
        .get("attempt_index")
        .and_then(|s| s.parse().ok())
        .unwrap_or(u32::MAX);
    if susp_att != claimed_attempt.0 {
        return Ok(None);
    }
    let close_reason = susp.get("close_reason").map(String::as_str).unwrap_or("");
    if close_reason != "resumed" {
        return Ok(None);
    }
    let wp_id_str = susp
        .get("waitpoint_id")
        .map(String::as_str)
        .unwrap_or_default();
    if wp_id_str.is_empty() {
        return Ok(None);
    }
    let waitpoint_id = WaitpointId::parse(wp_id_str).map_err(|e| {
        SdkError::from(ScriptError::Parse {
            fcall: "resume_waitpoint_id_from_suspension".into(),
            execution_id: None,
            message: format!(
            "resume_signals: suspension_current.waitpoint_id is not a valid UUID: {e}"
        ),
        })
    })?;
    Ok(Some(waitpoint_id))
}

#[cfg_attr(not(feature = "direct-valkey-claim"), allow(dead_code))]
pub(crate) fn parse_success_result(raw: &Value, function_name: &str) -> Result<(), SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_success_result".into(),
                execution_id: None,
                message: format!(
                "{function_name}: expected Array, got non-array"
            ),
            }));
        }
    };

    if arr.is_empty() {
        return Err(SdkError::from(ScriptError::Parse {
            fcall: "parse_success_result".into(),
            execution_id: None,
            message: format!(
            "{function_name}: empty result array"
        ),
        }));
    }

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_success_result".into(),
                execution_id: None,
                message: format!(
                "{function_name}: expected Int at index 0"
            ),
            }));
        }
    };

    if status_code == 1 {
        Ok(())
    } else {
        // Extract error code from index 1 and optional detail from index 2
        // (e.g. `capability_mismatch` ships missing tokens there). Variants
        // that carry a String payload pick the detail up via
        // `from_code_with_detail`; other variants ignore it.
        let field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        // Collect all detail slots (idx >= 2). Most variants only read
        // slot 0; ExecutionNotActive consumes slots 0..=3 (terminal_outcome,
        // lease_epoch, lifecycle_phase, attempt_id) for terminal-op replay
        // reconciliation after a network drop.
        let details: Vec<String> = (2..arr.len()).map(field_str).collect();
        let detail_refs: Vec<&str> = details.iter().map(|s| s.as_str()).collect();

        let script_err = ScriptError::from_code_with_details(&error_code, &detail_refs)
            .unwrap_or_else(|| {
                ScriptError::Parse {
                    fcall: "parse_success_result".into(),
                    execution_id: None,
                    message: format!("{function_name}: unknown error: {error_code}"),
                }
            });

        Err(SdkError::from(script_err))
    }
}

// RFC-013 Stage 1d — `parse_suspend_result` removed. The backend impl
// in `ff_backend_valkey` now parses the Lua return and produces a typed
// `SuspendOutcome`; the SDK forwarder consumes that directly.

/// Parse ff_deliver_signal result:
///   ok(signal_id, effect)
///   ok_duplicate(existing_signal_id)
pub(crate) fn parse_signal_result(raw: &Value) -> Result<SignalOutcome, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_signal_result".into(),
                execution_id: None,
                message: "ff_deliver_signal: expected Array".into(),
            }));
        }
    };

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_signal_result".into(),
                execution_id: None,
                message: "ff_deliver_signal: bad status code".into(),
            }));
        }
    };

    if status_code != 1 {
        let err_field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = err_field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        let detail = err_field_str(2);
        return Err(SdkError::from(
            ScriptError::from_code_with_detail(&error_code, &detail).unwrap_or_else(|| {
                ScriptError::Parse {
                    fcall: "parse_signal_result".into(),
                    execution_id: None,
                    message: format!("ff_deliver_signal: {error_code}"),
                }
            }),
        ));
    }

    let sub_status = arr
        .get(1)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    if sub_status == "DUPLICATE" {
        let existing_id = arr
            .get(2)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        return Ok(SignalOutcome::Duplicate {
            existing_signal_id: existing_id,
        });
    }

    // Parse: {1, "OK", signal_id, effect}
    let signal_id_str = arr
        .get(2)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let effect = arr
        .get(3)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let signal_id = SignalId::parse(&signal_id_str).map_err(|e| {
        SdkError::from(ScriptError::Parse {
            fcall: "parse_signal_result".into(),
            execution_id: None,
            message: format!(
            "ff_deliver_signal: invalid signal_id from Lua: {e}"
        ),
        })
    })?;

    if effect == "resume_condition_satisfied" {
        Ok(SignalOutcome::TriggeredResume { signal_id })
    } else {
        Ok(SignalOutcome::Accepted { signal_id, effect })
    }
}

/// Parse ff_fail_execution result:
///   ok("retry_scheduled", delay_until)
///   ok("terminal_failed")
#[allow(dead_code)]
fn parse_fail_result(raw: &Value) -> Result<FailOutcome, SdkError> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_fail_result".into(),
                execution_id: None,
                message: "ff_fail_execution: expected Array".into(),
            }));
        }
    };

    let status_code = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => {
            return Err(SdkError::from(ScriptError::Parse {
                fcall: "parse_fail_result".into(),
                execution_id: None,
                message: "ff_fail_execution: bad status code".into(),
            }));
        }
    };

    if status_code != 1 {
        let err_field_str = |idx: usize| -> String {
            arr.get(idx)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        let error_code = {
            let s = err_field_str(1);
            if s.is_empty() { "unknown".to_owned() } else { s }
        };
        let details: Vec<String> = (2..arr.len()).map(err_field_str).collect();
        let detail_refs: Vec<&str> = details.iter().map(|s| s.as_str()).collect();
        return Err(SdkError::from(
            ScriptError::from_code_with_details(&error_code, &detail_refs).unwrap_or_else(|| {
                ScriptError::Parse {
                    fcall: "parse_fail_result".into(),
                    execution_id: None,
                    message: format!("ff_fail_execution: {error_code}"),
                }
            }),
        ));
    }

    // Parse sub-status from field[2] (index 2 = first field after status+OK)
    let sub_status = arr
        .get(2)
        .and_then(|v| match v {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    match sub_status.as_str() {
        "retry_scheduled" => {
            // Lua returns: ok("retry_scheduled", tostring(delay_until))
            // arr[3] = delay_until
            let delay_str = arr
                .get(3)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::Int(n)) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            let delay_until = delay_str.parse::<i64>().unwrap_or(0);

            Ok(FailOutcome::RetryScheduled {
                delay_until: TimestampMs::from_millis(delay_until),
            })
        }
        "terminal_failed" => Ok(FailOutcome::TerminalFailed),
        _ => Err(SdkError::from(ScriptError::Parse {
            fcall: "parse_fail_result".into(),
            execution_id: None,
            message: format!(
            "ff_fail_execution: unexpected sub-status: {sub_status}"
        ),
        })),
    }
}

// ── Stream read / tail (consumer API, RFC-006 #2) ──

/// Maximum tail block duration accepted by [`tail_stream`]. Mirrors the REST
/// endpoint ceiling so SDK callers can't wedge a connection longer than the
/// server would accept.
pub const MAX_TAIL_BLOCK_MS: u64 = 30_000;

/// Maximum frames per read/tail call. Mirrors
/// `ff_core::contracts::STREAM_READ_HARD_CAP` — re-exported here so SDK
/// callers don't need to import ff-core just to read the bound.
pub use ff_core::contracts::STREAM_READ_HARD_CAP;

/// Result of [`read_stream`] / [`tail_stream`] — frames plus the terminal
/// signal so polling consumers can exit cleanly.
///
/// Re-export of `ff_core::contracts::StreamFrames` for SDK ergonomics.
pub use ff_core::contracts::StreamFrames;

/// Opaque cursor for [`read_stream`] / [`tail_stream`] — re-export of
/// `ff_core::contracts::StreamCursor`. Wire tokens: `"start"`, `"end"`,
/// `"<ms>"`, `"<ms>-<seq>"`. Bare `-` / `+` are rejected — use
/// `StreamCursor::Start` / `StreamCursor::End` instead.
pub use ff_core::contracts::StreamCursor;

/// Reject `Start` / `End` cursors at the XREAD (`tail_stream`) boundary
/// — XREAD does not accept the open markers. Pulled out as a bare
/// function so unit tests can exercise the guard without constructing a
/// live `ferriskey::Client`.
fn validate_tail_cursor(after: &StreamCursor) -> Result<(), SdkError> {
    if !after.is_concrete() {
        return Err(SdkError::Config {
            context: "tail_stream".into(),
            field: Some("after".into()),
            message: "XREAD cursor must be a concrete entry id; pass \
                      StreamCursor::from_beginning() to start from the \
                      beginning"
                .into(),
        });
    }
    Ok(())
}

fn validate_stream_read_count(count_limit: u64) -> Result<(), SdkError> {
    if count_limit == 0 {
        return Err(SdkError::Config {
            context: "read_stream_frames".into(),
            field: Some("count_limit".into()),
            message: "count_limit must be >= 1".into(),
        });
    }
    if count_limit > STREAM_READ_HARD_CAP {
        return Err(SdkError::Config {
            context: "read_stream_frames".into(),
            field: Some("count_limit".into()),
            message: format!(
                "count_limit exceeds STREAM_READ_HARD_CAP ({STREAM_READ_HARD_CAP})"
            ),
        });
    }
    Ok(())
}

/// Read frames from a completed or in-flight attempt's stream.
///
/// `from` / `to` are [`StreamCursor`] values — `StreamCursor::Start` /
/// `StreamCursor::End` are equivalent to XRANGE `-` / `+`, and
/// `StreamCursor::At("<id>")` reads from a concrete entry id.
/// `count_limit` MUST be in `1..=STREAM_READ_HARD_CAP` —
/// `0` returns [`SdkError::Config`].
///
/// Returns a [`StreamFrames`] including `closed_at`/`closed_reason` so
/// consumers know when the producer has finalized the stream. A
/// never-written attempt and an in-progress stream are indistinguishable
/// here — both present as `frames=[]`, `closed_at=None`.
///
/// Intended for consumers (audit, checkpoint replay) that hold a ferriskey
/// client but are not the lease-holding worker — no lease check is
/// performed.
///
/// # Head-of-line note
///
/// A max-limit XRANGE reply (10_000 frames × ~64 KB each) is a
/// multi-MB reply serialized on one TCP socket. Like [`tail_stream`],
/// calling this on a `client` that is also serving FCALLs stalls those
/// FCALLs behind the reply. The REST server isolates reads on its
/// `tail_client`; direct SDK callers should either use a dedicated
/// client OR paginate through smaller `count_limit` slices.
pub async fn read_stream(
    backend: &dyn EngineBackend,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    from: StreamCursor,
    to: StreamCursor,
    count_limit: u64,
) -> Result<StreamFrames, SdkError> {
    validate_stream_read_count(count_limit)?;
    Ok(backend
        .read_stream(execution_id, attempt_index, from, to, count_limit)
        .await?)
}

/// Tail a live attempt's stream.
///
/// `after` is an exclusive [`StreamCursor`] — XREAD returns entries
/// with id strictly greater than `after`. Pass
/// `StreamCursor::from_beginning()` (i.e. `At("0-0")`) to start from
/// the beginning. `StreamCursor::Start` / `StreamCursor::End` are
/// REJECTED at this boundary because XREAD does not accept `-` / `+`
/// as cursors — an invalid `after` surfaces as [`SdkError::Config`].
///
/// `block_ms == 0` → non-blocking peek. `block_ms > 0` → blocks up to that
/// many ms. Rejects `block_ms > MAX_TAIL_BLOCK_MS` and `count_limit`
/// outside `1..=STREAM_READ_HARD_CAP` with [`SdkError::Config`] to keep
/// SDK and REST ceilings aligned.
///
/// Returns a [`StreamFrames`] including `closed_at`/`closed_reason` —
/// polling consumers should loop until `result.is_closed()` is true, then
/// drain and exit. Timeout with no new frames presents as
/// `frames=[], closed_at=None`.
///
/// # Head-of-line warning — use a dedicated client
///
/// `ferriskey::Client` is a pipelined multiplexed connection; Valkey
/// processes commands FIFO on it. `XREAD BLOCK block_ms` does not yield
/// the read side until a frame arrives or the block elapses. If the
/// `client` you pass here is ALSO used for claims, completes, fails,
/// appends, or any other FCALL, a 30-second tail will stall all those
/// calls for up to 30 seconds.
///
/// **Strongly recommended**: build a separate `ferriskey::Client` for
/// tail callers — mirrors the `Server::tail_client` split that the REST
/// server uses internally (see `crates/ff-server/src/server.rs` and
/// RFC-006 Impl Notes §"Dedicated stream-op connection").
///
/// # Tail parallelism caveat (same mux)
///
/// Even a dedicated tail client is still one multiplexed TCP connection.
/// Valkey processes `XREAD BLOCK` calls FIFO on that one socket, and
/// ferriskey's per-call `request_timeout` starts at future-poll — so
/// two concurrent tails against the same client can time out spuriously:
/// the second call's BLOCK budget elapses while it waits for the first
/// BLOCK to return. The REST server handles this internally with a
/// `tokio::sync::Mutex` that serializes `xread_block` calls, giving
/// each call its full `block_ms` budget at the server.
///
/// **Direct SDK callers that need concurrent tails**: either
///   (1) build ONE `ferriskey::Client` per concurrent tail call (a small
///       pool of clients, rotated by the caller), OR
///   (2) wrap `tail_stream` calls in your own `tokio::sync::Mutex` so
///       only one BLOCK is in flight per client at a time.
/// If you need the REST-side backpressure (429 on contention) and the
/// built-in serializer, go through the
/// `/v1/executions/{eid}/attempts/{idx}/stream/tail` endpoint rather
/// than calling this directly.
///
/// This SDK does not enforce either pattern — the mutex belongs at the
/// application layer, and the connection pool belongs at the SDK
/// caller's DI layer; neither has a structured place inside this
/// helper.
///
/// # Timeout handling
///
/// Blocking calls do not hit ferriskey's default `request_timeout` (5s on
/// the server default). For `XREAD`/`XREADGROUP` with a `BLOCK` argument,
/// ferriskey's `get_request_timeout` returns `BlockingCommand(block_ms +
/// 500ms)`, overriding the client's default per-call. A tail with
/// `block_ms = 30_000` gets a 30_500ms effective transport timeout even if
/// the client was built with a shorter `request_timeout`. No custom client
/// configuration is required for timeout reasons — only for head-of-line
/// isolation above.
pub async fn tail_stream(
    backend: &dyn EngineBackend,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    after: StreamCursor,
    block_ms: u64,
    count_limit: u64,
) -> Result<StreamFrames, SdkError> {
    tail_stream_with_visibility(
        backend,
        execution_id,
        attempt_index,
        after,
        block_ms,
        count_limit,
        ff_core::backend::TailVisibility::All,
    )
    .await
}

/// Tail helper with an explicit RFC-015
/// [`TailVisibility`](ff_core::backend::TailVisibility) filter.
pub async fn tail_stream_with_visibility(
    backend: &dyn EngineBackend,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    after: StreamCursor,
    block_ms: u64,
    count_limit: u64,
    visibility: ff_core::backend::TailVisibility,
) -> Result<StreamFrames, SdkError> {
    if block_ms > MAX_TAIL_BLOCK_MS {
        return Err(SdkError::Config {
            context: "tail_stream".into(),
            field: Some("block_ms".into()),
            message: format!("exceeds {MAX_TAIL_BLOCK_MS}ms ceiling"),
        });
    }
    validate_stream_read_count(count_limit)?;
    // XREAD does not accept `-` / `+` markers as cursors — reject at
    // the SDK boundary with `SdkError::Config` rather than forwarding
    // an invalid `-`/`+` into the backend (which would surface as an
    // opaque `EngineError::Transport`).
    validate_tail_cursor(&after)?;

    Ok(backend
        .tail_stream(
            execution_id,
            attempt_index,
            after,
            block_ms,
            count_limit,
            visibility,
        )
        .await?)
}

#[cfg(test)]
mod tail_stream_boundary_tests {
    use super::*;

    // `validate_tail_cursor` rejects `StreamCursor::Start` and
    // `StreamCursor::End` before `tail_stream` touches the client —
    // same shape as the `count_limit` guard above. The matching
    // full-path rejection on the REST layer is covered by
    // `ff-server::api`.

    #[test]
    fn rejects_start_cursor() {
        let err = validate_tail_cursor(&StreamCursor::Start)
            .expect_err("Start must be rejected");
        match err {
            SdkError::Config { field, context, .. } => {
                assert_eq!(field.as_deref(), Some("after"));
                assert_eq!(context, "tail_stream");
            }
            other => panic!("expected SdkError::Config, got {other:?}"),
        }
    }

    #[test]
    fn rejects_end_cursor() {
        let err = validate_tail_cursor(&StreamCursor::End)
            .expect_err("End must be rejected");
        assert!(matches!(err, SdkError::Config { .. }));
    }

    #[test]
    fn accepts_at_cursor() {
        validate_tail_cursor(&StreamCursor::At("0-0".into()))
            .expect("At cursor must be accepted");
        validate_tail_cursor(&StreamCursor::from_beginning())
            .expect("from_beginning() must be accepted");
        validate_tail_cursor(&StreamCursor::At("123-0".into()))
            .expect("concrete id must be accepted");
    }
}

#[cfg(test)]
mod parse_report_usage_result_tests {
    use super::*;

    /// `Value::SimpleString` from a `&str`. `usage_field_str` handles
    /// BulkString and SimpleString uniformly (see
    /// `usage_field_str` — `Value::BulkString(b)` → `String::from_utf8_lossy`,
    /// `Value::SimpleString(s)` → clone). SimpleString avoids a
    /// dev-dependency on `bytes` just for test construction.
    fn s(v: &str) -> Result<Value, ferriskey::Error> {
        Ok(Value::SimpleString(v.to_owned()))
    }

    fn int(n: i64) -> Result<Value, ferriskey::Error> {
        Ok(Value::Int(n))
    }

    fn arr(items: Vec<Result<Value, ferriskey::Error>>) -> Value {
        Value::Array(items)
    }

    #[test]
    fn ok_status() {
        let raw = arr(vec![int(1), s("OK")]);
        assert_eq!(parse_report_usage_result(&raw).unwrap(), ReportUsageResult::Ok);
    }

    #[test]
    fn already_applied_status() {
        let raw = arr(vec![int(1), s("ALREADY_APPLIED")]);
        assert_eq!(
            parse_report_usage_result(&raw).unwrap(),
            ReportUsageResult::AlreadyApplied
        );
    }

    #[test]
    fn soft_breach_status() {
        let raw = arr(vec![int(1), s("SOFT_BREACH"), s("tokens"), s("150"), s("100")]);
        match parse_report_usage_result(&raw).unwrap() {
            ReportUsageResult::SoftBreach { dimension, current_usage, soft_limit } => {
                assert_eq!(dimension, "tokens");
                assert_eq!(current_usage, 150);
                assert_eq!(soft_limit, 100);
            }
            other => panic!("expected SoftBreach, got {other:?}"),
        }
    }

    #[test]
    fn hard_breach_status() {
        let raw = arr(vec![int(1), s("HARD_BREACH"), s("requests"), s("10001"), s("10000")]);
        match parse_report_usage_result(&raw).unwrap() {
            ReportUsageResult::HardBreach { dimension, current_usage, hard_limit } => {
                assert_eq!(dimension, "requests");
                assert_eq!(current_usage, 10001);
                assert_eq!(hard_limit, 10000);
            }
            other => panic!("expected HardBreach, got {other:?}"),
        }
    }

    /// Negative case: non-Array input. Guards against a future Lua refactor
    /// that accidentally returns a bare string/int — the parser must fail
    /// loudly rather than silently succeed or panic.
    #[test]
    fn non_array_input_is_parse_error() {
        let raw = Value::SimpleString("OK".to_owned());
        let err = parse_report_usage_result(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("expected array"),
            "error should mention expected shape, got: {msg}"
        );
    }

    /// Negative case: Array whose first element isn't an Int status code.
    /// The Lua function's first return slot is always `status_code` (1 on
    /// success, an error code otherwise); a non-Int there is a wire-format
    /// break that must surface as a parse error.
    #[test]
    fn first_element_non_int_is_parse_error() {
        let raw = arr(vec![s("not_an_int"), s("OK")]);
        let err = parse_report_usage_result(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_lowercase().contains("int"),
            "error should mention Int status code, got: {msg}"
        );
    }

    /// Negative case: SOFT_BREACH with a non-numeric `current_usage`
    /// field. Guards against the silent-coercion defect cross-review
    /// caught: the old parser used `.unwrap_or(0)` on numeric fields,
    /// which would have surfaced Lua-side wire-format drift as a
    /// `SoftBreach { current_usage: 0, ... }` — arithmetically valid
    /// but semantically wrong (a "breach with zero usage" is nonsense
    /// and masks the real error).
    #[test]
    fn soft_breach_non_numeric_current_is_parse_error() {
        let raw = arr(vec![
            int(1),
            s("SOFT_BREACH"),
            s("tokens"),
            s("not_a_number"), // current_usage — must fail, not coerce to 0
            s("100"),
        ]);
        let err = parse_report_usage_result(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("SOFT_BREACH") && msg.contains("current_usage"),
            "error should identify sub-status + field, got: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("u64"),
            "error should mention the expected type (u64), got: {msg}"
        );
    }

    /// Negative case: HARD_BREACH with the limit slot missing
    /// entirely. Same defence as the non-numeric test above: a
    /// truncated response must fail loudly rather than coerce to 0.
    #[test]
    fn hard_breach_missing_limit_is_parse_error() {
        let raw = arr(vec![
            int(1),
            s("HARD_BREACH"),
            s("requests"),
            s("10001"),
            // no index 4 — hard_limit missing
        ]);
        let err = parse_report_usage_result(&raw).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("HARD_BREACH") && msg.contains("hard_limit"),
            "error should identify sub-status + field, got: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("missing"),
            "error should say 'missing', got: {msg}"
        );
    }
}

/// RFC-014 helper: pick the first waitpoint_key out of a composite
/// tree so `try_suspend_inner` can rebind a Fresh waitpoint to the
/// user-supplied key. Single-waitpoint scoping: all
/// Single.waitpoint_key / Count.waitpoints[i] in the tree must be
/// equal; this function returns the first one it encounters.
fn composite_first_waitpoint_key(body: &CompositeBody) -> Option<String> {
    match body {
        CompositeBody::AllOf { members } => members.iter().find_map(|m| match m {
            ResumeCondition::Single { waitpoint_key, .. } => Some(waitpoint_key.clone()),
            ResumeCondition::Composite(inner) => composite_first_waitpoint_key(inner),
            _ => None,
        }),
        CompositeBody::Count { waitpoints, .. } => waitpoints.first().cloned(),
        _ => None,
    }
}


#[cfg(test)]
mod resume_signals_tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn empty_suspension_returns_none() {
        let susp = m(&[]);
        let out = resume_waitpoint_id_from_suspension(&susp, AttemptIndex::new(0)).unwrap();
        assert!(out.is_none(), "no suspension record → None");
    }

    #[test]
    fn stale_prior_attempt_returns_none() {
        let wp = WaitpointId::new();
        let susp = m(&[
            ("attempt_index", "0"),
            ("close_reason", "resumed"),
            ("waitpoint_id", &wp.to_string()),
        ]);
        // Claimed attempt is 1; suspension belongs to 0 → stale.
        let out = resume_waitpoint_id_from_suspension(&susp, AttemptIndex::new(1)).unwrap();
        assert!(out.is_none(), "attempt_index mismatch → None");
    }

    #[test]
    fn non_resumed_close_returns_none() {
        let wp = WaitpointId::new();
        for reason in ["timeout", "cancelled", "", "expired"] {
            let susp = m(&[
                ("attempt_index", "0"),
                ("close_reason", reason),
                ("waitpoint_id", &wp.to_string()),
            ]);
            let out = resume_waitpoint_id_from_suspension(&susp, AttemptIndex::new(0)).unwrap();
            assert!(out.is_none(), "close_reason={reason:?} must not return signals");
        }
    }

    #[test]
    fn resumed_same_attempt_returns_waitpoint() {
        let wp = WaitpointId::new();
        let susp = m(&[
            ("attempt_index", "2"),
            ("close_reason", "resumed"),
            ("waitpoint_id", &wp.to_string()),
        ]);
        let out = resume_waitpoint_id_from_suspension(&susp, AttemptIndex::new(2)).unwrap();
        assert_eq!(out, Some(wp));
    }

    #[test]
    fn malformed_waitpoint_id_is_error() {
        let susp = m(&[
            ("attempt_index", "0"),
            ("close_reason", "resumed"),
            ("waitpoint_id", "not-a-uuid"),
        ]);
        let err = resume_waitpoint_id_from_suspension(&susp, AttemptIndex::new(0)).unwrap_err();
        assert!(
            format!("{err}").contains("not a valid UUID"),
            "error should mention invalid UUID, got: {err}"
        );
    }

    #[test]
    fn empty_waitpoint_id_returns_none() {
        // Defensive: an empty waitpoint_id field (shouldn't happen on
        // resumed records, but guard against partial writes) is None, not an error.
        let susp = m(&[
            ("attempt_index", "0"),
            ("close_reason", "resumed"),
            ("waitpoint_id", ""),
        ]);
        let out = resume_waitpoint_id_from_suspension(&susp, AttemptIndex::new(0)).unwrap();
        assert!(out.is_none());
    }

    // The previous `matched_signal_ids_from_condition` helper was
    // removed when the production path switched from unbounded
    // HGETALL to a bounded HGET + per-matcher HMGET loop (review
    // feedback on unbounded condition-hash reply size). That loop
    // is exercised by the integration tests in `ff-test`.
}

#[cfg(test)]
mod terminal_replay_parsing_tests {
    //! Unit tests for the SDK's parse path of the enriched
    //! `execution_not_active` error returned on a terminal-op replay.
    //! The integration test in ff-test/tests/e2e_lifecycle.rs proves
    //! the Lua side emits the 4-slot detail; these tests prove the
    //! Rust parser threads all 4 slots into the `ExecutionNotActive`
    //! variant so the reconciler in complete()/fail()/cancel() can
    //! match on them.

    use super::*;
    use ferriskey::Value;

    // Use SimpleString rather than BulkString to avoid depending on bytes::Bytes
    // in the test harness. parse_success_result + parse_fail_result handle both.
    fn bulk(s: &str) -> Value {
        Value::SimpleString(s.to_owned())
    }

    /// parse_success_result must fold idx 2..=5 into ExecutionNotActive.
    #[test]
    fn parse_success_result_extracts_all_four_detail_slots() {
        let raw = Value::Array(vec![
            Ok(Value::Int(0)),
            Ok(bulk("execution_not_active")),
            Ok(bulk("success")),
            Ok(bulk("42")),
            Ok(bulk("terminal")),
            Ok(bulk("11111111-1111-1111-1111-111111111111")),
        ]);
        let err = parse_success_result(&raw, "test").unwrap_err();
        let unboxed = match err {
            SdkError::Engine(b) => *b,
            other => panic!("expected ExecutionNotActive struct variant, got {other:?}"),
        };
        match unboxed {
            crate::EngineError::Contention(
                crate::ContentionKind::ExecutionNotActive {
                    terminal_outcome,
                    lease_epoch,
                    lifecycle_phase,
                    attempt_id,
                },
            ) => {
                assert_eq!(terminal_outcome, "success");
                assert_eq!(lease_epoch, "42");
                assert_eq!(lifecycle_phase, "terminal");
                assert_eq!(attempt_id, "11111111-1111-1111-1111-111111111111");
            }
            other => panic!("expected ExecutionNotActive struct variant, got {other:?}"),
        }
    }

    /// parse_fail_result must extract all detail slots too so the
    /// reconciler in fail() can match lifecycle_phase = "runnable" for
    /// retry-scheduled replays.
    #[test]
    fn parse_fail_result_extracts_all_four_detail_slots() {
        let raw = Value::Array(vec![
            Ok(Value::Int(0)),
            Ok(bulk("execution_not_active")),
            Ok(bulk("none")),
            Ok(bulk("7")),
            Ok(bulk("runnable")),
            Ok(bulk("22222222-2222-2222-2222-222222222222")),
        ]);
        let err = parse_fail_result(&raw).unwrap_err();
        let unboxed = match err {
            SdkError::Engine(b) => *b,
            other => panic!("expected ExecutionNotActive struct variant, got {other:?}"),
        };
        match unboxed {
            crate::EngineError::Contention(
                crate::ContentionKind::ExecutionNotActive {
                    terminal_outcome,
                    lease_epoch,
                    lifecycle_phase,
                    attempt_id,
                },
            ) => {
                assert_eq!(terminal_outcome, "none");
                assert_eq!(lease_epoch, "7");
                assert_eq!(lifecycle_phase, "runnable");
                assert_eq!(attempt_id, "22222222-2222-2222-2222-222222222222");
            }
            other => panic!("expected ExecutionNotActive struct variant, got {other:?}"),
        }
    }

    /// Empty detail slots must default to "" (not panic) so older-Lua
    /// producers or malformed replies degrade to an unreconcilable
    /// variant rather than a Parse error.
    #[test]
    fn parse_success_result_missing_slots_defaults_to_empty() {
        let raw = Value::Array(vec![
            Ok(Value::Int(0)),
            Ok(bulk("execution_not_active")),
        ]);
        let err = parse_success_result(&raw, "test").unwrap_err();
        let unboxed = match err {
            SdkError::Engine(b) => *b,
            other => panic!("expected ExecutionNotActive struct variant, got {other:?}"),
        };
        match unboxed {
            crate::EngineError::Contention(
                crate::ContentionKind::ExecutionNotActive {
                    terminal_outcome,
                    lease_epoch,
                    lifecycle_phase,
                    attempt_id,
                },
            ) => {
                assert_eq!(terminal_outcome, "");
                assert_eq!(lease_epoch, "");
                assert_eq!(lifecycle_phase, "");
                assert_eq!(attempt_id, "");
            }
            other => panic!("expected ExecutionNotActive struct variant, got {other:?}"),
        }
    }
}
