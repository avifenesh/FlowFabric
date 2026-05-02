//! `EngineBackend` implementation backed by Valkey FCALL.
//! See RFC-012 §5.1 for the migration plan.
//!
//! **RFC-012 Stage 1a:** this crate lands the [`ValkeyBackend`] struct
//! and the `impl EngineBackend for ValkeyBackend` block. The hot-path
//! methods (`claim`, `renew`, `complete`, `fail`, …) return
//! [`EngineError::Unavailable`] at this stage; hot-path wiring lands
//! across Stages 1b-1d (see issue #89 migration plan). The one
//! method implemented in Stage 1a is [`cancel_flow`], whose thin
//! FCALL wrapper already exists in `ff-script::functions::flow` —
//! this crate wires it up to the trait's `CancelFlowPolicy` /
//! `CancelFlowWait` types.
//!
//! The `EngineBackend` trait stays object-safe; consumers can hold
//! `Arc<dyn EngineBackend>`.

// `EngineError` is ~200 bytes; the `EngineBackend` trait's method
// signatures return `Result<_, EngineError>` throughout (that is the
// public contract). Allow the lint crate-wide so intra-crate helpers
// that mirror the trait's return shape don't need a per-fn allow.
// A future PR can box `EngineError::Transport.source` / the larger
// variants to shrink the `Err` side globally; that is a cross-crate
// design change out of scope for Stage 1b.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{
    AppendFrameOutcome, BackendConnection, CancelFlowPolicy, CancelFlowWait,
    CapabilitySet, ClaimPolicy, FailOutcome, FailureClass, FailureReason, Frame, Handle,
    HandleKind, LeaseRenewal, PatchKind, PendingWaitpoint, ResumeSignal, ResumeToken,
    StreamMode, SummaryDocument, TailVisibility, UsageDimensions, WaitpointHmac,
};
use ff_core::contracts::decode::{
    build_edge_snapshot, build_execution_snapshot, build_flow_snapshot,
};
use ff_core::contracts::{
    AdditionalWaitpointBinding, BlockRouteArgs, BlockRouteOutcome, CancelFlowArgs,
    CancelFlowResult, ClaimExecutionArgs, ClaimExecutionResult, ClaimResumedExecutionArgs,
    ClaimResumedExecutionResult, CompositeBody,
    ClaimGrantOutcome, CountKind, DeliverApprovalSignalArgs, DeliverSignalArgs, DeliverSignalResult,
    EdgeDirection, EdgeSnapshot, ExecutionContext, ExecutionSnapshot,
    ExpiredLeaseInfo, ExpiredLeasesCursor, FlowSnapshot, FlowStatus,
    FlowSummary, HeartbeatWorkerArgs, HeartbeatWorkerOutcome, IssueClaimGrantArgs,
    IssueClaimGrantOutcome, IssueGrantAndClaimArgs,
    IssueReclaimGrantArgs,
    IssueReclaimGrantOutcome, LIST_EXPIRED_LEASES_DEFAULT_LIMIT,
    LIST_EXPIRED_LEASES_DEFAULT_MAX_PARTITIONS, LIST_EXPIRED_LEASES_MAX_LIMIT,
    ListExecutionsPage, ListExpiredLeasesArgs, ListExpiredLeasesResult,
    ListFlowsPage, ListLanesPage, ListSuspendedPage, ListWorkersArgs, ListWorkersResult,
    MARK_WORKER_DEAD_REASON_MAX_BYTES, MarkWorkerDeadArgs, MarkWorkerDeadOutcome,
    ReclaimExecutionArgs, ReclaimExecutionOutcome, ReclaimGrant,
    RecordSpendArgs, RegisterWorkerArgs, RegisterWorkerOutcome,
    ReleaseBudgetArgs, ScanEligibleArgs,
    ReportUsageResult, ResumeCondition, ResumePolicy, ResumeTarget,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretAllEntry,
    RotateWaitpointHmacSecretAllResult, RotateWaitpointHmacSecretArgs, SeedOutcome,
    SeedWaitpointHmacSecretArgs, SignalMatcher, SuspendArgs,
    SuspendOutcome, SuspendOutcomeDetails, SuspendedExecutionEntry, WaitpointBinding,
    WorkerInfo,
};
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::engine_error::{StateKind, ValidationKind};
use ff_core::partition::PartitionKey;
use ff_core::engine_backend::{EngineBackend, ExpirePhase, ReconcileCounts, SiblingCancelReconcileAction};
use ff_core::engine_error::EngineError;
use ff_core::keys::{
    ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys,
    worker_alive_key_ns, worker_caps_key_ns, workers_index_key_ns,
};
use ff_core::partition::{PartitionConfig, execution_partition, flow_partition};
use ff_core::types::{
    AttemptId, AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, LeaseEpoch, LeaseFence,
    LeaseId, SignalId, TimestampMs, WaitpointId, WaitpointToken, WorkerId, WorkerInstanceId,
};
use ff_script::engine_error_ext::transport_script;
use ff_script::error::ScriptError;
use ff_script::functions::execution::{
    CancelExecutionResultPartial, ClaimExecutionResultPartial, ExecOpKeys, ff_claim_execution,
    ff_complete_execution, ff_create_execution, ff_fail_execution,
};
use ff_script::functions::flow::{
    DepOpKeys, FlowStructOpKeys, ResolveDependencyKeys, ff_add_execution_to_flow,
    ff_apply_dependency_to_child, ff_cancel_flow, ff_create_flow, ff_replay_execution,
    ff_resolve_dependency, ff_stage_dependency_edge,
};
use ff_script::functions::lease::{ff_renew_lease, ff_revoke_lease};
use ff_script::functions::suspension::{ResumeOpKeys, ff_resume_execution};
use ff_script::functions::quota::ff_check_admission_and_record;
use ff_script::functions::scheduling::{
    ChangePriorityResultPartial, SchedOpKeys, ff_change_priority, ff_issue_grant_and_claim,
};
use ff_script::functions::budget::{BudgetOpKeys, ff_create_budget, ff_record_spend, ff_release_budget, ff_report_usage_and_check, ff_reset_budget};
use ff_script::functions::quota::{QuotaOpKeys, ff_create_quota_policy};
use ff_script::functions::signal::{SignalOpKeys, ff_claim_resumed_execution, ff_deliver_signal};
use ff_script::functions::worker_registry::{
    RegisterWorkerArgv, RegisterWorkerKeys, ff_register_worker,
};
use ff_script::result::{FcallResult, FromFcallResult};

pub mod backend_error;
pub mod boot;
mod cascade;
mod completion;
mod handle_codec;

pub use backend_error::{
    backend_error_from_ferriskey, classify_ferriskey_kind, BackendErrorWrapper,
};
pub use completion::COMPLETION_CHANNEL;
// DX (HHH v0.3.4 re-smoke): consumers that have already imported
// `ff_backend_valkey` shouldn't need to also dip into
// `ff_core::backend` just to name `BackendConfig`. Re-export it here
// so `ff_backend_valkey::BackendConfig` works as a single-crate path.
pub use ff_core::backend::BackendConfig;

/// Valkey-FCALL–backed `EngineBackend`.
///
/// Holds a shared [`ferriskey::Client`] + the partition config the
/// Lua functions need to route keys. Construction goes through
/// [`ValkeyBackend::connect`], which dials Valkey (standalone or
/// cluster per [`ValkeyConnection::cluster`]) and loads the
/// deployment's partition counts from `ff:config:partitions` so
/// key routing aligns with ff-server. Consumers interacting with
/// the trait never see `ferriskey::Client` directly (RFC-012 §1.3
/// — trait-ifying the write surface removes the ferriskey leak
/// from the SDK's public API).
///
/// [`ValkeyConnection::cluster`]: ff_core::backend::ValkeyConnection::cluster
/// RFC-018 Stage A: build a [`ff_core::capability::Supports`] snapshot
/// for the Valkey backend. Reflects the current in-tree `impl
/// EngineBackend for ValkeyBackend` coverage. `claim_for_worker` is
/// overridden by the caller (`capabilities()`) based on runtime
/// scheduler presence.
///
/// `subscribe_completion` is reported `true` despite being
/// pubsub-backed (non-durable cursor, at-most-once over the live
/// subscription window); the bool says "the trait method is
/// callable," and the non-durable-cursor caveat lives in
/// `EngineBackend::subscribe_completion` rustdoc +
/// `docs/POSTGRES_PARITY_MATRIX.md`.
///
/// `subscribe_instance_tags` is `false` pending concrete cairn demand
/// (#311).
///
/// `Supports` is `#[non_exhaustive]` so struct-literal construction
/// from this crate is forbidden; we start from
/// [`ff_core::capability::Supports::none`] and mutate named fields.
fn valkey_supports_base() -> ff_core::capability::Supports {
    let mut s = ff_core::capability::Supports::none();
    s.cancel_execution = true;
    s.change_priority = true;
    s.replay_execution = true;
    s.revoke_lease = true;
    s.read_execution_state = true;
    s.read_execution_info = true;
    s.get_execution_result = true;
    s.budget_admin = true;
    s.quota_admin = true;
    s.rotate_waitpoint_hmac_secret_all = true;
    s.seed_waitpoint_hmac_secret = true;
    s.list_pending_waitpoints = true;
    s.cancel_flow_header = true;
    s.cancel_flow_wait_timeout = true;
    s.cancel_flow_wait_indefinite = true;
    s.ack_cancel_member = true;
    s.claim_for_worker = true; // runtime-gated by capabilities() caller
    s.issue_reclaim_grant = true; // RFC-024 PR-F: Lua FCALLs exist pre-v0.12
    // RFC-025 Phase 2 — worker-registry trait methods land on Valkey.
    s.register_worker = true;
    s.heartbeat_worker = true;
    s.mark_worker_dead = true;
    s.list_expired_leases = true;
    s.list_workers = true;
    s.prepare = true;
    s.subscribe_lease_history = true;
    s.subscribe_completion = true;
    s.subscribe_signal_delivery = true;
    s.subscribe_instance_tags = false;
    s.stream_durable_summary = true;
    s.stream_best_effort_live = true;
    s
}

pub struct ValkeyBackend {
    client: ferriskey::Client,
    partition_config: PartitionConfig,
    /// Connection config retained so [`CompletionBackend`] can open
    /// dedicated RESP3 subscriber clients that reach the same
    /// deployment. `None` when the backend was constructed via
    /// [`ValkeyBackend::from_client_and_partitions`] without a
    /// connection; in that case `subscribe_completions` returns
    /// `EngineError::Unavailable`.
    subscriber_connection: Option<ff_core::backend::ValkeyConnection>,
    /// Optional observability handle. When set, the `EngineBackend`
    /// trait impl fires handles (today: `inc_lease_renewal`) at the
    /// matching call sites. `None` falls back to no-op (issue #154).
    /// The `Metrics` type is a zero-cost shim unless this crate (or
    /// a transitive dep) enables `ff-observability/enabled`.
    metrics: Option<Arc<ff_observability::Metrics>>,
    /// Stream-op back-pressure gate (RFC-017 §6, Stage B). Relocated
    /// from `ff-server::Server::stream_semaphore`. Bounds concurrent
    /// `read_stream` + `tail_stream` calls server-wide; contention
    /// surfaces as [`EngineError::ResourceExhausted { pool:
    /// "stream_ops", max, .. }`] at the trait boundary (HTTP 429 at
    /// the REST boundary after `ServerError::from` maps it). Closed
    /// during [`ValkeyBackend::shutdown_prepare`] so no new stream
    /// ops can start while in-flight ones drain.
    stream_semaphore: Arc<tokio::sync::Semaphore>,
    /// Max-permit ceiling for `stream_semaphore`, preserved verbatim
    /// on `EngineError::ResourceExhausted.max` so callers can tune
    /// backoff or size their concurrency budget.
    stream_semaphore_max: u32,
    /// Bounded fan-out concurrency for the rotate-waitpoint-secret
    /// admin FCALL. `16` matches the pre-RFC-017 `BOOT_INIT_CONCURRENCY`
    /// on `Server`.
    admin_rotate_fanout_concurrency: u32,
    /// RFC-017 Stage C — `claim_for_worker` trait impl forwards here.
    /// `None` ⇒ trait method returns `EngineError::Unavailable { op:
    /// "claim_for_worker" }`. Wired via [`ValkeyBackend::with_scheduler`]
    /// at `ff-server` boot before the backend is sealed into
    /// `Arc<dyn EngineBackend>`; ff-sdk consumers that don't run a
    /// scheduler leave it `None` (SDK workers don't dispatch the
    /// scheduler-routed claim path).
    scheduler: Option<Arc<ff_scheduler::Scheduler>>,
}

/// Default ceiling for `stream_semaphore` when the caller uses
/// [`ValkeyBackend::from_client_and_partitions`] or `connect*`
/// without explicitly sizing the pool. Mirrors the `ff-server`
/// default of `FF_MAX_CONCURRENT_STREAM_OPS = 64` (RFC-017 §6).
pub const DEFAULT_STREAM_SEMAPHORE_PERMITS: u32 = 64;

/// Default admin-rotate fan-out concurrency. Matches the pre-RFC-017
/// `ff-server::Server::BOOT_INIT_CONCURRENCY = 16`.
pub const DEFAULT_ADMIN_ROTATE_FANOUT_CONCURRENCY: u32 = 16;

impl ValkeyBackend {
    /// Dial a Valkey node with [`BackendConfig`] and return the
    /// backend as `Arc<dyn EngineBackend>`. The returned handle is
    /// `Send + Sync + 'static` so it can be stored on long-lived
    /// worker structs.
    ///
    /// **Stage 1a scope:** this constructor exists so ff-sdk's new
    /// `FlowFabricWorker::connect_with(backend)` path has something
    /// to hand in. The Valkey dial delegates to ferriskey's
    /// [`ClientBuilder`] so host/port, TLS, and cluster flags flow
    /// through, [`BackendTimeouts::request`] maps to
    /// `ClientBuilder::request_timeout` when set (`None` ⇒
    /// ferriskey's default), and [`BackendRetry`] maps to
    /// `ClientBuilder::retry_strategy` when any field is set
    /// (all-`None` ⇒ ferriskey's builder default, i.e. no
    /// `.retry_strategy(..)` call).
    ///
    /// [`BackendRetry`]: ff_core::backend::BackendRetry
    ///
    /// # Capability erasure
    ///
    /// The return type is `Arc<dyn EngineBackend>`, which cannot be
    /// re-upcast to `Arc<dyn ff_core::completion_backend::CompletionBackend>`
    /// (Rust's trait-object model does not support cross-trait
    /// upcasts). Consumers that want BOTH the write surface and the
    /// completion-subscription surface from a single dial must either:
    ///
    /// 1. Build the client + `ValkeyConnection` directly and call
    ///    [`Self::from_client_partitions_and_connection`], holding
    ///    the concrete `Arc<ValkeyBackend>` and cloning it into each
    ///    trait-object position; or
    /// 2. Construct via ff-server's own wiring (which keeps the
    ///    concrete `Arc<ValkeyBackend>` for both positions).
    ///
    /// [`ValkeyBackend::connect`] is the simplest entry point for
    /// write-only consumers; completion subscribers need one of the
    /// patterns above. See `docs/cairn-migration-v0.4.0.md` §5 +
    /// §15.
    pub async fn connect(config: BackendConfig) -> Result<Arc<dyn EngineBackend>, EngineError> {
        Self::connect_inner(config, None, "connect").await
    }

    /// Shared dial + partition-config-load body for [`Self::connect`]
    /// and [`Self::connect_with_metrics`]. The `op_label` feeds the
    /// `EngineError::Unavailable.op` string when the config's
    /// connection is not a `Valkey` variant.
    async fn connect_inner(
        config: BackendConfig,
        metrics: Option<Arc<ff_observability::Metrics>>,
        op_label: &'static str,
    ) -> Result<Arc<dyn EngineBackend>, EngineError> {
        // `BackendConnection` is `#[non_exhaustive]` for future
        // backends; the compiler treats the pattern as refutable,
        // hence `let ... else`. Today only `Valkey` exists; a
        // non-Valkey BackendConnection handed to `ValkeyBackend`
        // surfaces as `EngineError::Unavailable` so callers get a
        // typed error rather than a panic.
        let BackendConnection::Valkey(v) = config.connection.clone() else {
            // `op_label` is `&'static str`; use `match` to keep the
            // `op: &'static str` shape without heap alloc.
            return Err(EngineError::Unavailable {
                op: match op_label {
                    "connect_with_metrics" => {
                        "ValkeyBackend::connect_with_metrics (non-Valkey BackendConnection)"
                    }
                    _ => "ValkeyBackend::connect (non-Valkey BackendConnection)",
                },
            });
        };
        let client = build_client(&config).await?;
        // Load the deployment's partition config from
        // `ff:config:partitions` so `flow_partition` / key routing
        // aligns with what ff-server published. Using
        // `PartitionConfig::default()` (256/32/32) would silently
        // mis-route keys on any non-default deployment; Copilot
        // review comment on PR #114 flagged this as a correctness
        // bug. Mirrors `ff_sdk::worker::read_partition_config`'s
        // warn-and-default behaviour when the hash is missing (e.g.
        // SDK-only tests where ff-server never wrote the hash);
        // transport-level errors propagate so operators notice
        // connectivity issues.
        let partition_config = match load_partition_config(&client).await {
            Ok(cfg) => cfg,
            Err(EngineError::Transport { source, .. })
                if matches!(
                    source.downcast_ref::<ScriptError>(),
                    Some(ScriptError::Parse { .. })
                ) =>
            {
                tracing::warn!(
                    error = %source,
                    "ff:config:partitions not found, using PartitionConfig::default()"
                );
                PartitionConfig::default()
            }
            Err(e) => return Err(e),
        };
        Ok(Arc::new(Self {
            client,
            partition_config,
            subscriber_connection: Some(v),
            metrics,
            stream_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_STREAM_SEMAPHORE_PERMITS as usize,
            )),
            stream_semaphore_max: DEFAULT_STREAM_SEMAPHORE_PERMITS,
            admin_rotate_fanout_concurrency: DEFAULT_ADMIN_ROTATE_FANOUT_CONCURRENCY,
            scheduler: None,
        }))
    }

    /// Borrow the underlying `ferriskey::Client`. Backend-internal
    /// use; call sites outside this crate should route through the
    /// trait rather than reach in here.
    pub fn client(&self) -> &ferriskey::Client {
        &self.client
    }

    /// Wrap an already-dialed `ferriskey::Client` + known
    /// `PartitionConfig` into a `ValkeyBackend`. Used by ff-sdk's
    /// legacy `FlowFabricWorker::connect` path (RFC-012 Stage 1b) to
    /// synthesise a backend around the client it dialed itself,
    /// rather than re-dialing through
    /// [`ValkeyBackend::connect`]. Keeps the Stage 1b migration a
    /// pure refactor — no new round-trips, no second Valkey
    /// connection.
    pub fn from_client_and_partitions(
        client: ferriskey::Client,
        partition_config: PartitionConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            partition_config,
            subscriber_connection: None,
            metrics: None,
            stream_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_STREAM_SEMAPHORE_PERMITS as usize,
            )),
            stream_semaphore_max: DEFAULT_STREAM_SEMAPHORE_PERMITS,
            admin_rotate_fanout_concurrency: DEFAULT_ADMIN_ROTATE_FANOUT_CONCURRENCY,
            scheduler: None,
        })
    }

    /// Like [`Self::from_client_and_partitions`] but retains the
    /// connection config so the backend's [`CompletionBackend`] impl
    /// can open dedicated RESP3 subscriber clients. Used by ff-server
    /// wiring where we want a single `Arc` serving both the write
    /// (`EngineBackend`) and completion-subscription surfaces.
    pub fn from_client_partitions_and_connection(
        client: ferriskey::Client,
        partition_config: PartitionConfig,
        connection: ff_core::backend::ValkeyConnection,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            partition_config,
            subscriber_connection: Some(connection),
            metrics: None,
            stream_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_STREAM_SEMAPHORE_PERMITS as usize,
            )),
            stream_semaphore_max: DEFAULT_STREAM_SEMAPHORE_PERMITS,
            admin_rotate_fanout_concurrency: DEFAULT_ADMIN_ROTATE_FANOUT_CONCURRENCY,
            scheduler: None,
        })
    }

    /// Attach an `ff_observability::Metrics` handle so the trait
    /// impl's metric-emitting sites fire (issue #154). Returns `true`
    /// when the handle was installed (`Arc::get_mut` succeeded — this
    /// requires the caller to hold the only outstanding `Arc<Self>`),
    /// `false` otherwise. If the backend was constructed behind an
    /// `Arc<dyn EngineBackend>` (e.g. via [`ValkeyBackend::connect`]),
    /// use [`ValkeyBackend::connect_with_metrics`] instead — you
    /// cannot mutate through `Arc<dyn …>`.
    pub fn with_metrics(
        self: &mut Arc<Self>,
        metrics: Arc<ff_observability::Metrics>,
    ) -> bool {
        if let Some(inner) = Arc::get_mut(self) {
            inner.metrics = Some(metrics);
            true
        } else {
            false
        }
    }

    /// Dial + attach `Metrics` in one step. Alternative to
    /// [`ValkeyBackend::connect`] that wires the metrics handle
    /// before the returned `Arc<dyn EngineBackend>` is sealed. (issue #154)
    ///
    /// # Boot-step ordering (RFC-017 §4 row 12)
    ///
    /// The deployment-init steps relocated from `ff-server` in
    /// RFC-017 Wave 8 Stage D are run through
    /// [`Self::initialize_deployment`]. The ordering contract is
    /// load-bearing: FUNCTION LOAD precedes lanes SADD (SADD scripts
    /// reference the library) and HMAC init precedes the lanes seed
    /// (pending-waitpoint reads depend on the secret). Callers that
    /// bypass `connect_with_metrics` (e.g. `ff-server` which dials
    /// via [`Self::from_client_partitions_and_connection`]) MUST call
    /// `initialize_deployment` themselves before handing out the
    /// `Arc<dyn EngineBackend>`.
    pub async fn connect_with_metrics(
        config: BackendConfig,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Arc<dyn EngineBackend>, EngineError> {
        Self::connect_inner(config, Some(metrics), "connect_with_metrics").await
    }

    /// RFC-017 Stage C: install the `ff_scheduler::Scheduler` handle
    /// that drives [`EngineBackend::claim_for_worker`]. Returns `true`
    /// when the handle was installed (`Arc::get_mut` saw a unique
    /// handle); `false` otherwise. When absent, `claim_for_worker`
    /// returns `EngineError::Unavailable { op: "claim_for_worker" }`.
    /// Call this before cloning the `Arc` into other positions
    /// (e.g. before casting to `Arc<dyn EngineBackend>`).
    pub fn with_scheduler(
        self: &mut Arc<Self>,
        scheduler: Arc<ff_scheduler::Scheduler>,
    ) -> bool {
        if let Some(inner) = Arc::get_mut(self) {
            inner.scheduler = Some(scheduler);
            true
        } else {
            false
        }
    }

    /// RFC-017 Stage C: test/diagnostic accessor for the wired
    /// scheduler handle. `None` before [`Self::with_scheduler`] is
    /// called.
    #[doc(hidden)]
    pub fn scheduler(&self) -> Option<&Arc<ff_scheduler::Scheduler>> {
        self.scheduler.as_ref()
    }

    /// Build a `ValkeyBackend` with an embedded scheduler instance, so
    /// [`EngineBackend::claim_for_worker`] works from direct-`Arc<dyn
    /// EngineBackend>` consumers (i.e. outside of `ff-server`'s boot
    /// path). Consumers that run their own claim loop (no ff-server)
    /// should use this constructor; consumers that talk to a running
    /// ff-server via HTTP should continue to use
    /// `FlowFabricWorker::claim_via_server` and build the backend
    /// via [`ValkeyBackend::connect`] (no scheduler).
    ///
    /// Dials the Valkey client from `config`, loads (or defaults) the
    /// deployment's `PartitionConfig`, constructs an internal
    /// [`ff_scheduler::Scheduler`] over that client + partition
    /// config (sharing `metrics` so claim counters land in the
    /// caller's OTEL registry), and wires it before sealing the
    /// returned `Arc<dyn EngineBackend>`. Closes issue #293.
    pub async fn with_embedded_scheduler(
        config: BackendConfig,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Arc<dyn EngineBackend>, EngineError> {
        let BackendConnection::Valkey(v) = config.connection.clone() else {
            return Err(EngineError::Unavailable {
                op: "ValkeyBackend::with_embedded_scheduler (non-Valkey BackendConnection)",
            });
        };
        let client = build_client(&config).await?;
        let partition_config = match load_partition_config(&client).await {
            Ok(cfg) => cfg,
            Err(EngineError::Transport { source, .. })
                if matches!(
                    source.downcast_ref::<ScriptError>(),
                    Some(ScriptError::Parse { .. })
                ) =>
            {
                tracing::warn!(
                    error = %source,
                    "ff:config:partitions not found, using PartitionConfig::default()"
                );
                PartitionConfig::default()
            }
            Err(e) => return Err(e),
        };
        let scheduler = Arc::new(ff_scheduler::Scheduler::with_metrics(
            client.clone(),
            partition_config,
            metrics.clone(),
        ));
        let backend = Arc::new(Self {
            client,
            partition_config,
            subscriber_connection: Some(v),
            metrics: Some(metrics),
            stream_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_STREAM_SEMAPHORE_PERMITS as usize,
            )),
            stream_semaphore_max: DEFAULT_STREAM_SEMAPHORE_PERMITS,
            admin_rotate_fanout_concurrency: DEFAULT_ADMIN_ROTATE_FANOUT_CONCURRENCY,
            scheduler: Some(scheduler),
        });
        Ok(backend as Arc<dyn EngineBackend>)
    }

    /// RFC-017 Wave 8 Stage D (§4 row 12): run the Valkey-specific
    /// deployment-initialisation steps on the backend's client. This
    /// method owns the boot primitives `ff-server` used to run inline
    /// inside `Server::start_with_metrics`.
    ///
    /// **Ordering contract (load-bearing, RFC-017 §4 row 12):**
    ///
    /// 1. `verify_valkey_version` (reject pre-7.2)
    /// 2. `validate_or_create_partition_config`
    /// 3. `initialize_waitpoint_hmac_secret` **(precedes lanes seed —
    ///    pending-waitpoint reads depend on the secret being
    ///    installed)**
    /// 4. `ensure_library` (FUNCTION LOAD) **(precedes lanes SADD —
    ///    SADD scripts reference the library)**
    /// 5. Lanes SADD seed
    ///
    /// The order matches the pre-relocation `Server::start_with_metrics`
    /// body byte-for-byte.
    ///
    /// Returns `EngineError::Validation { kind: Corruption, .. }` for
    /// version-too-low / partition-mismatch / redis-rejected cases;
    /// `EngineError::Transport` / `Contextual` for Valkey IO faults.
    pub async fn initialize_deployment(
        &self,
        waitpoint_hmac_secret: &str,
        lanes: &[LaneId],
        skip_library_load: bool,
    ) -> Result<(), EngineError> {
        boot::initialize_deployment_steps(
            &self.client,
            &self.partition_config,
            waitpoint_hmac_secret,
            lanes,
            skip_library_load,
        )
        .await
    }

    /// RFC-017 Stage B: override the default stream-op concurrency
    /// ceiling. Returns `true` when the new ceiling was installed
    /// (`Arc::get_mut` saw a unique handle); `false` when other
    /// `Arc` holders prevent mutation. Callers construct the backend
    /// via `from_*`, then size the pool before cloning into other
    /// positions.
    pub fn with_stream_semaphore_permits(self: &mut Arc<Self>, max: u32) -> bool {
        if let Some(inner) = Arc::get_mut(self) {
            inner.stream_semaphore = Arc::new(tokio::sync::Semaphore::new(max as usize));
            inner.stream_semaphore_max = max;
            true
        } else {
            false
        }
    }

    /// RFC-017 Stage B: configured ceiling for the stream-op back-
    /// pressure pool. Exposed so operators + tests can assert the
    /// 429 `max` surfaced in `EngineError::ResourceExhausted` matches
    /// what they configured.
    pub fn stream_semaphore_permits(&self) -> u32 {
        self.stream_semaphore_max
    }

    /// RFC-017 Stage B: approximate count of currently-available
    /// permits in the stream-op pool. Used by server-side retry-hint
    /// heuristics; semaphore semantics mean the value is racy and
    /// MUST NOT be used for correctness decisions.
    pub fn stream_semaphore_available(&self) -> usize {
        self.stream_semaphore.available_permits()
    }

    /// RFC-017 Stage D1 (§8): Valkey-only inherent fetch of the raw
    /// RFC-017 §14.8 mandatory Stage B CI test hook. Exposes a clone
    /// of the internal semaphore so the shutdown-under-load test can
    /// hold permits directly without dispatching a live FCALL.
    /// Marked `#[doc(hidden)]` so it does not leak into the public
    /// rustdoc; the method is public only because the integration
    /// test lives in a sibling crate (`ff-server`).
    #[doc(hidden)]
    pub fn stream_semaphore_clone_for_tests(&self) -> Arc<tokio::sync::Semaphore> {
        self.stream_semaphore.clone()
    }


    /// Encode the minimum set of attempt-cookie fields into a
    /// Valkey-tagged [`Handle`]. Stage 1b's `ClaimedTask::synth_handle`
    /// calls this on every trait-forwarder entry; Stage 1d will move
    /// the encode onto the claim path itself (so `ClaimedTask` caches
    /// one `Handle` rather than synthesising per op).
    ///
    /// `kind` today is always `HandleKind::Fresh` at the ff-sdk call
    /// site — Stage 1b's 8 migrated ops do not dispatch on
    /// `Handle.kind`, so the SDK does not yet distinguish
    /// resumed-claim handles on the trait boundary. Stage 1d (or
    /// the call-site that claims from a reclaim grant) will start
    /// passing `HandleKind::Resumed` once a trait op needs the
    /// distinction. The Lua side does not inspect the kind today;
    /// it is carried on the `Handle` so trait methods that want to
    /// match on lifecycle state (`suspend` returns a
    /// `HandleKind::Suspended`) can do so additively.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_handle(
        execution_id: ExecutionId,
        attempt_index: AttemptIndex,
        attempt_id: AttemptId,
        lease_id: LeaseId,
        lease_epoch: LeaseEpoch,
        lease_ttl_ms: u64,
        lane_id: LaneId,
        worker_instance_id: WorkerInstanceId,
        kind: HandleKind,
    ) -> Handle {
        let fields = handle_codec::HandleFields::new(
            execution_id,
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            lease_ttl_ms,
            lane_id,
            worker_instance_id,
        );
        handle_codec::encode_handle(&fields, kind)
    }

    /// Apply a `ScannerFilter` to one candidate eid inside the Valkey
    /// reconciler path. Mirrors `ff_engine::scanner::should_skip_candidate`
    /// exactly (noop → keep; parse error → skip; namespace mismatch → skip;
    /// tag mismatch → skip) so cluster 2b-A tally-recompute scanners observe
    /// the same filter semantics they did before PR-7b trait routing.
    /// See that helper's rustdoc for fail-closed rationale (§Failure mode).
    async fn scanner_skip_candidate(
        &self,
        filter: &ff_core::backend::ScannerFilter,
        eid: &str,
    ) -> bool {
        if filter.is_noop() {
            return false;
        }
        let Ok(exec_id) = ExecutionId::parse(eid) else {
            return true;
        };
        if let Some(ref want_ns) = filter.namespace {
            match self.get_execution_namespace(&exec_id).await {
                Ok(Some(ref got)) if got == want_ns.as_str() => {}
                _ => return true,
            }
        }
        if let Some((ref tag_key, ref want_value)) = filter.instance_tag {
            match self.get_execution_tag(&exec_id, tag_key.as_str()).await {
                Ok(Some(v)) if &v == want_value => {}
                _ => return true,
            }
        }
        false
    }
}

/// Map [`CancelFlowPolicy`] to the Lua-side policy string.
/// Build a dialed `ferriskey::Client` from a [`BackendConfig`].
///
/// Isolated from [`ValkeyBackend::connect`] so the `BackendConfig` →
/// `ClientBuilder` mapping (host/port, TLS, cluster, timeouts) can
/// be exercised directly in tests without the partition-config
/// loading step that follows. Uses ferriskey's `ClientBuilder` so
/// both standalone and cluster paths share one wiring point;
/// `.cluster()` switches the builder to topology-discovery mode.
/// `request_timeout` is applied only when the caller set it —
/// `None` leaves ferriskey's default in place. `retry_strategy` is
/// applied only when at least one `BackendRetry` field is `Some`;
/// all-`None` skips the call so ferriskey's builder default stands.
/// Fields that are `None` within a partially-populated `BackendRetry`
/// fall back to `ConnectionRetryStrategy::default()` per-field (0 /
/// 0 / 0 / None); callers opting into any field should set all
/// fields they care about.
pub async fn build_client(config: &BackendConfig) -> Result<ferriskey::Client, EngineError> {
    let BackendConnection::Valkey(v) = &config.connection else {
        return Err(EngineError::Unavailable {
            op: "ValkeyBackend::connect (non-Valkey BackendConnection)",
        });
    };
    let mut builder = ferriskey::ClientBuilder::new().host(&v.host, v.port);
    if v.tls {
        builder = builder.tls();
    }
    if v.cluster {
        builder = builder.cluster();
    }
    if let Some(request_timeout) = config.timeouts.request {
        builder = builder.request_timeout(request_timeout);
    }
    let retry = &config.retry;
    if retry.exponent_base.is_some()
        || retry.factor.is_some()
        || retry.number_of_retries.is_some()
        || retry.jitter_percent.is_some()
    {
        let default = ferriskey::client::ConnectionRetryStrategy::default();
        let strategy = ferriskey::client::ConnectionRetryStrategy {
            exponent_base: retry.exponent_base.unwrap_or(default.exponent_base),
            factor: retry.factor.unwrap_or(default.factor),
            number_of_retries: retry.number_of_retries.unwrap_or(default.number_of_retries),
            jitter_percent: retry.jitter_percent.or(default.jitter_percent),
        };
        builder = builder.retry_strategy(strategy);
    }
    builder
        .build()
        .await
        .map_err(|e| transport_script(ScriptError::Valkey(e)))
}

fn cancel_policy_to_str(p: CancelFlowPolicy) -> &'static str {
    match p {
        CancelFlowPolicy::FlowOnly => "flow_only",
        CancelFlowPolicy::CancelAll => "cancel_all",
        CancelFlowPolicy::CancelPending => "cancel_pending",
        // `CancelFlowPolicy` is `#[non_exhaustive]`. Fall back to
        // the least-destructive recognised policy (`flow_only`) so a
        // newly-added variant does NOT silently widen the cancel
        // scope. Widening defaults lose work; narrowing defaults
        // are safely retryable by the caller via an explicit policy.
        // Follow-up PRs that add variants must still update this
        // match explicitly.
        _ => "flow_only",
    }
}

/// Cancel-flow FCALL wrapper. Runs the synchronous state-flip +
/// member-cancel dispatch FCALL; the `wait` parameter is honoured by
/// the trait-level `cancel_flow` via the shared
/// [`wait_for_flow_cancellation`](ff_core::engine_backend::wait_for_flow_cancellation)
/// helper which polls `describe_flow` once the FCALL commits. See
/// RFC-012 §3.1.1 for the cancel_flow policy / wait matrix.
#[tracing::instrument(
    name = "ff.cancel_flow",
    skip_all,
    fields(backend = "valkey", flow_id = %flow_id)
)]
async fn cancel_flow_fcall(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_id: &FlowId,
    policy: CancelFlowPolicy,
) -> Result<CancelFlowResult, EngineError> {
    let partition = flow_partition(flow_id, partition_config);
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let fidx = FlowIndexKeys::new(&partition);
    let keys = FlowStructOpKeys {
        fctx: &fctx,
        fidx: &fidx,
    };
    let now = now_ms_timestamp();
    let args = CancelFlowArgs {
        flow_id: flow_id.clone(),
        reason: String::new(),
        cancellation_policy: cancel_policy_to_str(policy).to_string(),
        now,
    };
    ff_cancel_flow(client, &keys, &args)
        .await
        .map_err(EngineError::from)
}

/// RFC-016 Stage B: set the inbound-edge-group policy for a downstream
/// execution. Stage B lifts the Stage-A restriction — `AnyOf` and
/// `Quorum` are accepted and flow into the Lua resolver's four-counter
/// state machine. Only invalid shapes (`k == 0`, absurdly large `k`)
/// and unknown `#[non_exhaustive]` variants are rejected here.
async fn set_edge_group_policy_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_id: &FlowId,
    downstream_eid: &ExecutionId,
    policy: ff_core::contracts::EdgeDependencyPolicy,
) -> Result<ff_core::contracts::SetEdgeGroupPolicyResult, EngineError> {
    use ff_core::contracts::EdgeDependencyPolicy;
    use ff_script::functions::flow::{
        ff_set_edge_group_policy, SetEdgeGroupPolicyKeys,
    };

    // Stage B validation. `k <= n` is enforced at resolve time (§8.4):
    // with dynamic expansion, `n` may grow after this call, so only
    // absolute invariants on `k` are checked here.
    match &policy {
        EdgeDependencyPolicy::AllOf => {}
        EdgeDependencyPolicy::AnyOf { .. } => {}
        EdgeDependencyPolicy::Quorum { k, .. } => {
            if *k == 0 {
                return Err(EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::InvalidInput,
                    detail: "quorum k must be >= 1".to_string(),
                });
            }
            // Guard against wraparound / pathological inputs. `usize::MAX
            // / 2` is the RFC-cited cap; on 64-bit this comfortably
            // exceeds any realistic fanout.
            if (*k as u64) > (u32::MAX / 2) as u64 {
                return Err(EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::InvalidInput,
                    detail: "quorum k exceeds supported maximum".to_string(),
                });
            }
        }
        // Forward-compat: any future variant must be reviewed here
        // before it can reach Lua. Fail closed with a typed error.
        _ => {
            return Err(EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::InvalidInput,
                detail: "unknown EdgeDependencyPolicy variant".to_string(),
            });
        }
    }

    let partition = flow_partition(flow_id, partition_config);
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let keys = SetEdgeGroupPolicyKeys {
        fctx: &fctx,
        downstream_eid,
    };
    let args = ff_core::contracts::SetEdgeGroupPolicyArgs {
        flow_id: flow_id.clone(),
        downstream_execution_id: downstream_eid.clone(),
        policy,
        now: now_ms_timestamp(),
    };
    ff_set_edge_group_policy(client, &keys, &args)
        .await
        .map_err(EngineError::from)
}

/// Pipeline two `HGETALL`s (exec_core + tags) on the execution's
/// partition and decode via [`build_execution_snapshot`]. `Ok(None)`
/// when exec_core is absent. Decode failures surface as
/// `EngineError::Validation { kind: Corruption, .. }`.
///
/// Mirrors the pre-T3 ff-sdk pipeline shape: the two keys share
/// `{fp:N}` so cluster mode routes them to the same slot.
#[tracing::instrument(
    name = "ff.describe_execution",
    skip_all,
    fields(backend = "valkey", execution_id = %id)
)]
async fn describe_execution_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<Option<ExecutionSnapshot>, EngineError> {
    let partition = execution_partition(id, partition_config);
    let ctx = ExecKeyContext::new(&partition, id);
    let core_key = ctx.core();
    let tags_key = ctx.tags();

    let mut pipe = client.pipeline();
    let core_slot = pipe
        .cmd::<HashMap<String, String>>("HGETALL")
        .arg(&core_key)
        .finish();
    let tags_slot = pipe
        .cmd::<HashMap<String, String>>("HGETALL")
        .arg(&tags_key)
        .finish();
    pipe.execute().await.map_err(transport_fk)?;

    let core = core_slot.value().map_err(transport_fk)?;
    if core.is_empty() {
        return Ok(None);
    }
    let tags_raw = tags_slot.value().map_err(transport_fk)?;
    build_execution_snapshot(id.clone(), &core, tags_raw)
}

/// Pipeline `GET :payload` + `HGETALL :core` +
/// `HGETALL :tags` on the execution's partition.
///
/// Payload is stored as an opaque blob at the `:payload` key; an empty
/// or absent value surfaces as an empty `Vec<u8>`. `execution_kind`
/// lives on the `:core` hash; absence surfaces as the empty string.
/// Tags come from the dedicated `:tags` hash, keyed by tag name.
///
/// A missing `:core` hash surfaces as
/// `EngineError::Validation { kind: InvalidInput, .. }` — matching the
/// PG + SQLite siblings. The SDK worker only calls this post-claim, so
/// a missing row is an invariant violation rather than a silent empty.
///
/// All three keys share `{fp:N}` so cluster mode routes them to the
/// same slot — matches [`describe_execution_impl`].
#[tracing::instrument(
    name = "ff.read_execution_context",
    skip_all,
    fields(backend = "valkey", execution_id = %id)
)]
async fn read_execution_context_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<ExecutionContext, EngineError> {
    let partition = execution_partition(id, partition_config);
    let ctx = ExecKeyContext::new(&partition, id);
    let payload_key = ctx.payload();
    let core_key = ctx.core();
    let tags_key = ctx.tags();

    let mut pipe = client.pipeline();
    let payload_slot = pipe
        .cmd::<Option<Vec<u8>>>("GET")
        .arg(&payload_key)
        .finish();
    let core_slot = pipe
        .cmd::<HashMap<String, String>>("HGETALL")
        .arg(&core_key)
        .finish();
    let tags_slot = pipe
        .cmd::<HashMap<String, String>>("HGETALL")
        .arg(&tags_key)
        .finish();
    pipe.execute().await.map_err(transport_fk)?;

    let core = core_slot.value().map_err(transport_fk)?;
    // Missing-execution parity with PG + SQLite siblings: the SDK worker
    // only calls this post-claim, so an absent core hash is an invariant
    // violation rather than a silent empty read. See
    // `exec_core::read_execution_context_impl` (PG) and
    // `reads::read_execution_context_impl` (SQLite).
    if core.is_empty() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("read_execution_context: execution not found: {id}"),
        });
    }

    let input_payload = payload_slot
        .value()
        .map_err(transport_fk)?
        .unwrap_or_default();
    let execution_kind = core
        .get("execution_kind")
        .cloned()
        .unwrap_or_default();
    let tags = tags_slot.value().map_err(transport_fk)?;

    Ok(ExecutionContext::new(input_payload, execution_kind, tags))
}

/// Point-read of the execution's current attempt-index from the
/// `{exec}:core` hash. Single `HGET current_attempt_index` — the same
/// pattern the SDK worker previously issued inline before it dispatched
/// `ff_claim_resumed_execution`.
///
/// **Missing-data semantics (Valkey-specific — diverges from PG/SQLite).**
/// Both the missing-field case (`exec_core` present but
/// `current_attempt_index` absent / empty-string, i.e. pre-claim state)
/// and the missing-row case (no `exec_core` hash at all) are mapped to
/// `AttemptIndex(0)`. This preserves the pre-PR-3 inline-`HGET`
/// semantic (`.and_then(parse).unwrap_or(0)`) so a resume-grant consumed
/// against a not-yet-claimed execution reaches the downstream FCALL,
/// which then surfaces the proper business-logic error
/// (`NotAResumedExecution` / `ExecutionNotLeaseable`) instead of the
/// pre-read blowing up with `Corruption`.
///
/// PG and SQLite siblings instead return
/// [`EngineError::Validation { kind: InvalidInput, .. }`] on the
/// missing-row case; the asymmetry is intentional — Valkey's happy
/// path requires `exec_core` to exist (grant issuance is the only
/// caller) so a missing row there is already prevented by the
/// invariant. See the trait rustdoc on
/// `EngineBackend::read_current_attempt_index` (see
/// `ff_core::engine_backend`) for the cross-backend contract.
#[tracing::instrument(
    name = "ff.read_current_attempt_index",
    skip_all,
    fields(backend = "valkey", execution_id = %id)
)]
async fn read_current_attempt_index_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<AttemptIndex, EngineError> {
    let partition = execution_partition(id, partition_config);
    let ctx = ExecKeyContext::new(&partition, id);

    let raw: Option<String> = client
        .cmd("HGET")
        .arg(ctx.core())
        .arg("current_attempt_index")
        .execute()
        .await
        .map_err(transport_fk)?;

    // Pre-claim state — `exec_core` exists but `current_attempt_index`
    // is absent or empty-string until the first claim fires. Mirror the
    // prior SDK inline-`HGET` semantic (`.and_then(parse).unwrap_or(0)`)
    // so a resume-grant consumed against a not-yet-claimed execution
    // reaches the backend FCALL / SQL, which then surfaces the proper
    // business-logic error (`NotAResumedExecution` /
    // `ExecutionNotLeaseable`) instead of this pre-read blowing up
    // with `Corruption`. The missing-row case is likewise impossible
    // on the resume-grant path (grant issuance requires `exec_core`),
    // so `HGET` on an absent key returning `nil` is mapped to 0 too.
    let idx = raw
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    Ok(AttemptIndex::new(idx))
}

/// Point-read of the execution's **total attempt counter** from the
/// `{exec}:core` hash. Single `HGET total_attempt_count` — the same
/// field Lua 5920's `ff_claim_execution` consults when computing
/// `next_att_idx`. Missing hash / missing field / empty string all
/// map to `0` (pre-claim state, first-claim about to mint attempt 0);
/// the FCALL itself surfaces the loud error if the exec truly
/// doesn't exist.
///
/// Distinct from [`read_current_attempt_index_impl`] — this is the
/// monotonic counter used to assign the *next* attempt on a fresh
/// claim, not the pointer at the currently-leased attempt row. See
/// `EngineBackend::read_total_attempt_count` rustdoc for the
/// retry-path bug this split fixes.
#[tracing::instrument(
    name = "ff.read_total_attempt_count",
    skip_all,
    fields(backend = "valkey", execution_id = %id)
)]
async fn read_total_attempt_count_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<AttemptIndex, EngineError> {
    let partition = execution_partition(id, partition_config);
    let ctx = ExecKeyContext::new(&partition, id);

    let raw: Option<String> = client
        .cmd("HGET")
        .arg(ctx.core())
        .arg("total_attempt_count")
        .execute()
        .await
        .map_err(transport_fk)?;

    let count = raw
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    Ok(AttemptIndex::new(count))
}

/// List suspended executions in one partition, cursor-paginated,
/// with suspension `reason_code` populated per entry (issue #183).
///
/// The engine maintains per-lane suspended ZSETs
/// (`ff:idx:<tag>:lane:<lane_id>:suspended`) rather than a single
/// partition-wide ZSET, so this impl issues a bounded `SCAN` for the
/// lane-suspended key set under the partition's hash tag (single-
/// slot under RFC-011 co-location), `ZRANGE`s each, merges by score
/// ascending (with execution id as lex tiebreak), skips past the
/// supplied cursor, and pipelines `HMGET suspension:current reason_code`
/// for the returned slice.
///
/// Cluster note: the SCAN is issued with a MATCH pattern pinned to
/// the partition's hash tag. ferriskey's SCAN routing may broadcast
/// across nodes; the MATCH filter still yields only keys whose slot
/// maps to the tag, so the result set is correct but the scan cost
/// is per-node. Operator-tooling call shape — not hot path.
#[tracing::instrument(
    name = "ff.list_suspended",
    skip_all,
    fields(backend = "valkey", partition = %partition)
)]
async fn list_suspended_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    partition: PartitionKey,
    cursor: Option<ExecutionId>,
    limit: usize,
) -> Result<ListSuspendedPage, EngineError> {
    if limit == 0 {
        return Ok(ListSuspendedPage::new(Vec::new(), None));
    }
    let parsed = partition.parse().map_err(|e| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: format!("list_suspended: partition: {e}"),
    })?;
    let tag = parsed.hash_tag();

    // 1. Enumerate per-lane suspended ZSETs in this partition via
    //    `SCAN MATCH ff:idx:{tag}:lane:*:suspended`. Two paths:
    //
    //    * Standalone: loop `SCAN` with a string cursor until the
    //      sentinel "0" is returned.
    //    * Cluster: `Client::cluster_scan` with the same match
    //      pattern. ferriskey recognises the embedded hash tag
    //      (`{tag}`) and pins the scan to the single primary that
    //      owns the tag's slot, finishing as soon as that node's
    //      cursor wraps — not a cluster-wide broadcast. Using
    //      plain `cmd("SCAN")` in cluster mode would route to one
    //      arbitrary primary via `RouteBy::Undefined` and miss the
    //      partition if the owning primary differs.
    //
    //    Safety cap — refuse to enumerate pathologically large key
    //    spaces. 10k unique lane-suspended keys per partition is a
    //    loud operational anomaly; the cap bounds the enumeration
    //    cost at query time.
    const MAX_LANE_KEYS: usize = 10_000;
    let match_pat = format!("ff:idx:{tag}:lane:*:suspended");
    let mut lane_keys: Vec<String> = Vec::new();
    if client.is_cluster().await {
        let args = ferriskey::ClusterScanArgs::builder()
            .with_match_pattern(match_pat.as_str())
            .with_count(100)
            .build();
        let mut cursor = ferriskey::ScanStateRC::new();
        loop {
            let (next_cursor, values) = client
                .cluster_scan(&cursor, args.clone())
                .await
                .map_err(transport_fk)?;
            for v in values {
                if lane_keys.len() >= MAX_LANE_KEYS {
                    break;
                }
                if let Ok(s) = ferriskey::from_owned_value::<String>(v) {
                    lane_keys.push(s);
                }
            }
            if next_cursor.is_finished() || lane_keys.len() >= MAX_LANE_KEYS {
                break;
            }
            cursor = next_cursor;
        }
    } else {
        let mut scan_cursor = "0".to_string();
        loop {
            let raw: ferriskey::Value = client
                .cmd("SCAN")
                .arg(scan_cursor.as_str())
                .arg("MATCH")
                .arg(match_pat.as_str())
                .arg("COUNT")
                .arg("100")
                .execute()
                .await
                .map_err(transport_fk)?;
            let (next_cursor, keys) = parse_scan_response(&raw);
            for k in keys {
                if lane_keys.len() >= MAX_LANE_KEYS {
                    break;
                }
                lane_keys.push(k);
            }
            scan_cursor = next_cursor;
            if scan_cursor == "0" || lane_keys.len() >= MAX_LANE_KEYS {
                break;
            }
        }
    }
    if lane_keys.is_empty() {
        return Ok(ListSuspendedPage::new(Vec::new(), None));
    }

    // 2. Pipelined ZRANGE WITHSCORES on every lane ZSET. Each ZSET
    //    is small-to-medium (suspended executions on one lane);
    //    pulling the full range lets us merge + skip-past-cursor on
    //    the client side with correct (score, eid) ordering.
    let mut pipe = client.pipeline();
    let slots: Vec<_> = lane_keys
        .iter()
        .map(|k| {
            pipe.cmd::<Vec<(String, f64)>>("ZRANGE")
                .arg(k.as_str())
                .arg("0")
                .arg("-1")
                .arg("WITHSCORES")
                .finish()
        })
        .collect();
    pipe.execute().await.map_err(transport_fk)?;

    let mut merged: Vec<(ExecutionId, i64)> = Vec::new();
    for (lane_key, slot) in lane_keys.iter().zip(slots) {
        let pairs: Vec<(String, f64)> = slot.value().map_err(transport_fk)?;
        for (eid_str, score) in pairs {
            match ExecutionId::parse(&eid_str) {
                Ok(eid) => merged.push((eid, score as i64)),
                Err(e) => {
                    tracing::warn!(
                        raw_id = %eid_str,
                        error = %e,
                        zset = %lane_key,
                        "list_suspended: ZSET member failed to parse as ExecutionId"
                    );
                }
            }
        }
    }

    // 3. Sort by (score asc, execution_id lex asc) for deterministic
    //    cursor continuation.
    merged.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
    });

    // 4. Advance past cursor (exclusive) if supplied. Sort key is
    //    (score asc, eid lex asc); locate the cursor's entry and
    //    start at the next index. If the cursor's eid is no longer
    //    present (resumed between pages), fall back to eid-lex
    //    comparison.
    let start_idx = if let Some(c) = &cursor {
        if let Some(pos) = merged.iter().position(|(eid, _)| eid == c) {
            pos + 1
        } else {
            let c_str = c.to_string();
            merged
                .iter()
                .position(|(eid, _)| eid.to_string() > c_str)
                .unwrap_or(merged.len())
        }
    } else {
        0
    };

    let end_idx = (start_idx + limit).min(merged.len());
    let page: Vec<(ExecutionId, i64)> = merged[start_idx..end_idx].to_vec();
    let next_cursor = if end_idx < merged.len() {
        page.last().map(|(eid, _)| eid.clone())
    } else {
        None
    };

    if page.is_empty() {
        return Ok(ListSuspendedPage::new(Vec::new(), next_cursor));
    }

    // 5. Pipelined `HMGET suspension:current reason_code` for each
    //    returned execution. `suspension:current` lives under the
    //    execution's partition (same partition as the caller's
    //    `partition` under RFC-011 co-location, but we compute the
    //    per-eid context so alternate routing remains correct).
    let mut pipe = client.pipeline();
    let slots: Vec<_> = page
        .iter()
        .map(|(eid, _)| {
            let ep = execution_partition(eid, partition_config);
            let ctx = ExecKeyContext::new(&ep, eid);
            pipe.cmd::<Vec<Option<String>>>("HMGET")
                .arg(ctx.suspension_current())
                .arg("reason_code")
                .finish()
        })
        .collect();
    pipe.execute().await.map_err(transport_fk)?;

    let mut entries = Vec::with_capacity(page.len());
    for ((eid, score), slot) in page.into_iter().zip(slots) {
        let fields: Vec<Option<String>> = slot.value().map_err(transport_fk)?;
        let reason = fields
            .into_iter()
            .next()
            .flatten()
            .unwrap_or_default();
        entries.push(SuspendedExecutionEntry::new(eid, score, reason));
    }

    Ok(ListSuspendedPage::new(entries, next_cursor))
}

/// Parse a SCAN/SSCAN reply `[cursor, [key1, key2, ...]]`. Mirrors
/// the pattern in `ff_engine::scanner::quota_reconciler`. Used on
/// the standalone `list_suspended` path; the cluster path uses
/// `Client::cluster_scan` which returns typed values directly.
fn parse_scan_response(val: &ferriskey::Value) -> (String, Vec<String>) {
    let arr = match val {
        ferriskey::Value::Array(a) if a.len() >= 2 => a,
        _ => return ("0".to_string(), vec![]),
    };
    let cursor = match &arr[0] {
        Ok(ferriskey::Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
        _ => return ("0".to_string(), vec![]),
    };
    let mut keys = Vec::new();
    if let Ok(ferriskey::Value::Array(inner)) = &arr[1] {
        for item in inner {
            if let Ok(ferriskey::Value::BulkString(b)) = item {
                keys.push(String::from_utf8_lossy(b).into_owned());
            }
        }
    }
    (cursor, keys)
}

/// Single `HGETALL flow_core` + decode via [`build_flow_snapshot`].
/// `Ok(None)` when flow_core is absent. Decode failures surface as
/// `EngineError::Validation { kind: Corruption, .. }`.
#[tracing::instrument(
    name = "ff.describe_flow",
    skip_all,
    fields(backend = "valkey", flow_id = %id)
)]
async fn describe_flow_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    id: &FlowId,
) -> Result<Option<FlowSnapshot>, EngineError> {
    let partition = flow_partition(id, partition_config);
    let ctx = FlowKeyContext::new(&partition, id);
    let core_key = ctx.core();

    let raw: HashMap<String, String> = client
        .cmd("HGETALL")
        .arg(&core_key)
        .execute()
        .await
        .map_err(transport_fk)?;
    if raw.is_empty() {
        return Ok(None);
    }

    // RFC-016 Stage A: collect inbound-edge-group snapshots from the
    // per-downstream edgegroup hashes. Enumerate flow members, then for
    // each member that has an `in:<eid>` adjacency SET, read its
    // edgegroup hash. When the hash is absent (pre-Stage-A flow), fall
    // back to the legacy `deps_meta.unsatisfied_required_count` counter
    // on the member's exec partition (co-located under `{fp:N}`).
    let edge_groups = read_edge_groups(client, &ctx, &partition, id).await?;
    build_flow_snapshot(id.clone(), &raw, edge_groups).map(Some)
}

/// `ff_set_execution_tags` single-pair forwarder. Matches the
/// `EngineBackend::set_execution_tag` semantics — caller-namespaced key
/// validated by `ff_core::engine_backend::validate_tag_key` before
/// entry; the Lua contract re-validates server-side.
async fn set_execution_tag_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    key: &str,
    value: &str,
) -> Result<(), EngineError> {
    use ff_core::contracts::SetExecutionTagsArgs;
    use ff_script::functions::execution::ff_set_execution_tags;
    use std::collections::BTreeMap;

    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let mut tags = BTreeMap::new();
    tags.insert(key.to_owned(), value.to_owned());
    let args = SetExecutionTagsArgs {
        execution_id: execution_id.clone(),
        tags,
    };
    let _ = ff_set_execution_tags(client, &ctx, &args)
        .await
        .map_err(EngineError::from)?;
    Ok(())
}

/// `ff_set_flow_tags` single-pair forwarder.
async fn set_flow_tag_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_id: &FlowId,
    key: &str,
    value: &str,
) -> Result<(), EngineError> {
    use ff_core::contracts::SetFlowTagsArgs;
    use ff_script::functions::flow::ff_set_flow_tags;
    use std::collections::BTreeMap;

    let partition = flow_partition(flow_id, partition_config);
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let mut tags = BTreeMap::new();
    tags.insert(key.to_owned(), value.to_owned());
    let args = SetFlowTagsArgs {
        flow_id: flow_id.clone(),
        tags,
    };
    let _ = ff_set_flow_tags(client, &fctx, &args)
        .await
        .map_err(EngineError::from)?;
    Ok(())
}

/// `HGET :tags <key>` on the execution's partition. `Ok(None)` for
/// absent field or absent hash (indistinguishable on Valkey — if the
/// caller needs existence semantics, use `describe_execution` first).
async fn get_execution_tag_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    key: &str,
) -> Result<Option<String>, EngineError> {
    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let raw: Option<String> = client
        .cmd("HGET")
        .arg(ctx.tags())
        .arg(key)
        .execute()
        .await
        .map_err(transport_fk)?;
    Ok(raw)
}

/// `HGET :core namespace` on the execution's partition. `Ok(None)`
/// for absent field or absent hash (see `get_execution_tag_impl`
/// rationale).
async fn get_execution_namespace_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
) -> Result<Option<String>, EngineError> {
    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let raw: Option<String> = client
        .cmd("HGET")
        .arg(ctx.core())
        .arg("namespace")
        .execute()
        .await
        .map_err(transport_fk)?;
    Ok(raw)
}

/// `HGET :tags <key>` on the flow's partition.
async fn get_flow_tag_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_id: &FlowId,
    key: &str,
) -> Result<Option<String>, EngineError> {
    let partition = flow_partition(flow_id, partition_config);
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let raw: Option<String> = client
        .cmd("HGET")
        .arg(fctx.tags())
        .arg(key)
        .execute()
        .await
        .map_err(transport_fk)?;
    Ok(raw)
}

/// Stage A edge-group reader. Walks the flow's `members` SET, skips
/// members without inbound edges, and decodes either the edgegroup
/// hash (new) or the `deps_meta` fallback (existing flows). AllOf is
/// the only variant Stage A emits.
async fn read_edge_groups(
    client: &ferriskey::Client,
    fctx: &FlowKeyContext,
    partition: &ff_core::partition::Partition,
    _flow_id: &FlowId,
) -> Result<Vec<ff_core::contracts::EdgeGroupSnapshot>, EngineError> {
    use ff_core::contracts::{
        EdgeDependencyPolicy, EdgeGroupSnapshot, EdgeGroupState,
    };

    // Read flow members. A flow with no members has no edge groups.
    let members: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(fctx.members())
        .execute()
        .await
        .map_err(transport_fk)?;
    if members.is_empty() {
        return Ok(Vec::new());
    }

    let mut groups: Vec<EdgeGroupSnapshot> = Vec::new();
    for member_str in members {
        let member_eid = match ExecutionId::parse(&member_str) {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Skip members with no inbound edges (no group exists).
        let in_count: u64 = client
            .cmd("SCARD")
            .arg(fctx.incoming(&member_eid))
            .execute()
            .await
            .map_err(transport_fk)?;
        if in_count == 0 {
            continue;
        }

        // Prefer the edgegroup hash when present.
        let group_raw: HashMap<String, String> = client
            .cmd("HGETALL")
            .arg(fctx.edgegroup(&member_eid))
            .execute()
            .await
            .map_err(transport_fk)?;

        if !group_raw.is_empty() {
            let policy_str = group_raw
                .get("policy_variant")
                .map(String::as_str)
                .unwrap_or("all_of");
            let n: u32 = group_raw
                .get("n")
                .and_then(|s| s.parse().ok())
                .unwrap_or(in_count as u32);
            let succeeded: u32 = group_raw
                .get("succeeded")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let failed: u32 = group_raw
                .get("failed")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let skipped: u32 = group_raw
                .get("skipped")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let group_state = EdgeGroupState::from_literal(
                group_raw
                    .get("group_state")
                    .map(String::as_str)
                    .unwrap_or("pending"),
            );
            let running = n.saturating_sub(succeeded + failed + skipped);
            // RFC-016 Stage B: decode the on_satisfied / k side fields.
            let on_sat_str = group_raw
                .get("on_satisfied")
                .map(String::as_str)
                .unwrap_or("");
            let on_satisfied = match on_sat_str {
                "let_run" => ff_core::contracts::OnSatisfied::LetRun,
                _ => ff_core::contracts::OnSatisfied::CancelRemaining,
            };
            let policy = match policy_str {
                "all_of" => EdgeDependencyPolicy::AllOf,
                "any_of" => EdgeDependencyPolicy::AnyOf {
                    on_satisfied: on_satisfied.clone(),
                },
                "quorum" => {
                    let k: u32 = group_raw
                        .get("k")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1);
                    EdgeDependencyPolicy::Quorum {
                        k,
                        on_satisfied: on_satisfied.clone(),
                    }
                }
                // Unknown variant — forward-compat (§6.4). Surface as
                // AllOf so operator tooling renders something rather
                // than crashing; engines refuse the resolve path.
                _ => EdgeDependencyPolicy::AllOf,
            };
            groups.push(EdgeGroupSnapshot::new(
                member_eid.clone(),
                policy,
                n,
                succeeded,
                failed,
                skipped,
                running,
                group_state,
            ));
            continue;
        }

        // Backward-compat shim: read `deps_meta` on the member's exec
        // partition (co-located under `{fp:N}` post-RFC-011). AllOf is
        // the only meaningful policy for existing flows.
        let member_exec_ctx = ff_core::keys::ExecKeyContext::new(partition, &member_eid);
        let deps_meta_raw: HashMap<String, String> = client
            .cmd("HGETALL")
            .arg(member_exec_ctx.deps_meta())
            .execute()
            .await
            .map_err(transport_fk)?;
        let n = in_count as u32;
        let unsatisfied: u32 = deps_meta_raw
            .get("unsatisfied_required_count")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let impossible: u32 = deps_meta_raw
            .get("impossible_required_count")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        // `succeeded` derives as (n - unsatisfied - impossible) from
        // the legacy counters — accurate for the AllOf case.
        let succeeded = n.saturating_sub(unsatisfied + impossible);
        let group_state = if impossible > 0 {
            EdgeGroupState::Impossible
        } else if unsatisfied == 0 {
            EdgeGroupState::Satisfied
        } else {
            EdgeGroupState::Pending
        };
        groups.push(EdgeGroupSnapshot::new(
            member_eid,
            EdgeDependencyPolicy::AllOf,
            n,
            succeeded,
            impossible, // lump impossibles into failed_count
            0,
            unsatisfied,
            group_state,
        ));
    }

    Ok(groups)
}

/// Page the `flow_index` SET for a partition, ordered by `flow_id`
/// (UUID byte-lexicographic), and pipeline `HGETALL flow_core` per row
/// to build [`FlowSummary`] rows.
///
/// Cursor semantics: `cursor == None` starts from the smallest
/// `flow_id`; `Some(fid)` starts strictly after `fid`. `next_cursor`
/// is `Some(last_row_flow_id)` when the partition has more rows after
/// the returned page, else `None`.
///
/// Missing `flow_core` for an indexed id is surfaced as
/// `EngineError::Validation { kind: Corruption, .. }` — the same
/// posture `list_edges_impl` takes for a drifted adjacency SET. FF's
/// invariant is that a live `flow_index` entry has a present
/// `flow_core`; the flow projector removes the index entry on
/// terminal cleanup.
#[tracing::instrument(
    name = "ff.list_flows",
    skip_all,
    fields(backend = "valkey", partition = %partition)
)]
async fn list_flows_impl(
    client: &ferriskey::Client,
    partition: &ff_core::partition::PartitionKey,
    cursor: Option<FlowId>,
    limit: usize,
) -> Result<ListFlowsPage, EngineError> {
    if limit == 0 {
        return Ok(ListFlowsPage::new(Vec::new(), None));
    }

    // Parse the opaque PartitionKey into a typed Partition so we can
    // construct the {fp:N} flow-index key. Malformed keys surface as
    // validation errors at the caller's boundary.
    let part = partition.parse().map_err(|e| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::InvalidInput,
        detail: format!("list_flows: partition: {e}"),
    })?;
    let fidx = FlowIndexKeys::new(&part);
    let index_key = fidx.flow_index();

    // SMEMBERS the full index and sort client-side. The flow projector
    // keeps this SET bounded by pruning terminal flows, so partition-
    // scoped listings stay tractable. A Postgres backend would serve
    // `WHERE partition_key = $1 AND flow_id > $cursor ORDER BY
    // flow_id LIMIT $limit + 1` directly; this impl is the SET
    // equivalent.
    let all_ids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&index_key)
        .execute()
        .await
        .map_err(transport_fk)?;

    // Parse + sort by UUID bytes. Corrupt entries (non-UUID strings)
    // fail loud.
    let mut parsed: Vec<FlowId> = Vec::with_capacity(all_ids.len());
    for raw in &all_ids {
        let fid = FlowId::parse(raw).map_err(|e| EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::Corruption,
            detail: format!(
                "list_flows: flow_index: '{raw}' is not a valid FlowId \
                 (key corruption?): {e}"
            ),
        })?;
        parsed.push(fid);
    }
    // UUID byte-lexicographic order — matches the Postgres `uuid`
    // column's default ordering so the trait contract is backend-
    // agnostic.
    parsed.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

    // Slice after the cursor (exclusive). binary_search lets us skip
    // past the entire prefix in O(log n) rather than a linear scan.
    let start = match &cursor {
        Some(c) => match parsed.binary_search_by(|probe| probe.as_bytes().cmp(c.as_bytes())) {
            Ok(pos) => pos + 1,
            Err(pos) => pos,
        },
        None => 0,
    };

    let page: Vec<FlowId> = parsed.iter().skip(start).take(limit).cloned().collect();
    if page.is_empty() {
        return Ok(ListFlowsPage::new(Vec::new(), None));
    }
    let has_more = start + page.len() < parsed.len();

    // Pipeline one HGETALL flow_core per page row. All keys share the
    // partition's {fp:N} tag so a single pipeline is cluster-safe.
    let mut pipe = client.pipeline();
    let slots: Vec<_> = page
        .iter()
        .map(|fid| {
            let fctx = FlowKeyContext::new(&part, fid);
            pipe.cmd::<HashMap<String, String>>("HGETALL")
                .arg(fctx.core())
                .finish()
        })
        .collect();
    pipe.execute().await.map_err(transport_fk)?;

    let mut flows: Vec<FlowSummary> = Vec::with_capacity(page.len());
    for (fid, slot) in page.iter().zip(slots) {
        let raw = slot.value().map_err(transport_fk)?;
        if raw.is_empty() {
            // flow_index drift — index entry without a flow_core. FF
            // invariants say the projector removes the index entry
            // before the flow_core disappears; treat as corruption.
            return Err(EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::Corruption,
                detail: format!(
                    "list_flows: flow_index entry '{fid}' has no flow_core \
                     (index/core drift — projector bug?)"
                ),
            });
        }
        // list_flows returns the lightweight summary — edge groups
        // are not surfaced here; pass an empty vec to save the extra
        // per-member HGETALL pipeline rounds.
        let snap = build_flow_snapshot(fid.clone(), &raw, Vec::new())?;
        flows.push(FlowSummary::new(
            snap.flow_id,
            snap.created_at,
            FlowStatus::from_public_flow_state(&snap.public_flow_state),
        ));
    }

    let next_cursor = if has_more {
        flows.last().map(|f| f.flow_id.clone())
    } else {
        None
    };
    Ok(ListFlowsPage::new(flows, next_cursor))
}

/// Read all edges adjacent to `subject_eid` on the requested side.
///
/// Mirrors the ff-sdk free-fn `list_edges_from_set` pipeline shape
/// (`SMEMBERS adj_set` + pipelined `HGETALL edge_hash`) but routes
/// every parse / identity failure through
/// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`]
/// via [`ff_core::contracts::decode::build_edge_snapshot`]. The
/// caller's `flow_id` is trusted — unlike the ff-sdk free-fn there
/// is no `HGET exec_core.flow_id` resolution round trip; the trait
/// method requires callers to pass the flow id they already know.
///
/// The adjacency SET's endpoint cross-check still runs here (the
/// returned edge's `upstream_execution_id` for Outgoing, or
/// `downstream_execution_id` for Incoming, must match
/// `direction.subject()`) so a drifted SET entry does not silently
/// surface an unrelated edge to the caller.
#[tracing::instrument(
    name = "ff.list_edges",
    skip_all,
    fields(backend = "valkey", flow_id = %flow_id)
)]
async fn list_edges_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_id: &FlowId,
    direction: EdgeDirection,
) -> Result<Vec<EdgeSnapshot>, EngineError> {
    let partition = flow_partition(flow_id, partition_config);
    let fctx = FlowKeyContext::new(&partition, flow_id);

    let (adj_key, subject_eid, side_is_outgoing) = match &direction {
        EdgeDirection::Outgoing { from_node } => (fctx.outgoing(from_node), from_node, true),
        EdgeDirection::Incoming { to_node } => (fctx.incoming(to_node), to_node, false),
    };

    let edge_id_strs: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&adj_key)
        .execute()
        .await
        .map_err(transport_fk)?;
    if edge_id_strs.is_empty() {
        return Ok(Vec::new());
    }

    // Parse every edge id up front so a corrupt SET entry fails loud
    // before we spend a round trip on it. Mirrors the ff-sdk posture.
    let mut edge_ids: Vec<EdgeId> = Vec::with_capacity(edge_id_strs.len());
    for raw in &edge_id_strs {
        let parsed = EdgeId::parse(raw).map_err(|e| EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::Corruption,
            detail: format!(
                "list_edges: adjacency_set: edge_id: '{raw}' is not a valid EdgeId \
                 (key corruption?): {e}"
            ),
        })?;
        edge_ids.push(parsed);
    }

    let mut pipe = client.pipeline();
    let slots: Vec<_> = edge_ids
        .iter()
        .map(|eid| {
            pipe.cmd::<HashMap<String, String>>("HGETALL")
                .arg(fctx.edge(eid))
                .finish()
        })
        .collect();
    pipe.execute().await.map_err(transport_fk)?;

    let mut out: Vec<EdgeSnapshot> = Vec::with_capacity(edge_ids.len());
    for (edge_id, slot) in edge_ids.iter().zip(slots) {
        let raw = slot.value().map_err(transport_fk)?;
        if raw.is_empty() {
            // Adjacency SET references an edge hash that no longer
            // exists. FF never deletes edge hashes (staging is
            // write-once) so treat as corruption.
            return Err(EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::Corruption,
                detail: format!(
                    "list_edges: adjacency_set: refers to edge_id '{edge_id}' but its \
                     edge_hash is absent (key corruption?)"
                ),
            });
        }
        let snap = build_edge_snapshot(flow_id, edge_id, &raw)?;
        // Endpoint cross-check: the decoded edge's endpoint on the
        // listed side must match the subject execution.
        let endpoint = if side_is_outgoing {
            &snap.upstream_execution_id
        } else {
            &snap.downstream_execution_id
        };
        if endpoint != subject_eid {
            let side = if side_is_outgoing {
                "Outgoing"
            } else {
                "Incoming"
            };
            return Err(EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::Corruption,
                detail: format!(
                    "list_edges: adjacency_set: for execution '{subject_eid}' \
                     (side={side}) contains edge '{edge_id}' whose stored endpoint is \
                     '{endpoint}' (adjacency/edge-hash drift?)"
                ),
            });
        }
        out.push(snap);
    }
    Ok(out)
}

/// Single `HGETALL edge_hash` + decode via [`build_edge_snapshot`].
/// `Ok(None)` when the edge hash is absent. Decode failures surface
/// as `EngineError::Validation { kind: Corruption, .. }`.
#[tracing::instrument(
    name = "ff.describe_edge",
    skip_all,
    fields(backend = "valkey", flow_id = %flow_id, edge_id = %edge_id)
)]
async fn describe_edge_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_id: &FlowId,
    edge_id: &EdgeId,
) -> Result<Option<EdgeSnapshot>, EngineError> {
    let partition = flow_partition(flow_id, partition_config);
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let edge_key = fctx.edge(edge_id);

    let raw: HashMap<String, String> = client
        .cmd("HGETALL")
        .arg(&edge_key)
        .execute()
        .await
        .map_err(transport_fk)?;
    if raw.is_empty() {
        return Ok(None);
    }
    build_edge_snapshot(flow_id, edge_id, &raw).map(Some)
}

/// Single `HGET exec_core flow_id` + parse. `Ok(None)` when the
/// exec_core hash is absent OR the `flow_id` field is empty
/// (standalone execution). A present-but-malformed value surfaces as
/// `EngineError::Validation { kind: Corruption, .. }`.
#[tracing::instrument(
    name = "ff.resolve_execution_flow_id",
    skip_all,
    fields(backend = "valkey", execution_id = %eid)
)]
async fn resolve_execution_flow_id_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    eid: &ExecutionId,
) -> Result<Option<FlowId>, EngineError> {
    let partition = execution_partition(eid, partition_config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let raw: Option<String> = client
        .cmd("HGET")
        .arg(ctx.core())
        .arg("flow_id")
        .execute()
        .await
        .map_err(transport_fk)?;
    let Some(raw) = raw.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let flow_id = FlowId::parse(&raw).map_err(|e| EngineError::Validation {
        kind: ff_core::engine_error::ValidationKind::Corruption,
        detail: format!(
            "resolve_execution_flow_id: exec_core: flow_id: '{raw}' is not a valid UUID \
             (key corruption?): {e}"
        ),
    })?;
    Ok(Some(flow_id))
}

/// Read the global lane registry (`ff:idx:lanes`) as a sorted page.
///
/// `SMEMBERS ff:idx:lanes` + validate each entry via
/// [`LaneId::try_new`] (corrupt entries surface as
/// [`EngineError::Validation { kind: ValidationKind::Corruption, .. }`])
/// + sort by lane name + slice on `(cursor, limit)`. The SET is
/// bounded by the registry size, not the per-lane execution count, so
/// a single-shot read is cheap enough for the registry sizes FF
/// targets.
///
/// `ff:idx:lanes` has no hash tag — in cluster deployments it lives
/// on its own slot, which is fine for a single-key SMEMBERS read.
#[tracing::instrument(
    name = "ff.list_lanes",
    skip_all,
    fields(backend = "valkey", limit)
)]
async fn list_lanes_impl(
    client: &ferriskey::Client,
    cursor: Option<LaneId>,
    limit: usize,
) -> Result<ListLanesPage, EngineError> {
    if limit == 0 {
        return Ok(ListLanesPage::new(Vec::new(), None));
    }

    let key = ff_core::keys::lanes_index_key();
    let raw: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&key)
        .execute()
        .await
        .map_err(transport_fk)?;

    // Validate + parse every member up front. A bad entry (e.g. an
    // empty string or a lane name that violates `LaneId::try_new`
    // bounds) indicates registry corruption and fails loud rather
    // than silently dropping.
    let mut lanes: Vec<LaneId> = Vec::with_capacity(raw.len());
    for entry in raw {
        let lane = LaneId::try_new(entry.clone()).map_err(|e| EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::Corruption,
            detail: format!(
                "list_lanes: ff:idx:lanes: member '{entry}' is not a valid LaneId \
                 (key corruption?): {e:?}"
            ),
        })?;
        lanes.push(lane);
    }
    lanes.sort();

    // Slice `(cursor, cursor+limit]` — cursor is exclusive.
    let start = match cursor {
        Some(c) => lanes.partition_point(|l| l <= &c),
        None => 0,
    };
    let end = start.saturating_add(limit).min(lanes.len());
    let page: Vec<LaneId> = lanes[start..end].to_vec();
    let next_cursor = if end < lanes.len() {
        page.last().cloned()
    } else {
        None
    };
    Ok(ListLanesPage::new(page, next_cursor))
}

/// Partition-scoped forward-only cursor listing of executions.
///
/// Reads `SMEMBERS ff:idx:{p:N}:all_executions`, parses every member
/// as [`ExecutionId`] (failing loud on corruption), sorts lexicographic
/// (stable across calls — ExecutionId prefix is the partition hash tag
/// so intra-partition order is UUID-suffix order), filters to members
/// strictly greater than `cursor`, and truncates to `limit`.
/// `next_cursor` is the last emitted id iff at least one more member
/// remains past the page boundary.
///
/// v1 note: a full `SMEMBERS` scan per page is acceptable for
/// partitions holding O(10k) executions (retention trims terminal ids
/// out; live-set cardinality stays bounded by worker concurrency). A
/// future optimisation may introduce a parallel ZSET keyed by
/// `created_at` so paging becomes a constant-time `ZRANGEBYSCORE` —
/// that is out of scope for the list-executions trait landing.
#[tracing::instrument(
    name = "ff.list_executions",
    skip_all,
    fields(backend = "valkey", partition = %partition_key)
)]
async fn list_executions_impl(
    client: &ferriskey::Client,
    partition_key: &PartitionKey,
    cursor: Option<&ExecutionId>,
    limit: usize,
) -> Result<ListExecutionsPage, EngineError> {
    // `limit == 0` is a legitimate caller request (e.g. probing for
    // cursor validity); short-circuit before touching Valkey.
    if limit == 0 {
        return Ok(ListExecutionsPage::new(Vec::new(), None));
    }

    let partition = partition_key
        .parse()
        .map_err(|e| EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::InvalidInput,
            detail: format!(
                "list_executions: partition: '{partition_key}' is not a valid PartitionKey: {e}"
            ),
        })?;
    let idx = IndexKeys::new(&partition);
    let all_key = idx.all_executions();

    let raw_members: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&all_key)
        .execute()
        .await
        .map_err(transport_fk)?;

    if raw_members.is_empty() {
        return Ok(ListExecutionsPage::new(Vec::new(), None));
    }

    // Parse every member up-front; a corrupt SET entry surfaces as
    // Validation { Corruption } rather than being silently skipped.
    let mut parsed: Vec<ExecutionId> = Vec::with_capacity(raw_members.len());
    for raw in &raw_members {
        let eid = ExecutionId::parse(raw).map_err(|e| EngineError::Validation {
            kind: ff_core::engine_error::ValidationKind::Corruption,
            detail: format!(
                "list_executions: {all_key}: member: '{raw}' is not a valid ExecutionId \
                 (key corruption?): {e}"
            ),
        })?;
        parsed.push(eid);
    }

    // Lex sort on the wire string so the ordering is stable across
    // concurrent inserts (ExecutionId has no Ord impl).
    parsed.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    // Forward-only cursor: take members strictly greater than cursor.
    let filtered: Vec<ExecutionId> = if let Some(c) = cursor {
        let cs = c.as_str();
        parsed
            .into_iter()
            .filter(|e| e.as_str() > cs)
            .collect()
    } else {
        parsed
    };

    // Cap limit at 1000 per RFC-012 read-surface defaults.
    let effective_limit = limit.min(1000);
    let has_more = filtered.len() > effective_limit;
    let page: Vec<ExecutionId> = filtered.into_iter().take(effective_limit).collect();
    let next_cursor = if has_more { page.last().cloned() } else { None };
    Ok(ListExecutionsPage::new(page, next_cursor))
}

/// Read the deployment's partition config from
/// `ff:config:partitions`. Keeps `ValkeyBackend` aligned with
/// ff-server's published `num_flow_partitions` / budget / quota
/// counts. Mirrors the ff-sdk `worker::read_partition_config` helper
/// (Stage 1c will deduplicate once the hot-path migration lands).
async fn load_partition_config(client: &ferriskey::Client) -> Result<PartitionConfig, EngineError> {
    let key = ff_core::keys::global_config_partitions();
    let fields: HashMap<String, String> = client
        .hgetall(&key)
        .await
        .map_err(|e| transport_script(ScriptError::Valkey(e)))?;
    if fields.is_empty() {
        // Distinct Err so `connect()` can warn-and-default instead
        // of silently routing with the wrong partition counts.
        // Mirrors `ff-sdk::worker::read_partition_config`'s
        // error-on-missing + warn-at-call-site pattern (#111).
        return Err(transport_script(ScriptError::Parse {
            fcall: "load_partition_config".into(),
            execution_id: None,
            message: format!("{key} not found in Valkey"),
        }));
    }
    let parse = |field: &str, default: u16| -> u16 {
        fields
            .get(field)
            .and_then(|v| v.parse().ok())
            .filter(|&n: &u16| n > 0)
            .unwrap_or(default)
    };
    Ok(PartitionConfig {
        num_flow_partitions: parse("num_flow_partitions", 256),
        num_budget_partitions: parse("num_budget_partitions", 32),
        num_quota_partitions: parse("num_quota_partitions", 32),
    })
}

fn now_ms_timestamp() -> TimestampMs {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    TimestampMs::from_millis(now)
}

/// Map a ferriskey transport error into an `EngineError::Transport` with
/// the Valkey backend tag + a `ScriptError::Valkey` payload (so
/// `valkey_kind()` downcasts still recover the `ErrorKind`). Used by
/// every Stage 1b forwarder that bypasses the typed `ff_function!`
/// wrappers in favour of direct `client.fcall(...)` to preserve
/// byte-for-byte KEYS/ARGV parity with the SDK's pre-migration code.
fn transport_fk(e: ferriskey::Error) -> EngineError {
    transport_script(ScriptError::Valkey(e))
}

/// RFC-017 §8 (Stage D1): parse a stored `waitpoint_token` of the form
/// `<kid>:<40hex>` into its `(kid, 16-hex-prefix-of-digest)` components.
///
/// Robust against malformed input: returns `("", "")` when the stored
/// value does not match the expected shape. Callers log + skip on
/// empty tokens upstream, so this helper never panics or surfaces a
/// typed error — the presence of the raw token is already checked
/// before this runs.
fn parse_waitpoint_token_kid_fp(raw: &str) -> (String, String) {
    match raw.split_once(':') {
        Some((kid, hex)) if !kid.is_empty() && !hex.is_empty() => {
            let fp_len = hex.len().min(16);
            (kid.to_owned(), hex[..fp_len].to_owned())
        }
        _ => (String::new(), String::new()),
    }
}

/// Parse a raw `{1, "OK", ...}` / `{0, "error", ...}` FCALL result into
/// `EngineError` on the error path. The success path's field vector is
/// discarded; callers that need fields fall through to
/// `parse_success_fields` below.
fn parse_success_only(raw: &ferriskey::Value) -> Result<(), EngineError> {
    let _ = FcallResult::parse(raw)
        .map_err(EngineError::from)?
        .into_success()
        .map_err(EngineError::from)?;
    Ok(())
}

/// Stage 1b — `renew` FCALL body. Migrated from
/// `ff_sdk::task::renew_lease_inner` with byte-for-byte KEYS/ARGV
/// parity (lease_history_grace_ms = 5000, 4 KEYS, 7 ARGV). The
/// `LeaseRenewal` return is synthesised from the Lua reply's
/// `expires_at`; `lease_epoch` is threaded from the caller's handle
/// (Lua's `ff_renew_lease` does not bump epoch, so the handle's value
/// is still authoritative).
#[tracing::instrument(
    level = "debug",
    name = "ff.renew",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn renew_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
) -> Result<LeaseRenewal, EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
    ];

    // `lease_history_grace_ms = 5000` preserved from the pre-Stage-1b
    // SDK body (ff_sdk::task::renew_lease_inner). Diverges from the
    // `RenewLeaseArgs::lease_history_grace_ms` serde default (60_000) —
    // the SDK's 5_000 is load-bearing for cleanup timing and must not
    // change under this refactor.
    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.attempt_index.to_string(),
        f.attempt_id.to_string(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
        f.lease_ttl_ms.to_string(),
        "5000".to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_renew_lease", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    let parsed = FcallResult::parse(&raw)
        .map_err(EngineError::from)?
        .into_success()
        .map_err(EngineError::from)?;
    // Lua returns: ok(new_expires_at_string). Surface parse failure
    // as `Transport` (wraps `ScriptError::Parse`) so callers' existing
    // error-handling paths (which already branch on transport +
    // ScriptError::Parse downcast) continue to fire.
    let expires_ms: i64 = parsed.field_str(0).parse().map_err(|_| {
        EngineError::from(ScriptError::Parse {
            fcall: "ff_renew_lease".into(),
            execution_id: None,
            message: format!("invalid expires_at: {}", parsed.field_str(0)),
        })
    })?;
    Ok(LeaseRenewal::new(expires_ms.max(0) as u64, f.lease_epoch.0))
}

/// Stage 1b — `progress` FCALL body. Migrated from
/// `ff_sdk::task::ClaimedTask::update_progress`. 1 KEY, 5 ARGV.
#[tracing::instrument(
    level = "debug",
    name = "ff.progress",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn progress_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    percent: Option<u8>,
    message: Option<&str>,
) -> Result<(), EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);

    let keys: Vec<String> = vec![ctx.core()];
    // Pre-migration SDK always sent a pct byte and a message string.
    // Preserve that wire by defaulting `None` to empty / 0 so the Lua
    // function sees the exact same ARGV shape.
    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
        percent
            .map(|p| p.to_string())
            .unwrap_or_else(|| "0".to_string()),
        message.unwrap_or("").to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_update_progress", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    parse_success_only(&raw)
}

/// Stage 1b — `observe_signals` body. Migrated from
/// `ff_sdk::task::ClaimedTask::resume_signals`. Reads
/// `suspension:current`, filters by `attempt_index`, pulls matched
/// `signal_id`s via `HMGET`, then pipelines per-signal
/// `HGETALL signal_hash` + `GET signal_payload`.
#[tracing::instrument(
    level = "debug",
    name = "ff.observe_signals",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn observe_signals_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
) -> Result<Vec<ResumeSignal>, EngineError> {
    use std::collections::HashMap;

    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);

    let susp: HashMap<String, String> = client
        .hgetall(&ctx.suspension_current())
        .await
        .map_err(transport_fk)?;

    let Some(waitpoint_id) = resume_waitpoint_id_from_suspension(&susp, f.attempt_index)? else {
        return Ok(Vec::new());
    };

    let wp_cond_key = ctx.waitpoint_condition(&waitpoint_id);
    // RFC-014: composite suspensions stash the satisfier signal id list
    // under `suspension_current.all_satisfier_signals` (JSON array).
    // Fall through to the legacy matcher-array path only for non-
    // composite (RFC-013 Single/Operator/Timeout) conditions.
    let mut signal_ids: Vec<SignalId> = Vec::new();
    if susp.get("all_satisfier_signals").map(|s| !s.is_empty()).unwrap_or(false) {
        let raw = susp.get("all_satisfier_signals").cloned().unwrap_or_default();
        if let Ok(arr) = serde_json::from_str::<Vec<String>>(&raw) {
            for s in arr {
                if let Ok(sid) = SignalId::parse(&s) {
                    signal_ids.push(sid);
                }
            }
        }
    }
    if signal_ids.is_empty() {
        let total_str: Option<String> = client
            .hget(&wp_cond_key, "total_matchers")
            .await
            .map_err(transport_fk)?;
        let total: usize = total_str
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        for i in 0..total {
            let fields: Vec<Option<String>> = client
                .cmd("HMGET")
                .arg(&wp_cond_key)
                .arg(format!("matcher:{i}:satisfied"))
                .arg(format!("matcher:{i}:signal_id"))
                .execute()
                .await
                .map_err(transport_fk)?;
            let satisfied = fields.first().and_then(|o| o.as_deref());
            if satisfied != Some("1") {
                continue;
            }
            let Some(raw) = fields
                .get(1)
                .and_then(|o| o.as_deref())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            match SignalId::parse(raw) {
                Ok(sid) => signal_ids.push(sid),
                Err(e) => {
                    tracing::warn!(
                        execution_id = %f.execution_id,
                        waitpoint_id = %waitpoint_id,
                        raw = %raw,
                        error = %e,
                        "observe_signals: matcher signal_id failed to parse, skipping"
                    );
                }
            }
        }
    }

    if signal_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut pipe = client.pipeline();
    let mut slots = Vec::with_capacity(signal_ids.len());
    for signal_id in &signal_ids {
        let hash_slot = pipe
            .cmd::<HashMap<String, String>>("HGETALL")
            .arg(ctx.signal(signal_id))
            .finish();
        let payload_slot = pipe
            .cmd::<Option<ferriskey::Value>>("GET")
            .arg(ctx.signal_payload(signal_id))
            .finish();
        slots.push((hash_slot, payload_slot));
    }
    pipe.execute().await.map_err(transport_fk)?;

    let mut out: Vec<ResumeSignal> = Vec::with_capacity(signal_ids.len());
    for (signal_id, (hash_slot, payload_slot)) in signal_ids.into_iter().zip(slots) {
        let sig: HashMap<String, String> = hash_slot.value().map_err(transport_fk)?;
        if sig.is_empty() {
            continue;
        }
        let payload_raw: Option<ferriskey::Value> = payload_slot.value().map_err(transport_fk)?;
        let payload: Option<Vec<u8>> = match payload_raw {
            Some(ferriskey::Value::BulkString(b)) => Some(b.to_vec()),
            Some(ferriskey::Value::SimpleString(s)) => Some(s.into_bytes()),
            _ => None,
        };
        let accepted_at = sig
            .get("accepted_at")
            .and_then(|s| s.parse::<i64>().ok())
            .map(TimestampMs::from_millis)
            .unwrap_or_else(|| TimestampMs::from_millis(0));

        out.push(ResumeSignal {
            signal_id,
            signal_name: sig.get("signal_name").cloned().unwrap_or_default(),
            signal_category: sig.get("signal_category").cloned().unwrap_or_default(),
            source_type: sig.get("source_type").cloned().unwrap_or_default(),
            source_identity: sig.get("source_identity").cloned().unwrap_or_default(),
            correlation_id: sig.get("correlation_id").cloned().unwrap_or_default(),
            accepted_at,
            payload,
        });
    }
    Ok(out)
}

/// Port of `ff_sdk::task::resume_waitpoint_id_from_suspension` — same
/// invariants, same error shape. Kept crate-local (module-private) so
/// Stage 1c consolidation can decide whether to promote.
fn resume_waitpoint_id_from_suspension(
    susp: &std::collections::HashMap<String, String>,
    claimed_attempt: AttemptIndex,
) -> Result<Option<WaitpointId>, EngineError> {
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
        EngineError::from(ScriptError::Parse {
            fcall: "observe_signals".into(),
            execution_id: None,
            message: format!(
                "observe_signals: suspension_current.waitpoint_id is not a valid UUID: {e}"
            ),
        })
    })?;
    Ok(Some(waitpoint_id))
}

// ── RFC-012 §R7: append_frame / create_waitpoint / report_usage bodies ──

/// Map `FrameKind` → the Lua-side `frame_type` string. The Lua wire is
/// free-form (`ff_append_frame` stores `frame_type` opaquely), so the
/// mapping is a stable encoding of the enum variant names matching the
/// values the SDK callers used pre-migration.
fn frame_kind_to_str(k: ff_core::backend::FrameKind) -> &'static str {
    match k {
        ff_core::backend::FrameKind::Stdout => "stdout",
        ff_core::backend::FrameKind::Stderr => "stderr",
        ff_core::backend::FrameKind::Event => "event",
        ff_core::backend::FrameKind::Blob => "blob",
        // `FrameKind` is `#[non_exhaustive]`. Unknown variants fall
        // back to "event" (the most generic of the four) so a
        // newly-added kind does not hard-fail an append on an
        // intermediate-version backend; follow-up PRs that add a
        // variant update this match explicitly.
        _ => "event",
    }
}

/// Round-7 — `append_frame` FCALL body. Migrated from
/// `ff_sdk::task::ClaimedTask::append_frame`. 3 KEYS, 13 ARGV.
///
/// Byte-for-byte ARGV parity with the SDK's pre-migration call (see
/// `crates/ff-sdk/src/task.rs` at the `ff_append_frame` FCALL site):
/// retention_maxlen = "10000", source = "worker",
/// max_payload_bytes = "65536", encoding = "utf8". A trait-level knob
/// for those constants is future work (RFC-012 §R7.5.6 shape
/// commitment — not changing the wire under this refactor).
#[tracing::instrument(
    level = "debug",
    name = "ff.append_frame",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn append_frame_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    frame: Frame,
) -> Result<AppendFrameOutcome, EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);

    let now = now_ms_timestamp();
    let payload_str = String::from_utf8_lossy(&frame.bytes).into_owned();
    // Free-form `frame_type` wins when populated (SDK forwarder path
    // sets it to "delta" / "agent_step" / etc.); otherwise fall back
    // to the stable `FrameKind` encoding for typed-only callers.
    let frame_type: String = if frame.frame_type.is_empty() {
        frame_kind_to_str(frame.kind).to_owned()
    } else {
        frame.frame_type.clone()
    };
    let correlation_id = frame.correlation_id.clone().unwrap_or_default();

    // KEYS (4): exec_core, stream_data, stream_meta, stream_summary
    // (stream_summary added for RFC-015 DurableSummary; co-located in
    // the same `{p:N}` slot so the Lua applier can HGET/HSET atomically).
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.stream(f.attempt_index),
        ctx.stream_meta(f.attempt_index),
        ctx.stream_summary(f.attempt_index),
    ];

    // Durability-mode wire encoding (RFC-015 §1). `StreamMode` and
    // `PatchKind` are `#[non_exhaustive]`; the wildcard arms default
    // newer variants to the pre-RFC-015 durable wire encoding (safe
    // fallback) so an intermediate-version backend never silently
    // mis-applies a newer mode.
    // `mode_wire`, `patch_kind_wire`, `ttl_ms_wire` + the three RFC-015
    // §4.2 dynamic-MAXLEN knobs (maxlen_floor, maxlen_ceiling, ema_alpha).
    // The knobs are zero'd for non-BestEffortLive modes — Lua ignores
    // ARGV 17-19 unless `stream_mode == "best_effort"`.
    let (mode_wire, patch_kind_wire, ttl_ms_wire, maxlen_floor, maxlen_ceiling, ema_alpha): (
        &str,
        &str,
        String,
        String,
        String,
        String,
    ) = match frame.mode {
        StreamMode::Durable => (
            "durable",
            "",
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
        ),
        StreamMode::DurableSummary { patch_kind } => {
            let pk = match patch_kind {
                PatchKind::JsonMergePatch => "json-merge-patch",
                _ => "json-merge-patch",
            };
            (
                "summary",
                pk,
                "0".to_owned(),
                "0".to_owned(),
                "0".to_owned(),
                "0".to_owned(),
            )
        }
        StreamMode::BestEffortLive { config } => (
            "best_effort",
            "",
            config.ttl_ms.to_string(),
            config.maxlen_floor.to_string(),
            config.maxlen_ceiling.to_string(),
            format!("{:.6}", config.ema_alpha),
        ),
        _ => (
            "durable",
            "",
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
        ),
    };

    // ARGV (19): execution_id, attempt_index, lease_id, lease_epoch,
    //            frame_type, ts, payload, encoding, correlation_id,
    //            source, retention_maxlen, attempt_id, max_payload_bytes,
    //            stream_mode, patch_kind, ttl_ms,
    //            maxlen_floor, maxlen_ceiling, ema_alpha
    // RFC-015 wire evolution:
    //   - Pre-RFC-015: 13 ARGV (durable-only).
    //   - RFC-015 Phase 1/2: ARGV 14-16 added (mode, patch_kind, ttl_ms).
    //   - RFC-015 §4.2 dynamic MAXLEN: ARGV 17-19 added
    //     (maxlen_floor, maxlen_ceiling, ema_alpha). Lua defaults
    //     missing ARGV to the §4.2 RFC-final values, so older Rust
    //     callers stay backwards-compatible.
    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.attempt_index.to_string(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
        frame_type,
        now.to_string(),
        payload_str,
        "utf8".to_owned(),
        correlation_id,
        "worker".to_owned(),
        "10000".to_owned(),
        f.attempt_id.to_string(),
        "65536".to_owned(),
        mode_wire.to_owned(),
        patch_kind_wire.to_owned(),
        ttl_ms_wire,
        maxlen_floor,
        maxlen_ceiling,
        ema_alpha,
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_append_frame", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    let parsed = FcallResult::parse(&raw)
        .map_err(EngineError::from)?
        .into_success()
        .map_err(EngineError::from)?;

    // ok(entry_id, frame_count[, summary_version]) — the third field is
    // only present on DurableSummary appends (RFC-015 §9).
    let stream_id = parsed.field_str(0);
    let frame_count: u64 = parsed.field_str(1).parse().unwrap_or(0);
    let summary_version: Option<u64> = {
        let raw = parsed.field_str(2);
        if raw.is_empty() {
            None
        } else {
            raw.parse().ok()
        }
    };

    let mut outcome = AppendFrameOutcome::new(stream_id, frame_count);
    if let Some(v) = summary_version {
        outcome = outcome.with_summary_version(v);
    }
    Ok(outcome)
}

/// Compute `expires_at_ms` for a pending waitpoint, clamping overflow to
/// `i64::MAX` to match the PG + SQLite `create_waitpoint` behaviour
/// (see `ff-backend-postgres::suspend_ops::create_waitpoint` ~L722-730 and
/// `ff-backend-sqlite::suspend_ops::create_waitpoint_impl`). A stricter
/// "overflow => Validation" contract would require a one-shot trait-docs +
/// all-three-backends change; keep parity until then.
fn waitpoint_expires_at_ms(now_ms: i64, expires_in: Duration) -> i64 {
    let expires_in_ms = i64::try_from(expires_in.as_millis()).unwrap_or(i64::MAX);
    now_ms.saturating_add(expires_in_ms)
}

/// Round-7 — `create_waitpoint` FCALL body. Migrated from
/// `ff_sdk::task::ClaimedTask::create_pending_waitpoint`. 4 KEYS, 5
/// ARGV. Mints a fresh `WaitpointId` client-side and returns the
/// server-assigned HMAC token.
#[tracing::instrument(
    name = "ff.create_waitpoint",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn create_waitpoint_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    waitpoint_key: &str,
    expires_in: Duration,
) -> Result<PendingWaitpoint, EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    let waitpoint_id = WaitpointId::new();
    let expires_at = TimestampMs::from_millis(waitpoint_expires_at_ms(
        now_ms_timestamp().0,
        expires_in,
    ));

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint(&waitpoint_id),
        idx.pending_waitpoint_expiry(),
        idx.waitpoint_hmac_secrets(),
    ];

    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.attempt_index.to_string(),
        waitpoint_id.to_string(),
        waitpoint_key.to_owned(),
        expires_at.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_create_pending_waitpoint", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    let parsed = FcallResult::parse(&raw)
        .map_err(EngineError::from)?
        .into_success()
        .map_err(EngineError::from)?;

    // Response fields (after status+OK): waitpoint_id, waitpoint_key, waitpoint_token.
    let token_str = parsed.field_str(2);
    if token_str.is_empty() {
        return Err(EngineError::from(ScriptError::Parse {
            fcall: "ff_create_pending_waitpoint".into(),
            execution_id: Some(f.execution_id.to_string()),
            message: "missing waitpoint_token in response".into(),
        }));
    }

    Ok(PendingWaitpoint::new(
        waitpoint_id,
        ff_core::backend::WaitpointHmac::new(token_str),
    ))
}

/// Point-read of a waitpoint's stored HMAC token. Targets the
/// `ff:wp:<tag>:<wp_id>` hash's `waitpoint_token` field (written by
/// the Lua suspension path, see `lua/suspension.lua:289`).
///
/// Empty string / missing field / missing hash all map to
/// `Ok(None)` — signal delivery can legitimately race waitpoint
/// consumption or expiry, and the signal-bridge authenticates on the
/// token presence rather than on waitpoint liveness. The raw
/// partition hash-tag is extracted from the opaque
/// [`PartitionKey`] so this helper does not require a
/// [`PartitionConfig`] round-trip.
#[tracing::instrument(
    name = "ff.read_waitpoint_token",
    skip_all,
    fields(backend = "valkey", waitpoint_id = %waitpoint_id)
)]
async fn read_waitpoint_token_impl(
    client: &ferriskey::Client,
    partition: &PartitionKey,
    waitpoint_id: &WaitpointId,
) -> Result<Option<String>, EngineError> {
    let key = format!("ff:wp:{}:{}", partition.as_str(), waitpoint_id);
    let raw: Option<String> = client
        .cmd("HGET")
        .arg(&key)
        .arg("waitpoint_token")
        .execute()
        .await
        .map_err(transport_fk)?;
    Ok(raw.filter(|s| !s.is_empty()))
}

/// Round-7 — `report_usage` FCALL body. Migrated from
/// `ff_sdk::task::ClaimedTask::report_usage`. 3 KEYS, N ARGV (variable
/// by dimension count).
///
/// `UsageDimensions::custom` carries `(dim_name, delta)` pairs; the
/// trait today exposes only the `custom` map (plus `input_tokens /
/// output_tokens / wall_ms` as reserved fields). The wire transmits
/// only the caller-supplied custom dimensions in the same format the
/// SDK used pre-migration: dim_count, dim_1..N, delta_1..N, now_ms,
/// dedup_key. `input_tokens`/`output_tokens`/`wall_ms` are currently
/// reserved-but-inert on the wire — the SDK never surfaced them
/// either, so preserving that behaviour keeps byte-for-byte wire
/// parity.
#[tracing::instrument(
    name = "ff.report_usage",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
        budget_id = %budget,
    )
)]
async fn report_usage_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    budget: &BudgetId,
    dimensions: UsageDimensions,
) -> Result<ReportUsageResult, EngineError> {
    use ff_core::keys::{usage_dedup_key, BudgetKeyContext};
    use ff_core::partition::budget_partition;

    let partition = budget_partition(budget, partition_config);
    let bctx = BudgetKeyContext::new(&partition, budget);

    let keys: Vec<String> = vec![bctx.usage(), bctx.limits(), bctx.definition()];

    let now = now_ms_timestamp();
    let dim_count = dimensions.custom.len();
    let mut argv: Vec<String> = Vec::with_capacity(3 + dim_count * 2);
    argv.push(dim_count.to_string());
    for name in dimensions.custom.keys() {
        argv.push(name.clone());
    }
    for delta in dimensions.custom.values() {
        argv.push(delta.to_string());
    }
    argv.push(now.to_string());
    let dedup_key_val = dimensions
        .dedup_key
        .as_deref()
        .filter(|k| !k.is_empty())
        .map(|k| usage_dedup_key(bctx.hash_tag(), k))
        .unwrap_or_default();
    argv.push(dedup_key_val);

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_report_usage_and_check", &key_refs, &argv_refs)
        .await
        .map_err(transport_fk)?;

    parse_report_usage(&raw, &f.execution_id)
}

/// Parse a `ff_report_usage_and_check` reply into `ReportUsageResult`.
///
/// Lua wire: `{1, "OK"}`, `{1, "ALREADY_APPLIED"}`,
/// `{1, "SOFT_BREACH", dim, current, limit}`,
/// `{1, "HARD_BREACH", dim, current, limit}`; `{0, <code>, …}` on
/// failure. The status code is always `1` on any recognised outcome
/// (the breach shapes are sub-statuses of success on the wire —
/// `ReportUsageResult` IS the outcome space, and the `Err` path is
/// reserved for transport/invariant faults).
fn parse_report_usage(
    raw: &ferriskey::Value,
    execution_id: &ExecutionId,
) -> Result<ReportUsageResult, EngineError> {
    let parsed = FcallResult::parse(raw)
        .map_err(EngineError::from)?
        .into_success()
        .map_err(EngineError::from)?;

    match parsed.status.as_str() {
        "OK" => Ok(ReportUsageResult::Ok),
        "ALREADY_APPLIED" => Ok(ReportUsageResult::AlreadyApplied),
        "SOFT_BREACH" => {
            let dim = parsed.field_str(0);
            let current = parse_u64_field(&parsed, 1, "SOFT_BREACH", "current_usage", execution_id)?;
            let limit = parse_u64_field(&parsed, 2, "SOFT_BREACH", "soft_limit", execution_id)?;
            Ok(ReportUsageResult::SoftBreach {
                dimension: dim,
                current_usage: current,
                soft_limit: limit,
            })
        }
        "HARD_BREACH" => {
            let dim = parsed.field_str(0);
            let current = parse_u64_field(&parsed, 1, "HARD_BREACH", "current_usage", execution_id)?;
            let limit = parse_u64_field(&parsed, 2, "HARD_BREACH", "hard_limit", execution_id)?;
            Ok(ReportUsageResult::HardBreach {
                dimension: dim,
                current_usage: current,
                hard_limit: limit,
            })
        }
        other => Err(EngineError::from(ScriptError::Parse {
            fcall: "ff_report_usage_and_check".into(),
            execution_id: Some(execution_id.to_string()),
            message: format!("unknown sub-status: {other}"),
        })),
    }
}

/// Parse a required u64 field from a `FcallResult` wire reply. Loud
/// failure on missing/non-numeric — see the SDK-side parser for the
/// rationale (silent coercion hides producer/consumer drift).
fn parse_u64_field(
    parsed: &FcallResult,
    index: usize,
    sub_status: &str,
    field_name: &str,
    execution_id: &ExecutionId,
) -> Result<u64, EngineError> {
    let s = parsed.field_str(index);
    s.parse::<u64>().map_err(|_| {
        EngineError::from(ScriptError::Parse {
            fcall: "ff_report_usage_and_check".into(),
            execution_id: Some(execution_id.to_string()),
            message: format!("{sub_status}: {field_name} (index {index}) not a u64: {s:?}"),
        })
    })
}

/// cairn #454 Phase 3a — `record_spend` Valkey body.
///
/// Open-set spend recording. Dispatches to the `ff_record_spend` FCALL,
/// which shares the check-then-increment semantics of
/// `ff_report_usage_and_check` but accepts a `BTreeMap<String, u64>` of
/// tenant-defined dimension deltas. Return shape is `ReportUsageResult`
/// verbatim so `parse_report_usage` is reused.
#[tracing::instrument(
    name = "ff.record_spend",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        budget_id = %args.budget_id,
        dim_count = args.deltas.len(),
    )
)]
async fn record_spend_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: RecordSpendArgs,
) -> Result<ReportUsageResult, EngineError> {
    use ff_core::keys::BudgetKeyContext;
    use ff_core::partition::budget_partition;

    let partition = budget_partition(&args.budget_id, partition_config);
    let bctx = BudgetKeyContext::new(&partition, &args.budget_id);
    let usage = bctx.usage();
    let limits = bctx.limits();
    let definition = bctx.definition();
    let by_exec = bctx.by_exec(&args.execution_id.to_string());
    let by_exec_index = bctx.by_exec_index();
    let hash_tag = bctx.hash_tag().to_string();
    let keys = BudgetOpKeys {
        usage_key: &usage,
        limits_key: &limits,
        def_key: &definition,
        hash_tag: &hash_tag,
    };
    let now = now_ms_timestamp();
    ff_record_spend(client, &keys, &by_exec, &by_exec_index, &args, now)
        .await
        .map_err(EngineError::from)
}

/// cairn #454 Phase 3b - release_budget Valkey body.
///
/// Reverses this execution's attribution by reading the per-execution
/// ledger written during record_spend and negating each dim on the
/// aggregate usage. Idempotent - replays are safe no-ops.
#[tracing::instrument(
    name = "ff.release_budget",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        budget_id = %args.budget_id,
    )
)]
async fn release_budget_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: ReleaseBudgetArgs,
) -> Result<(), EngineError> {
    use ff_core::keys::BudgetKeyContext;
    use ff_core::partition::budget_partition;

    let partition = budget_partition(&args.budget_id, partition_config);
    let bctx = BudgetKeyContext::new(&partition, &args.budget_id);
    let usage = bctx.usage();
    let by_exec = bctx.by_exec(&args.execution_id.to_string());
    let by_exec_index = bctx.by_exec_index();
    ff_release_budget(client, &usage, &by_exec, &by_exec_index, &args)
        .await
        .map_err(EngineError::from)
}

// ── Tranche 2: terminal writes (delay, wait_children, complete, fail, cancel) ──

/// Stage 1b — `delay` FCALL body. Migrated from
/// `ff_sdk::task::ClaimedTask::delay_execution`. 9 KEYS, 5 ARGV.
#[tracing::instrument(
    name = "ff.delay",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn delay_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    delay_until: TimestampMs,
) -> Result<(), EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(f.attempt_index),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&f.worker_instance_id),
        idx.lane_active(&f.lane_id),
        idx.lane_delayed(&f.lane_id),
        idx.attempt_timeout(),
    ];

    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
        f.attempt_id.to_string(),
        delay_until.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_delay_execution", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    parse_success_only(&raw)
}

/// Stage 1b — `wait_children` FCALL body. Migrated from
/// `ff_sdk::task::ClaimedTask::move_to_waiting_children`. 9 KEYS, 4
/// ARGV.
#[tracing::instrument(
    name = "ff.wait_children",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn wait_children_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
) -> Result<(), EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(f.attempt_index),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&f.worker_instance_id),
        idx.lane_active(&f.lane_id),
        idx.lane_blocked_dependencies(&f.lane_id),
        idx.attempt_timeout(),
    ];

    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
        f.attempt_id.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_move_to_waiting_children", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    parse_success_only(&raw)
}

/// Stage 1b — `complete` FCALL body + replay reconciliation.
/// Migrated from `ff_sdk::task::ClaimedTask::complete`. 12 KEYS, 5
/// ARGV.
///
/// # Replay reconciliation
///
/// If the FCALL returns `ExecutionNotActive` AND the stored
/// `terminal_outcome` is `"success"` AND `lease_epoch` +
/// `attempt_id` match the caller's handle, the Ok path is taken:
/// the prior commit landed and the network drop hit after commit.
/// Any other `ExecutionNotActive` combination surfaces the error
/// so the caller learns what actually happened.
#[tracing::instrument(
    level = "debug",
    name = "ff.complete",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn complete_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    payload: Option<Vec<u8>>,
) -> Result<(), EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(f.attempt_index),
        idx.lease_expiry(),
        idx.worker_leases(&f.worker_instance_id),
        idx.lane_terminal(&f.lane_id),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&f.lane_id),
        ctx.stream_meta(f.attempt_index),
        ctx.result(),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];

    let result_bytes = payload.unwrap_or_default();
    let result_str = String::from_utf8_lossy(&result_bytes);

    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
        f.attempt_id.to_string(),
        result_str.into_owned(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_complete_execution", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    let err = match parse_success_only(&raw) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if reconcile_terminal_replay(&err, f, "success") {
        return Ok(());
    }
    Err(err)
}

/// Stage 1b — `cancel` FCALL body + replay reconciliation. Migrated
/// from `ff_sdk::task::ClaimedTask::cancel_inner`. 21 KEYS (of which
/// slots 9/10 depend on the `current_waitpoint_id` field in
/// `exec_core`; if absent, a placeholder `WaitpointId` is used — the
/// Lua side tolerates this), 5 ARGV. Reconciles to `Ok` when the
/// stored `terminal_outcome == "cancelled"`.
#[tracing::instrument(
    name = "ff.cancel",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn cancel_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    reason: &str,
) -> Result<(), EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    // Read `current_waitpoint_id` exactly as the SDK did; needed for
    // slots 9 + 10. Falls back to a placeholder UUID for not-suspended
    // executions; Lua tolerates the placeholder.
    let wp_id_str: Option<String> = client
        .hget(&ctx.core(), "current_waitpoint_id")
        .await
        .map_err(transport_fk)?;
    let wp_id = match wp_id_str.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match WaitpointId::parse(s) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    execution_id = %f.execution_id,
                    raw = %s,
                    error = %e,
                    "corrupt waitpoint_id in exec_core, using placeholder"
                );
                WaitpointId::new()
            }
        },
        None => WaitpointId::default(),
    };

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(f.attempt_index),
        ctx.stream_meta(f.attempt_index),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&f.worker_instance_id),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_id),
        ctx.waitpoint_condition(&wp_id),
        idx.suspension_timeout(),
        idx.lane_terminal(&f.lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_eligible(&f.lane_id),
        idx.lane_delayed(&f.lane_id),
        idx.lane_blocked_dependencies(&f.lane_id),
        idx.lane_blocked_budget(&f.lane_id),
        idx.lane_blocked_quota(&f.lane_id),
        idx.lane_blocked_route(&f.lane_id),
        idx.lane_blocked_operator(&f.lane_id),
    ];

    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        reason.to_owned(),
        "worker".to_owned(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_cancel_execution", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    let err = match parse_success_only(&raw) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    if reconcile_terminal_replay(&err, f, "cancelled") {
        return Ok(());
    }
    Err(err)
}

/// Stage 1b — `fail` FCALL body + replay reconciliation. Migrated
/// from `ff_sdk::task::ClaimedTask::fail`. 12 KEYS, 7 ARGV.
///
/// The `FailureClass` is mapped to the Lua `error_category` string
/// at the call boundary; the SDK's string shape was free-form so we
/// pick the Lua-side canonical lower_snake_case for each enum
/// variant. Future trait amendments (issue #117 family) may widen
/// `FailureClass` with a `Custom(String)` arm; for now the 5 named
/// variants cover every shape the SDK exercises today.
///
/// `FailureReason.message` maps to the `failure_reason` ARGV slot.
/// `FailureReason.detail` is not yet surfaced to Lua — the SDK's
/// pre-Stage-1b call site passed the reason string straight through
/// and Lua records that as `failure_reason`; Stage 1b preserves the
/// shape to keep a zero-behavior-change guarantee. A future commit
/// can thread `detail` once Lua grows a slot for it.
#[tracing::instrument(
    name = "ff.fail",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn fail_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    reason: FailureReason,
    classification: FailureClass,
) -> Result<FailOutcome, EngineError> {
    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(f.attempt_index),
        idx.lease_expiry(),
        idx.worker_leases(&f.worker_instance_id),
        idx.lane_terminal(&f.lane_id),
        idx.lane_delayed(&f.lane_id),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&f.lane_id),
        ctx.stream_meta(f.attempt_index),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];

    let retry_policy_json = read_retry_policy_json(client, &ctx, &f.execution_id).await?;

    // Category string resolution: prefer the caller's raw-string
    // stash in `FailureReason.detail` (see ff-sdk's
    // `ClaimedTask::fail` carrier note). When the detail bytes are
    // valid UTF-8 and non-empty, Lua sees the caller's exact
    // category. Otherwise fall through to the trait-enum mapping
    // (Transient / Permanent / …) so trait-direct callers still
    // get a sensible string. Stage 1d retires the stash once
    // `FailureClass::Custom(String)` lands.
    let category_owned: Option<String> = reason
        .detail
        .as_ref()
        .filter(|d| !d.is_empty())
        .and_then(|d| std::str::from_utf8(d).ok())
        .map(String::from);
    let error_category: String =
        category_owned.unwrap_or_else(|| failure_class_to_lua_string(classification).to_owned());

    let args: Vec<String> = vec![
        f.execution_id.to_string(),
        f.lease_id.to_string(),
        f.lease_epoch.to_string(),
        f.attempt_id.to_string(),
        reason.message,
        error_category,
        retry_policy_json,
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw = client
        .fcall::<ferriskey::Value>("ff_fail_execution", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    match parse_fail_result_engine(&raw) {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            // Replay reconciliation: two shapes valid for fail.
            //   - lifecycle=terminal, outcome=failed -> TerminalFailed
            //   - lifecycle=runnable                 -> RetryScheduled { delay_until = 0 }
            if let EngineError::Contention(
                ff_core::engine_error::ContentionKind::ExecutionNotActive {
                    ref terminal_outcome,
                    ref lease_epoch,
                    ref lifecycle_phase,
                    ref attempt_id,
                },
            ) = err
                && lease_epoch == &f.lease_epoch.to_string()
            {
                match (lifecycle_phase.as_str(), terminal_outcome.as_str()) {
                    ("terminal", "failed") if attempt_id == &f.attempt_id.to_string() => {
                        return Ok(FailOutcome::TerminalFailed);
                    }
                    ("runnable", _) => {
                        return Ok(FailOutcome::RetryScheduled {
                            delay_until: TimestampMs::from_millis(0),
                        });
                    }
                    _ => {}
                }
            }
            Err(err)
        }
    }
}

/// Reconcile a terminal-op `ExecutionNotActive` against the caller's
/// handle: `true` iff the stored `terminal_outcome` matches
/// `expected_outcome` AND the epoch + attempt both match this
/// caller's claim. Shared by `complete` / `cancel`; `fail` has a
/// two-shape path inlined above.
fn reconcile_terminal_replay(
    err: &EngineError,
    f: &handle_codec::HandleFields,
    expected_outcome: &str,
) -> bool {
    if let EngineError::Contention(ff_core::engine_error::ContentionKind::ExecutionNotActive {
        ref terminal_outcome,
        ref lease_epoch,
        ref attempt_id,
        ..
    }) = *err
    {
        terminal_outcome == expected_outcome
            && lease_epoch == &f.lease_epoch.to_string()
            && attempt_id == &f.attempt_id.to_string()
    } else {
        false
    }
}

/// Parse `ff_fail_execution` success envelope into `FailOutcome`.
/// Mirrors `ff_sdk::task::parse_fail_result` on the success side;
/// errors route through `FcallResult`/`ScriptError`→`EngineError`.
fn parse_fail_result_engine(raw: &ferriskey::Value) -> Result<FailOutcome, EngineError> {
    let parsed = FcallResult::parse(raw)
        .map_err(EngineError::from)?
        .into_success()
        .map_err(EngineError::from)?;
    // `field_str(0)` is the sub_status (Lua returns:
    //   ok("retry_scheduled", tostring(delay_until))
    //   ok("terminal_failed")
    // `FcallResult::into_success` strips the leading `1` + `"OK"`
    // wrapper, so fields[0] here is the sub-status slot.
    let sub_status = parsed.field_str(0);
    match sub_status.as_str() {
        "retry_scheduled" => {
            let delay_str = parsed.field_str(1);
            let delay_until = delay_str.parse::<i64>().unwrap_or(0);
            Ok(FailOutcome::RetryScheduled {
                delay_until: TimestampMs::from_millis(delay_until),
            })
        }
        "terminal_failed" => Ok(FailOutcome::TerminalFailed),
        other => Err(EngineError::from(ScriptError::Parse {
            fcall: "ff_fail_execution".into(),
            execution_id: None,
            message: format!("unexpected sub-status: {other}"),
        })),
    }
}

/// Map the trait's `FailureClass` enum to the Lua-side `error_category`
/// ARGV string. The 5 variants plus a non-exhaustive fallback cover
/// every call site the SDK exercises today. When issue #117 or a
/// follow-up RFC widens `FailureClass` with a `Custom(String)` arm,
/// this match gains a pass-through for the new arm.
fn failure_class_to_lua_string(c: FailureClass) -> &'static str {
    match c {
        FailureClass::Transient => "transient",
        FailureClass::Permanent => "permanent",
        FailureClass::InfraCrash => "infra_crash",
        FailureClass::Timeout => "timeout",
        FailureClass::Cancelled => "cancelled",
        // `#[non_exhaustive]` — fall through to the least-destructive
        // known category so a newly-added variant does not silently
        // hard-fail an attempt. Follow-up RFCs that add variants must
        // update this match explicitly.
        _ => "transient",
    }
}

/// Read the execution's retry policy JSON from `exec_policy`. Used by
/// `fail_impl` — the FCALL's `retry_policy_json` ARGV is the
/// serialised `retry_policy` key extracted from the stored policy
/// JSON. Malformed stored JSON degrades to an empty string (matches
/// the SDK's pre-migration behaviour — Lua then applies no-retry
/// defaults).
async fn read_retry_policy_json(
    client: &ferriskey::Client,
    ctx: &ExecKeyContext,
    execution_id: &ExecutionId,
) -> Result<String, EngineError> {
    let policy_str: Option<String> = client.get(&ctx.policy()).await.map_err(transport_fk)?;
    match policy_str {
        Some(json) => match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(policy) => {
                if let Some(retry) = policy.get("retry_policy") {
                    return Ok(serde_json::to_string(retry).unwrap_or_default());
                }
                Ok(String::new())
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "malformed retry policy JSON, treating as no policy"
                );
                Ok(String::new())
            }
        },
        None => Ok(String::new()),
    }
}

// ── RFC-013 Stage 1d: suspend_impl ────────────────────────────────────

/// Grace window added to caller-supplied `timeout_at` when deriving the
/// dedup-hash TTL (RFC-013 §2.2).
const SUSPEND_DEDUP_GRACE_MS: u64 = 60_000;
/// Upper bound on the dedup-hash TTL (RFC-013 §2.2). Also the default
/// when `timeout_at` is `None`. 7 days.
const SUSPEND_DEDUP_MAX_TTL_MS: u64 = 604_800_000;
/// Clock-skew tolerance for `timeout_at` validation (RFC-013 §2.2).
const SUSPEND_CLOCK_SKEW_TOLERANCE_MS: i64 = 600_000;

/// RFC-014 §3.3 — emit a compact tree spec the Lua evaluator walks
/// per-signal. Each node carries `kind` + per-kind fields plus a
/// `path` string that doubles as the satisfier-token prefix for
/// non-leaf satisfaction (`node:<path>`).
fn composite_node_to_json(cond: &ResumeCondition, path: &str) -> serde_json::Value {
    match cond {
        ResumeCondition::Single {
            waitpoint_key,
            matcher,
        } => serde_json::json!({
            "kind": "Single",
            "path": path,
            "waitpoint_key": waitpoint_key,
            "matcher": signal_matcher_to_json(matcher),
        }),
        ResumeCondition::Composite(body) => composite_body_to_json(body, path),
        ResumeCondition::OperatorOnly | ResumeCondition::TimeoutOnly => {
            // Not meaningfully satisfiable by signal delivery under a
            // composite; Lua treats these as never-satisfied leaves.
            serde_json::json!({ "kind": "NeverBySignal", "path": path })
        }
        _ => serde_json::json!({ "kind": "NeverBySignal", "path": path }),
    }
}

fn composite_body_to_json(body: &CompositeBody, path: &str) -> serde_json::Value {
    match body {
        CompositeBody::AllOf { members } => {
            let members_json: Vec<serde_json::Value> = members
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let child_path = if path.is_empty() {
                        format!("members[{i}]")
                    } else {
                        format!("{path}.members[{i}]")
                    };
                    composite_node_to_json(m, &child_path)
                })
                .collect();
            serde_json::json!({
                "kind": "AllOf",
                "path": path,
                "members": members_json,
            })
        }
        CompositeBody::Count {
            n,
            count_kind,
            matcher,
            waitpoints,
        } => {
            let ck = match count_kind {
                CountKind::DistinctWaitpoints => "DistinctWaitpoints",
                CountKind::DistinctSignals => "DistinctSignals",
                CountKind::DistinctSources => "DistinctSources",
                _ => "DistinctWaitpoints",
            };
            serde_json::json!({
                "kind": "Count",
                "path": path,
                "n": n,
                "count_kind": ck,
                "matcher": matcher.as_ref().map(signal_matcher_to_json),
                "waitpoints": waitpoints,
            })
        }
        // Future CompositeBody variants (e.g. RFC-016 additions) are not
        // serializable by this tree walker until they land. Emit a
        // never-satisfiable sentinel so Lua rejects rather than panics.
        _ => serde_json::json!({ "kind": "NeverBySignal", "path": path }),
    }
}

fn composite_tree_to_json(body: &CompositeBody) -> serde_json::Value {
    composite_body_to_json(body, "")
}

fn signal_matcher_to_json(m: &SignalMatcher) -> serde_json::Value {
    match m {
        SignalMatcher::ByName(n) => serde_json::json!({ "kind": "ByName", "name": n }),
        SignalMatcher::Wildcard => serde_json::json!({ "kind": "Wildcard" }),
        _ => serde_json::json!({ "kind": "Wildcard" }),
    }
}

/// Serialize a [`ResumeCondition`] into the `resume_condition_json` ARGV
/// string today's Lua reads. Internal to the backend — consumers never
/// see this string.
fn resume_condition_to_json(
    cond: &ResumeCondition,
    timeout_behavior: &ff_core::contracts::TimeoutBehavior,
) -> Result<String, EngineError> {
    let v: serde_json::Value = match cond {
        ResumeCondition::Single {
            waitpoint_key: _,
            matcher,
        } => {
            let names: Vec<String> = match matcher {
                SignalMatcher::ByName(n) => vec![n.clone()],
                SignalMatcher::Wildcard => Vec::new(),
                _ => Vec::new(),
            };
            serde_json::json!({
                "condition_type": "signal_set",
                "required_signal_names": names,
                "signal_match_mode": "any",
                "minimum_signal_count": 1,
                "timeout_behavior": timeout_behavior.as_wire_str(),
                "allow_operator_override": true,
            })
        }
        ResumeCondition::OperatorOnly => serde_json::json!({
            "condition_type": "signal_set",
            // No signal name will ever match this sentinel — only an
            // operator resume closes the waitpoint (RFC-013 §2.4).
            "required_signal_names": ["__operator_only__"],
            "signal_match_mode": "all",
            "minimum_signal_count": 1,
            "timeout_behavior": timeout_behavior.as_wire_str(),
            "allow_operator_override": true,
        }),
        ResumeCondition::TimeoutOnly => serde_json::json!({
            "condition_type": "signal_set",
            "required_signal_names": ["__timeout_only__"],
            "signal_match_mode": "all",
            "minimum_signal_count": 1,
            "timeout_behavior": timeout_behavior.as_wire_str(),
            "allow_operator_override": true,
        }),
        ResumeCondition::Composite(body) => {
            // RFC-014 §7.2 wire format. Lua consumes:
            //   composite  = "1"          — branch marker
            //   version    = 1            — rejects v>1
            //   tree       = <compact spec>
            //   member_map = [[wp_id_key, node_path], ...]
            //                (populated at `suspend_execution` from the
            //                 waitpoint_id that Lua mints; the Rust side
            //                 sends `waitpoint_key` tokens that Lua
            //                 rewrites to ids. Simpler: we emit node
            //                 waitpoint_keys directly and Lua resolves.)
            // See §3 algorithm.
            let tree = composite_tree_to_json(body);
            serde_json::json!({
                "v": 1,
                "composite": true,
                "timeout_behavior": timeout_behavior.as_wire_str(),
                "allow_operator_override": true,
                // Legacy-shape stubs so older consumers of the stored
                // resume_condition_json (operator diagnostics) can still
                // discover mode/name without parsing the tree.
                "condition_type": "composite",
                "required_signal_names": [],
                "signal_match_mode": "composite",
                "minimum_signal_count": 1,
                "tree": tree,
            })
        }
        _ => {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "resume_condition: unsupported variant".into(),
            });
        }
    };
    Ok(v.to_string())
}

/// Serialize a [`ResumePolicy`] into the `resume_policy_json` ARGV
/// string Lua reads.
fn resume_policy_to_json(p: &ResumePolicy) -> String {
    let target = match p.resume_target {
        ResumeTarget::Runnable => "runnable",
        _ => "runnable",
    };
    let mut obj = serde_json::json!({
        "resume_target": target,
        "close_waitpoint_on_resume": p.close_waitpoint_on_resume,
        "consume_matched_signals": p.consume_matched_signals,
        "retain_signal_buffer_until_closed": p.retain_signal_buffer_until_closed,
    });
    if let Some(d) = p.resume_delay_ms {
        obj.as_object_mut()
            .expect("json object")
            .insert("resume_delay_ms".into(), serde_json::json!(d));
    }
    obj.to_string()
}

/// Rust-side pre-FCALL validation (RFC-013 §4 + RFC-014 Pattern 3).
#[allow(clippy::collapsible_if)]
fn validate_suspend_args(args: &SuspendArgs) -> Result<(), EngineError> {
    // Non-empty waitpoints vector (RFC-014 §2 Pattern 3 invariant).
    if args.waitpoints.is_empty() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "waitpoints_empty".into(),
        });
    }
    // waitpoint_key cross-field invariant (primary vs Single condition).
    if let (
        WaitpointBinding::Fresh { waitpoint_key: a, .. },
        ResumeCondition::Single { waitpoint_key: b, .. },
    ) = (args.primary(), &args.resume_condition)
    {
        if a != b {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "waitpoint_key_mismatch".into(),
            });
        }
    }
    // RFC-014 Pattern 3 — every additional binding must be Fresh with a
    // non-empty waitpoint_key. UsePending with extras is out of scope for
    // this RFC (pending-activation composes an existing pending waitpoint,
    // which is inherently single-waitpoint).
    if args.waitpoints.len() > 1 {
        for (i, b) in args.waitpoints.iter().enumerate().skip(1) {
            match b {
                WaitpointBinding::Fresh { waitpoint_key, .. } if !waitpoint_key.is_empty() => {}
                WaitpointBinding::Fresh { .. } => {
                    return Err(EngineError::Validation {
                        kind: ValidationKind::InvalidInput,
                        detail: format!(
                            "additional_waitpoint_binding_empty_key at waitpoints[{i}]"
                        ),
                    });
                }
                _ => {
                    return Err(EngineError::Validation {
                        kind: ValidationKind::InvalidInput,
                        detail: format!(
                            "additional_waitpoint_binding_must_be_fresh at waitpoints[{i}]"
                        ),
                    });
                }
            }
        }
        // Cross-check: every waitpoint_key referenced by the resume
        // condition must correspond to a binding, and vice versa — the
        // Pattern 3 invariant.
        let binding_keys: Vec<String> = args
            .waitpoints
            .iter()
            .filter_map(|b| match b {
                WaitpointBinding::Fresh { waitpoint_key, .. } => Some(waitpoint_key.clone()),
                _ => None,
            })
            .collect();
        let referenced = args.resume_condition.referenced_waitpoint_keys();
        for r in &referenced {
            if !binding_keys.iter().any(|b| b == r) {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: format!("referenced_waitpoint_key_missing_binding: {r}"),
                });
            }
        }
        for b in &binding_keys {
            if !referenced.iter().any(|r| r == b) {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: format!("extra_binding_not_referenced_by_condition: {b}"),
                });
            }
        }
    }
    // TimeoutOnly requires timeout_at.
    if matches!(args.resume_condition, ResumeCondition::TimeoutOnly)
        && args.timeout_at.is_none()
    {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "timeout_only_without_deadline".into(),
        });
    }
    // Empty ByName rejected (wildcard is its own variant).
    if let ResumeCondition::Single {
        matcher: SignalMatcher::ByName(n),
        ..
    } = &args.resume_condition
    {
        if n.is_empty() {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "resume_condition: empty signal name (use Wildcard instead)".into(),
            });
        }
    }
    // timeout_at in the past (with clock-skew tolerance).
    if let Some(at) = args.timeout_at {
        if at.0 + SUSPEND_CLOCK_SKEW_TOLERANCE_MS < args.now.0 {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "timeout_at_in_past".into(),
            });
        }
    }
    // RFC-014 §5.1 composite structural validation.
    if let Err(e) = args.resume_condition.validate_composite() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: e.detail,
        });
    }
    Ok(())
}

/// Compute dedup TTL per RFC-013 §2.2:
/// `min(timeout_at - now + GRACE, MAX_TTL)`; `MAX_TTL` when no deadline.
fn compute_dedup_ttl_ms(args: &SuspendArgs) -> u64 {
    match args.timeout_at {
        Some(at) => {
            let delta = (at.0 - args.now.0).max(0) as u64;
            delta.saturating_add(SUSPEND_DEDUP_GRACE_MS).min(SUSPEND_DEDUP_MAX_TTL_MS)
        }
        None => SUSPEND_DEDUP_MAX_TTL_MS,
    }
}

#[tracing::instrument(
    name = "ff.suspend",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %f.execution_id,
        attempt_id = %f.attempt_id,
        lease_epoch = %f.lease_epoch,
    )
)]
async fn suspend_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    f: &handle_codec::HandleFields,
    args: SuspendArgs,
) -> Result<SuspendOutcome, EngineError> {
    validate_suspend_args(&args)?;

    let partition = execution_partition(&f.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &f.execution_id);
    let idx = IndexKeys::new(&partition);

    // Extract (waitpoint_id, waitpoint_key, use_pending) from the
    // primary binding. `UsePending` supplies an empty waitpoint_key;
    // the Lua side resolves the authoritative key from the waitpoint
    // hash.
    let (wp_id, wp_key, use_pending) = match args.primary() {
        WaitpointBinding::Fresh {
            waitpoint_id,
            waitpoint_key,
        } => (waitpoint_id.clone(), waitpoint_key.clone(), false),
        WaitpointBinding::UsePending { waitpoint_id } => {
            (waitpoint_id.clone(), String::new(), true)
        }
        _ => {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "waitpoint: unsupported binding variant".into(),
            });
        }
    };

    // RFC-014 Pattern 3 — additional bindings. Validator has already
    // enforced each extra is Fresh with a non-empty waitpoint_key
    // (UsePending composites are out of scope for Pattern 3).
    let extras: Vec<(WaitpointId, String)> = args
        .waitpoints
        .iter()
        .skip(1)
        .map(|b| match b {
            WaitpointBinding::Fresh {
                waitpoint_id,
                waitpoint_key,
            } => (waitpoint_id.clone(), waitpoint_key.clone()),
            _ => unreachable!("validator rejects non-Fresh extras"),
        })
        .collect();

    let resume_condition_json =
        resume_condition_to_json(&args.resume_condition, &args.timeout_behavior)?;
    let resume_policy_json = resume_policy_to_json(&args.resume_policy);

    let idem_key = args
        .idempotency_key
        .as_ref()
        .map(|k| k.as_str().to_owned())
        .unwrap_or_default();
    let dedup_ttl = if idem_key.is_empty() {
        0
    } else {
        compute_dedup_ttl_ms(&args)
    };

    let dedup_hash_key = if idem_key.is_empty() {
        // Noop placeholder — shares partition hash tag so KEYS stay
        // on-slot in cluster mode.
        ctx.noop()
    } else {
        ctx.suspend_dedup(&idem_key)
    };

    // KEYS (18 base, per RFC-013 §9.2; followed by 3 × N_extra for
    // RFC-014 Pattern 3 additional waitpoints: wp_hash, wp_signals,
    // wp_condition per extra, in the same order as `extras`).
    let mut keys: Vec<String> = vec![
        ctx.core(),                                  // 1
        ctx.attempt_hash(f.attempt_index),           // 2
        ctx.lease_current(),                         // 3
        ctx.lease_history(),                         // 4
        idx.lease_expiry(),                          // 5
        idx.worker_leases(&f.worker_instance_id),    // 6
        ctx.suspension_current(),                    // 7
        ctx.waitpoint(&wp_id),                       // 8
        ctx.waitpoint_signals(&wp_id),               // 9
        idx.suspension_timeout(),                    // 10
        idx.pending_waitpoint_expiry(),              // 11
        idx.lane_active(&f.lane_id),                 // 12
        idx.lane_suspended(&f.lane_id),              // 13
        ctx.waitpoints(),                            // 14
        ctx.waitpoint_condition(&wp_id),             // 15
        idx.attempt_timeout(),                       // 16
        idx.waitpoint_hmac_secrets(),                // 17
        dedup_hash_key,                              // 18
    ];
    for (extra_id, _extra_key) in &extras {
        keys.push(ctx.waitpoint(extra_id));            // 19+3k
        keys.push(ctx.waitpoint_signals(extra_id));    // 20+3k
        keys.push(ctx.waitpoint_condition(extra_id));  // 21+3k
    }

    // ARGV (19 base + 1 N_extra slot + 2 × N_extra): RFC-013 §9.2 plus
    // RFC-014 Pattern 3 tail (num_extra, then (wp_id, wp_key) pairs).
    let mut argv: Vec<String> = vec![
        f.execution_id.to_string(),                                                   // 1
        f.attempt_index.to_string(),                                                  // 2
        f.attempt_id.to_string(),                                                     // 3
        f.lease_id.to_string(),                                                       // 4
        f.lease_epoch.to_string(),                                                    // 5
        args.suspension_id.to_string(),                                               // 6
        wp_id.to_string(),                                                            // 7
        wp_key,                                                                       // 8
        args.reason_code.as_wire_str().to_owned(),                                    // 9
        args.requested_by.as_wire_str().to_owned(),                                   // 10
        args.timeout_at.map_or(String::new(), |t| t.to_string()),                     // 11
        resume_condition_json,                                                        // 12
        resume_policy_json,                                                           // 13
        args.continuation_metadata_pointer.clone().unwrap_or_default(),               // 14
        if use_pending { "1".to_owned() } else { String::new() },                     // 15
        args.timeout_behavior.as_wire_str().to_owned(),                               // 16
        "1000".to_owned(),                                                            // 17 lease_history_maxlen
        idem_key,                                                                     // 18
        dedup_ttl.to_string(),                                                        // 19
        extras.len().to_string(),                                                     // 20 N_extra
    ];
    for (extra_id, extra_key) in &extras {
        argv.push(extra_id.to_string());
        argv.push(extra_key.clone());
    }

    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();

    let raw: ferriskey::Value = client
        .fcall("ff_suspend_execution", &key_refs, &arg_refs)
        .await
        .map_err(transport_fk)?;

    let result = FcallResult::parse(&raw).map_err(EngineError::from)?;
    if !result.success {
        return Err(EngineError::from(
            result.into_success().unwrap_err(),
        ));
    }

    // Success: fields are [suspension_id, waitpoint_id, waitpoint_key, waitpoint_token].
    let s_id = result.field_str(0);
    let w_id_str = result.field_str(1);
    let w_key = result.field_str(2);
    let w_tok = result.field_str(3);

    let suspension_id = ff_core::types::SuspensionId::parse(&s_id).map_err(|e| {
        transport_script(ScriptError::Parse {
            fcall: "suspend_impl".into(),
            execution_id: Some(f.execution_id.to_string()),
            message: format!("bad suspension_id: {e}"),
        })
    })?;
    let waitpoint_id = WaitpointId::parse(&w_id_str).map_err(|e| {
        transport_script(ScriptError::Parse {
            fcall: "suspend_impl".into(),
            execution_id: Some(f.execution_id.to_string()),
            message: format!("bad waitpoint_id: {e}"),
        })
    })?;

    // RFC-014 Pattern 3 — parse additional waitpoint tokens from the
    // Lua response tail. Wire shape: after the primary 4 fields the
    // Lua returns `N_extra` then `N_extra × (wp_id, wp_key, token)`.
    let n_extra: usize = result.field_str(4).parse().unwrap_or(0);
    let mut additional: Vec<AdditionalWaitpointBinding> = Vec::with_capacity(n_extra);
    for i in 0..n_extra {
        let base = 5 + i * 3;
        let ex_id = result.field_str(base);
        let ex_key = result.field_str(base + 1);
        let ex_tok = result.field_str(base + 2);
        let wpid = WaitpointId::parse(&ex_id).map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "suspend_impl".into(),
                execution_id: Some(f.execution_id.to_string()),
                message: format!("bad additional waitpoint_id [{i}]: {e}"),
            })
        })?;
        additional.push(AdditionalWaitpointBinding::new(
            wpid,
            ex_key,
            WaitpointHmac::new(ex_tok),
        ));
    }

    let details = SuspendOutcomeDetails::new(
        suspension_id,
        waitpoint_id,
        w_key,
        WaitpointHmac::new(w_tok),
    )
    .with_additional_waitpoints(additional);

    match result.status.as_str() {
        "ALREADY_SATISFIED" => Ok(SuspendOutcome::AlreadySatisfied { details }),
        _ => {
            // Mint a fresh Suspended-kind handle carrying the caller's
            // execution identity. The new handle has the same fence
            // triple as the caller's pre-suspend handle — its purpose
            // is to carry a `HandleKind::Suspended` tag for
            // `observe_signals` / `claim_from_resume_grant` routing.
            let suspended_handle = handle_codec::encode_handle(f, HandleKind::Suspended);
            Ok(SuspendOutcome::Suspended {
                details,
                handle: suspended_handle,
            })
        }
    }
}

/// Thin forwarder to `ff_script::functions::signal::ff_deliver_signal`.
///
/// Reads `lane_id` off `exec_core` (the caller's args carry no lane —
/// the Lua KEYS require it to locate the lane_eligible / lane_suspended
/// / lane_delayed index keys) and delegates. The `ff-sdk` worker's
/// `deliver_signal` public API does the same HGET pre-read before
/// firing the FCALL; this mirrors it at the backend layer so alternate
/// backends don't need to teach callers about the lane.
#[tracing::instrument(
    name = "ff.deliver_signal",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        waitpoint_id = %args.waitpoint_id,
        signal_id = %args.signal_id,
    )
)]
async fn deliver_signal_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: DeliverSignalArgs,
) -> Result<DeliverSignalResult, EngineError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    // Pre-read lane_id from exec_core. An absent lane_id means the
    // execution record is missing / malformed — let the FCALL surface
    // the canonical `execution_not_found` / `invalid_state` error.
    let lane_str: Option<String> = client
        .cmd("HGET")
        .arg(ctx.core())
        .arg("lane_id")
        .execute()
        .await
        .map_err(transport_fk)?;
    let lane_id = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));

    let keys = SignalOpKeys {
        ctx: &ctx,
        idx: &idx,
        lane_id: &lane_id,
    };
    ff_deliver_signal(client, &keys, &args)
        .await
        .map_err(EngineError::from)
}

/// Cairn #454 Phase 3c — operator-driven approval-signal delivery.
///
/// Pre-shaped variant of [`deliver_signal_impl`] where the caller does
/// **not** carry the waitpoint token. The backend reads the token from
/// `ff:wp:<tag>:<wp_id>` via [`read_waitpoint_token_impl`], HMAC-verifies
/// server-side (Lua `validate_waitpoint_token` inside `ff_deliver_signal`),
/// and dispatches. The operator API never handles the token bytes.
///
/// Missing waitpoint → [`EngineError::NotFound`] `{ entity: "waitpoint" }`.
/// Other invariants (execution-gone, dedup, capacity) surface unchanged
/// through the shared [`ff_deliver_signal`] FCALL.
#[tracing::instrument(
    name = "ff.deliver_approval_signal",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        waitpoint_id = %args.waitpoint_id,
        lane = %args.lane_id,
    )
)]
async fn deliver_approval_signal_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: DeliverApprovalSignalArgs,
) -> Result<DeliverSignalResult, EngineError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let partition_key = PartitionKey::from(&partition);

    // Server-side token lookup. Cairn #454 comment 4355865937 option
    // (a): the operator never carries the HMAC token; the backend
    // reads it here and the Lua re-verifies it alongside the signal
    // delivery in one FCALL.
    let token_str = read_waitpoint_token_impl(client, &partition_key, &args.waitpoint_id)
        .await?
        .ok_or(EngineError::NotFound {
            entity: "waitpoint",
        })?;

    // Compose DeliverSignalArgs. Conventions per the task brief:
    //   - signal_category = "approval"
    //   - source_type     = "operator"
    //   - source_identity = ""          (cairn audit log holds decided_by)
    //   - idempotency_key = approval:<signal_name>:<idempotency_suffix>
    //   - target_scope    = "execution" (parity with deliver_signal default
    //                                    used by ff-server + smoke)
    //   - payload / payload_encoding / correlation_id / resume_delay_ms:
    //     absent (approval signals carry no body; cairn stores side data).
    let idempotency_key =
        format!("approval:{}:{}", args.signal_name, args.idempotency_suffix);

    // Pre-read the authoritative `lane_id` from `exec_core` — the same
    // guard `deliver_signal_impl` uses. `ff_deliver_signal` mutates
    // lane-specific index ZSETs purely from the KEYS we pass, so trusting
    // the caller-supplied `args.lane_id` would let a stale or wrong value
    // leak state into the wrong lane's suspended/eligible/delayed index
    // (PR #465 Copilot review).
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let lane_str: Option<String> = client
        .cmd("HGET")
        .arg(ctx.core())
        .arg("lane_id")
        .execute()
        .await
        .map_err(transport_fk)?;
    let authoritative_lane = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));
    if authoritative_lane.as_str() != args.lane_id.as_str() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "lane_mismatch: args.lane_id={} exec_core.lane_id={}",
                args.lane_id.as_str(),
                authoritative_lane.as_str()
            ),
        });
    }

    // Capture one wall-clock sample and reuse it for both `created_at`
    // and `now` so the args represent a single dispatch moment
    // (PR #465 gemini review).
    let now = TimestampMs::now();
    let ds = DeliverSignalArgs {
        execution_id: args.execution_id.clone(),
        waitpoint_id: args.waitpoint_id.clone(),
        signal_id: SignalId::new(),
        signal_name: args.signal_name.clone(),
        signal_category: "approval".to_owned(),
        source_type: "operator".to_owned(),
        source_identity: String::new(),
        payload: None,
        payload_encoding: None,
        correlation_id: None,
        idempotency_key: Some(idempotency_key),
        target_scope: "execution".to_owned(),
        created_at: Some(now),
        dedup_ttl_ms: Some(args.signal_dedup_ttl_ms),
        resume_delay_ms: None,
        max_signals_per_execution: args.max_signals_per_execution,
        signal_maxlen: args.maxlen,
        waitpoint_token: WaitpointToken::new(token_str),
        now,
    };

    // Dispatch through the shared deliver_signal body — same ExecKey /
    // Index / lane-key wiring, and the same Lua FCALL re-verifies the
    // token we just looked up.
    let idx = IndexKeys::new(&partition);
    let keys = SignalOpKeys {
        ctx: &ctx,
        idx: &idx,
        lane_id: &authoritative_lane,
    };
    ff_deliver_signal(client, &keys, &ds)
        .await
        .map_err(EngineError::from)
}

/// Fresh-find claim implementation (Wave 2, v0.7).
///
/// Scans the lane's eligible ZSET across every execution partition,
/// filters by capability subset-match on each candidate's
/// `required_capabilities`, and on the first match issues a claim
/// grant + invokes `ff_claim_execution`. Returns `Ok(None)` when no
/// partition has an eligible execution the worker can serve.
///
/// Returns the encoded Valkey `Handle` on success.
#[tracing::instrument(
    name = "ff.claim",
    skip_all,
    fields(
        backend = "valkey",
        lane = %lane,
        worker_id = %policy.worker_id,
        worker_instance_id = %policy.worker_instance_id,
    )
)]
async fn claim_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    lane: &LaneId,
    capabilities: &CapabilitySet,
    policy: &ClaimPolicy,
) -> Result<Option<Handle>, EngineError> {
    use ff_core::caps::CapabilityRequirement;

    let num_partitions = partition_config.num_flow_partitions;
    if num_partitions == 0 {
        return Ok(None);
    }

    // Build the caps CSV (sorted, deterministic) for the grant FCALL.
    let mut sorted_caps: Vec<String> = capabilities.tokens.clone();
    sorted_caps.sort();
    sorted_caps.dedup();
    let caps_csv = sorted_caps.join(",");

    for p_idx in 0..num_partitions {
        let partition = Partition {
            family: PartitionFamily::Execution,
            index: p_idx,
        };
        let idx = IndexKeys::new(&partition);
        let eligible_key = idx.lane_eligible(lane);

        // Pick the highest-priority eligible candidate on this partition.
        let candidates: Vec<String> = client
            .cmd("ZRANGEBYSCORE")
            .arg(&eligible_key)
            .arg("-inf")
            .arg("+inf")
            .arg("LIMIT")
            .arg("0")
            .arg("1")
            .execute()
            .await
            .map_err(transport_fk)?;

        let eid_str = match candidates.first() {
            Some(s) => s.clone(),
            None => continue,
        };

        let execution_id = ExecutionId::parse(&eid_str).map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "ff_claim_execution".into(),
                execution_id: None,
                message: format!("bad execution_id in eligible set: {e}"),
            })
        })?;

        let ctx = ExecKeyContext::new(&partition, &execution_id);

        // Capability pre-check: if the execution declares required caps
        // that aren't a subset of the worker's CapabilitySet, skip.
        let required_csv: Option<String> = client
            .cmd("HGET")
            .arg(ctx.core())
            .arg("required_capabilities")
            .execute()
            .await
            .map_err(transport_fk)?;
        if let Some(req) = required_csv.as_deref()
            && !req.is_empty()
        {
            let required = CapabilityRequirement::from_csv(req);
            if !ff_core::caps::matches(&required, capabilities) {
                continue;
            }
        }

        // Step 1 — issue the claim grant.
        let grant_keys_owned: [String; 3] =
            [ctx.core(), ctx.claim_grant(), eligible_key.clone()];
        let grant_keys_ref: [&str; 3] = [
            grant_keys_owned[0].as_str(),
            grant_keys_owned[1].as_str(),
            grant_keys_owned[2].as_str(),
        ];
        let eid_s = execution_id.to_string();
        let worker_id_s = policy.worker_id.to_string();
        let worker_instance_s = policy.worker_instance_id.to_string();
        let lane_s = lane.to_string();
        let grant_argv: [&str; 9] = [
            &eid_s,
            &worker_id_s,
            &worker_instance_s,
            &lane_s,
            "",            // capability_hash
            "5000",        // grant_ttl_ms (5s)
            "",            // route_snapshot_json
            "",            // admission_summary
            &caps_csv,     // worker_capabilities_csv (sorted)
        ];
        let raw: ferriskey::Value = client
            .fcall("ff_issue_claim_grant", &grant_keys_ref, &grant_argv)
            .await
            .map_err(transport_fk)?;
        // Parse grant result: {1, "OK", ...}
        let ok = match &raw {
            ferriskey::Value::Array(arr) => {
                matches!(arr.first(), Some(Ok(ferriskey::Value::Int(1))))
            }
            _ => false,
        };
        if !ok {
            // Non-OK grant (capability mismatch mid-race, already granted, etc.).
            // Surface as a script error so the caller sees a typed error; callers
            // that want to continue polling will loop on their own.
            let code = match &raw {
                ferriskey::Value::Array(arr) => arr
                    .get(1)
                    .and_then(|v| match v {
                        Ok(ferriskey::Value::BulkString(b)) => {
                            Some(String::from_utf8_lossy(b).into_owned())
                        }
                        Ok(ferriskey::Value::SimpleString(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if let Some(err) = ScriptError::from_code_with_detail(&code, "") {
                // Retryable errors (capability_mismatch, grant_already_issued,
                // exec_not_eligible) → skip this partition, keep scanning.
                use ff_core::error::ErrorClass;
                let engine_err = EngineError::from(err);
                if matches!(
                    ff_script::engine_error_ext::class(&engine_err),
                    ErrorClass::Retryable | ErrorClass::Informational
                ) {
                    continue;
                }
                return Err(engine_err);
            }
            continue;
        }

        // Step 2 — claim the execution via ff_claim_execution.
        let att_idx_str: Option<String> = client
            .cmd("HGET")
            .arg(ctx.core())
            .arg("total_attempt_count")
            .execute()
            .await
            .map_err(transport_fk)?;
        let next_idx = att_idx_str
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let att_idx = AttemptIndex::new(next_idx);

        let lease_id = LeaseId::new();
        let attempt_id = AttemptId::new();

        let args = ClaimExecutionArgs::new(
            execution_id.clone(),
            policy.worker_id.clone(),
            policy.worker_instance_id.clone(),
            lane.clone(),
            lease_id.clone(),
            u64::from(policy.lease_ttl_ms),
            attempt_id.clone(),
            att_idx,
            "{}".to_owned(),
            None,
            None,
            TimestampMs::now(),
        );

        let exec_keys = ExecOpKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: lane,
            worker_instance_id: &policy.worker_instance_id,
        };

        let partial = match ff_claim_execution(client, &exec_keys, &args).await {
            Ok(p) => p,
            Err(err) => {
                use ff_core::error::ErrorClass;
                let engine_err = EngineError::from(err);
                if matches!(
                    ff_script::engine_error_ext::class(&engine_err),
                    ErrorClass::Retryable | ErrorClass::Informational
                ) {
                    continue;
                }
                return Err(engine_err);
            }
        };
        let ClaimExecutionResultPartial::Claimed(claimed) = partial;

        let fields = handle_codec::HandleFields::new(
            execution_id,
            claimed.attempt_index,
            claimed.attempt_id,
            claimed.lease_id,
            claimed.lease_epoch,
            u64::from(policy.lease_ttl_ms),
            lane.clone(),
            policy.worker_instance_id.clone(),
        );
        return Ok(Some(handle_codec::encode_handle(&fields, HandleKind::Fresh)));
    }

    Ok(None)
}

/// Resume-claim implementation — consumes a `ResumeToken` and routes
/// through `ff_claim_resumed_execution`. Worker identity + lease TTL
/// ride on the token (Wave 2 additive extension).
#[tracing::instrument(
    name = "ff.claim_from_resume_grant",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %token.grant.execution_id,
        worker_instance_id = %token.worker_instance_id,
    )
)]
async fn claim_from_resume_grant_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    token: ResumeToken,
) -> Result<Option<Handle>, EngineError> {
    let execution_id = token.grant.execution_id.clone();
    let lane_id = token.grant.lane_id.clone();
    let partition = execution_partition(&execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &execution_id);

    // Pre-read current_attempt_index for the existing attempt hash KEY.
    let att_idx_str: Option<String> = client
        .cmd("HGET")
        .arg(ctx.core())
        .arg("current_attempt_index")
        .execute()
        .await
        .map_err(transport_fk)?;
    let att_idx = AttemptIndex::new(
        att_idx_str
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0),
    );

    let lease_id = LeaseId::new();
    let args = ClaimResumedExecutionArgs {
        execution_id: execution_id.clone(),
        worker_id: token.worker_id.clone(),
        worker_instance_id: token.worker_instance_id.clone(),
        lane_id: lane_id.clone(),
        lease_id: lease_id.clone(),
        lease_ttl_ms: u64::from(token.lease_ttl_ms),
        current_attempt_index: att_idx,
        remaining_attempt_timeout_ms: None,
        now: TimestampMs::now(),
    };

    let ClaimResumedExecutionResult::Claimed(claimed) =
        claim_resumed_execution_impl(client, partition_config, args).await?;

    let fields = handle_codec::HandleFields::new(
        execution_id,
        claimed.attempt_index,
        claimed.attempt_id,
        claimed.lease_id,
        claimed.lease_epoch,
        u64::from(token.lease_ttl_ms),
        lane_id,
        token.worker_instance_id,
    );
    Ok(Some(handle_codec::encode_handle(&fields, HandleKind::Resumed)))
}

// ─── RFC-024 PR-F: issue_reclaim_grant + reclaim_execution ────────────

/// Valkey wiring for [`EngineBackend::issue_reclaim_grant`]. Forwards
/// directly to `ff_issue_reclaim_grant` (`flowfabric.lua:3898`).
///
/// KEYS (3): exec_core, claim_grant, lease_expiry
/// ARGV (9): execution_id, worker_id, worker_instance_id, lane_id,
///           capability_hash, grant_ttl_ms, route_snapshot_json,
///           admission_summary, worker_capabilities_csv
#[tracing::instrument(
    name = "ff.issue_reclaim_grant",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        worker_id = %args.worker_id,
        worker_instance_id = %args.worker_instance_id,
    )
)]
async fn issue_reclaim_grant_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    // Build the caps CSV (sorted, deterministic) matching `claim_impl`.
    // `worker_capabilities` is a `BTreeSet<String>`, already sorted +
    // deduplicated by the collection itself — iterate directly.
    let caps_csv = args
        .worker_capabilities
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");

    let core_k = ctx.core();
    let grant_k = ctx.claim_grant();
    let lease_expiry_k = idx.lease_expiry();
    let keys: [&str; 3] = [core_k.as_str(), grant_k.as_str(), lease_expiry_k.as_str()];

    let eid_s = args.execution_id.to_string();
    let worker_id_s = args.worker_id.to_string();
    let worker_instance_s = args.worker_instance_id.to_string();
    let lane_s = args.lane_id.to_string();
    let cap_hash_s = args.capability_hash.clone().unwrap_or_default();
    let ttl_s = args.grant_ttl_ms.to_string();
    let route_s = args.route_snapshot_json.clone().unwrap_or_default();
    let admission_s = args.admission_summary.clone().unwrap_or_default();
    let argv: [&str; 9] = [
        &eid_s,
        &worker_id_s,
        &worker_instance_s,
        &lane_s,
        &cap_hash_s,
        &ttl_s,
        &route_s,
        &admission_s,
        &caps_csv,
    ];

    let raw: ferriskey::Value = client
        .fcall("ff_issue_reclaim_grant", &keys, &argv)
        .await
        .map_err(transport_fk)?;

    let parsed = FcallResult::parse(&raw).map_err(EngineError::from)?;
    if parsed.success {
        // RFC-024 §3.1: grant-carried fields come from server. Lua returns
        // `ok(execution_id, grant_expires_at)` where `grant_expires_at`
        // is derived from the server's `TIME` + `grant_ttl_ms` — use it
        // directly rather than recomputing `args.now + grant_ttl_ms`
        // client-side (which would diverge under Rust↔Valkey clock skew).
        let expires_at_ms = parsed.field_str(1).parse::<u64>().map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "ff_issue_reclaim_grant".into(),
                execution_id: Some(args.execution_id.to_string()),
                message: format!("bad grant_expires_at: {e}"),
            })
        })?;
        let grant = ReclaimGrant::new(
            args.execution_id.clone(),
            ff_core::partition::PartitionKey::from(&partition),
            grant_k,
            expires_at_ms,
            args.lane_id.clone(),
        );
        return Ok(IssueReclaimGrantOutcome::Granted(grant));
    }
    match parsed.into_success().unwrap_err() {
        ScriptError::ExecutionNotReclaimable => {
            Ok(IssueReclaimGrantOutcome::NotReclaimable {
                execution_id: args.execution_id,
                detail: "execution_not_reclaimable".into(),
            })
        }
        ScriptError::CapabilityMismatch(missing) => {
            Ok(IssueReclaimGrantOutcome::NotReclaimable {
                execution_id: args.execution_id,
                detail: format!("capability_mismatch: {missing}"),
            })
        }
        err => Err(EngineError::from(err)),
    }
}

/// Valkey wiring for [`EngineBackend::reclaim_execution`]. Forwards to
/// `ff_reclaim_execution` (`flowfabric.lua:2985`) with ARGV[9] threading
/// the caller's [`ReclaimExecutionArgs::max_reclaim_count`] (defaulting
/// to 1000 per RFC-024 §4.6).
#[tracing::instrument(
    name = "ff.reclaim_execution",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        worker_id = %args.worker_id,
        worker_instance_id = %args.worker_instance_id,
    )
)]
async fn reclaim_execution_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: ReclaimExecutionArgs,
) -> Result<ReclaimExecutionOutcome, EngineError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    let old_att_idx = args.current_attempt_index;
    let new_att_idx = AttemptIndex::new(old_att_idx.0 + 1);

    // KEYS (14) — mirror the e2e_lifecycle helper + the Lua KEYS comment
    // at flowfabric.lua:2977.
    let core_k = ctx.core();
    let grant_k = ctx.claim_grant();
    let old_att_k = ctx.attempt_hash(old_att_idx);
    let old_stream_k = ctx.stream_meta(old_att_idx);
    let new_att_k = ctx.attempt_hash(new_att_idx);
    let new_att_usage_k = ctx.attempt_usage(new_att_idx);
    let attempts_k = ctx.attempts();
    let lease_cur_k = ctx.lease_current();
    let lease_hist_k = ctx.lease_history();
    let lease_expiry_k = idx.lease_expiry();
    let worker_leases_k = idx.worker_leases(&args.worker_instance_id);
    let lane_active_k = idx.lane_active(&args.lane_id);
    let att_timeout_k = idx.attempt_timeout();
    let exec_deadline_k = idx.execution_deadline();
    let keys: [&str; 14] = [
        core_k.as_str(),
        grant_k.as_str(),
        old_att_k.as_str(),
        old_stream_k.as_str(),
        new_att_k.as_str(),
        new_att_usage_k.as_str(),
        attempts_k.as_str(),
        lease_cur_k.as_str(),
        lease_hist_k.as_str(),
        lease_expiry_k.as_str(),
        worker_leases_k.as_str(),
        lane_active_k.as_str(),
        att_timeout_k.as_str(),
        exec_deadline_k.as_str(),
    ];

    // RFC-024 §4.6: Rust-surface default is 1000 when `None`.
    let default_max = args.max_reclaim_count.unwrap_or(1000);

    let eid_s = args.execution_id.to_string();
    let worker_id_s = args.worker_id.to_string();
    let worker_instance_s = args.worker_instance_id.to_string();
    let lane_s = args.lane_id.to_string();
    let lease_id_s = args.lease_id.to_string();
    let lease_ttl_s = args.lease_ttl_ms.to_string();
    let attempt_id_s = args.attempt_id.to_string();
    let attempt_policy = if args.attempt_policy_json.is_empty() {
        "{}".to_owned()
    } else {
        args.attempt_policy_json.clone()
    };
    let default_max_s = default_max.to_string();
    // ARGV (9) — 8 existing + 1 new ARGV[9] = default_max_reclaim_count.
    let argv: [&str; 9] = [
        &eid_s,
        &worker_id_s,
        &worker_instance_s,
        &lane_s,
        &lease_id_s,
        &lease_ttl_s,
        &attempt_id_s,
        &attempt_policy,
        &default_max_s,
    ];

    let raw: ferriskey::Value = client
        .fcall("ff_reclaim_execution", &keys, &argv)
        .await
        .map_err(transport_fk)?;

    let parsed = FcallResult::parse(&raw).map_err(EngineError::from)?;
    if parsed.success {
        // ok(lease_id, epoch, expires_at, attempt_id, attempt_index, "reclaim")
        let lease_id = LeaseId::parse(&parsed.field_str(0)).map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "ff_reclaim_execution".into(),
                execution_id: Some(args.execution_id.to_string()),
                message: format!("bad lease_id: {e}"),
            })
        })?;
        let epoch = parsed.field_str(1).parse::<u64>().map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "ff_reclaim_execution".into(),
                execution_id: Some(args.execution_id.to_string()),
                message: format!("bad epoch: {e}"),
            })
        })?;
        let attempt_id = AttemptId::parse(&parsed.field_str(3)).map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "ff_reclaim_execution".into(),
                execution_id: Some(args.execution_id.to_string()),
                message: format!("bad attempt_id: {e}"),
            })
        })?;
        let attempt_index_u: u32 = parsed.field_str(4).parse().map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "ff_reclaim_execution".into(),
                execution_id: Some(args.execution_id.to_string()),
                message: format!("bad attempt_index: {e}"),
            })
        })?;

        let fields = handle_codec::HandleFields::new(
            args.execution_id,
            AttemptIndex::new(attempt_index_u),
            attempt_id,
            lease_id,
            LeaseEpoch::new(epoch),
            args.lease_ttl_ms,
            args.lane_id,
            args.worker_instance_id,
        );
        let handle = handle_codec::encode_handle(&fields, HandleKind::Reclaimed);
        return Ok(ReclaimExecutionOutcome::Claimed(handle));
    }
    // Pre-extract the authoritative cap value before we consume `parsed`
    // with `into_success()`. For `max_retries_exhausted` the Lua tail is
    // `err("max_retries_exhausted", tostring(max_reclaim))` where
    // `max_reclaim` is the post-policy-override enforced cap (RFC-024
    // §4.6 resolution order). Falling back to `default_max` preserves
    // the previous behaviour if the field is missing or unparseable.
    let authoritative_cap: u32 = parsed
        .field_str(0)
        .parse::<u32>()
        .unwrap_or(default_max);
    match parsed.into_success().unwrap_err() {
        ScriptError::InvalidClaimGrant => Ok(ReclaimExecutionOutcome::GrantNotFound {
            execution_id: args.execution_id,
        }),
        ScriptError::ExecutionNotReclaimable => Ok(ReclaimExecutionOutcome::NotReclaimable {
            execution_id: args.execution_id,
            detail: "execution_not_reclaimable".into(),
        }),
        ScriptError::MaxRetriesExhausted => {
            // Lua set terminal_failed + returned
            // `err("max_retries_exhausted", tostring(max_reclaim))`. Use
            // the authoritative value (which reflects per-execution
            // policy override if present) rather than the caller's
            // Rust-side `default_max`.
            Ok(ReclaimExecutionOutcome::ReclaimCapExceeded {
                execution_id: args.execution_id,
                reclaim_count: authoritative_cap,
            })
        }
        err => Err(EngineError::from(err)),
    }
}

/// Thin forwarder to
/// `ff_script::functions::signal::ff_claim_resumed_execution`. The Lua
/// returns a partial result (omits `execution_id`, which the caller
/// already holds in the args); we re-hydrate before returning.
#[tracing::instrument(
    name = "ff.claim_resumed_execution",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        worker_instance_id = %args.worker_instance_id,
        lease_id = %args.lease_id,
    )
)]
async fn claim_resumed_execution_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: ClaimResumedExecutionArgs,
) -> Result<ClaimResumedExecutionResult, EngineError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    let keys = ExecOpKeys {
        ctx: &ctx,
        idx: &idx,
        lane_id: &args.lane_id,
        worker_instance_id: &args.worker_instance_id,
    };
    let execution_id = args.execution_id.clone();
    let partial = ff_claim_resumed_execution(client, &keys, &args)
        .await
        .map_err(EngineError::from)?;
    let mut result = partial.complete(execution_id);
    // v0.12 PR-5.5: overwrite the stub handle with the real
    // Valkey-encoded one (`HandleKind::Resumed`). Lane + worker_instance
    // ride on `args`.
    let ClaimResumedExecutionResult::Claimed(claimed) = &mut result;
    let fields = handle_codec::HandleFields::new(
        claimed.execution_id.clone(),
        claimed.attempt_index,
        claimed.attempt_id.clone(),
        claimed.lease_id.clone(),
        claimed.lease_epoch,
        args.lease_ttl_ms,
        args.lane_id.clone(),
        args.worker_instance_id.clone(),
    );
    claimed.handle = handle_codec::encode_handle(&fields, HandleKind::Resumed);
    Ok(result)
}

/// Scan a lane's eligible ZSET on one partition via `ZRANGEBYSCORE`.
/// Lifted verbatim from the pre-v0.12-PR-5 SDK inline helper
/// (`FlowFabricWorker::claim_next` at `ff-sdk/src/worker.rs:~624-637`).
/// Keep the command shape byte-for-byte identical so bench traces
/// match pre-PR.
#[tracing::instrument(
    name = "ff.scan_eligible_executions",
    skip_all,
    fields(backend = "valkey", lane_id = %args.lane_id, limit = args.limit)
)]
async fn scan_eligible_executions_impl(
    client: &ferriskey::Client,
    args: ScanEligibleArgs,
) -> Result<Vec<ExecutionId>, EngineError> {
    let idx = IndexKeys::new(&args.partition);
    let eligible_key = idx.lane_eligible(&args.lane_id);
    let limit = args.limit.to_string();

    // Score format: -(priority * 1_000_000_000_000) + created_at_ms.
    // `ZRANGEBYSCORE -inf +inf LIMIT 0 <limit>` yields lowest-score-
    // first == highest-priority-first.
    let raw: ferriskey::Value = client
        .cmd("ZRANGEBYSCORE")
        .arg(&eligible_key)
        .arg("-inf")
        .arg("+inf")
        .arg("LIMIT")
        .arg("0")
        .arg(&limit)
        .execute()
        .await
        .map_err(transport_fk)?;

    let strings: Vec<String> = match raw {
        ferriskey::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item {
                    Ok(ferriskey::Value::BulkString(b)) => {
                        out.push(String::from_utf8_lossy(&b).into_owned());
                    }
                    Ok(ferriskey::Value::SimpleString(s)) => out.push(s),
                    _ => {}
                }
            }
            out
        }
        _ => Vec::new(),
    };

    let mut ids = Vec::with_capacity(strings.len());
    for s in strings {
        let id = ExecutionId::parse(&s).map_err(|e| {
            transport_script(ScriptError::Parse {
                fcall: "claim_execution_from_eligible_set".into(),
                execution_id: None,
                message: format!("bad execution_id in eligible set: {e}"),
            })
        })?;
        ids.push(id);
    }
    Ok(ids)
}

/// Issue a claim grant via `ff_issue_claim_grant`. Lifted verbatim
/// from the pre-v0.12-PR-5 SDK inline helper
/// (`FlowFabricWorker::issue_claim_grant` at
/// `ff-sdk/src/worker.rs:~763-804`). Wire (KEYS/ARGV) shape is
/// byte-for-byte identical so bench traces match pre-PR.
#[tracing::instrument(
    name = "ff.issue_claim_grant",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        worker_id = %args.worker_id,
        lane_id = %args.lane_id,
    )
)]
async fn issue_claim_grant_impl(
    client: &ferriskey::Client,
    args: IssueClaimGrantArgs,
) -> Result<IssueClaimGrantOutcome, EngineError> {
    let ctx = ExecKeyContext::new(&args.partition, &args.execution_id);
    let idx = IndexKeys::new(&args.partition);

    // KEYS (3): exec_core, claim_grant_key, eligible_zset
    let keys_owned: [String; 3] = [
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&args.lane_id),
    ];
    let keys_ref: [&str; 3] = [
        keys_owned[0].as_str(),
        keys_owned[1].as_str(),
        keys_owned[2].as_str(),
    ];

    // BTreeSet iterates in sorted order → stable CSV for Lua match.
    let caps_csv: String = args
        .worker_capabilities
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");

    // ARGV (9): eid, worker_id, worker_instance_id, lane_id,
    //           capability_hash, grant_ttl_ms, route_snapshot_json,
    //           admission_summary, worker_capabilities_csv (sorted)
    let eid_s = args.execution_id.to_string();
    let worker_id_s = args.worker_id.to_string();
    let worker_instance_s = args.worker_instance_id.to_string();
    let lane_s = args.lane_id.to_string();
    let grant_ttl_s = args.grant_ttl_ms.to_string();
    let cap_hash = args.capability_hash.clone().unwrap_or_default();
    let route_snap = args.route_snapshot_json.clone().unwrap_or_default();
    let admission_sum = args.admission_summary.clone().unwrap_or_default();
    let argv: [&str; 9] = [
        &eid_s,
        &worker_id_s,
        &worker_instance_s,
        &lane_s,
        &cap_hash,
        &grant_ttl_s,
        &route_snap,
        &admission_sum,
        &caps_csv,
    ];

    let raw: ferriskey::Value = client
        .fcall("ff_issue_claim_grant", &keys_ref, &argv)
        .await
        .map_err(transport_fk)?;

    // Parse grant result: {1, "OK", ...} (same shape the SDK
    // inline helper parsed via `parse_success_result`).
    //
    // Lua rejects (`CapabilityMismatch`, `already_granted`, ...) must
    // flow through `From<ScriptError> for EngineError` so typed
    // variants (`EngineError::Validation { CapabilityMismatch, .. }`)
    // reach `FlowFabricWorker::claim_next`'s block-on-mismatch arm.
    // Only raw ferriskey transport faults use `transport_script`.
    let parsed = ff_script::result::FcallResult::parse(&raw).map_err(EngineError::from)?;
    let _ = parsed.into_success().map_err(EngineError::from)?;
    Ok(IssueClaimGrantOutcome::Granted {
        execution_id: args.execution_id,
    })
}

/// Block a route via `ff_block_execution_for_admission`. Lifted
/// verbatim from the pre-v0.12-PR-5 SDK inline helper
/// (`FlowFabricWorker::block_route` at
/// `ff-sdk/src/worker.rs:~818-866`). Wire (KEYS/ARGV) shape is
/// byte-for-byte identical so bench traces match pre-PR.
#[tracing::instrument(
    name = "ff.block_route",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        lane_id = %args.lane_id,
    )
)]
async fn block_route_impl(
    client: &ferriskey::Client,
    args: BlockRouteArgs,
) -> Result<BlockRouteOutcome, EngineError> {
    let ctx = ExecKeyContext::new(&args.partition, &args.execution_id);
    let idx = IndexKeys::new(&args.partition);

    let core_key = ctx.core();
    let eligible_key = idx.lane_eligible(&args.lane_id);
    let blocked_key = idx.lane_blocked_route(&args.lane_id);
    let eid_s = args.execution_id.to_string();
    let now_ms = args.now.0.to_string();

    let keys: [&str; 3] = [&core_key, &eligible_key, &blocked_key];
    let argv: [&str; 4] = [
        &eid_s,
        &args.reason_code,
        &args.reason_detail,
        &now_ms,
    ];

    let raw: ferriskey::Value = client
        .fcall("ff_block_execution_for_admission", &keys, &argv)
        .await
        .map_err(transport_fk)?;

    // Parse the Lua response so a logical reject (e.g. execution went
    // terminal mid-flight) surfaces as `LuaRejected` rather than
    // silently succeeding. Mirrors the SDK-side
    // `parse_success_result(&v, "ff_block_execution_for_admission")`
    // check with the non-transport branch promoted into the outcome
    // enum.
    // Parse-time errors are transport-level (malformed FCALL
    // envelope). Business-logic Lua rejects surface via
    // `into_success()` and are folded into `LuaRejected` so the
    // caller's best-effort continue-on-reject semantic holds.
    let parsed = ff_script::result::FcallResult::parse(&raw).map_err(EngineError::from)?;
    match parsed.into_success() {
        Ok(_) => Ok(BlockRouteOutcome::Blocked {
            execution_id: args.execution_id,
        }),
        Err(e) => Ok(BlockRouteOutcome::LuaRejected {
            message: e.to_string(),
        }),
    }
}

/// Grant-consumer path for [`EngineBackend::claim_execution`].
///
/// Fires exactly one `ff_claim_execution` FCALL against the
/// execution's partition, consuming the scheduler-issued claim grant
/// and minting the attempt row. Key/ARGV shape is identical to what
/// `claim_impl` (the scheduler-bypass scanner) fires post-grant and
/// to the pre-PR-4 direct-FCALL helper on the SDK worker — this
/// extraction preserves every observable Valkey write byte-for-byte.
#[tracing::instrument(
    name = "ff.claim_execution",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        worker_instance_id = %args.worker_instance_id,
        lease_id = %args.lease_id,
    )
)]
async fn claim_execution_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: ClaimExecutionArgs,
) -> Result<ClaimExecutionResult, EngineError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    let keys = ExecOpKeys {
        ctx: &ctx,
        idx: &idx,
        lane_id: &args.lane_id,
        worker_instance_id: &args.worker_instance_id,
    };
    let execution_id = args.execution_id.clone();
    let partial = ff_claim_execution(client, &keys, &args)
        .await
        .map_err(EngineError::from)?;
    let mut result = partial.complete(execution_id);
    // v0.12 PR-5.5: overwrite the stub handle seeded by
    // `ClaimExecutionResultPartial::complete` with the real Valkey-encoded
    // one so the SDK's `ClaimedTask::new` no longer synthesises via
    // `ValkeyBackend::encode_handle` at the worker layer.
    // `ClaimExecutionResult` is `#[non_exhaustive]` with `Claimed` as
    // the only variant today; if a future variant needs a handle the
    // new arm will land here.
    if let ClaimExecutionResult::Claimed(claimed) = &mut result {
        let fields = handle_codec::HandleFields::new(
            claimed.execution_id.clone(),
            claimed.attempt_index,
            claimed.attempt_id.clone(),
            claimed.lease_id.clone(),
            claimed.lease_epoch,
            args.lease_ttl_ms,
            args.lane_id.clone(),
            args.worker_instance_id.clone(),
        );
        claimed.handle = handle_codec::encode_handle(&fields, HandleKind::Fresh);
    }
    Ok(result)
}

/// cairn #454 Phase 3d — backend-atomic `issue_claim_grant` +
/// `claim_execution` composition. One `ff_issue_grant_and_claim` FCALL
/// writes the grant, DEL's it, and mints the lease in a single Lua
/// execution so a mid-op crash cannot leak a dangling grant.
///
/// The trait-level [`IssueGrantAndClaimArgs`] is intentionally minimal
/// (`execution_id`, `lane_id`, `lease_duration_ms`) because this is an
/// operator/control-plane path: cairn names an execution and demands a
/// lease, the backend mints synthetic worker/lease/attempt identities
/// to stamp onto the attempt + lease records. The minted
/// `WorkerId`/`WorkerInstanceId` use stable `"operator"` labels so
/// dashboards can distinguish control-plane leases from worker-dialed
/// ones.
///
/// Dispatch between fresh-claim and resume-claim happens inside the
/// Lua based on `exec_core.attempt_state`; the Rust side doesn't
/// pre-read the dispatch bit because TOCTOU risk + re-reading
/// `current_attempt_index` would require a second round trip (defeating
/// the whole point of the fused FCALL). `expected_attempt_index` is
/// passed as a sentinel — the Lua builds attempt keys dynamically
/// from `total_attempt_count` (fresh) or `current_attempt_index`
/// (resume), and the corresponding KEYS slots were dropped from the
/// FCALL signature so no Rust-side round trip is needed to compute
/// placeholder keys.
#[tracing::instrument(
    name = "ff.issue_grant_and_claim",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %args.execution_id,
        lane_id = %args.lane_id,
        lease_duration_ms = args.lease_duration_ms,
    )
)]
async fn issue_grant_and_claim_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    args: IssueGrantAndClaimArgs,
) -> Result<ClaimGrantOutcome, EngineError> {
    let partition = execution_partition(&args.execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, &args.execution_id);
    let idx = IndexKeys::new(&partition);

    // Synthetic operator identity. Keeps `current_worker_id` /
    // `current_worker_instance_id` populated on the exec_core for
    // dashboards + lease-history audit without pretending the lease
    // is owned by a real worker-dialed SDK instance.
    let worker_id = WorkerId::new("operator");
    let worker_instance_id = WorkerInstanceId::new("operator");
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();

    // No pre-read of `total_attempt_count`: the Lua body builds
    // attempt keys dynamically from the index it reads internally, and
    // attempt hash / usage / policy keys are no longer declared in the
    // FCALL's KEYS vector (all attempt keys share the `{tag}` hash so
    // they live in the same cluster slot as `exec_core`). The
    // `expected_attempt_index` field on `ClaimExecutionArgs` is unused
    // by this FCALL — pass a zero sentinel.
    let ce_args = ClaimExecutionArgs::new(
        args.execution_id.clone(),
        worker_id,
        worker_instance_id.clone(),
        args.lane_id.clone(),
        lease_id,
        args.lease_duration_ms,
        attempt_id,
        AttemptIndex::new(0),
        "{}".to_owned(),
        None,
        None,
        TimestampMs::now(),
    );

    let keys = ExecOpKeys {
        ctx: &ctx,
        idx: &idx,
        lane_id: &args.lane_id,
        worker_instance_id: &worker_instance_id,
    };

    let partial = ff_issue_grant_and_claim(client, &keys, &ce_args)
        .await
        .map_err(EngineError::from)?;
    Ok(partial.complete())
}

/// RFC-017 Stage B: acquire a stream-op permit off
/// `ValkeyBackend::stream_semaphore`. Non-blocking — saturation
/// surfaces as [`EngineError::ResourceExhausted { pool: "stream_ops",
/// max, retry_after_ms: Some(..) }`] which the REST boundary maps to
/// HTTP 429. `Closed` (set during `shutdown_prepare`) surfaces as
/// [`EngineError::Unavailable { op: "stream_ops" }`] → HTTP 503.
fn acquire_stream_permit(
    backend: &ValkeyBackend,
) -> Result<tokio::sync::OwnedSemaphorePermit, EngineError> {
    match backend.stream_semaphore.clone().try_acquire_owned() {
        Ok(p) => Ok(p),
        Err(tokio::sync::TryAcquireError::NoPermits) => {
            // Retry hint: `25ms × (max / available+1)` is a crude
            // back-off proxy — avoids the "retry immediately"
            // stampede when a single slow tail holds a permit. A
            // production-grade heuristic lives at the REST boundary;
            // this one is adequate and keeps the retry-hint wire
            // populated.
            let available = backend.stream_semaphore.available_permits();
            let hint_ms =
                25u32.saturating_mul(backend.stream_semaphore_max).saturating_div(
                    (available as u32).saturating_add(1),
                );
            Err(EngineError::ResourceExhausted {
                pool: "stream_ops",
                max: backend.stream_semaphore_max,
                retry_after_ms: Some(hint_ms.max(25)),
            })
        }
        Err(tokio::sync::TryAcquireError::Closed) => Err(EngineError::Unavailable {
            op: "stream_ops",
        }),
    }
}

#[async_trait]
impl EngineBackend for ValkeyBackend {
    fn as_any(&self) -> &(dyn std::any::Any + 'static) {
        self
    }

    async fn claim(
        &self,
        lane: &LaneId,
        capabilities: &CapabilitySet,
        policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        claim_impl(&self.client, &self.partition_config, lane, capabilities, &policy)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(e, "claim: FCALL ff_claim_execution"))
    }

    async fn renew(&self, handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        // Decode first. A decode failure is caller-input
        // malformation (corrupt backend tag / version / field shape)
        // — it is NOT an attempted renewal, so we deliberately do
        // NOT fire `inc_lease_renewal` on this path. The counter
        // measures renew RPCs, and a caller handing us a bad handle
        // never issued one.
        let f = handle_codec::decode_handle(handle)?;
        let result = renew_impl(&self.client, &self.partition_config, &f)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(e, "renew: FCALL ff_renew_lease"));
        // Issue #154 — fire the production `inc_lease_renewal` counter
        // from the trait boundary. The handle is a no-op when the
        // `observability` feature is off; when on, this is the first
        // production emission site (was tests-only previously).
        if let Some(metrics) = &self.metrics {
            metrics.inc_lease_renewal(if result.is_ok() { "ok" } else { "error" });
        }
        result
    }

    async fn progress(
        &self,
        handle: &Handle,
        percent: Option<u8>,
        message: Option<String>,
    ) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        progress_impl(
            &self.client,
            &self.partition_config,
            &f,
            percent,
            message.as_deref(),
        )
        .await
        .map_err(|e| ff_core::engine_error::backend_context(e, "progress: FCALL ff_update_progress"))
    }

    async fn append_frame(
        &self,
        handle: &Handle,
        frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        append_frame_impl(&self.client, &self.partition_config, &f, frame)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "append_frame: FCALL ff_append_frame")
            })
    }

    async fn complete(&self, handle: &Handle, payload: Option<Vec<u8>>) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        let result = complete_impl(&self.client, &self.partition_config, &f, payload)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "complete: FCALL ff_complete_execution")
            });
        // Fire `ff_attempt_outcome_total` only after the FCALL-side
        // terminal is confirmed (Ok, including reconciled replays).
        // Errors pre-terminal do NOT count as an outcome.
        if let (Ok(()), Some(metrics)) = (&result, &self.metrics) {
            metrics.inc_attempt_outcome(f.lane_id.as_str(), ff_observability::AttemptOutcome::Ok);
        }
        result
    }

    async fn fail(
        &self,
        handle: &Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        let is_timeout = classification == FailureClass::Timeout;
        let result = fail_impl(
            &self.client,
            &self.partition_config,
            &f,
            reason,
            classification,
        )
        .await
        .map_err(|e| ff_core::engine_error::backend_context(e, "fail: FCALL ff_fail_execution"));
        // Map FailOutcome + classification to the metric label:
        //   RetryScheduled          → "retry"
        //   TerminalFailed(Timeout) → "timeout"
        //   TerminalFailed(other)   → "error"
        if let (Ok(outcome), Some(metrics)) = (&result, &self.metrics) {
            let label = match outcome {
                FailOutcome::RetryScheduled { .. } => ff_observability::AttemptOutcome::Retry,
                FailOutcome::TerminalFailed if is_timeout => {
                    ff_observability::AttemptOutcome::Timeout
                }
                FailOutcome::TerminalFailed => ff_observability::AttemptOutcome::Error,
            };
            metrics.inc_attempt_outcome(f.lane_id.as_str(), label);
        }
        result
    }

    async fn cancel(&self, handle: &Handle, reason: &str) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        let result = cancel_impl(&self.client, &self.partition_config, &f, reason)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "cancel: FCALL ff_cancel_execution")
            });
        if let (Ok(()), Some(metrics)) = (&result, &self.metrics) {
            metrics.inc_attempt_outcome(
                f.lane_id.as_str(),
                ff_observability::AttemptOutcome::Cancelled,
            );
        }
        result
    }

    async fn suspend(
        &self,
        handle: &Handle,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        // Pre-FCALL handle-kind check (RFC-013 §3.2 — the Suspended-kind
        // pre-check fires before any dedup lookup and is not dedup-
        // dodgeable).
        if handle.kind == HandleKind::Suspended {
            return Err(EngineError::State(StateKind::AlreadySuspended));
        }
        // RFC-013 §4 — Rust-side input validation fires before the
        // handle decode so malformed SuspendArgs surface as
        // `Validation(InvalidInput)` regardless of handle bytes shape.
        validate_suspend_args(&args)?;
        let f = handle_codec::decode_handle(handle)?;
        suspend_impl(&self.client, &self.partition_config, &f, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "suspend: FCALL ff_suspend_execution")
            })
    }

    async fn suspend_by_triple(
        &self,
        exec_id: ExecutionId,
        triple: LeaseFence,
        args: SuspendArgs,
    ) -> Result<SuspendOutcome, EngineError> {
        // RFC-013 §4 — validate args before any pre-read so malformed
        // SuspendArgs surface as `Validation(InvalidInput)` regardless
        // of execution state. Mirrors `suspend`.
        validate_suspend_args(&args)?;

        // Pre-read the exec_core fields that `suspend_impl` would
        // otherwise pull from the Handle payload: lane_id, current
        // attempt_index, current worker_instance_id. Lease fence fields
        // (lease_id, lease_epoch, attempt_id) come from `triple`
        // directly — the Lua side fences against these on `lease_current`.
        let partition = execution_partition(&exec_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &exec_id);
        let core_key = ctx.core();
        let core: HashMap<String, String> = self
            .client
            .cmd("HGETALL")
            .arg(&core_key)
            .execute()
            .await
            .map_err(transport_fk)?;
        if core.is_empty() {
            return Err(EngineError::NotFound { entity: "execution" });
        }
        let lane_str = core.get("lane_id").cloned().unwrap_or_default();
        if lane_str.is_empty() {
            return Err(EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: "suspend_by_triple: exec_core missing lane_id".into(),
            });
        }
        let att_idx_str = core
            .get("current_attempt_index")
            .cloned()
            .unwrap_or_default();
        let att_idx: u32 = att_idx_str.parse().map_err(|_| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "suspend_by_triple: exec_core.current_attempt_index not a u32: {att_idx_str:?}"
            ),
        })?;
        let wiid_str = core
            .get("current_worker_instance_id")
            .cloned()
            .unwrap_or_default();
        // An empty `current_worker_instance_id` means the lease was
        // already released — the fence check on `lease_current` will
        // fail with `stale_lease`; pass a placeholder so we still reach
        // the FCALL and surface the canonical error.
        let worker_instance_id = WorkerInstanceId::new(wiid_str);

        let fields = handle_codec::HandleFields::new(
            exec_id,
            AttemptIndex::new(att_idx),
            triple.attempt_id,
            triple.lease_id,
            triple.lease_epoch,
            // lease_ttl_ms is unused on the suspend FCALL path; pass 0.
            0,
            LaneId::new(lane_str),
            worker_instance_id,
        );
        suspend_impl(&self.client, &self.partition_config, &fields, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "suspend_by_triple: FCALL ff_suspend_execution",
                )
            })
    }

    async fn create_waitpoint(
        &self,
        handle: &Handle,
        waitpoint_key: &str,
        expires_in: Duration,
    ) -> Result<PendingWaitpoint, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        create_waitpoint_impl(
            &self.client,
            &self.partition_config,
            &f,
            waitpoint_key,
            expires_in,
        )
        .await
        .map_err(|e| {
            ff_core::engine_error::backend_context(
                e,
                "create_waitpoint: FCALL ff_create_pending_waitpoint",
            )
        })
    }

    async fn read_waitpoint_token(
        &self,
        partition: PartitionKey,
        waitpoint_id: &WaitpointId,
    ) -> Result<Option<String>, EngineError> {
        read_waitpoint_token_impl(&self.client, &partition, waitpoint_id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "read_waitpoint_token: HGET ff:wp:<tag>:<wp_id> waitpoint_token",
                )
            })
    }

    async fn observe_signals(
        &self,
        handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        observe_signals_impl(&self.client, &self.partition_config, &f)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "observe_signals: HGETALL suspension + HMGET matcher slots",
                )
            })
    }

    async fn claim_from_resume_grant(
        &self,
        token: ResumeToken,
    ) -> Result<Option<Handle>, EngineError> {
        claim_from_resume_grant_impl(&self.client, &self.partition_config, token)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "claim_from_resume_grant: FCALL ff_claim_resumed_execution",
                )
            })
    }

    async fn issue_reclaim_grant(
        &self,
        args: IssueReclaimGrantArgs,
    ) -> Result<IssueReclaimGrantOutcome, EngineError> {
        issue_reclaim_grant_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "issue_reclaim_grant: FCALL ff_issue_reclaim_grant",
                )
            })
    }

    async fn reclaim_execution(
        &self,
        args: ReclaimExecutionArgs,
    ) -> Result<ReclaimExecutionOutcome, EngineError> {
        reclaim_execution_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "reclaim_execution: FCALL ff_reclaim_execution",
                )
            })
    }

    async fn delay(&self, handle: &Handle, delay_until: TimestampMs) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        delay_impl(&self.client, &self.partition_config, &f, delay_until)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "delay: FCALL ff_delay_execution")
            })
    }

    async fn wait_children(&self, handle: &Handle) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        wait_children_impl(&self.client, &self.partition_config, &f)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "wait_children: FCALL ff_move_to_waiting_children",
                )
            })
    }

    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        describe_execution_impl(&self.client, &self.partition_config, id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "describe_execution: HGETALL exec_core + tags",
                )
            })
    }

    async fn read_execution_context(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<ExecutionContext, EngineError> {
        read_execution_context_impl(&self.client, &self.partition_config, execution_id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "read_execution_context: GET payload + HGETALL core + HGETALL tags",
                )
            })
    }

    async fn read_current_attempt_index(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<AttemptIndex, EngineError> {
        read_current_attempt_index_impl(&self.client, &self.partition_config, execution_id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "read_current_attempt_index: HGET exec_core.current_attempt_index",
                )
            })
    }

    async fn read_total_attempt_count(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<AttemptIndex, EngineError> {
        read_total_attempt_count_impl(&self.client, &self.partition_config, execution_id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "read_total_attempt_count: HGET exec_core.total_attempt_count",
                )
            })
    }

    async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError> {
        describe_flow_impl(&self.client, &self.partition_config, id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "describe_flow: HGETALL flow_core")
            })
    }

    async fn set_execution_tag(
        &self,
        execution_id: &ExecutionId,
        key: &str,
        value: &str,
    ) -> Result<(), EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        set_execution_tag_impl(&self.client, &self.partition_config, execution_id, key, value)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "set_execution_tag: FCALL ff_set_execution_tags",
                )
            })
    }

    async fn set_flow_tag(
        &self,
        flow_id: &FlowId,
        key: &str,
        value: &str,
    ) -> Result<(), EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        set_flow_tag_impl(&self.client, &self.partition_config, flow_id, key, value)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "set_flow_tag: FCALL ff_set_flow_tags",
                )
            })
    }

    async fn get_execution_tag(
        &self,
        execution_id: &ExecutionId,
        key: &str,
    ) -> Result<Option<String>, EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        get_execution_tag_impl(&self.client, &self.partition_config, execution_id, key)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "get_execution_tag: HGET :tags")
            })
    }

    async fn get_flow_tag(
        &self,
        flow_id: &FlowId,
        key: &str,
    ) -> Result<Option<String>, EngineError> {
        ff_core::engine_backend::validate_tag_key(key)?;
        get_flow_tag_impl(&self.client, &self.partition_config, flow_id, key)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "get_flow_tag: HGET :tags")
            })
    }

    async fn get_execution_namespace(
        &self,
        execution_id: &ExecutionId,
    ) -> Result<Option<String>, EngineError> {
        get_execution_namespace_impl(&self.client, &self.partition_config, execution_id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "get_execution_namespace: HGET :core namespace",
                )
            })
    }

    async fn list_edges(
        &self,
        flow_id: &FlowId,
        direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        list_edges_impl(&self.client, &self.partition_config, flow_id, direction)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "list_edges: pipeline SMEMBERS adj + HGETALL edge",
                )
            })
    }

    async fn describe_edge(
        &self,
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, EngineError> {
        describe_edge_impl(&self.client, &self.partition_config, flow_id, edge_id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "describe_edge: HGETALL edge")
            })
    }

    async fn resolve_execution_flow_id(
        &self,
        eid: &ExecutionId,
    ) -> Result<Option<FlowId>, EngineError> {
        resolve_execution_flow_id_impl(&self.client, &self.partition_config, eid)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "resolve_execution_flow_id: HGET exec_core.flow_id",
                )
            })
    }

    async fn list_flows(
        &self,
        partition: ff_core::partition::PartitionKey,
        cursor: Option<FlowId>,
        limit: usize,
    ) -> Result<ListFlowsPage, EngineError> {
        list_flows_impl(&self.client, &partition, cursor, limit)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "list_flows: SMEMBERS flow_index + pipeline HGETALL flow_core",
                )
            })
    }

    async fn list_lanes(
        &self,
        cursor: Option<LaneId>,
        limit: usize,
    ) -> Result<ListLanesPage, EngineError> {
        list_lanes_impl(&self.client, cursor, limit)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "list_lanes: SMEMBERS ff:idx:lanes")
            })
    }

    async fn list_suspended(
        &self,
        partition: PartitionKey,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListSuspendedPage, EngineError> {
        list_suspended_impl(&self.client, &self.partition_config, partition, cursor, limit)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "list_suspended: SCAN lane:*:suspended + ZRANGE + HMGET reason",
                )
            })
    }

    async fn list_executions(
        &self,
        partition: PartitionKey,
        cursor: Option<ExecutionId>,
        limit: usize,
    ) -> Result<ListExecutionsPage, EngineError> {
        list_executions_impl(&self.client, &partition, cursor.as_ref(), limit)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "list_executions: SMEMBERS all_executions + sort/filter",
                )
            })
    }

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        let result = cancel_flow_fcall(&self.client, &self.partition_config, id, policy)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "cancel_flow: FCALL ff_cancel_flow")
            })?;
        if let Some(deadline) = ff_core::engine_backend::cancel_flow_wait_deadline(wait) {
            ff_core::engine_backend::wait_for_flow_cancellation(self, id, deadline).await?;
        }
        Ok(result)
    }

    async fn set_edge_group_policy(
        &self,
        flow_id: &FlowId,
        downstream_execution_id: &ExecutionId,
        policy: ff_core::contracts::EdgeDependencyPolicy,
    ) -> Result<ff_core::contracts::SetEdgeGroupPolicyResult, EngineError> {
        let policy_label = policy.variant_str();
        let result = set_edge_group_policy_impl(
            &self.client,
            &self.partition_config,
            flow_id,
            downstream_execution_id,
            policy,
        )
        .await
        .map_err(|e| {
            ff_core::engine_error::backend_context(
                e,
                "set_edge_group_policy: FCALL ff_set_edge_group_policy",
            )
        })?;
        if let Some(m) = &self.metrics {
            // `policy_label` is one of the static &'static str literals
            // returned by `EdgeDependencyPolicy::variant_str`. Safe to
            // pass to `inc_edge_group_policy` which expects &'static str.
            m.inc_edge_group_policy(policy_label);
        }
        Ok(result)
    }

    async fn deliver_signal(
        &self,
        args: DeliverSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        deliver_signal_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "deliver_signal: FCALL ff_deliver_signal")
            })
    }

    async fn deliver_approval_signal(
        &self,
        args: DeliverApprovalSignalArgs,
    ) -> Result<DeliverSignalResult, EngineError> {
        deliver_approval_signal_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "deliver_approval_signal: HGET ff:wp:<tag>:<wp_id> + FCALL ff_deliver_signal",
                )
            })
    }

    async fn claim_resumed_execution(
        &self,
        args: ClaimResumedExecutionArgs,
    ) -> Result<ClaimResumedExecutionResult, EngineError> {
        claim_resumed_execution_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "claim_resumed_execution: FCALL ff_claim_resumed_execution",
                )
            })
    }

    async fn claim_execution(
        &self,
        args: ClaimExecutionArgs,
    ) -> Result<ClaimExecutionResult, EngineError> {
        claim_execution_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "claim_execution: FCALL ff_claim_execution",
                )
            })
    }

    async fn scan_eligible_executions(
        &self,
        args: ScanEligibleArgs,
    ) -> Result<Vec<ExecutionId>, EngineError> {
        scan_eligible_executions_impl(&self.client, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "scan_eligible_executions: ZRANGEBYSCORE eligible_zset",
                )
            })
    }

    async fn issue_claim_grant(
        &self,
        args: IssueClaimGrantArgs,
    ) -> Result<IssueClaimGrantOutcome, EngineError> {
        issue_claim_grant_impl(&self.client, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "issue_claim_grant: FCALL ff_issue_claim_grant",
                )
            })
    }

    /// cairn #454 Phase 3d — backend-atomic grant-then-claim.
    async fn issue_grant_and_claim(
        &self,
        args: IssueGrantAndClaimArgs,
    ) -> Result<ClaimGrantOutcome, EngineError> {
        issue_grant_and_claim_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "issue_grant_and_claim: FCALL ff_issue_grant_and_claim",
                )
            })
    }

    async fn block_route(
        &self,
        args: BlockRouteArgs,
    ) -> Result<BlockRouteOutcome, EngineError> {
        block_route_impl(&self.client, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "block_route: FCALL ff_block_execution_for_admission",
                )
            })
    }

    async fn report_usage(
        &self,
        handle: &Handle,
        budget: &BudgetId,
        dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        report_usage_impl(&self.client, &self.partition_config, &f, budget, dimensions)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "report_usage: FCALL ff_report_usage_and_check",
                )
            })
    }

    /// cairn #454 Phase 3a — open-set spend recording.
    async fn record_spend(
        &self,
        args: RecordSpendArgs,
    ) -> Result<ReportUsageResult, EngineError> {
        record_spend_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "record_spend: FCALL ff_record_spend",
                )
            })
    }

    /// cairn #454 Phase 3b - per-execution budget attribution release.
    async fn release_budget(
        &self,
        args: ReleaseBudgetArgs,
    ) -> Result<(), EngineError> {
        release_budget_impl(&self.client, &self.partition_config, args)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    e,
                    "release_budget: FCALL ff_release_budget",
                )
            })
    }

    /// Cluster-wide waitpoint HMAC secret rotation (v0.7 Q4).
    ///
    /// Concretely fans out one
    /// `ff_rotate_waitpoint_hmac_secret` FCALL per execution
    /// partition, mirroring
    /// [`ff_sdk::admin::rotate_waitpoint_hmac_secret_all_partitions`]'s
    /// partial-success contract — a failure on one partition's
    /// FCALL is recorded as an inner `Err` on that entry and the
    /// fan-out continues. Sequential (one partition at a time) to
    /// keep the implementation futures-free; callers that need
    /// parallelism can wrap at a higher layer.
    async fn rotate_waitpoint_hmac_secret_all(
        &self,
        args: RotateWaitpointHmacSecretAllArgs,
    ) -> Result<RotateWaitpointHmacSecretAllResult, EngineError> {
        // RFC-017 Stage B: fan-out moves inside the Valkey impl (row
        // 11 of §4). Bounded-parallel dispatch matches the pre-Stage-B
        // `ff-server::Server::rotate_waitpoint_secret` body — sequential
        // rotation at 30ms cross-AZ RTT × 256 partitions was ~7.7s,
        // uncomfortably close to the 120s HTTP endpoint ceiling.
        use futures::stream::{FuturesUnordered, StreamExt};
        let per_partition_args = RotateWaitpointHmacSecretArgs {
            new_kid: args.new_kid.clone(),
            new_secret_hex: args.new_secret_hex.clone(),
            grace_ms: args.grace_ms,
        };
        let num = self.partition_config.num_flow_partitions;
        let concurrency = self.admin_rotate_fanout_concurrency as usize;
        let mut pending: FuturesUnordered<_> = FuturesUnordered::new();
        let mut next_index: u16 = 0;
        // Index-keyed so we can stitch the unordered completions back
        // into partition-index order before returning — consumers rely
        // on `entries[i].partition_id == i` (cairn dashboards + tests).
        let mut by_index: Vec<Option<Result<ff_core::contracts::RotateWaitpointHmacSecretOutcome, EngineError>>> =
            (0..num).map(|_| None).collect();
        loop {
            while pending.len() < concurrency && next_index < num {
                let partition = Partition {
                    family: PartitionFamily::Execution,
                    index: next_index,
                };
                let idx = IndexKeys::new(&partition);
                let args_clone = per_partition_args.clone();
                let client = &self.client;
                let i = next_index;
                pending.push(async move {
                    let res = ff_script::functions::suspension::ff_rotate_waitpoint_hmac_secret(
                        client,
                        &idx,
                        &args_clone,
                    )
                    .await
                    .map_err(|e| {
                        ff_core::engine_error::backend_context(
                            transport_script(e),
                            "rotate_waitpoint_hmac_secret_all: FCALL ff_rotate_waitpoint_hmac_secret",
                        )
                    });
                    (i, res)
                });
                next_index += 1;
            }
            match pending.next().await {
                Some((i, res)) => {
                    by_index[i as usize] = Some(res);
                }
                None => break,
            }
        }
        let mut entries = Vec::with_capacity(num as usize);
        for (i, slot) in by_index.into_iter().enumerate() {
            let res = slot.expect("every partition rotated exactly once");
            entries.push(RotateWaitpointHmacSecretAllEntry::new(i as u16, res));
        }
        Ok(RotateWaitpointHmacSecretAllResult::new(entries))
    }

    /// Seed the initial waitpoint HMAC secret across every execution
    /// partition (issue #280). Idempotent — intended to be called on
    /// every boot by cairn-fabric + similar consumers, letting the
    /// backend decide whether any partition needs an install.
    ///
    /// Semantics per partition, using the existing on-disk layout
    /// (`waitpoint_hmac_secrets:{p:N}` hash with `current_kid` +
    /// `secret:<kid>` fields):
    ///
    /// * `current_kid` absent → HSET both fields, partition reports
    ///   "installed".
    /// * `current_kid == args.kid` and stored secret matches →
    ///   "already-seeded, same secret".
    /// * `current_kid == args.kid` and stored secret differs →
    ///   "already-seeded, different secret".
    /// * `current_kid != args.kid` → `Validation(InvalidInput)`.
    ///
    /// The per-partition HSET is NOT atomic across partitions; a
    /// crash mid-fanout leaves some partitions installed and others
    /// empty. The next boot-time seed call repairs by installing the
    /// still-empty partitions. This mirrors the pre-#280
    /// `initialize_waitpoint_hmac_secret` boot behaviour which cairn
    /// was calling via raw HSET.
    ///
    /// Return shape: `Seeded { kid }` when at least one partition
    /// installed; otherwise `AlreadySeeded { kid, same_secret }`
    /// where `same_secret` is true iff every partition had both the
    /// supplied kid as `current_kid` AND matching secret bytes.
    async fn seed_waitpoint_hmac_secret(
        &self,
        args: SeedWaitpointHmacSecretArgs,
    ) -> Result<SeedOutcome, EngineError> {
        use futures::stream::{FuturesUnordered, StreamExt};

        if args.secret_hex.len() != 64 || !args.secret_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "secret_hex must be 64 hex characters (256-bit secret)".into(),
            });
        }
        if args.kid.is_empty() {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "kid must be non-empty".into(),
            });
        }

        enum PerPart {
            Installed,
            Match,
            SameKidDifferentSecret,
            DifferentKid(String),
        }

        let num = self.partition_config.num_flow_partitions;
        let concurrency = self.admin_rotate_fanout_concurrency as usize;
        let mut pending: FuturesUnordered<_> = FuturesUnordered::new();
        let mut next_index: u16 = 0;
        let mut by_index: Vec<Option<Result<PerPart, EngineError>>> =
            (0..num).map(|_| None).collect();

        loop {
            while pending.len() < concurrency && next_index < num {
                let partition = Partition {
                    family: PartitionFamily::Execution,
                    index: next_index,
                };
                let key = IndexKeys::new(&partition).waitpoint_hmac_secrets();
                let client = self.client.clone();
                let kid = args.kid.clone();
                let secret_hex = args.secret_hex.clone();
                let i = next_index;
                pending.push(async move {
                    let res: Result<PerPart, EngineError> = async {
                        let stored_kid: Option<String> = client
                            .cmd("HGET")
                            .arg(&key)
                            .arg("current_kid")
                            .execute()
                            .await
                            .map_err(|e| {
                                ff_core::engine_error::backend_context(
                                    transport_fk(e),
                                    format!("HGET {key} current_kid (seed probe)"),
                                )
                            })?;
                        if let Some(stored_kid) = stored_kid {
                            if stored_kid != kid {
                                return Ok(PerPart::DifferentKid(stored_kid));
                            }
                            let field = format!("secret:{stored_kid}");
                            let stored_secret: Option<String> = client
                                .hget(&key, &field)
                                .await
                                .map_err(|e| {
                                    ff_core::engine_error::backend_context(
                                        transport_fk(e),
                                        format!("HGET {key} secret:<kid> (seed probe)"),
                                    )
                                })?;
                            match stored_secret.as_deref() {
                                Some(s) if s == secret_hex => Ok(PerPart::Match),
                                Some(_) => Ok(PerPart::SameKidDifferentSecret),
                                None => {
                                    client
                                        .hset(&key, &field, &secret_hex)
                                        .await
                                        .map_err(|e| {
                                            ff_core::engine_error::backend_context(
                                                transport_fk(e),
                                                format!(
                                                    "HSET {key} secret:<kid> (seed repair torn write)"
                                                ),
                                            )
                                        })?;
                                    Ok(PerPart::Installed)
                                }
                            }
                        } else {
                            let secret_field = format!("secret:{kid}");
                            let _: i64 = client
                                .cmd("HSET")
                                .arg(&key)
                                .arg("current_kid")
                                .arg(&kid)
                                .arg(&secret_field)
                                .arg(&secret_hex)
                                .execute()
                                .await
                                .map_err(|e| {
                                    ff_core::engine_error::backend_context(
                                        transport_fk(e),
                                        format!("HSET {key} (seed install)"),
                                    )
                                })?;
                            Ok(PerPart::Installed)
                        }
                    }
                    .await;
                    (i, res)
                });
                next_index += 1;
            }
            match pending.next().await {
                Some((i, res)) => by_index[i as usize] = Some(res),
                None => break,
            }
        }

        let mut installed = 0usize;
        let mut matched = 0usize;
        let mut different_secret = 0usize;
        let mut different_kid: Option<String> = None;
        for slot in by_index.into_iter() {
            let res = slot.expect("every partition probed exactly once");
            match res? {
                PerPart::Installed => installed += 1,
                PerPart::Match => matched += 1,
                PerPart::SameKidDifferentSecret => different_secret += 1,
                PerPart::DifferentKid(k) => {
                    different_kid.get_or_insert(k);
                }
            }
        }

        if let Some(stored) = different_kid {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!(
                    "seed_waitpoint_hmac_secret: stored current_kid {stored:?} differs \
                     from supplied kid {:?}; use rotate_waitpoint_hmac_secret_all to change kid",
                    args.kid
                ),
            });
        }
        if installed > 0 {
            Ok(SeedOutcome::Seeded { kid: args.kid })
        } else {
            let same_secret = different_secret == 0 && matched > 0;
            Ok(SeedOutcome::AlreadySeeded {
                kid: args.kid,
                same_secret,
            })
        }
    }

    #[cfg(feature = "streaming")]
    async fn read_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from: ff_core::contracts::StreamCursor,
        to: ff_core::contracts::StreamCursor,
        count_limit: u64,
    ) -> Result<ff_core::contracts::StreamFrames, EngineError> {
        let _permit = acquire_stream_permit(self)?;
        read_stream_impl(
            &self.client,
            &self.partition_config,
            execution_id,
            attempt_index,
            from,
            to,
            count_limit,
        )
        .await
        .map_err(|e| ff_core::engine_error::backend_context(e, "read_stream: XRANGE"))
    }

    #[cfg(feature = "streaming")]
    async fn tail_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        after: ff_core::contracts::StreamCursor,
        block_ms: u64,
        count_limit: u64,
        visibility: TailVisibility,
    ) -> Result<ff_core::contracts::StreamFrames, EngineError> {
        let _permit = acquire_stream_permit(self)?;
        tail_stream_impl(
            &self.client,
            &self.partition_config,
            execution_id,
            attempt_index,
            after,
            block_ms,
            count_limit,
            visibility,
        )
        .await
        .map_err(|e| ff_core::engine_error::backend_context(e, "tail_stream: XREAD BLOCK"))
    }

    fn backend_label(&self) -> &'static str {
        "valkey"
    }

    /// RFC-018 Stage A: populate the `Capabilities` snapshot from the
    /// static [`valkey_supports_base`] shape, then adjust for
    /// runtime-gated fields. Today the only runtime-gated field is
    /// [`ff_core::capability::Supports::claim_for_worker`], which
    /// requires the scheduler to be wired via
    /// [`ValkeyBackend::with_scheduler`] or
    /// [`ValkeyBackend::with_embedded_scheduler`]; a plain
    /// [`ValkeyBackend::connect`] reports `false` (dispatching through
    /// `claim_for_worker` without a wired scheduler surfaces
    /// [`EngineError::Unavailable`]).
    fn capabilities(&self) -> ff_core::capability::Capabilities {
        let mut supports = valkey_supports_base();
        supports.claim_for_worker = self.scheduler.is_some();
        ff_core::capability::Capabilities::new(
            ff_core::capability::BackendIdentity::new(
                "valkey",
                ff_core::capability::Version::new(0, 11, 0),
                "E-shipped",
            ),
            supports,
        )
    }

    /// RFC-017 Stage B: backend-scoped drain hook (§5.4). Closes
    /// `stream_semaphore` so new `read_stream` / `tail_stream` calls
    /// fail fast with [`EngineError::Unavailable`]; awaits in-flight
    /// permits up to `grace`. Exceeding `grace` surfaces as
    /// [`EngineError::Timeout`] so the server can increment
    /// `ff_shutdown_timeout_total` and proceed best-effort.
    async fn shutdown_prepare(&self, grace: Duration) -> Result<(), EngineError> {
        // Close FIRST so `try_acquire_owned` returns `Closed` for any
        // new arrivals rather than a stale `NoPermits` decision.
        self.stream_semaphore.close();
        let start = std::time::Instant::now();
        let max_permits = self.stream_semaphore_max as usize;
        let poll_interval = Duration::from_millis(25);
        loop {
            // In-flight count = max - available. On `close()` Tokio's
            // `Semaphore::available_permits` still returns the live
            // count excluding held permits; the sum `max - available`
            // therefore equals the number of in-flight holders.
            let available = self.stream_semaphore.available_permits();
            if available >= max_permits {
                return Ok(());
            }
            if start.elapsed() >= grace {
                return Err(EngineError::Timeout {
                    op: "shutdown_prepare",
                    elapsed: start.elapsed(),
                });
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    #[cfg(feature = "streaming")]
    async fn read_summary(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
    ) -> Result<Option<SummaryDocument>, EngineError> {
        read_summary_impl(
            &self.client,
            &self.partition_config,
            execution_id,
            attempt_index,
        )
        .await
        .map_err(|e| ff_core::engine_error::backend_context(e, "read_summary: HGETALL summary"))
    }

    // ─── RFC-017 Stage A overrides (ingress + cheap wrappers) ─────────
    //
    // Each method below wraps an existing `ff_script::functions::*`
    // helper — the FCALL remains the source of truth. Bodies that
    // need HGET/HMGET pre-reads (cancel_execution, change_priority,
    // replay_execution, revoke_lease, get_budget_status) inherit the
    // default `Unavailable` impl; Stage C migration lands those
    // bodies alongside the matching handler cutover. See
    // `docs/POSTGRES_PARITY_MATRIX.md` for per-method status.

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn create_execution(
        &self,
        args: ff_core::contracts::CreateExecutionArgs,
    ) -> Result<ff_core::contracts::CreateExecutionResult, EngineError> {
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);
        // `worker_instance_id` is an unused field on the struct for
        // the create path; `ff_create_execution` ignores it. Use a
        // fresh placeholder rather than threading an Option through
        // the shared key-context.
        let placeholder_wiid = WorkerInstanceId::new("");
        let lane = args.lane_id.clone();
        let keys = ExecOpKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: &lane,
            worker_instance_id: &placeholder_wiid,
        };
        ff_create_execution(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "create_execution: FCALL ff_create_execution",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn create_flow(
        &self,
        args: ff_core::contracts::CreateFlowArgs,
    ) -> Result<ff_core::contracts::CreateFlowResult, EngineError> {
        let partition = flow_partition(&args.flow_id, &self.partition_config);
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);
        let fidx = FlowIndexKeys::new(&partition);
        let keys = FlowStructOpKeys { fctx: &fctx, fidx: &fidx };
        ff_create_flow(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "create_flow: FCALL ff_create_flow",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn add_execution_to_flow(
        &self,
        args: ff_core::contracts::AddExecutionToFlowArgs,
    ) -> Result<ff_core::contracts::AddExecutionToFlowResult, EngineError> {
        // RFC-011 §7.3: exec_core co-locates with its parent flow on
        // `{fp:N}`. Validate this invariant up-front so a mismatched
        // `execution_id` (e.g. a `solo`-minted id instead of
        // `ExecutionId::for_flow`) surfaces as a typed
        // `EngineError::Validation` rather than a raw Valkey
        // CROSSSLOT on clustered deployments. Matches the pre-Stage-D1
        // `Server::add_execution_to_flow` pre-flight check.
        let partition = flow_partition(&args.flow_id, &self.partition_config);
        let exec_partition =
            execution_partition(&args.execution_id, &self.partition_config);
        if exec_partition.index != partition.index {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!(
                    "add_execution_to_flow: execution_id's partition {} != flow_id's partition {}. \
                     Post-RFC-011 §7.3 co-location requires mint via \
                     `ExecutionId::for_flow(&flow_id, config)` so the exec's hash-tag \
                     matches the flow's `{{fp:N}}`.",
                    exec_partition.index, partition.index
                ),
            });
        }
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);
        let fidx = FlowIndexKeys::new(&partition);
        let ectx = ExecKeyContext::new(&exec_partition, &args.execution_id);
        let keys = ff_script::functions::flow::AddExecutionToFlowKeys {
            fctx: &fctx,
            fidx: &fidx,
            exec_ctx: &ectx,
        };
        ff_add_execution_to_flow(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "add_execution_to_flow: FCALL ff_add_execution_to_flow",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn stage_dependency_edge(
        &self,
        args: ff_core::contracts::StageDependencyEdgeArgs,
    ) -> Result<ff_core::contracts::StageDependencyEdgeResult, EngineError> {
        let partition = flow_partition(&args.flow_id, &self.partition_config);
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);
        let fidx = FlowIndexKeys::new(&partition);
        let keys = FlowStructOpKeys { fctx: &fctx, fidx: &fidx };
        ff_stage_dependency_edge(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "stage_dependency_edge: FCALL ff_stage_dependency_edge",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn apply_dependency_to_child(
        &self,
        args: ff_core::contracts::ApplyDependencyToChildArgs,
    ) -> Result<ff_core::contracts::ApplyDependencyToChildResult, EngineError> {
        let partition = execution_partition(&args.downstream_execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.downstream_execution_id);
        let idx = IndexKeys::new(&partition);
        let flow_part = flow_partition(&args.flow_id, &self.partition_config);
        let flow_ctx = FlowKeyContext::new(&flow_part, &args.flow_id);
        // Pre-read lane_id — same HGET that `ff-server::Server::apply_dependency_to_child`
        // performs today. Moves verbatim into the backend per RFC-017 §4
        // row 15.
        let lane_str: Option<String> = self
            .client
            .hget(&ctx.core(), "lane_id")
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "apply_dependency_to_child: HGET lane_id",
            ))?;
        let lane = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));
        let keys = DepOpKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: &lane,
            flow_ctx: &flow_ctx,
            downstream_eid: &args.downstream_execution_id,
        };
        ff_apply_dependency_to_child(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "apply_dependency_to_child: FCALL ff_apply_dependency_to_child",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    //
    // PR-7b Step 0: wraps `ff_resolve_dependency` (RFC-016 Stage C
    // signature). Both the dep_reconciler scanner and the post-
    // completion dispatch loop route through this trait method.
    async fn resolve_dependency(
        &self,
        args: ff_core::contracts::ResolveDependencyArgs,
    ) -> Result<ff_core::contracts::ResolveDependencyOutcome, EngineError> {
        let ctx = ExecKeyContext::new(&args.partition, &args.downstream_execution_id);
        let idx = IndexKeys::new(&args.partition);
        let upstream_ctx = ExecKeyContext::new(&args.partition, &args.upstream_execution_id);
        // Flow partition co-locates with the exec partition under
        // `{fp:N}` via RFC-011 flow-affinity; use the same partition
        // so the FCALL remains single-slot.
        let flow_ctx = FlowKeyContext::new(&args.partition, &args.flow_id);
        let flow_idx = ff_core::keys::FlowIndexKeys::new(&args.partition);
        let keys = ResolveDependencyKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: &args.lane_id,
            upstream_ctx: &upstream_ctx,
            flow_ctx: &flow_ctx,
            flow_idx: &flow_idx,
            downstream_eid: &args.downstream_execution_id,
            current_attempt_index: args.current_attempt_index,
        };
        let result = ff_resolve_dependency(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "resolve_dependency: FCALL ff_resolve_dependency",
            ))?;
        Ok(result.into())
    }

    // PR-7b Cluster 4. Trait-routed cascade for the completion listener.
    // Synchronous semantics: by the time this returns, the full
    // recursive cascade (up to MAX_CASCADE_DEPTH) has executed — see
    // trait rustdoc "Timing semantics" for the PG divergence.
    async fn cascade_completion(
        &self,
        payload: &ff_core::backend::CompletionPayload,
    ) -> Result<ff_core::contracts::CascadeOutcome, EngineError> {
        cascade::run_cascade(&self.client, &self.partition_config, payload).await
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn create_budget(
        &self,
        args: ff_core::contracts::CreateBudgetArgs,
    ) -> Result<ff_core::contracts::CreateBudgetResult, EngineError> {
        let partition = ff_core::partition::budget_partition(&args.budget_id, &self.partition_config);
        let bctx = ff_core::keys::BudgetKeyContext::new(&partition, &args.budget_id);
        let def = bctx.definition();
        let limits = bctx.limits();
        let usage = bctx.usage();
        let resets = ff_core::keys::budget_resets_key(bctx.hash_tag());
        let policies_idx = ff_core::keys::budget_policies_index(bctx.hash_tag());
        let k = BudgetOpKeys {
            usage_key: &usage,
            limits_key: &limits,
            def_key: &def,
            hash_tag: bctx.hash_tag(),
        };
        ff_create_budget(&self.client, &k, &resets, &policies_idx, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "create_budget: FCALL ff_create_budget",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn reset_budget(
        &self,
        args: ff_core::contracts::ResetBudgetArgs,
    ) -> Result<ff_core::contracts::ResetBudgetResult, EngineError> {
        let partition = ff_core::partition::budget_partition(&args.budget_id, &self.partition_config);
        let bctx = ff_core::keys::BudgetKeyContext::new(&partition, &args.budget_id);
        let def = bctx.definition();
        let limits = bctx.limits();
        let usage = bctx.usage();
        let resets = ff_core::keys::budget_resets_key(bctx.hash_tag());
        let k = BudgetOpKeys {
            usage_key: &usage,
            limits_key: &limits,
            def_key: &def,
            hash_tag: bctx.hash_tag(),
        };
        ff_reset_budget(&self.client, &k, &resets, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "reset_budget: FCALL ff_reset_budget",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn create_quota_policy(
        &self,
        args: ff_core::contracts::CreateQuotaPolicyArgs,
    ) -> Result<ff_core::contracts::CreateQuotaPolicyResult, EngineError> {
        let partition = ff_core::partition::quota_partition(&args.quota_policy_id, &self.partition_config);
        let qctx = ff_core::keys::QuotaKeyContext::new(&partition, &args.quota_policy_id);
        // `execution_id` is unused by `ff_create_quota_policy` (it reads
        // only `ctx.definition/window/concurrency/admitted_set`). Construct
        // a placeholder to satisfy the shared `QuotaOpKeys` struct.
        let placeholder_eid = ExecutionId::solo(&LaneId::new("default"), &self.partition_config);
        let keys = QuotaOpKeys {
            ctx: &qctx,
            dimension: "requests_per_window",
            execution_id: &placeholder_eid,
        };
        ff_create_quota_policy(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "create_quota_policy: FCALL ff_create_quota_policy",
            ))
    }

    // core-gated at trait level; ff-backend-valkey propagates ff-core/core
    async fn report_usage_admin(
        &self,
        budget: &BudgetId,
        args: ff_core::contracts::ReportUsageAdminArgs,
    ) -> Result<ReportUsageResult, EngineError> {
        // Admin path shares the same FCALL as worker-path `report_usage`;
        // the distinction is that no worker `Handle` is consumed on the
        // way in (RFC-017 §5 round-1 F4). Translate the admin args to
        // the worker-facing `ReportUsageArgs` shape that the ff-script
        // helper expects.
        let partition = ff_core::partition::budget_partition(budget, &self.partition_config);
        let bctx = ff_core::keys::BudgetKeyContext::new(&partition, budget);
        let def = bctx.definition();
        let limits = bctx.limits();
        let usage = bctx.usage();
        let k = BudgetOpKeys {
            usage_key: &usage,
            limits_key: &limits,
            def_key: &def,
            hash_tag: bctx.hash_tag(),
        };
        let worker_args = ff_core::contracts::ReportUsageArgs {
            dimensions: args.dimensions,
            deltas: args.deltas,
            now: args.now,
            dedup_key: args.dedup_key,
        };
        ff_report_usage_and_check(&self.client, &k, &worker_args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "report_usage_admin: FCALL ff_report_usage_and_check",
            ))
    }

    async fn ping(&self) -> Result<(), EngineError> {
        let _: ferriskey::Value = self
            .client
            .cmd("PING")
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "ping: PING",
            ))?;
        Ok(())
    }

    /// Issue #281: trait-surface boot preparation. Delegates to
    /// [`ff_script::loader::ensure_library`] — same retry semantics
    /// as [`ValkeyBackend::initialize_deployment`]'s step 4 (3×
    /// attempts with 1 s backoff on transient transport faults;
    /// permanent compile errors fail loud without retry). Reported as
    /// [`PrepareOutcome::Applied`] with `description =
    /// "FUNCTION LOAD (flowfabric lib v<N>)"` so operators see the
    /// booted library version in logs.
    ///
    /// [`PrepareOutcome::Applied`]: ff_core::backend::PrepareOutcome::Applied
    async fn prepare(
        &self,
    ) -> Result<ff_core::backend::PrepareOutcome, EngineError> {
        ff_script::loader::ensure_library(&self.client)
            .await
            .map_err(boot::load_error_to_engine)?;
        Ok(ff_core::backend::PrepareOutcome::applied(format!(
            "FUNCTION LOAD (flowfabric lib v{})",
            ff_script::LIBRARY_VERSION
        )))
    }

    async fn get_execution_result(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let partition = execution_partition(id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, id);
        // Binary-safe read. Mirrors `ff-server::Server::get_execution_result`
        // verbatim so the Stage C handler migration is a one-line delegate.
        let payload: Option<Vec<u8>> = self
            .client
            .cmd("GET")
            .arg(ctx.result())
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "get_execution_result: GET result",
            ))?;
        Ok(payload)
    }

    // ── RFC-017 Stage D1 — list_pending_waitpoints (§8) ─────────

    /// List pending/active waitpoints for an execution. Stage D1
    /// implementation (RFC-017 §8): SSCAN the waitpoint index,
    /// pipelined 2× HMGET for each waitpoint + HGET
    /// `total_matchers`, optional pass-2 HMGET for matcher names,
    /// parse the stored `<kid>:<hex>` token into
    /// `(token_kid, token_fingerprint)` and DROP the raw token
    /// before returning across the trait boundary.
    ///
    /// Pagination: the caller's `args.after` is used to skip
    /// waitpoint ids lexicographically `<= after` after the SSCAN
    /// dedup. `args.limit` defaults to 100, is capped at 1000. A
    /// `next_cursor` is returned if more entries were available
    /// beyond the requested page.
    async fn list_pending_waitpoints(
        &self,
        args: ff_core::contracts::ListPendingWaitpointsArgs,
    ) -> Result<ff_core::contracts::ListPendingWaitpointsResult, EngineError> {
        use ff_core::contracts::{ListPendingWaitpointsResult, PendingWaitpointInfo};

        const DEFAULT_LIMIT: u32 = 100;
        const MAX_LIMIT: u32 = 1000;
        const WAITPOINTS_SSCAN_COUNT: usize = 100;
        const WP_FIELDS: [&str; 6] = [
            "state",
            "waitpoint_key",
            "waitpoint_token",
            "created_at",
            "activated_at",
            "expires_at",
        ];

        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
        let execution_id = args.execution_id.clone();
        let after_str = args.after.as_ref().map(|wp| wp.to_string());

        let partition = execution_partition(&execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &execution_id);

        // Existence check — surface NotFound early so callers don't
        // get an empty page for a non-existent execution.
        let core_exists: bool = self
            .client
            .cmd("EXISTS")
            .arg(ctx.core())
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "list_pending_waitpoints: EXISTS exec_core",
            ))?;
        if !core_exists {
            return Err(EngineError::NotFound {
                entity: "execution",
            });
        }

        let waitpoints_key = ctx.waitpoints();
        let mut wp_ids_raw: Vec<String> = Vec::new();
        let mut cursor: String = "0".to_owned();
        loop {
            let reply: (String, Vec<String>) = self
                .client
                .cmd("SSCAN")
                .arg(&waitpoints_key)
                .arg(&cursor)
                .arg("COUNT")
                .arg(WAITPOINTS_SSCAN_COUNT.to_string().as_str())
                .execute()
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "list_pending_waitpoints: SSCAN waitpoints",
                ))?;
            cursor = reply.0;
            wp_ids_raw.extend(reply.1);
            if cursor == "0" {
                break;
            }
        }

        // SSCAN may dup; dedup + sort for deterministic pagination.
        wp_ids_raw.sort_unstable();
        wp_ids_raw.dedup();

        // Apply `after` cursor — skip ids `<= after`.
        if let Some(after) = &after_str {
            let start = wp_ids_raw.partition_point(|s| s.as_str() <= after.as_str());
            wp_ids_raw.drain(..start);
        }

        if wp_ids_raw.is_empty() {
            return Ok(ListPendingWaitpointsResult::new(Vec::new()));
        }

        // Page: request `limit + 1` so we can detect "more to come"
        // without a second round-trip. We'll cap to wp_ids_raw.len().
        let page_end = (limit + 1).min(wp_ids_raw.len());
        let has_more_raw = wp_ids_raw.len() > limit;
        let page_ids_raw: Vec<String> = wp_ids_raw[..page_end].to_vec();

        // Parse, skip unparseable ids.
        let mut wp_ids: Vec<WaitpointId> = Vec::with_capacity(page_ids_raw.len());
        for raw in &page_ids_raw {
            match WaitpointId::parse(raw) {
                Ok(id) => wp_ids.push(id),
                Err(e) => tracing::warn!(
                    raw_id = %raw,
                    error = %e,
                    execution_id = %execution_id,
                    "list_pending_waitpoints: skipping unparseable waitpoint_id"
                ),
            }
        }
        if wp_ids.is_empty() {
            return Ok(ListPendingWaitpointsResult::new(Vec::new()));
        }

        // Pipeline pass-1: HMGET + HGET total_matchers per waitpoint.
        let mut pass1 = self.client.pipeline();
        let mut wp_slots = Vec::with_capacity(wp_ids.len());
        let mut cond_slots = Vec::with_capacity(wp_ids.len());
        for wp_id in &wp_ids {
            let mut cmd = pass1.cmd::<Vec<Option<String>>>("HMGET");
            cmd = cmd.arg(ctx.waitpoint(wp_id));
            for f in WP_FIELDS {
                cmd = cmd.arg(f);
            }
            wp_slots.push(cmd.finish());

            cond_slots.push(
                pass1
                    .cmd::<Option<String>>("HGET")
                    .arg(ctx.waitpoint_condition(wp_id))
                    .arg("total_matchers")
                    .finish(),
            );
        }
        pass1.execute().await.map_err(|e| {
            ff_core::engine_error::backend_context(
                transport_fk(e),
                "list_pending_waitpoints: pipeline HMGET waitpoints + HGET total_matchers",
            )
        })?;

        struct Kept {
            wp_id: WaitpointId,
            wp_fields: Vec<Option<String>>,
            total_matchers: usize,
        }
        let mut kept: Vec<Kept> = Vec::with_capacity(wp_ids.len());
        for ((wp_id, wp_slot), cond_slot) in
            wp_ids.iter().zip(wp_slots).zip(cond_slots)
        {
            let wp_fields: Vec<Option<String>> = wp_slot.value().map_err(|e| {
                ff_core::engine_error::backend_context(
                    transport_fk(e),
                    format!("list_pending_waitpoints: slot HMGET waitpoint {wp_id}"),
                )
            })?;
            if wp_fields.iter().all(Option::is_none) {
                let _ = cond_slot.value();
                continue;
            }
            let state_ref = wp_fields.first().and_then(|v| v.as_deref()).unwrap_or("");
            if state_ref != "pending" && state_ref != "active" {
                let _ = cond_slot.value();
                continue;
            }
            let token_ref = wp_fields.get(2).and_then(|v| v.as_deref()).unwrap_or("");
            if token_ref.is_empty() {
                let _ = cond_slot.value();
                tracing::warn!(
                    waitpoint_id = %wp_id,
                    execution_id = %execution_id,
                    "list_pending_waitpoints: waitpoint hash missing waitpoint_token — skipping"
                );
                continue;
            }
            let total_matchers = cond_slot
                .value()
                .map_err(|e| {
                    ff_core::engine_error::backend_context(
                        transport_fk(e),
                        format!("list_pending_waitpoints: slot HGET total_matchers {wp_id}"),
                    )
                })?
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            kept.push(Kept {
                wp_id: wp_id.clone(),
                wp_fields,
                total_matchers,
            });
        }

        if kept.is_empty() {
            return Ok(ListPendingWaitpointsResult::new(Vec::new()));
        }

        // Pass-2: matcher names for waitpoints with total_matchers > 0.
        let mut pass2 = self.client.pipeline();
        let mut matcher_slots: Vec<Option<_>> = Vec::with_capacity(kept.len());
        let mut pass2_needed = false;
        for k in &kept {
            if k.total_matchers == 0 {
                matcher_slots.push(None);
                continue;
            }
            pass2_needed = true;
            let mut cmd = pass2.cmd::<Vec<Option<String>>>("HMGET");
            cmd = cmd.arg(ctx.waitpoint_condition(&k.wp_id));
            for i in 0..k.total_matchers {
                cmd = cmd.arg(format!("matcher:{i}:name"));
            }
            matcher_slots.push(Some(cmd.finish()));
        }
        if pass2_needed {
            pass2.execute().await.map_err(|e| {
                ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "list_pending_waitpoints: pipeline HMGET wp_condition matchers",
                )
            })?;
        }

        let parse_ts = |raw: &str| -> Option<TimestampMs> {
            if raw.is_empty() {
                None
            } else {
                raw.parse::<i64>().ok().map(TimestampMs)
            }
        };

        let mut out: Vec<PendingWaitpointInfo> = Vec::with_capacity(kept.len());
        let requested_len = kept.len().min(limit);
        for (k, slot) in kept.into_iter().zip(matcher_slots).take(limit) {
            let get = |i: usize| -> &str {
                k.wp_fields.get(i).and_then(|v| v.as_deref()).unwrap_or("")
            };
            let required_signal_names: Vec<String> = match slot {
                None => Vec::new(),
                Some(s) => {
                    let vals: Vec<Option<String>> = s.value().map_err(|e| {
                        ff_core::engine_error::backend_context(
                            transport_fk(e),
                            format!(
                                "list_pending_waitpoints: slot HMGET matchers {}",
                                k.wp_id
                            ),
                        )
                    })?;
                    vals.into_iter()
                        .flatten()
                        .filter(|name| !name.is_empty())
                        .collect()
                }
            };

            // Parse `<kid>:<40hex>` into (kid, first-16-hex fingerprint).
            let token_raw = get(2);
            let (token_kid, token_fingerprint) = parse_waitpoint_token_kid_fp(token_raw);

            let mut info = PendingWaitpointInfo::new(
                k.wp_id,
                get(1).to_owned(),
                get(0).to_owned(),
                parse_ts(get(3)).unwrap_or(TimestampMs(0)),
                execution_id.clone(),
                token_kid,
                token_fingerprint,
            );
            if !required_signal_names.is_empty() {
                info = info.with_required_signal_names(required_signal_names);
            }
            if let Some(ts) = parse_ts(get(4)) {
                info = info.with_activated_at(ts);
            }
            if let Some(ts) = parse_ts(get(5)) {
                info = info.with_expires_at(ts);
            }
            out.push(info);
        }

        let next_cursor = if has_more_raw {
            out.last().map(|e| e.waitpoint_id.clone())
        } else {
            None
        };
        let mut result = ListPendingWaitpointsResult::new(out);
        if let Some(cursor) = next_cursor {
            result = result.with_next_cursor(cursor);
        }
        let _ = requested_len; // reserved for future diagnostics
        Ok(result)
    }

    // ── RFC-017 Stage C — Operator control (4) ────────────────

    async fn cancel_execution(
        &self,
        args: ff_core::contracts::CancelExecutionArgs,
    ) -> Result<ff_core::contracts::CancelExecutionResult, EngineError> {
        // HMGET pre-read (§4 row 2 semantics preserved): lane_id,
        // current_attempt_index, current_waitpoint_id,
        // current_worker_instance_id. Same order as the legacy
        // `server.rs::build_cancel_execution_fcall` helper so the
        // variadic KEYS shape fed into `ff_cancel_execution` stays
        // byte-identical.
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        let dyn_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(ctx.core())
            .arg("lane_id")
            .arg("current_attempt_index")
            .arg("current_waitpoint_id")
            .arg("current_worker_instance_id")
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "cancel_execution: HMGET exec_core",
            ))?;
        let lane = LaneId::new(
            dyn_fields
                .first()
                .and_then(|v| v.as_ref())
                .cloned()
                .unwrap_or_else(|| "default".to_owned()),
        );
        let att_idx_val = dyn_fields
            .get(1)
            .and_then(|v| v.as_ref())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let att_idx = AttemptIndex::new(att_idx_val);
        let wp_id_str = dyn_fields
            .get(2)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();
        let wp_id = if wp_id_str.is_empty() {
            WaitpointId::new()
        } else {
            WaitpointId::parse(&wp_id_str).unwrap_or_else(|_| WaitpointId::new())
        };
        let wiid_str = dyn_fields
            .get(3)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();
        let wiid = WorkerInstanceId::new(&wiid_str);

        // The ff_script `ff_cancel_execution` helper uses a fixed
        // `ExecOpKeys` struct whose `k.ctx.attempt_hash(AttemptIndex::new(0))`
        // placeholders don't match the live attempt_index / waitpoint
        // required by the Lua body for in-flight executions. Build
        // KEYS/ARGV explicitly using the same layout as the legacy
        // `build_cancel_execution_fcall` helper on `Server`.
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.attempt_hash(att_idx),
            ctx.stream_meta(att_idx),
            ctx.lease_current(),
            ctx.lease_history(),
            idx.lease_expiry(),
            idx.worker_leases(&wiid),
            ctx.suspension_current(),
            ctx.waitpoint(&wp_id),
            ctx.waitpoint_condition(&wp_id),
            idx.suspension_timeout(),
            idx.lane_terminal(&lane),
            idx.attempt_timeout(),
            idx.execution_deadline(),
            idx.lane_eligible(&lane),
            idx.lane_delayed(&lane),
            idx.lane_blocked_dependencies(&lane),
            idx.lane_blocked_budget(&lane),
            idx.lane_blocked_quota(&lane),
            idx.lane_blocked_route(&lane),
            idx.lane_blocked_operator(&lane),
        ];
        let argv: Vec<String> = vec![
            args.execution_id.to_string(),
            args.reason.clone(),
            args.source.to_string(),
            args.lease_id.as_ref().map(|l| l.to_string()).unwrap_or_default(),
            args.lease_epoch.as_ref().map(|e| e.to_string()).unwrap_or_default(),
        ];
        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

        let raw: ferriskey::Value = self
            .client
            .fcall("ff_cancel_execution", &key_refs, &arg_refs)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "cancel_execution: FCALL ff_cancel_execution",
            ))?;

        let partial = CancelExecutionResultPartial::from_fcall_result(&raw)
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "cancel_execution: parse",
            ))?;
        Ok(partial.complete(args.execution_id.clone()))
    }

    async fn change_priority(
        &self,
        args: ff_core::contracts::ChangePriorityArgs,
    ) -> Result<ff_core::contracts::ChangePriorityResult, EngineError> {
        // HGET lane_id pre-read (§4 row 17). `ChangePriorityArgs`
        // carries `lane_id` as of RFC-017 Stage A backfill, so the
        // caller already decided which lane to re-score. Defence-in-
        // depth: if the caller passes a stub lane, we still accept it
        // — the Lua `ff_change_priority` validates eligibility against
        // the exec_core's authoritative lane itself. No extra
        // round-trip needed.
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        // ff_script's `ff_change_priority` uses SchedOpKeys, which
        // indexes `lane_eligible` on the supplied lane. When the
        // caller-supplied lane is empty (pre-claim / solo path),
        // HGET the authoritative value.
        let lane = if args.lane_id.as_str().is_empty() {
            let lane_str: Option<String> = self
                .client
                .hget(&ctx.core(), "lane_id")
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "change_priority: HGET lane_id",
                ))?;
            LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()))
        } else {
            args.lane_id.clone()
        };

        let keys = SchedOpKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: &lane,
        };
        let partial: ChangePriorityResultPartial = ff_change_priority(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "change_priority: FCALL ff_change_priority",
            ))?;
        Ok(partial.complete(args.execution_id.clone()))
    }

    async fn replay_execution(
        &self,
        args: ff_core::contracts::ReplayExecutionArgs,
    ) -> Result<ff_core::contracts::ReplayExecutionResult, EngineError> {
        // §4 row 3 Hard — variadic KEYS driven by inbound-edge count
        // of a skipped flow member. Preserve the legacy
        // `server.rs::replay_execution` body verbatim: HMGET pre-read
        // (lane_id + flow_id + terminal_outcome) + conditional
        // SMEMBERS over the flow-partition incoming-edge set when the
        // member was skipped.
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        let dyn_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(ctx.core())
            .arg("lane_id")
            .arg("flow_id")
            .arg("terminal_outcome")
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "replay_execution: HMGET replay pre-read",
            ))?;
        let lane = LaneId::new(
            dyn_fields
                .first()
                .and_then(|v| v.as_ref())
                .cloned()
                .unwrap_or_else(|| "default".to_owned()),
        );
        let flow_id_str = dyn_fields
            .get(1)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();
        let terminal_outcome = dyn_fields
            .get(2)
            .and_then(|v| v.as_ref())
            .cloned()
            .unwrap_or_default();

        let is_skipped_flow_member =
            terminal_outcome == "skipped" && !flow_id_str.is_empty();

        // Base path (non-flow or non-skipped): use the fixed-KEYS
        // ff_script helper.
        if !is_skipped_flow_member {
            let keys = DepOpKeys {
                ctx: &ctx,
                idx: &idx,
                lane_id: &lane,
                flow_ctx: &FlowKeyContext::new(
                    &flow_partition(&FlowId::new(), &self.partition_config),
                    &FlowId::new(),
                ),
                downstream_eid: &args.execution_id,
            };
            return ff_replay_execution(&self.client, &keys, &args)
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_script(e),
                    "replay_execution: FCALL ff_replay_execution",
                ));
        }

        // Skipped-flow-member variadic-KEYS path — legacy raw FCALL.
        let flow_id = FlowId::parse(&flow_id_str)
            .map_err(|e| EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::InvalidInput,
                detail: format!("replay_execution: bad flow_id: {e}"),
            })?;
        let flow_part = flow_partition(&flow_id, &self.partition_config);
        let flow_ctx = FlowKeyContext::new(&flow_part, &flow_id);
        let edge_ids: Vec<String> = self
            .client
            .cmd("SMEMBERS")
            .arg(flow_ctx.incoming(&args.execution_id))
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "replay_execution: SMEMBERS replay edges",
            ))?;

        let now = args.now;
        let mut fcall_keys: Vec<String> = vec![
            ctx.core(),
            idx.lane_terminal(&lane),
            idx.lane_eligible(&lane),
            ctx.lease_history(),
            idx.lane_blocked_dependencies(&lane),
            ctx.deps_meta(),
            ctx.deps_unresolved(),
        ];
        let mut fcall_args: Vec<String> = vec![args.execution_id.to_string(), now.to_string()];
        for eid_str in &edge_ids {
            let edge_id = EdgeId::parse(eid_str).unwrap_or_else(|_| EdgeId::new());
            fcall_keys.push(ctx.dep_edge(&edge_id));
            fcall_args.push(eid_str.clone());
        }
        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        let raw: ferriskey::Value = self
            .client
            .fcall("ff_replay_execution", &key_refs, &arg_refs)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "replay_execution: FCALL ff_replay_execution (variadic)",
            ))?;
        ff_core::contracts::ReplayExecutionResult::from_fcall_result(&raw)
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "replay_execution: parse",
            ))
    }

    async fn revoke_lease(
        &self,
        args: ff_core::contracts::RevokeLeaseArgs,
    ) -> Result<ff_core::contracts::RevokeLeaseResult, EngineError> {
        // HGET current_worker_instance_id pre-read (§4 row 19). If
        // the caller-supplied `worker_instance_id` is empty, read
        // authoritative. If the execution has no active lease, surface
        // `EngineError::NotFound`.
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);

        let effective_wiid = if args.worker_instance_id.as_str().is_empty() {
            let wiid_str: Option<String> = self
                .client
                .hget(&ctx.core(), "current_worker_instance_id")
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "revoke_lease: HGET current_worker_instance_id",
                ))?;
            match wiid_str {
                Some(s) if !s.is_empty() => WorkerInstanceId::new(&s),
                _ => {
                    // Legacy `Server::revoke_lease` surfaced this as
                    // HTTP 404. Preserve the semantics on the trait
                    // surface by returning the domain-level
                    // `AlreadySatisfied` variant — `RevokeLeaseResult`
                    // already carries this as a benign no-op shape, so
                    // the HTTP handler keeps a 200 Ok + the same JSON
                    // body it already returned when Lua reported
                    // "already revoked". Callers that want the
                    // hard-404 behaviour check `AlreadySatisfied`
                    // client-side (the reason string carries enough
                    // detail for operator triage).
                    return Ok(ff_core::contracts::RevokeLeaseResult::AlreadySatisfied {
                        reason: "no_active_lease".to_owned(),
                    });
                }
            }
        } else {
            args.worker_instance_id.clone()
        };

        // `ff_revoke_lease` helper needs the full WIID in the key,
        // which it reads off `args.worker_instance_id`. Rebuild args
        // with the resolved WIID.
        let args = ff_core::contracts::RevokeLeaseArgs {
            execution_id: args.execution_id,
            expected_lease_id: args.expected_lease_id,
            worker_instance_id: effective_wiid,
            reason: args.reason,
        };

        ff_revoke_lease(&self.client, &ctx, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "revoke_lease: FCALL ff_revoke_lease",
            ))
    }

    // ── cairn #389 — service-layer typed FCALL surface ────────────
    //
    // Each method mirrors the corresponding `ff_*` Lua primitive and
    // pre-reads `lane_id` / `current_worker_instance_id` /
    // `current_waitpoint_id` from `exec_core` when the args don't
    // already carry them. Same pattern as `cancel_execution` above.

    async fn complete_execution(
        &self,
        args: ff_core::contracts::CompleteExecutionArgs,
    ) -> Result<ff_core::contracts::CompleteExecutionResult, EngineError> {
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        // Pre-read lane_id + current_worker_instance_id so callers
        // only supply execution_id + fence + attempt_index.
        //
        // Empty/missing `current_worker_instance_id` is a typed error
        // rather than an empty-string fallback — `ExecOpKeys` feeds
        // the WIID into `idx.worker_leases(wiid)` which would collide
        // across multiple executions under the same "empty worker"
        // key on Valkey. Missing lane_id is also a typed error:
        // `exec_core` without a lane is an invariant violation
        // (`create_execution` writes lane_id atomically).
        let dyn_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(ctx.core())
            .arg("lane_id")
            .arg("current_worker_instance_id")
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "complete_execution: HMGET exec_core",
            ))?;
        let lane_str = dyn_fields.first().and_then(|v| v.as_ref()).filter(|s| !s.is_empty())
            .ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!(
                    "complete_execution: exec_core[{eid}].lane_id missing or empty",
                    eid = args.execution_id
                ),
            })?
            .clone();
        let wiid_str = dyn_fields.get(1).and_then(|v| v.as_ref()).filter(|s| !s.is_empty())
            .ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::InvalidLeaseForSuspend,
                detail: format!(
                    "complete_execution: exec_core[{eid}].current_worker_instance_id missing — no active lease",
                    eid = args.execution_id
                ),
            })?
            .clone();
        let lane = LaneId::new(lane_str);
        let wiid = WorkerInstanceId::new(wiid_str);

        let keys = ExecOpKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: &lane,
            worker_instance_id: &wiid,
        };
        let partial = ff_complete_execution(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "complete_execution: FCALL ff_complete_execution",
            ))?;
        Ok(partial.complete(args.execution_id))
    }

    async fn fail_execution(
        &self,
        args: ff_core::contracts::FailExecutionArgs,
    ) -> Result<ff_core::contracts::FailExecutionResult, EngineError> {
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        // Same pre-read contract as `complete_execution` above —
        // empty WIID / lane_id are typed errors, never silent
        // fallbacks.
        let dyn_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(ctx.core())
            .arg("lane_id")
            .arg("current_worker_instance_id")
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "fail_execution: HMGET exec_core",
            ))?;
        let lane_str = dyn_fields.first().and_then(|v| v.as_ref()).filter(|s| !s.is_empty())
            .ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!(
                    "fail_execution: exec_core[{eid}].lane_id missing or empty",
                    eid = args.execution_id
                ),
            })?
            .clone();
        let wiid_str = dyn_fields.get(1).and_then(|v| v.as_ref()).filter(|s| !s.is_empty())
            .ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::InvalidLeaseForSuspend,
                detail: format!(
                    "fail_execution: exec_core[{eid}].current_worker_instance_id missing — no active lease",
                    eid = args.execution_id
                ),
            })?
            .clone();
        let lane = LaneId::new(lane_str);
        let wiid = WorkerInstanceId::new(wiid_str);

        let keys = ExecOpKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: &lane,
            worker_instance_id: &wiid,
        };
        ff_fail_execution(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "fail_execution: FCALL ff_fail_execution",
            ))
    }

    async fn renew_lease(
        &self,
        args: ff_core::contracts::RenewLeaseArgs,
    ) -> Result<ff_core::contracts::RenewLeaseResult, EngineError> {
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        ff_renew_lease(&self.client, &ctx, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "renew_lease: FCALL ff_renew_lease",
            ))
    }

    async fn resume_execution(
        &self,
        args: ff_core::contracts::ResumeExecutionArgs,
    ) -> Result<ff_core::contracts::ResumeExecutionResult, EngineError> {
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let idx = IndexKeys::new(&partition);

        // Authoritative pre-read: lane_id + current_waitpoint_id live
        // on `exec_core`. Cairn's pre-migration code did the same
        // HGET before dispatching the raw FCALL — same semantics,
        // different routing (trait method instead of raw fcall()).
        //
        // Both fields are REQUIRED: `ff_resume_execution` builds
        // KEYS[3]/[4] from the waitpoint id and uses lane_id in the
        // index keys. An empty or unparseable waitpoint_id would make
        // the Lua operate on the wrong keys or on a made-up waitpoint
        // — surface `State::ExecutionNotSuspended` / `Corruption`
        // typed errors instead of fabricating a UUID.
        let dyn_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(ctx.core())
            .arg("lane_id")
            .arg("current_waitpoint_id")
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "resume_execution: HMGET exec_core",
            ))?;
        let lane_str = dyn_fields.first().and_then(|v| v.as_ref()).filter(|s| !s.is_empty())
            .ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!(
                    "resume_execution: exec_core[{eid}].lane_id missing or empty",
                    eid = args.execution_id
                ),
            })?
            .clone();
        let lane = LaneId::new(lane_str);
        let wp_id_str = dyn_fields
            .get(1)
            .and_then(|v| v.as_ref())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| EngineError::State(
                ff_core::engine_error::StateKind::ExecutionNotSuspended,
            ))?
            .clone();
        let wp_id = WaitpointId::parse(&wp_id_str).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "resume_execution: exec_core[{eid}].current_waitpoint_id unparseable: {e}",
                eid = args.execution_id
            ),
        })?;

        let keys = ResumeOpKeys {
            ctx: &ctx,
            idx: &idx,
            lane_id: &lane,
            waitpoint_id: &wp_id,
        };
        ff_resume_execution(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "resume_execution: FCALL ff_resume_execution",
            ))
    }

    async fn check_admission(
        &self,
        quota_policy_id: &ff_core::types::QuotaPolicyId,
        dimension: &str,
        args: ff_core::contracts::CheckAdmissionArgs,
    ) -> Result<ff_core::contracts::CheckAdmissionResult, EngineError> {
        let effective_dimension = if dimension.is_empty() {
            "default"
        } else {
            dimension
        };

        let partition = ff_core::partition::quota_partition(
            quota_policy_id,
            &self.partition_config,
        );
        let qctx = ff_core::keys::QuotaKeyContext::new(&partition, quota_policy_id);
        let keys = QuotaOpKeys {
            ctx: &qctx,
            dimension: effective_dimension,
            execution_id: &args.execution_id,
        };
        ff_check_admission_and_record(&self.client, &keys, &args)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "check_admission: FCALL ff_check_admission_and_record",
            ))
    }

    async fn evaluate_flow_eligibility(
        &self,
        args: ff_core::contracts::EvaluateFlowEligibilityArgs,
    ) -> Result<ff_core::contracts::EvaluateFlowEligibilityResult, EngineError> {
        // `ff_evaluate_flow_eligibility` takes KEYS=(exec_core,
        // deps_meta) + ARGV=(). The ff-script wrapper is expressed
        // over `DepOpKeys` because the macro standardises on that
        // shape for all dependency ops, but this op does not read
        // `flow_ctx` / `lane_id` / `idx` / `downstream_eid`. Rather
        // than synthesise unused values (brittle if the wrapper's
        // KEYS set ever grows), issue the FCALL directly — same
        // pattern `cancel_execution` and `revoke_lease` use when the
        // shared key-context doesn't match the Lua's key layout.
        //
        // KEYS definition: see `ff_evaluate_flow_eligibility` in
        // `crates/ff-script/src/functions/flow.rs` (KEYS(2) /
        // ARGV(0)) + `lua/flow.lua::ff_evaluate_flow_eligibility`.
        let partition = execution_partition(&args.execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, &args.execution_id);
        let keys: Vec<String> = vec![ctx.core(), ctx.deps_meta()];
        let argv: Vec<String> = vec![];
        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

        let raw: ferriskey::Value = self
            .client
            .fcall("ff_evaluate_flow_eligibility", &key_refs, &arg_refs)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "evaluate_flow_eligibility: FCALL ff_evaluate_flow_eligibility",
            ))?;
        ff_core::contracts::EvaluateFlowEligibilityResult::from_fcall_result(&raw)
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "evaluate_flow_eligibility: parse",
            ))
    }

    // ── RFC-017 Stage C — Budget status (read-only) ────────────

    async fn get_budget_status(
        &self,
        budget_id: &BudgetId,
    ) -> Result<ff_core::contracts::BudgetStatus, EngineError> {
        // §4 row 8 clause — 3× HGETALL direct reads, no FCALL.
        // Mirrors `ff-server::Server::get_budget_status` verbatim so
        // the Stage C handler migration is a thin delegate.
        let partition =
            ff_core::partition::budget_partition(budget_id, &self.partition_config);
        let bctx = ff_core::keys::BudgetKeyContext::new(&partition, budget_id);

        let def: HashMap<String, String> = self
            .client
            .hgetall(&bctx.definition())
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "get_budget_status: HGETALL budget_def",
            ))?;
        if def.is_empty() {
            // `NotFound.entity` is `&'static str`; include the dynamic
            // budget id in the contextual wrapper rather than the
            // entity slot so the HTTP 404 carries the budget id in
            // its body (matches pre-migration behaviour where
            // `ServerError::NotFound(format!("budget not found: {id}"))`
            // stringified through the 404 mapping).
            return Err(ff_core::engine_error::backend_context(
                EngineError::NotFound { entity: "budget" },
                format!("get_budget_status: {budget_id}"),
            ));
        }

        let usage_raw: HashMap<String, String> = self
            .client
            .hgetall(&bctx.usage())
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "get_budget_status: HGETALL budget_usage",
            ))?;
        let usage: HashMap<String, u64> = usage_raw
            .into_iter()
            .filter(|(k, _)| k != "_init")
            .map(|(k, v)| (k, v.parse().unwrap_or(0)))
            .collect();

        let limits_raw: HashMap<String, String> = self
            .client
            .hgetall(&bctx.limits())
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "get_budget_status: HGETALL budget_limits",
            ))?;
        let mut hard_limits: HashMap<String, u64> = HashMap::new();
        let mut soft_limits: HashMap<String, u64> = HashMap::new();
        for (k, v) in &limits_raw {
            if let Some(dim) = k.strip_prefix("hard:") {
                hard_limits.insert(dim.to_string(), v.parse().unwrap_or(0));
            } else if let Some(dim) = k.strip_prefix("soft:") {
                soft_limits.insert(dim.to_string(), v.parse().unwrap_or(0));
            }
        }

        let non_empty = |s: Option<&String>| -> Option<String> {
            s.filter(|v| !v.is_empty()).cloned()
        };

        Ok(ff_core::contracts::BudgetStatus {
            budget_id: budget_id.to_string(),
            scope_type: def.get("scope_type").cloned().unwrap_or_default(),
            scope_id: def.get("scope_id").cloned().unwrap_or_default(),
            enforcement_mode: def.get("enforcement_mode").cloned().unwrap_or_default(),
            usage,
            hard_limits,
            soft_limits,
            breach_count: def
                .get("breach_count")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            soft_breach_count: def
                .get("soft_breach_count")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            last_breach_at: non_empty(def.get("last_breach_at")),
            last_breach_dim: non_empty(def.get("last_breach_dim")),
            next_reset_at: non_empty(def.get("next_reset_at")),
            created_at: non_empty(def.get("created_at")),
        })
    }

    // ── RFC-017 Stage C — Scheduling (claim_for_worker) ────────

    async fn claim_for_worker(
        &self,
        args: ff_core::contracts::ClaimForWorkerArgs,
    ) -> Result<ff_core::contracts::ClaimForWorkerOutcome, EngineError> {
        let scheduler = self.scheduler.as_ref().ok_or(EngineError::Unavailable {
            op: "claim_for_worker (scheduler not wired on this ValkeyBackend)",
        })?;
        let grant_opt = scheduler
            .claim_for_worker(
                &args.lane_id,
                &args.worker_id,
                &args.worker_instance_id,
                &args.worker_capabilities,
                args.grant_ttl_ms,
            )
            .await
            .map_err(|e| match e {
                ff_scheduler::SchedulerError::Valkey(inner) => ff_core::engine_error::backend_context(
                    transport_fk(inner),
                    "claim_for_worker: scheduler",
                ),
                ff_scheduler::SchedulerError::ValkeyContext { source, context } => {
                    ff_core::engine_error::backend_context(transport_fk(source), context)
                }
                ff_scheduler::SchedulerError::Config(msg) => EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::InvalidInput,
                    detail: msg,
                },
            })?;
        Ok(match grant_opt {
            Some(g) => ff_core::contracts::ClaimForWorkerOutcome::granted(g),
            None => ff_core::contracts::ClaimForWorkerOutcome::no_work(),
        })
    }

    // ── RFC-017 Stage E2 — `Server::client` removal (header + reads) ───

    async fn cancel_flow_header(
        &self,
        args: CancelFlowArgs,
    ) -> Result<ff_core::contracts::CancelFlowHeader, EngineError> {
        use ff_core::contracts::CancelFlowHeader;

        let partition = flow_partition(&args.flow_id, &self.partition_config);
        let fctx = FlowKeyContext::new(&partition, &args.flow_id);
        let fidx = FlowIndexKeys::new(&partition);

        // Grace window matches the Server's pre-E2 constant.
        const CANCEL_RECONCILER_GRACE_MS: u64 = 30_000;

        let fcall_keys: Vec<String> = vec![
            fctx.core(),
            fctx.members(),
            fidx.flow_index(),
            fctx.pending_cancels(),
            fidx.cancel_backlog(),
        ];
        let fcall_args: Vec<String> = vec![
            args.flow_id.to_string(),
            args.reason.clone(),
            args.cancellation_policy.clone(),
            args.now.to_string(),
            CANCEL_RECONCILER_GRACE_MS.to_string(),
        ];
        let key_refs: Vec<&str> = fcall_keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = fcall_args.iter().map(|s| s.as_str()).collect();

        // FCALL with auto Lua-library reload on failover (previously in
        // `ff-server::fcall_with_reload_on_client`).
        let raw: ferriskey::Value = match self
            .client
            .fcall("ff_cancel_flow", &key_refs, &arg_refs)
            .await
        {
            Ok(v) => v,
            Err(e) if is_function_not_loaded_fk(&e) => {
                tracing::warn!(
                    function = "ff_cancel_flow",
                    "Lua library not found on server, reloading"
                );
                ff_script::loader::ensure_library(&self.client)
                    .await
                    .map_err(|le| EngineError::Transport {
                        backend: "valkey",
                        source: Box::new(le),
                    })?;
                self.client
                    .fcall("ff_cancel_flow", &key_refs, &arg_refs)
                    .await
                    .map_err(|e| ff_core::engine_error::backend_context(
                        transport_fk(e),
                        "cancel_flow_header: FCALL ff_cancel_flow (post reload)",
                    ))?
            }
            Err(e) => {
                return Err(ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "cancel_flow_header: FCALL ff_cancel_flow",
                ));
            }
        };

        // Parse the raw response. Shape:
        //   {1, "OK", cancellation_policy, member1, member2, ...}  — success
        //   {0, "flow_already_terminal", ...}                      — idempotent retry
        let arr = match &raw {
            ferriskey::Value::Array(arr) => arr,
            _ => {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: "ff_cancel_flow: expected Array".into(),
                });
            }
        };
        let status = match arr.first() {
            Some(Ok(ferriskey::Value::Int(n))) => *n,
            _ => {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: "ff_cancel_flow: bad status code".into(),
                });
            }
        };
        fn field_str(arr: &[Result<ferriskey::Value, ferriskey::Error>], index: usize) -> String {
            match arr.get(index) {
                Some(Ok(ferriskey::Value::BulkString(b))) => {
                    String::from_utf8_lossy(b).into_owned()
                }
                Some(Ok(ferriskey::Value::SimpleString(s))) => s.clone(),
                Some(Ok(ferriskey::Value::Int(n))) => n.to_string(),
                _ => String::new(),
            }
        }
        if status != 1 {
            let error_code = field_str(arr, 1);
            if error_code != "flow_already_terminal" {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: format!("ff_cancel_flow failed: {error_code}"),
                });
            }
            // Idempotent retry path: flow was already cancelled/completed/failed.
            // Fetch stored policy/reason (HMGET on flow_core) + full membership
            // (SMEMBERS on members_set) so the Server can return an
            // idempotent `Cancelled` with the historical policy.
            let flow_meta: Vec<Option<String>> = self
                .client
                .cmd("HMGET")
                .arg(fctx.core())
                .arg("cancellation_policy")
                .arg("cancel_reason")
                .execute()
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "cancel_flow_header (AlreadyTerminal): HMGET flow_core cancellation_policy,cancel_reason",
                ))?;
            let stored_cancellation_policy = flow_meta
                .first()
                .and_then(|v| v.as_ref())
                .filter(|s| !s.is_empty())
                .cloned();
            let stored_cancel_reason = flow_meta
                .get(1)
                .and_then(|v| v.as_ref())
                .filter(|s| !s.is_empty())
                .cloned();
            let members: Vec<String> = self
                .client
                .cmd("SMEMBERS")
                .arg(fctx.members())
                .execute()
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "cancel_flow_header (AlreadyTerminal): SMEMBERS flow members",
                ))?;
            return Ok(CancelFlowHeader::AlreadyTerminal {
                stored_cancellation_policy,
                stored_cancel_reason,
                member_execution_ids: members,
            });
        }

        // Success path: {1, "OK", cancellation_policy, member1, ...}.
        let policy = field_str(arr, 2);
        let mut members = Vec::with_capacity(arr.len().saturating_sub(3));
        for i in 3..arr.len() {
            members.push(field_str(arr, i));
        }
        Ok(CancelFlowHeader::Cancelled {
            cancellation_policy: policy,
            member_execution_ids: members,
        })
    }

    async fn ack_cancel_member(
        &self,
        flow_id: &FlowId,
        execution_id: &ExecutionId,
    ) -> Result<(), EngineError> {
        let partition = flow_partition(flow_id, &self.partition_config);
        let fctx = FlowKeyContext::new(&partition, flow_id);
        let fidx = FlowIndexKeys::new(&partition);
        let pending = fctx.pending_cancels();
        let backlog = fidx.cancel_backlog();
        let flow_id_str = flow_id.to_string();
        let eid_str = execution_id.to_string();
        let keys = [pending.as_str(), backlog.as_str()];
        let args_v = [eid_str.as_str(), flow_id_str.as_str()];
        let _: ferriskey::Value = self
            .client
            .fcall("ff_ack_cancel_member", &keys, &args_v)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "ack_cancel_member: FCALL ff_ack_cancel_member",
            ))?;
        Ok(())
    }

    async fn read_execution_info(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ff_core::contracts::ExecutionInfo>, EngineError> {
        use ff_core::contracts::ExecutionInfo;
        use ff_core::state::StateVector;
        use std::collections::HashMap;

        let partition = execution_partition(id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, id);
        let fields: HashMap<String, String> = self
            .client
            .hgetall(&ctx.core())
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "read_execution_info: HGETALL exec_core",
            ))?;
        if fields.is_empty() {
            return Ok(None);
        }
        let parse_enum = |field: &str| -> String {
            fields.get(field).cloned().unwrap_or_default()
        };
        fn deserialize<T: serde::de::DeserializeOwned>(
            field: &str,
            raw: &str,
        ) -> Result<T, EngineError> {
            let quoted = format!("\"{raw}\"");
            serde_json::from_str(&quoted).map_err(|e| EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!("invalid {field} '{raw}': {e}"),
            })
        }
        let lp_str = parse_enum("lifecycle_phase");
        let os_str = parse_enum("ownership_state");
        let es_str = parse_enum("eligibility_state");
        let br_str = parse_enum("blocking_reason");
        let to_str = parse_enum("terminal_outcome");
        let as_str = parse_enum("attempt_state");
        let ps_str = parse_enum("public_state");
        let state_vector = StateVector {
            lifecycle_phase: deserialize("lifecycle_phase", &lp_str)?,
            ownership_state: deserialize("ownership_state", &os_str)?,
            eligibility_state: deserialize("eligibility_state", &es_str)?,
            blocking_reason: deserialize("blocking_reason", &br_str)?,
            terminal_outcome: deserialize("terminal_outcome", &to_str)?,
            attempt_state: deserialize("attempt_state", &as_str)?,
            public_state: deserialize("public_state", &ps_str)?,
        };
        let flow_id_val = fields.get("flow_id").filter(|s| !s.is_empty()).cloned();
        let started_at_opt = fields.get("started_at").filter(|s| !s.is_empty()).cloned();
        let completed_at_opt = fields.get("completed_at").filter(|s| !s.is_empty()).cloned();
        Ok(Some(ExecutionInfo {
            execution_id: id.clone(),
            namespace: parse_enum("namespace"),
            lane_id: parse_enum("lane_id"),
            priority: fields.get("priority").and_then(|v| v.parse().ok()).unwrap_or(0),
            execution_kind: parse_enum("execution_kind"),
            state_vector,
            public_state: deserialize("public_state", &ps_str)?,
            created_at: parse_enum("created_at"),
            started_at: started_at_opt,
            completed_at: completed_at_opt,
            current_attempt_index: fields
                .get("current_attempt_index")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            flow_id: flow_id_val,
            blocking_detail: parse_enum("blocking_detail"),
        }))
    }

    async fn read_execution_state(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ff_core::state::PublicState>, EngineError> {
        let partition = execution_partition(id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, id);
        let state_str: Option<String> = self
            .client
            .hget(&ctx.core(), "public_state")
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "read_execution_state: HGET public_state",
            ))?;
        match state_str {
            Some(s) => {
                let quoted = format!("\"{s}\"");
                let parsed: ff_core::state::PublicState = serde_json::from_str(&quoted)
                    .map_err(|e| EngineError::Validation {
                        kind: ValidationKind::InvalidInput,
                        detail: format!("invalid public_state '{s}': {e}"),
                    })?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    // ── RFC-019 Stage A — `subscribe_lease_history` ───────────────
    //
    // Real Valkey impl. Opens a dedicated connection via
    // `duplicate_connection()`, loops `XREAD BLOCK 5000 STREAMS
    // <lease_history_stream> <cursor>`, yields each entry as a
    // `StreamEvent`. On backend disconnect the stream yields
    // `Err(EngineError::StreamDisconnected { cursor: last_seen })`
    // and ends; consumers reconnect by re-calling this method.
    //
    // Partition scope: one subscription tails the configured
    // partition's aggregate `lease_history` stream. Cross-partition
    // consumers instantiate one `ValkeyBackend` per partition and
    // merge streams consumer-side (RFC-019 §Backend Semantics).
    async fn subscribe_lease_history(
        &self,
        cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::LeaseHistorySubscription, EngineError> {
        subscribe_lease_history_impl(
            &self.client,
            cursor,
            build_filter_gate(self, filter),
        )
        .await
    }

    // ── RFC-019 Stage B — `subscribe_signal_delivery` (#310) ───────
    //
    // Mirror of `subscribe_lease_history` against the partition-level
    // aggregate `ff:part:{fp:N}:signal_delivery` stream. The producer
    // XADD lives in `lua/signal.lua::ff_deliver_signal` at new KEYS[15].
    async fn subscribe_signal_delivery(
        &self,
        cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::SignalDeliverySubscription, EngineError> {
        subscribe_signal_delivery_impl(
            &self.client,
            cursor,
            build_filter_gate(self, filter),
        )
        .await
    }

    // ── RFC-019 Stage B — `subscribe_completion` (issue #309) ─────
    //
    // Pubsub-backed wrap over the existing
    // `CompletionBackend::subscribe_completions` RESP3 subscriber
    // (`ff:dag:completions` channel, see `completion.rs`). Valkey
    // completions ride pubsub (at-most-once over the live subscription
    // window) so the `StreamCursor` surface is a non-durable placeholder
    // for Stage B: the incoming cursor is ignored and every emitted
    // event carries `StreamCursor::empty()`. Consumers who need
    // at-least-once replay with durable cursor resume use the Postgres
    // backend (see `docs/POSTGRES_PARITY_MATRIX.md` row
    // `subscribe_completion`). If someone later wants a durable Valkey
    // completion subscription we move completions from pubsub to an
    // XADD stream (separate issue — not Stage B scope per #309).
    //
    // Payload shape mirrors the Postgres adapter: outcome bytes go in
    // `StreamEvent.payload`, `execution_id` is inlined, cursor is the
    // family sentinel (empty for Valkey Stage B).
    async fn subscribe_completion(
        &self,
        _cursor: ff_core::stream_subscribe::StreamCursor,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ff_core::stream_events::CompletionSubscription, EngineError> {
        use ff_core::stream_events::{CompletionEvent, CompletionOutcome};
        use ff_core::stream_subscribe::StreamCursor;
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // Delegate to the RESP3 subscriber already exposed via the
        // `CompletionBackend` trait. When a non-noop filter is
        // supplied, forward it so the per-push HGET gate installed by
        // `subscribe_completions_filtered` runs inline. This shares
        // the same reconnect/parse loop (`completion::subscriber_loop`)
        // that ff-engine's `completion_listener` consumes — no
        // duplicate pubsub machinery.
        let inner = if filter.is_noop() {
            ff_core::completion_backend::CompletionBackend::subscribe_completions(self).await?
        } else {
            ff_core::completion_backend::CompletionBackend::subscribe_completions_filtered(
                self, filter,
            )
            .await?
        };

        struct Adapter {
            inner: ff_core::completion_backend::CompletionStream,
            /// Set once the inner stream ends + we've emitted the
            /// terminal `StreamDisconnected` error. Ensures we return
            /// `Ready(None)` on subsequent polls instead of looping on
            /// the error frame.
            disconnected_emitted: bool,
        }

        impl Stream for Adapter {
            type Item = Result<CompletionEvent, EngineError>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.disconnected_emitted {
                    return Poll::Ready(None);
                }
                match Pin::new(&mut self.inner).poll_next(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(None) => {
                        // Underlying mpsc closed → subscriber loop
                        // exited (consumer drop OR unrecoverable
                        // backend drop). RFC-019 §disconnect contract:
                        // surface a terminal `StreamDisconnected`
                        // carrying the cursor to reconnect from. Valkey
                        // Stage B has no durable cursor so yield the
                        // empty sentinel and end.
                        self.disconnected_emitted = true;
                        Poll::Ready(Some(Err(EngineError::StreamDisconnected {
                            cursor: StreamCursor::empty(),
                        })))
                    }
                    Poll::Ready(Some(payload)) => {
                        let event = CompletionEvent::new(
                            StreamCursor::empty(),
                            payload.execution_id,
                            CompletionOutcome::from_wire(&payload.outcome),
                            payload.produced_at_ms,
                        );
                        Poll::Ready(Some(Ok(event)))
                    }
                }
            }
        }

        Ok(Box::pin(Adapter {
            inner,
            disconnected_emitted: false,
        }))
    }

    // ── PR-7b Cluster 1 — Foundation scanner operations ─────────
    //
    // Thin wrappers around the existing scanner FCALLs. The per-
    // candidate key-formatting + pre-read logic mirrors what the
    // scanner bodies computed inline pre-PR-7b; moving it onto the
    // backend lets scanners dispatch through `EngineBackend` instead
    // of reaching for `ferriskey::Client` directly.

    async fn mark_lease_expired_if_due(
        &self,
        partition: Partition,
        execution_id: &ExecutionId,
    ) -> Result<(), EngineError> {
        let tag = partition.hash_tag();
        let eid_str = execution_id.as_str();
        let idx = IndexKeys::new(&partition);
        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let lease_current = format!("ff:exec:{}:{}:lease:current", tag, eid_str);
        let lease_history = format!("ff:exec:{}:{}:lease:history", tag, eid_str);
        let lease_expiry = idx.lease_expiry();
        let keys: [&str; 4] = [&exec_core, &lease_current, &lease_expiry, &lease_history];
        let argv: [&str; 1] = [eid_str];
        let _: ferriskey::Value = self
            .client
            .fcall("ff_mark_lease_expired_if_due", &keys, &argv)
            .await
            .map_err(transport_fk)?;
        Ok(())
    }

    async fn promote_delayed(
        &self,
        partition: Partition,
        lane: &LaneId,
        execution_id: &ExecutionId,
        now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let tag = partition.hash_tag();
        let eid_str = execution_id.as_str();
        let idx = IndexKeys::new(&partition);
        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let delayed_zset = idx.lane_delayed(lane);
        let eligible_zset = idx.lane_eligible(lane);
        let keys: [&str; 3] = [&exec_core, &delayed_zset, &eligible_zset];
        let now_str = now_ms.0.to_string();
        let argv: [&str; 2] = [eid_str, &now_str];
        let _: ferriskey::Value = self
            .client
            .fcall("ff_promote_delayed", &keys, &argv)
            .await
            .map_err(transport_fk)?;
        Ok(())
    }

    async fn close_waitpoint(
        &self,
        partition: Partition,
        execution_id: &ExecutionId,
        waitpoint_id: &str,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let tag = partition.hash_tag();
        let eid_str = execution_id.as_str();
        let idx = IndexKeys::new(&partition);
        let waitpoint_hash = format!("ff:wp:{}:{}", tag, waitpoint_id);
        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let pending_wp_expiry = idx.pending_waitpoint_expiry();
        let keys: [&str; 3] = [&exec_core, &waitpoint_hash, &pending_wp_expiry];
        let argv: [&str; 2] = [waitpoint_id, "never_committed"];
        let _: ferriskey::Value = self
            .client
            .fcall("ff_close_waitpoint", &keys, &argv)
            .await
            .map_err(transport_fk)?;
        Ok(())
    }

    async fn expire_execution(
        &self,
        partition: Partition,
        execution_id: &ExecutionId,
        phase: ExpirePhase,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let tag = partition.hash_tag();
        let eid_str = execution_id.as_str();
        let idx = IndexKeys::new(&partition);

        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let lease_current = format!("ff:exec:{}:{}:lease:current", tag, eid_str);
        let lease_history = format!("ff:exec:{}:{}:lease:history", tag, eid_str);
        let susp_current = format!("ff:exec:{}:{}:suspension:current", tag, eid_str);

        // Pre-read lane_id and current_attempt_index from exec_core —
        // the Lua function needs the real attempt index to find the
        // correct attempt_hash + stream_meta. Same pattern the
        // scanner body used pre-trait.
        let pre_fields: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(&exec_core)
            .arg("lane_id")
            .arg("current_attempt_index")
            .execute()
            .await
            .map_err(transport_fk)?;
        let lane = LaneId::new(
            pre_fields
                .first()
                .and_then(|v| v.as_deref())
                .unwrap_or("default"),
        );
        let att_idx = pre_fields
            .get(1)
            .and_then(|v| v.as_deref())
            .unwrap_or("0");

        let attempt_hash = format!("ff:attempt:{}:{}:{}", tag, eid_str, att_idx);
        let stream_meta = format!("ff:stream:{}:{}:{}:meta", tag, eid_str, att_idx);

        let lease_expiry = idx.lease_expiry();
        let worker_leases = idx.worker_leases(&WorkerInstanceId::new(""));
        let active = idx.lane_active(&lane);
        let terminal = idx.lane_terminal(&lane);
        let attempt_timeout = idx.attempt_timeout();
        let execution_deadline = idx.execution_deadline();
        let suspended = idx.lane_suspended(&lane);
        let suspension_timeout = idx.suspension_timeout();

        let keys: [&str; 14] = [
            &exec_core,
            &attempt_hash,
            &stream_meta,
            &lease_current,
            &lease_history,
            &lease_expiry,
            &worker_leases,
            &active,
            &terminal,
            &attempt_timeout,
            &execution_deadline,
            &suspended,
            &suspension_timeout,
            &susp_current,
        ];
        let argv: [&str; 2] = [eid_str, phase.as_str()];
        let _: ferriskey::Value = self
            .client
            .fcall("ff_expire_execution", &keys, &argv)
            .await
            .map_err(transport_fk)?;
        Ok(())
    }

    async fn expire_suspension(
        &self,
        partition: Partition,
        execution_id: &ExecutionId,
        _now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let tag = partition.hash_tag();
        let eid_str = execution_id.as_str();
        let idx = IndexKeys::new(&partition);

        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let suspension_current = format!("ff:exec:{}:{}:suspension:current", tag, eid_str);

        let wp_id: Option<String> = self
            .client
            .cmd("HGET")
            .arg(&exec_core)
            .arg("current_waitpoint_id")
            .execute()
            .await
            .map_err(transport_fk)?;
        let att_idx: Option<String> = self
            .client
            .cmd("HGET")
            .arg(&exec_core)
            .arg("current_attempt_index")
            .execute()
            .await
            .map_err(transport_fk)?;
        let lane: Option<String> = self
            .client
            .cmd("HGET")
            .arg(&exec_core)
            .arg("lane_id")
            .execute()
            .await
            .map_err(transport_fk)?;

        let wp_id = wp_id.unwrap_or_default();
        let att_idx = att_idx.unwrap_or_else(|| "0".to_string());
        let lane_str = lane.unwrap_or_else(|| "default".to_string());
        let lane_id = LaneId::new(&lane_str);

        let waitpoint_hash = format!("ff:wp:{}:{}", tag, wp_id);
        let wp_condition = format!("ff:wp:{}:{}:condition", tag, wp_id);
        let attempt_hash = format!("ff:attempt:{}:{}:{}", tag, eid_str, att_idx);
        let stream_meta = format!("ff:stream:{}:{}:{}:meta", tag, eid_str, att_idx);
        let suspension_timeout = idx.suspension_timeout();
        let suspended_zset = idx.lane_suspended(&lane_id);
        let terminal_zset = idx.lane_terminal(&lane_id);
        let eligible_zset = idx.lane_eligible(&lane_id);
        let delayed_zset = idx.lane_delayed(&lane_id);
        let lease_history = format!("ff:exec:{}:{}:lease:history", tag, eid_str);

        let keys: [&str; 12] = [
            &exec_core,
            &suspension_current,
            &waitpoint_hash,
            &wp_condition,
            &attempt_hash,
            &stream_meta,
            &suspension_timeout,
            &suspended_zset,
            &terminal_zset,
            &eligible_zset,
            &delayed_zset,
            &lease_history,
        ];
        let argv: [&str; 1] = [eid_str];
        let _: ferriskey::Value = self
            .client
            .fcall("ff_expire_suspension", &keys, &argv)
            .await
            .map_err(transport_fk)?;
        Ok(())
    }

    // PR-7b Cluster 2: wraps `ff_unblock_execution` (RFC-010 §6).
    // KEYS[3]: exec_core, <source_blocked_zset>, eligible_zset
    // ARGV[3]: execution_id, now_ms, expected_blocking_reason
    //
    // `expected_blocking_reason` selects which of the three blocked
    // ZSETs (`blocked:budget`, `blocked:quota`, `blocked:route`) is
    // drained. The Lua body re-validates `blocking_reason` on
    // `exec_core` to fence a stale unblock when the execution has
    // already transitioned.
    async fn unblock_execution(
        &self,
        partition: Partition,
        lane_id: &LaneId,
        execution_id: &ExecutionId,
        expected_blocking_reason: &str,
        now_ms: TimestampMs,
    ) -> Result<(), EngineError> {
        let tag = partition.hash_tag();
        let eid_str = execution_id.as_str();
        let idx = IndexKeys::new(&partition);

        let exec_core = format!("ff:exec:{}:{}:core", tag, eid_str);
        let blocked_zset = match expected_blocking_reason {
            "waiting_for_budget" => idx.lane_blocked_budget(lane_id),
            "waiting_for_quota" => idx.lane_blocked_quota(lane_id),
            "waiting_for_capable_worker" => idx.lane_blocked_route(lane_id),
            other => {
                return Err(EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::InvalidBlockingReason,
                    detail: format!("unblock_execution: {other}"),
                });
            }
        };
        let eligible_zset = idx.lane_eligible(lane_id);

        let keys: [&str; 3] = [&exec_core, &blocked_zset, &eligible_zset];
        let now_s = now_ms.to_string();
        let argv: [&str; 3] = [eid_str, &now_s, expected_blocking_reason];

        let _: ferriskey::Value = self
            .client
            .fcall("ff_unblock_execution", &keys, &argv)
            .await
            .map_err(transport_fk)?;
        Ok(())
    }

    /// PR-7b Cluster 3: route `edge_cancel_dispatcher::drain_group`
    /// through the trait. Wraps `FCALL ff_drain_sibling_cancel_group`
    /// with the same `(pending_cancel_groups, edgegroup)` KEYS and
    /// `(flow_id, downstream_eid)` ARGV the Valkey scanner body used.
    async fn drain_sibling_cancel_group(
        &self,
        flow_partition: Partition,
        flow_id: &FlowId,
        downstream_eid: &ExecutionId,
    ) -> Result<(), EngineError> {
        let fctx = FlowKeyContext::new(&flow_partition, flow_id);
        let fidx = FlowIndexKeys::new(&flow_partition);
        let pending_key = fidx.pending_cancel_groups();
        let edgegroup_key = fctx.edgegroup(downstream_eid);
        let flow_id_str = flow_id.to_string();
        let downstream_eid_str = downstream_eid.to_string();
        let keys = [pending_key.as_str(), edgegroup_key.as_str()];
        let argv = [flow_id_str.as_str(), downstream_eid_str.as_str()];
        let _: ferriskey::Value = self
            .client
            .fcall("ff_drain_sibling_cancel_group", &keys, &argv)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "drain_sibling_cancel_group: FCALL ff_drain_sibling_cancel_group",
                )
            })?;
        Ok(())
    }

    /// PR-7b Cluster 3: route `edge_cancel_reconciler::reconcile_one_group`
    /// through the trait. Wraps `FCALL ff_reconcile_sibling_cancel_group`
    /// and decodes the Lua `{1, "OK", <action>, ...}` reply shape into
    /// the typed [`SiblingCancelReconcileAction`].
    async fn reconcile_sibling_cancel_group(
        &self,
        flow_partition: Partition,
        flow_id: &FlowId,
        downstream_eid: &ExecutionId,
    ) -> Result<SiblingCancelReconcileAction, EngineError> {
        let fctx = FlowKeyContext::new(&flow_partition, flow_id);
        let fidx = FlowIndexKeys::new(&flow_partition);
        let pending_key = fidx.pending_cancel_groups();
        let edgegroup_key = fctx.edgegroup(downstream_eid);
        let flow_id_str = flow_id.to_string();
        let downstream_eid_str = downstream_eid.to_string();
        let keys = [pending_key.as_str(), edgegroup_key.as_str()];
        let argv = [flow_id_str.as_str(), downstream_eid_str.as_str()];
        let val: ferriskey::Value = self
            .client
            .fcall("ff_reconcile_sibling_cancel_group", &keys, &argv)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "reconcile_sibling_cancel_group: \
                     FCALL ff_reconcile_sibling_cancel_group",
                )
            })?;

        // Lua shape: `{1, "OK", <action>, <detail?>}`. Extract index 2.
        let action_str = match &val {
            ferriskey::Value::Array(arr) => arr
                .get(2)
                .and_then(|r| r.as_ref().ok())
                .and_then(|v| match v {
                    ferriskey::Value::BulkString(b) => {
                        Some(String::from_utf8_lossy(b).into_owned())
                    }
                    ferriskey::Value::SimpleString(s) => Some(s.clone()),
                    _ => None,
                }),
            _ => None,
        };

        match action_str.as_deref() {
            Some("sremmed_stale") => Ok(SiblingCancelReconcileAction::SremmedStale),
            Some("completed_drain") => Ok(SiblingCancelReconcileAction::CompletedDrain),
            Some("no_op") => Ok(SiblingCancelReconcileAction::NoOp),
            _ => Err(EngineError::Transport {
                backend: "valkey",
                source: "reconcile_sibling_cancel_group: unparsable FCALL \
                         reply (expected [..,..,action,..])"
                    .to_owned()
                    .into(),
            }),
        }
    }

    // ── PR-7b Cluster 2b-A — Tally-recompute reconciler scanners ─────
    //
    // Each method runs a full scan-and-fix pass on one partition using
    // the same Redis commands (SSCAN / HMGET / HGETALL / ZSCORE /
    // ZREMRANGEBYSCORE / SREM / GET / SET / HSET / HDEL) the pre-routing
    // scanner bodies used. The command shapes are preserved verbatim so
    // this migration is behavioural-parity with the previous direct-client
    // scanner path. No new Lua functions introduced.

    async fn reconcile_execution_index(
        &self,
        partition: Partition,
        lanes: &[LaneId],
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<ReconcileCounts, EngineError> {
        const SCAN_COUNT: u32 = 100;
        let idx = IndexKeys::new(&partition);
        let all_exec_key = idx.all_executions();
        let tag = partition.hash_tag();

        let mut cursor = "0".to_string();
        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        loop {
            let result: ferriskey::Value = match self
                .client
                .cmd("SSCAN")
                .arg(&all_exec_key)
                .arg(cursor.as_str())
                .arg("COUNT")
                .arg(SCAN_COUNT.to_string().as_str())
                .execute()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(partition = partition.index, error = %e,
                        "reconcile_execution_index: SSCAN failed");
                    return Ok(ReconcileCounts { processed, errors: errors + 1 });
                }
            };

            let (next_cursor, members) = match parse_sscan_response(&result) {
                Ok(v) => v,
                Err(e) => {
                    // Malformed SSCAN reply is a hard transport error —
                    // surfacing it lets the next reconciler cycle retry
                    // with a real error signal rather than silently
                    // ending the scan and hiding index drift.
                    tracing::warn!(
                        partition = partition.index,
                        hash_tag = %tag,
                        error = %e,
                        "reconcile_execution_index: malformed SSCAN reply; \
                         aborting cycle to surface error"
                    );
                    return Err(e);
                }
            };

            for eid_str in &members {
                if self.scanner_skip_candidate(filter, eid_str).await {
                    continue;
                }
                match check_execution_index(
                    &self.client,
                    &partition,
                    &tag,
                    &idx,
                    eid_str,
                    lanes,
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => processed += 1,
                    Err(e) => {
                        tracing::warn!(partition = partition.index,
                            execution_id = eid_str.as_str(), error = %e,
                            "reconcile_execution_index: check failed");
                        errors += 1;
                    }
                }
            }

            cursor = next_cursor;
            if cursor == "0" {
                break;
            }
        }

        Ok(ReconcileCounts { processed, errors })
    }

    async fn reconcile_budget_counters(
        &self,
        partition: Partition,
        now_ms: TimestampMs,
    ) -> Result<ReconcileCounts, EngineError> {
        let tag = partition.hash_tag();
        let policies_key = ff_core::keys::budget_policies_index(&tag);
        let now_ms_u: u64 = now_ms.0.max(0) as u64;

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;
        let mut cursor = "0".to_string();

        loop {
            let result: ferriskey::Value = match self
                .client
                .cmd("SSCAN")
                .arg(&policies_key)
                .arg(cursor.as_str())
                .arg("COUNT")
                .arg("100")
                .execute()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(partition = partition.index, error = %e,
                        "reconcile_budget_counters: SSCAN failed");
                    return Ok(ReconcileCounts { processed, errors: errors + 1 });
                }
            };

            let (next_cursor, budget_ids) = parse_sscan_tuple(&result);

            for bid in &budget_ids {
                match reconcile_one_budget(&self.client, &tag, &policies_key, bid, now_ms_u).await {
                    Ok(true) => processed += 1,
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(partition = partition.index,
                            budget_id = bid.as_str(), error = %e,
                            "reconcile_budget_counters: reconcile failed");
                        errors += 1;
                    }
                }
            }

            cursor = next_cursor;
            if cursor == "0" {
                break;
            }
        }

        Ok(ReconcileCounts { processed, errors })
    }

    async fn reconcile_quota_counters(
        &self,
        partition: Partition,
        now_ms: TimestampMs,
    ) -> Result<ReconcileCounts, EngineError> {
        let tag = partition.hash_tag();
        let policies_key = ff_core::keys::quota_policies_index(&tag);
        let now_ms_u: u64 = now_ms.0.max(0) as u64;

        let quota_ids: Vec<String> = match self
            .client
            .cmd("SMEMBERS")
            .arg(&policies_key)
            .execute()
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition = partition.index, error = %e,
                    "reconcile_quota_counters: SMEMBERS failed");
                return Ok(ReconcileCounts { processed: 0, errors: 1 });
            }
        };

        if quota_ids.is_empty() {
            return Ok(ReconcileCounts::default());
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for qid in &quota_ids {
            match reconcile_one_quota(&self.client, &tag, qid, now_ms_u).await {
                Ok(true) => processed += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(partition = partition.index,
                        quota_id = qid.as_str(), error = %e,
                        "reconcile_quota_counters: reconcile failed");
                    errors += 1;
                }
            }
        }

        Ok(ReconcileCounts { processed, errors })
    }

    // ── PR-7b Cluster 2b-B — Projection + retention scanners ─────────

    // PR-7b Cluster 2b-B: Rust-composed flow summary projection (no
    // FCALL). Mirrors the scanner's pre-PR-7b behaviour verbatim —
    // defensive core-exists prune, SRANDMEMBER sample, per-member
    // cross-partition public_state HGET, derived public_flow_state,
    // HSET summary, conditional SREM of the flow_index entry once
    // every member has been observed as terminal.
    async fn project_flow_summary(
        &self,
        partition: Partition,
        flow_id: &FlowId,
        now_ms: TimestampMs,
    ) -> Result<bool, EngineError> {
        project_flow_summary_impl(
            &self.client,
            partition,
            flow_id,
            now_ms,
            &self.partition_config,
        )
        .await
    }

    // PR-7b Cluster 2b-B: retention trimming over one partition+lane.
    // Lifts the ZRANGEBYSCORE + per-execution cascade-delete loop from
    // the engine scanner. Returns the count of executions purged in
    // this call so the scanner can loop if it saturated batch_size.
    async fn trim_retention(
        &self,
        partition: Partition,
        lane_id: &LaneId,
        retention_ms: u64,
        now_ms: TimestampMs,
        batch_size: u32,
        filter: &ff_core::backend::ScannerFilter,
    ) -> Result<u32, EngineError> {
        trim_retention_impl(
            self,
            partition,
            lane_id,
            retention_ms,
            now_ms,
            batch_size,
            filter,
        )
        .await
    }

    // ── PR-7b Wave 0a: exec_core field read ──

    async fn read_exec_core_fields(
        &self,
        partition: ff_core::partition::Partition,
        execution_id: &ff_core::types::ExecutionId,
        fields: &[&str],
    ) -> Result<std::collections::HashMap<String, Option<String>>, EngineError> {
        use ff_core::keys::ExecKeyContext;
        if fields.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let ctx = ExecKeyContext::new(&partition, execution_id);
        let core_key = ctx.core();
        let values: Vec<Option<String>> = self
            .client
            .cmd("HMGET")
            .arg(core_key.as_str())
            .arg(fields)
            .execute()
            .await
            .map_err(transport_fk)?;
        let mut out = std::collections::HashMap::with_capacity(fields.len());
        for (name, val) in fields.iter().zip(values) {
            out.insert((*name).to_string(), val);
        }
        Ok(out)
    }

    // ── PR-7b Wave 0a: clock primitive ──

    async fn server_time_ms(&self) -> Result<u64, EngineError> {
        let result: Vec<String> = self
            .client
            .cmd("TIME")
            .execute()
            .await
            .map_err(transport_fk)?;
        if result.len() < 2 {
            return Err(EngineError::Transport {
                backend: "valkey",
                source: "TIME returned fewer than 2 elements".into(),
            });
        }
        let secs: u64 = result[0].parse().map_err(|_| EngineError::Transport {
            backend: "valkey",
            source: "TIME: invalid seconds".into(),
        })?;
        let micros: u64 = result[1].parse().map_err(|_| EngineError::Transport {
            backend: "valkey",
            source: "TIME: invalid microseconds".into(),
        })?;
        Ok(secs.saturating_mul(1000).saturating_add(micros / 1000))
    }

    // ── RFC-025 worker registry ──────────────────────────────────
    //
    // Keys are namespace-prefixed (§9.1). The SDK preamble keeps
    // writing the pre-RFC-025 un-namespaced shape for the duration of
    // the Phase 2→Phase 5 rollout — both write paths coexist until
    // cairn migrates to the trait in Phase 5. Phase 2's PR description
    // flags this gap.

    async fn register_worker(
        &self,
        args: RegisterWorkerArgs,
    ) -> Result<RegisterWorkerOutcome, EngineError> {
        let alive_key = worker_alive_key_ns(&args.namespace, &args.worker_instance_id);
        let caps_key = worker_caps_key_ns(&args.namespace, &args.worker_instance_id);
        let index_key = workers_index_key_ns(&args.namespace);

        let lanes_csv = args
            .lanes
            .iter()
            .map(|l| l.0.as_str())
            .collect::<Vec<&str>>()
            .join(",");
        let caps_csv = args
            .capabilities
            .iter()
            .cloned()
            .collect::<Vec<String>>()
            .join(",");

        let keys = RegisterWorkerKeys {
            alive_key: alive_key.as_str(),
            caps_key: caps_key.as_str(),
            index_key: index_key.as_str(),
        };
        let argv = RegisterWorkerArgv {
            instance_id: args.worker_instance_id.as_str(),
            worker_id: args.worker_id.as_str(),
            lanes_csv: lanes_csv.as_str(),
            caps_csv: caps_csv.as_str(),
            ttl_ms: args.liveness_ttl_ms,
            now_ms: args.now.0,
        };

        ff_register_worker(&self.client, keys, argv)
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_script(e),
                "register_worker: FCALL ff_register_worker",
            ))
    }

    async fn heartbeat_worker(
        &self,
        args: HeartbeatWorkerArgs,
    ) -> Result<HeartbeatWorkerOutcome, EngineError> {
        let alive_key = worker_alive_key_ns(&args.namespace, &args.worker_instance_id);
        let caps_key = worker_caps_key_ns(&args.namespace, &args.worker_instance_id);

        // Single-round-trip TTL lookup. Absent hash == no prior
        // registration; callers re-register instead of heartbeating.
        let stored_ttl: Option<String> = self
            .client
            .cmd("HGET")
            .arg(caps_key.as_str())
            .arg("ttl_ms")
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "heartbeat_worker: HGET ttl_ms",
            ))?;

        let ttl_ms: u64 = match stored_ttl.as_deref() {
            Some(s) if !s.is_empty() => s.parse().map_err(|e| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("heartbeat_worker: stored ttl_ms not a u64: {e}"),
            })?,
            _ => return Ok(HeartbeatWorkerOutcome::NotRegistered),
        };

        // PEXPIRE returns 1 when the TTL was applied, 0 when the key
        // is absent (TTL elapsed between HGET and PEXPIRE, or
        // operator-initiated mark_worker_dead raced us). ferriskey
        // decodes this as a boolean, not an integer.
        let pexpire_applied: bool = self
            .client
            .cmd("PEXPIRE")
            .arg(alive_key.as_str())
            .arg(ttl_ms.to_string().as_str())
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "heartbeat_worker: PEXPIRE alive",
            ))?;

        if !pexpire_applied {
            Ok(HeartbeatWorkerOutcome::NotRegistered)
        } else {
            let next_expiry_ms = TimestampMs::from_millis(
                args.now.0.saturating_add(ttl_ms as i64),
            );
            Ok(HeartbeatWorkerOutcome::Refreshed { next_expiry_ms })
        }
    }

    async fn mark_worker_dead(
        &self,
        args: MarkWorkerDeadArgs,
    ) -> Result<MarkWorkerDeadOutcome, EngineError> {
        // RFC-025 §4 / §Rev-2 item 9: reason capped at 256 bytes, no
        // control chars. Mirror `fail_execution`'s discipline.
        if args.reason.len() > MARK_WORKER_DEAD_REASON_MAX_BYTES {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!(
                    "reason: exceeds {} bytes (got {})",
                    MARK_WORKER_DEAD_REASON_MAX_BYTES,
                    args.reason.len()
                ),
            });
        }
        if args.reason.chars().any(|c| c.is_control()) {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "reason: must not contain control characters".into(),
            });
        }

        let alive_key = worker_alive_key_ns(&args.namespace, &args.worker_instance_id);
        let caps_key = worker_caps_key_ns(&args.namespace, &args.worker_instance_id);
        let index_key = workers_index_key_ns(&args.namespace);

        // TODO(RFC-025 Phase 2): ferriskey's typed `Client` doesn't
        // expose MULTI/EXEC directly today. For Phase 2 we issue three
        // sequential commands; concurrent mark_worker_dead on the
        // same instance_id is benign (idempotent no-op on the
        // already-deleted branch). A concurrent register+mark race is
        // the same hazard either path has — documented in §Non-goals
        // item 1. Phase 5 can promote to a Lua FCALL if cairn's
        // stress tests surface a real-world ordering bug.
        let del_alive: i64 = self
            .client
            .cmd("DEL")
            .arg(alive_key.as_str())
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "mark_worker_dead: DEL alive",
            ))?;
        let del_caps: i64 = self
            .client
            .cmd("DEL")
            .arg(caps_key.as_str())
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "mark_worker_dead: DEL caps",
            ))?;
        let _: i64 = self
            .client
            .cmd("SREM")
            .arg(index_key.as_str())
            .arg(args.worker_instance_id.as_str())
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "mark_worker_dead: SREM index",
            ))?;

        if del_alive == 0 && del_caps == 0 {
            Ok(MarkWorkerDeadOutcome::NotRegistered)
        } else {
            Ok(MarkWorkerDeadOutcome::Marked)
        }
    }

    async fn list_expired_leases(
        &self,
        args: ListExpiredLeasesArgs,
    ) -> Result<ListExpiredLeasesResult, EngineError> {
        // Namespace is enforced at the admin-route boundary, not the
        // trait (§Rev-3 item 3). The underlying `ff:idx:{fp:N}:lease_expiry`
        // ZSETs are namespace-agnostic — all execution partitions
        // share the same lease-expiry index layout. Per-namespace
        // filtering would require an exec_core HGET on every entry
        // for `namespace`; that post-filter lands in Phase 3 when
        // PG's schema makes the filter cheap. For Phase 2 on Valkey,
        // the namespace arg is accepted-and-ignored; cross-namespace
        // operator tooling is the documented consumer.
        let _ = args.namespace;

        let limit = args
            .limit
            .unwrap_or(LIST_EXPIRED_LEASES_DEFAULT_LIMIT)
            .min(LIST_EXPIRED_LEASES_MAX_LIMIT) as usize;
        let max_partitions = args
            .max_partitions_per_call
            .unwrap_or(LIST_EXPIRED_LEASES_DEFAULT_MAX_PARTITIONS);

        let num_partitions = self.partition_config.num_flow_partitions;
        if num_partitions == 0 {
            return Ok(ListExpiredLeasesResult::new(Vec::new(), None));
        }

        // Partition-iteration start offset. Cursor exclusivity is
        // handled at the merge step; the partition index where the
        // cursor entry lives is the natural starting point.
        let (start_partition, cursor_expires_ms, cursor_eid): (u16, i64, Option<String>) =
            match args.after.as_ref() {
                Some(c) => {
                    let part = execution_partition(&c.execution_id, &self.partition_config);
                    (
                        part.index,
                        c.expires_at_ms.0,
                        Some(c.execution_id.to_string()),
                    )
                }
                None => (0, 0, None),
            };

        // Stable `(expires_at_ms, execution_id)` order across the
        // merged fan-out. Collect (score, eid, partition) so the
        // exec_core HMGET can re-use the partition index without a
        // second routing pass.
        let mut merged: Vec<(i64, String, Partition)> = Vec::new();

        let fan = max_partitions.min(num_partitions as u32) as u16;
        for offset in 0..fan {
            let idx_u16 = (start_partition + offset) % num_partitions;
            let partition = Partition {
                family: PartitionFamily::Execution,
                index: idx_u16,
            };
            let idx = IndexKeys::new(&partition);
            let key = idx.lease_expiry();

            // Fetch up to `limit` per partition; merge reduces to
            // `limit` across partitions below.
            let entries: Vec<String> = self
                .client
                .cmd("ZRANGEBYSCORE")
                .arg(key.as_str())
                .arg("-inf")
                .arg(args.as_of.0.to_string().as_str())
                .arg("WITHSCORES")
                .arg("LIMIT")
                .arg("0")
                .arg(limit.to_string().as_str())
                .execute()
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "list_expired_leases: ZRANGEBYSCORE lease_expiry",
                ))?;

            // ZRANGEBYSCORE ... WITHSCORES returns flat [member,
            // score, member, score, ...]; pair them up.
            let mut it = entries.into_iter();
            while let (Some(member), Some(score_str)) = (it.next(), it.next()) {
                let score: i64 = score_str
                    .parse()
                    .map_err(|e| EngineError::Validation {
                        kind: ValidationKind::Corruption,
                        detail: format!("lease_expiry.score: bad score {score_str:?}: {e}"),
                    })?;
                merged.push((score, member, partition));
            }
        }

        // Order: (expires_at_ms ASC, execution_id ASC). Cursor
        // exclusivity: drop any entry <= cursor.
        merged.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        if let Some(cursor_eid_ref) = cursor_eid.as_ref() {
            merged.retain(|(score, eid, _)| {
                *score > cursor_expires_ms
                    || (*score == cursor_expires_ms && eid.as_str() > cursor_eid_ref.as_str())
            });
        }
        let page_full = merged.len() > limit;
        merged.truncate(limit);

        // HGET exec_core for each candidate to materialise
        // ExpiredLeaseInfo. Pipelined via ferriskey's sequential
        // futures — the page cap (≤10_000) bounds cost.
        let mut out: Vec<ExpiredLeaseInfo> = Vec::with_capacity(merged.len());
        for (score, eid_str, partition) in &merged {
            let execution_id = ExecutionId::parse(eid_str).map_err(|e| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("lease_expiry.member: bad execution_id: {e}"),
            })?;
            let ctx = ExecKeyContext::new(partition, &execution_id);
            let raw: Vec<Option<String>> = self
                .client
                .cmd("HMGET")
                .arg(ctx.core().as_str())
                .arg("current_lease_id")
                .arg("current_lease_epoch")
                .arg("current_attempt_index")
                .arg("current_worker_instance_id")
                .execute()
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "list_expired_leases: HMGET exec_core",
                ))?;
            let lease_id_str = raw.first().and_then(|v| v.clone()).unwrap_or_default();
            let lease_epoch_str = raw.get(1).and_then(|v| v.clone()).unwrap_or_default();
            let attempt_idx_str = raw.get(2).and_then(|v| v.clone()).unwrap_or_default();
            let worker_inst_str = raw.get(3).and_then(|v| v.clone()).unwrap_or_default();

            // Stale zset entry with an already-cleared lease — skip
            // rather than fail. Lua-side lease expiry / revocation
            // cleans the zset atomically, so this is only reached if
            // a crash interleaved.
            if lease_id_str.is_empty() || worker_inst_str.is_empty() {
                continue;
            }
            let lease_id = LeaseId::parse(&lease_id_str).map_err(|e| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: format!("current_lease_id: bad lease_id: {e}"),
            })?;
            let lease_epoch: u64 = lease_epoch_str.parse().unwrap_or(0);
            let attempt_index: u32 = attempt_idx_str.parse().unwrap_or(0);

            out.push(ExpiredLeaseInfo::new(
                execution_id,
                lease_id,
                LeaseEpoch::new(lease_epoch),
                WorkerInstanceId::new(worker_inst_str),
                TimestampMs::from_millis(*score),
                AttemptIndex::new(attempt_index),
            ));
        }

        let cursor = if page_full {
            out.last().map(|e| ExpiredLeasesCursor::new(e.expires_at_ms, e.execution_id.clone()))
        } else {
            None
        };
        Ok(ListExpiredLeasesResult::new(out, cursor))
    }

    async fn list_workers(
        &self,
        args: ListWorkersArgs,
    ) -> Result<ListWorkersResult, EngineError> {
        // Phase 2 scope: per-namespace listing only. Cross-namespace
        // enumeration (`args.namespace = None`) would need a
        // persistent namespace-registry on Valkey that doesn't exist
        // today; the unblock scanner's namespace-scoped
        // `ff:idx:{ns}:workers` set (RFC-025 Phase 5 cutover) is the
        // per-namespace enumerable, but there is no parent index of
        // namespaces themselves. Phase 3 (PG) lands the authoritative
        // cross-namespace index via `ff_worker_registry` scan; until
        // then operators that need a cross-ns view iterate
        // namespaces out-of-band. Surfacing as `Unavailable` (not
        // silently returning an empty page) so callers notice.
        let ns = match args.namespace.as_ref() {
            Some(ns) => ns,
            None => {
                return Err(EngineError::Unavailable {
                    op: "list_workers (cross-namespace on Valkey — Phase 3 follow-up)",
                });
            }
        };

        let limit = args.limit.unwrap_or(1000) as usize;
        let index_key = workers_index_key_ns(ns);

        let mut instance_ids: Vec<String> = self
            .client
            .cmd("SMEMBERS")
            .arg(index_key.as_str())
            .execute()
            .await
            .map_err(|e| ff_core::engine_error::backend_context(
                transport_fk(e),
                "list_workers: SMEMBERS index",
            ))?;
        instance_ids.sort();

        if let Some(after) = args.after.as_ref() {
            let cutoff = after.as_str();
            instance_ids.retain(|id| id.as_str() > cutoff);
        }

        let page_full = instance_ids.len() > limit;
        instance_ids.truncate(limit);

        let mut out: Vec<WorkerInfo> = Vec::with_capacity(instance_ids.len());
        for id_str in &instance_ids {
            let wid = WorkerInstanceId::new(id_str.clone());
            let caps_key = worker_caps_key_ns(ns, &wid);
            let fields: HashMap<String, String> = self
                .client
                .cmd("HGETALL")
                .arg(caps_key.as_str())
                .execute()
                .await
                .map_err(|e| ff_core::engine_error::backend_context(
                    transport_fk(e),
                    "list_workers: HGETALL caps",
                ))?;
            if fields.is_empty() {
                // Index entry outlived the caps hash — TTL race on
                // evict. Log so operators can distinguish this from a
                // dropped worker without digging through keyspace.
                tracing::debug!(
                    instance_id = %id_str,
                    caps_key = %caps_key,
                    "list_workers: caps hash missing for indexed instance (TTL race); skipping"
                );
                continue;
            }
            let worker_id_str = fields.get("worker_id").cloned().unwrap_or_default();
            let lanes_csv = fields.get("lanes_csv").cloned().unwrap_or_default();
            let caps_csv = fields.get("caps_csv").cloned().unwrap_or_default();
            let ttl_ms: u64 = fields
                .get("ttl_ms")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let registered_at_ms: i64 = fields
                .get("registered_at_ms")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let lanes: std::collections::BTreeSet<LaneId> = if lanes_csv.is_empty() {
                Default::default()
            } else {
                lanes_csv
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(LaneId::new)
                    .collect()
            };
            let capabilities: std::collections::BTreeSet<String> = if caps_csv.is_empty() {
                Default::default()
            } else {
                caps_csv
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            };

            out.push(WorkerInfo::new(
                WorkerId::new(worker_id_str),
                wid,
                ns.clone(),
                lanes,
                capabilities,
                TimestampMs::from_millis(registered_at_ms),
                ttl_ms,
                TimestampMs::from_millis(registered_at_ms),
            ));
        }

        let cursor = if page_full {
            out.last().map(|w| w.worker_instance_id.clone())
        } else {
            None
        };
        Ok(ListWorkersResult::new(out, cursor))
    }
}

// ── Helpers for Cluster 2b-A reconcilers ─────────────────────────

/// Verify an execution's index membership against its core-hash state.
/// Returns Ok(true) if consistent, Ok(false) if an inconsistency was
/// logged. Mirrors the pre-routing `index_reconciler::check_execution_index`.
async fn check_execution_index(
    client: &ferriskey::Client,
    partition: &Partition,
    tag: &str,
    idx: &IndexKeys,
    eid_str: &str,
    _lanes: &[LaneId],
) -> Result<bool, ferriskey::Error> {
    let core_key = format!("ff:exec:{}:{}:core", tag, eid_str);

    let fields: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(&core_key)
        .arg("lifecycle_phase")
        .arg("eligibility_state")
        .arg("ownership_state")
        .arg("lane_id")
        .execute()
        .await?;

    if fields.is_empty() || fields[0].is_none() {
        tracing::warn!(
            partition = partition.index,
            hash_tag = %tag,
            execution_id = eid_str,
            "reconcile_execution_index: execution in all_executions but core hash missing"
        );
        return Ok(false);
    }

    let lifecycle = fields[0].as_deref().unwrap_or("");
    let eligibility = fields[1].as_deref().unwrap_or("");
    let ownership = fields[2].as_deref().unwrap_or("");
    let lane_str = fields[3].as_deref().unwrap_or("default");

    let expected_index = match (lifecycle, eligibility, ownership) {
        ("active", _, "leased") => "active",
        ("runnable", "eligible_now", _) => "eligible",
        ("runnable", "not_eligible_until_time", _) => "delayed",
        ("runnable", "blocked_by_dependencies", _) => "blocked:dependencies",
        ("runnable", "blocked_by_budget", _) => "blocked:budget",
        ("runnable", "blocked_by_quota", _) => "blocked:quota",
        ("runnable", "blocked_by_route", _) => "blocked:route",
        ("runnable", "blocked_by_operator", _) => "blocked:operator",
        ("suspended", _, _) => "suspended",
        ("terminal", _, _) => "terminal",
        _ => "unknown",
    };

    if expected_index == "unknown" {
        return Ok(true);
    }

    let lane = LaneId::new(lane_str);
    let expected_key = match expected_index {
        "active" => idx.lane_active(&lane),
        "eligible" => idx.lane_eligible(&lane),
        "delayed" => idx.lane_delayed(&lane),
        "blocked:dependencies" => idx.lane_blocked_dependencies(&lane),
        "blocked:budget" => idx.lane_blocked_budget(&lane),
        "blocked:quota" => idx.lane_blocked_quota(&lane),
        "blocked:route" => idx.lane_blocked_route(&lane),
        "blocked:operator" => idx.lane_blocked_operator(&lane),
        "suspended" => idx.lane_suspended(&lane),
        "terminal" => idx.lane_terminal(&lane),
        _ => return Ok(true),
    };

    let score: Option<String> = client
        .cmd("ZSCORE")
        .arg(&expected_key)
        .arg(eid_str)
        .execute()
        .await?;

    if score.is_none() {
        tracing::warn!(
            partition = partition.index,
            hash_tag = %tag,
            execution_id = eid_str,
            expected_index,
            expected_key = expected_key.as_str(),
            lifecycle,
            eligibility,
            ownership,
            "reconcile_execution_index: execution missing from expected index"
        );
        return Ok(false);
    }

    Ok(true)
}

/// Reconcile one budget (non-resetting only). Returns Ok(true) if the
/// breach marker flipped.
async fn reconcile_one_budget(
    client: &ferriskey::Client,
    tag: &str,
    policies_key: &str,
    budget_id: &str,
    now_ms: u64,
) -> Result<bool, ferriskey::Error> {
    let def_key = format!("ff:budget:{}:{}", tag, budget_id);
    let usage_key = format!("ff:budget:{}:{}:usage", tag, budget_id);
    let limits_key = format!("ff:budget:{}:{}:limits", tag, budget_id);

    // Fail-closed on transport errors (see pre-routing scanner rustdoc).
    let def_raw: Vec<String> = client.cmd("HGETALL").arg(&def_key).execute().await?;
    if def_raw.is_empty() {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(policies_key)
            .arg(budget_id)
            .execute()
            .await
            .unwrap_or(None);
        return Ok(false);
    }

    let def_map = pairs_to_map(&def_raw);
    let reset_interval = def_map.get("reset_interval_ms").copied();
    if let Some(ri) = reset_interval
        && !ri.is_empty()
        && ri != "0"
    {
        return Ok(false);
    }

    let usage_raw: Vec<String> = client.cmd("HGETALL").arg(&usage_key).execute().await?;
    let limits_raw: Vec<String> = client.cmd("HGETALL").arg(&limits_key).execute().await?;
    if limits_raw.is_empty() {
        return Ok(false);
    }

    let usage = pairs_to_map(&usage_raw);
    let limits = pairs_to_map(&limits_raw);

    let mut any_breached = false;
    for (field, limit_str) in &limits {
        let dim = match field.strip_prefix("hard:") {
            Some(d) => d,
            None => continue,
        };
        let limit: i64 = limit_str.parse().unwrap_or(i64::MAX);
        if limit <= 0 {
            continue;
        }
        let current: i64 = usage.get(dim).and_then(|v| v.parse().ok()).unwrap_or(0);
        if current > limit {
            any_breached = true;
            break;
        }
    }

    let currently_breached = usage.contains_key("breached_at");

    if any_breached && !currently_breached {
        let _: () = client
            .cmd("HSET")
            .arg(&usage_key)
            .arg("breached_at")
            .arg(now_ms.to_string().as_str())
            .execute()
            .await?;
        tracing::info!(budget_id, "reconcile_budget_counters: marked budget as breached");
    } else if !any_breached && currently_breached {
        let _: u32 = client
            .cmd("HDEL")
            .arg(&usage_key)
            .arg("breached_at")
            .execute()
            .await?;
        tracing::info!(budget_id, "reconcile_budget_counters: cleared budget breach");
    }

    Ok(any_breached != currently_breached)
}

/// Reconcile one quota policy. Returns Ok(true) if anything was
/// trimmed / counter-corrected.
async fn reconcile_one_quota(
    client: &ferriskey::Client,
    tag: &str,
    quota_id: &str,
    now_ms: u64,
) -> Result<bool, ferriskey::Error> {
    let mut did_work = false;
    let def_key = format!("ff:quota:{}:{}", tag, quota_id);

    let window_secs: Option<String> = client
        .cmd("HGET")
        .arg(&def_key)
        .arg("requests_per_window_seconds")
        .execute()
        .await?;

    if let Some(ref ws) = window_secs
        && let Ok(secs) = ws.parse::<u64>()
        && secs > 0
    {
        let window_ms = secs * 1000;
        let window_key = format!("ff:quota:{}:{}:window:requests_per_window", tag, quota_id);
        let cutoff = now_ms.saturating_sub(window_ms);

        let removed: u32 = client
            .cmd("ZREMRANGEBYSCORE")
            .arg(&window_key)
            .arg("-inf")
            .arg(cutoff.to_string().as_str())
            .execute()
            .await
            .unwrap_or(0);

        if removed > 0 {
            did_work = true;
            tracing::debug!(quota_id, removed,
                "reconcile_quota_counters: trimmed expired window entries");
        }
    }

    let concurrency_cap: Option<String> = client
        .cmd("HGET")
        .arg(&def_key)
        .arg("active_concurrency_cap")
        .execute()
        .await?;

    if let Some(ref cap_str) = concurrency_cap
        && let Ok(cap) = cap_str.parse::<u64>()
        && cap > 0
    {
        let counter_key = format!("ff:quota:{}:{}:concurrency", tag, quota_id);
        let admitted_set_key = format!("ff:quota:{}:{}:admitted_set", tag, quota_id);

        let mut live_count: u64 = 0;
        let mut cursor = "0".to_string();
        loop {
            let result: ferriskey::Value = client
                .cmd("SSCAN")
                .arg(&admitted_set_key)
                .arg(cursor.as_str())
                .arg("COUNT")
                .arg("100")
                .execute()
                .await?;

            let (next_cursor, members) = parse_sscan_tuple(&result);

            for eid in &members {
                let guard_key = format!("ff:quota:{}:{}:admitted:{}", tag, quota_id, eid);
                let exists: bool = client.exists(&guard_key).await.unwrap_or(false);
                if exists {
                    live_count += 1;
                } else {
                    let _: () = client
                        .cmd("SREM")
                        .arg(&admitted_set_key)
                        .arg(eid.as_str())
                        .execute()
                        .await
                        .unwrap_or_default();
                }
            }

            cursor = next_cursor;
            if cursor == "0" {
                break;
            }
        }

        let stored: Option<String> = client.cmd("GET").arg(&counter_key).execute().await?;
        let stored_count: i64 = stored.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);

        if stored_count != live_count as i64 {
            let _: () = client
                .cmd("SET")
                .arg(&counter_key)
                .arg(live_count.to_string().as_str())
                .execute()
                .await?;
            tracing::info!(
                quota_id,
                stored = stored_count,
                actual = live_count,
                "reconcile_quota_counters: corrected concurrency counter drift"
            );
            did_work = true;
        }
    }

    Ok(did_work)
}

fn pairs_to_map(flat: &[String]) -> std::collections::HashMap<&str, &str> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i + 1 < flat.len() {
        map.insert(flat[i].as_str(), flat[i + 1].as_str());
        i += 2;
    }
    map
}

/// Parse an `SSCAN` reply `[cursor, [member, ...]]`. Returns
/// `("0", vec![])` on any unexpected shape (matches pre-routing
/// scanner helper behaviour).
fn parse_sscan_tuple(val: &ferriskey::Value) -> (String, Vec<String>) {
    let arr = match val {
        ferriskey::Value::Array(a) if a.len() >= 2 => a,
        _ => return ("0".to_string(), vec![]),
    };

    let cursor = match &arr[0] {
        Ok(ferriskey::Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
        _ => return ("0".to_string(), vec![]),
    };

    let mut members = Vec::new();
    match &arr[1] {
        Ok(ferriskey::Value::Array(inner)) => {
            for item in inner {
                if let Ok(ferriskey::Value::BulkString(b)) = item {
                    members.push(String::from_utf8_lossy(b).into_owned());
                }
            }
        }
        Ok(ferriskey::Value::Set(inner)) => {
            for item in inner {
                if let ferriskey::Value::BulkString(b) = item {
                    members.push(String::from_utf8_lossy(b).into_owned());
                }
            }
        }
        _ => {}
    }

    (cursor, members)
}

/// Parse `SSCAN` into a strict `(cursor, members)` tuple — variant
/// used by the index reconciler which treats an unparsable reply as a
/// hard transport error rather than silently ending the scan (which
/// would hide index drift). Any unexpected outer shape, cursor
/// variant, or member variant maps to `EngineError::Transport`.
fn parse_sscan_response(
    val: &ferriskey::Value,
) -> Result<(String, Vec<String>), EngineError> {
    fn transport(detail: &'static str) -> EngineError {
        EngineError::Transport {
            backend: "valkey",
            source: format!("parse_sscan_response: {detail}").into(),
        }
    }

    let arr = match val {
        ferriskey::Value::Array(a) if a.len() >= 2 => a,
        _ => return Err(transport("expected Array(len >= 2) reply")),
    };

    let cursor = match &arr[0] {
        Ok(ferriskey::Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ferriskey::Value::SimpleString(s)) => s.clone(),
        _ => return Err(transport("cursor element has unexpected variant")),
    };

    let mut members = Vec::new();
    match &arr[1] {
        Ok(ferriskey::Value::Array(inner)) => {
            for item in inner {
                match item {
                    Ok(ferriskey::Value::BulkString(b)) => {
                        members.push(String::from_utf8_lossy(b).into_owned());
                    }
                    _ => {
                        return Err(transport(
                            "member element has unexpected variant",
                        ));
                    }
                }
            }
        }
        Ok(ferriskey::Value::Set(inner)) => {
            for item in inner {
                match item {
                    ferriskey::Value::BulkString(b) => {
                        members.push(String::from_utf8_lossy(b).into_owned());
                    }
                    _ => {
                        return Err(transport(
                            "member element has unexpected variant",
                        ));
                    }
                }
            }
        }
        _ => {
            return Err(transport(
                "members element has unexpected variant (expected Array/Set)",
            ));
        }
    }

    Ok((cursor, members))
}

/// Aggregate partition-level lease-history stream key. RFC-019 Stage A
/// subscription consumer; Stage B wires the producer side.
///
/// Format: `ff:part:{fp:N}:lease_history`. The hash-tag matches the
/// partition family prefix so a future writer (Lua function emitting
/// `XADD ff:part:{fp:N}:lease_history ...`) routes to the same slot as
/// the per-execution state it mirrors.
pub fn partition_lease_history_key(partition: &Partition) -> String {
    format!("ff:part:{}:lease_history", partition.hash_tag())
}

/// Aggregate partition-level signal-delivery stream key. RFC-019
/// Stage B (#310). Format: `ff:part:{fp:N}:signal_delivery`. Producer
/// XADD lives in `lua/signal.lua::ff_deliver_signal` at KEYS[15].
pub fn partition_signal_delivery_key(partition: &Partition) -> String {
    format!("ff:part:{}:signal_delivery", partition.hash_tag())
}

/// Build the per-subscriber `FilterGate` for `subscribe_lease_history` /
/// `subscribe_signal_delivery`. Returns `None` when the filter is a
/// no-op (short-circuit the per-event HGET path entirely). Shares the
/// exact shape used by `subscribe_completions_filtered` (#122) —
/// consumers sharing a Valkey keyspace see only their own events.
fn build_filter_gate(
    backend: &ValkeyBackend,
    filter: &ff_core::backend::ScannerFilter,
) -> Option<crate::completion::FilterGate> {
    if filter.is_noop() {
        return None;
    }
    Some(crate::completion::FilterGate {
        client: backend.client.clone(),
        partition_config: backend.partition_config,
        filter: filter.clone(),
    })
}

/// Valkey-side `subscribe_signal_delivery` — mirror of
/// `subscribe_lease_history_impl`, swapped to the signal-delivery
/// stream key + typed `SignalDeliveryEvent` decoding.
async fn subscribe_signal_delivery_impl(
    client: &ferriskey::Client,
    start_cursor: ff_core::stream_subscribe::StreamCursor,
    filter_gate: Option<crate::completion::FilterGate>,
) -> Result<ff_core::stream_events::SignalDeliverySubscription, EngineError> {
    use ff_core::stream_events::SignalDeliveryEvent;
    use ff_core::stream_subscribe::{decode_valkey_cursor, encode_valkey_cursor};
    use tokio_stream::wrappers::ReceiverStream;

    let start = decode_valkey_cursor(&start_cursor)
        .map_err(|msg| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: msg.to_string(),
        })?;

    let tail_client = client
        .duplicate_connection()
        .await
        .map_err(ff_script::error::ScriptError::from)
        .map_err(transport_script)?;

    let partition = Partition {
        family: PartitionFamily::Flow,
        index: 0,
    };
    let stream_key = partition_signal_delivery_key(&partition);

    let (tx, rx) = tokio::sync::mpsc::channel::<
        Result<SignalDeliveryEvent, EngineError>,
    >(64);

    tokio::spawn(async move {
        let mut last_cursor = start_cursor.clone();
        let mut last_id = match start {
            None => "$".to_string(),
            Some((ms, seq)) => format!("{ms}-{seq}"),
        };

        loop {
            let raw = tail_client
                .cmd("XREAD")
                .arg("COUNT")
                .arg(128_u64)
                .arg("BLOCK")
                .arg(5_000_u64)
                .arg("STREAMS")
                .arg(stream_key.as_str())
                .arg(last_id.as_str())
                .execute::<ferriskey::Value>()
                .await;

            let value = match raw {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "subscribe_signal_delivery: XREAD BLOCK error, terminating stream"
                    );
                    let _ = tx
                        .send(Err(EngineError::StreamDisconnected {
                            cursor: last_cursor.clone(),
                        }))
                        .await;
                    return;
                }
            };

            let entries = match parse_lease_history_xread(&value, &stream_key) {
                Ok(e) => e,
                Err(detail) => {
                    let _ = tx
                        .send(Err(EngineError::Validation {
                            kind: ValidationKind::Corruption,
                            detail,
                        }))
                        .await;
                    continue;
                }
            };

            for (id_ms, id_seq, payload) in entries {
                let cursor = encode_valkey_cursor(id_ms, id_seq);
                last_cursor = cursor.clone();
                last_id = format!("{id_ms}-{id_seq}");
                let fields = decode_nul_field_blob(&payload);
                let event = match decode_signal_delivery_event(
                    &fields,
                    cursor,
                    id_ms,
                ) {
                    Ok(ev) => ev,
                    Err(detail) => {
                        let _ = tx
                            .send(Err(EngineError::Validation {
                                kind: ValidationKind::Corruption,
                                detail,
                            }))
                            .await;
                        continue;
                    }
                };
                // #282: apply per-subscriber `ScannerFilter` via the
                // shared FilterGate — matches
                // `subscribe_completions_filtered`. On non-admit the
                // event is dropped silently; other consumers sharing
                // the keyspace receive their own gated copy.
                if let Some(ref g) = filter_gate
                    && !g.admits(&event.execution_id).await
                {
                    continue;
                }
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
        }
    });

    Ok(Box::pin(ReceiverStream::new(rx)))
}

/// Background loop that pumps `XREAD BLOCK` entries into an mpsc for
/// the `ReceiverStream` returned to the caller.
async fn subscribe_lease_history_impl(
    client: &ferriskey::Client,
    start_cursor: ff_core::stream_subscribe::StreamCursor,
    filter_gate: Option<crate::completion::FilterGate>,
) -> Result<ff_core::stream_events::LeaseHistorySubscription, EngineError> {
    use ff_core::stream_events::LeaseHistoryEvent;
    use ff_core::stream_subscribe::{decode_valkey_cursor, encode_valkey_cursor};
    use tokio_stream::wrappers::ReceiverStream;

    // Validate the cursor up front so caller bugs surface loudly at
    // subscribe-time rather than as a silent first-event error.
    let start = decode_valkey_cursor(&start_cursor)
        .map_err(|msg| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: msg.to_string(),
        })?;

    // Dedicated socket for the blocking XREAD so the multiplexed FCALL
    // connection never head-of-line-blocks on a long-BLOCK call (same
    // rationale as `tail_stream_impl`, issue #204).
    let tail_client = client
        .duplicate_connection()
        .await
        .map_err(ff_script::error::ScriptError::from)
        .map_err(transport_script)?;

    // Stage A is single-partition per backend instance — the
    // ValkeyBackend's configured `partition_config` still maps across
    // the 256-slot execution/flow family, but this subscription tails
    // partition 0 as the canonical aggregate key. Cross-partition
    // consumers merge consumer-side.
    let partition = Partition {
        family: PartitionFamily::Flow,
        index: 0,
    };
    let stream_key = partition_lease_history_key(&partition);

    // Bounded mpsc — if the consumer stalls, `send` awaits; the XREAD
    // loop stops pulling new entries until capacity frees up. This is
    // the owner-adjudicated "pull via Stream" backpressure shape
    // (RFC-019 §Open Questions #3).
    let (tx, rx) = tokio::sync::mpsc::channel::<
        Result<LeaseHistoryEvent, EngineError>,
    >(64);

    tokio::spawn(async move {
        let mut last_cursor = start_cursor.clone();
        // `last_id` is the XREAD exclusive cursor. Empty cursor →
        // start from `$` (tail-from-now); decoded cursor → resume
        // strictly after that (ms, seq) pair.
        let mut last_id = match start {
            None => "$".to_string(),
            Some((ms, seq)) => format!("{ms}-{seq}"),
        };

        loop {
            let raw = tail_client
                .cmd("XREAD")
                .arg("COUNT")
                .arg(128_u64)
                .arg("BLOCK")
                .arg(5_000_u64)
                .arg("STREAMS")
                .arg(stream_key.as_str())
                .arg(last_id.as_str())
                .execute::<ferriskey::Value>()
                .await;

            let value = match raw {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "subscribe_lease_history: XREAD BLOCK error, terminating stream"
                    );
                    let _ = tx
                        .send(Err(EngineError::StreamDisconnected {
                            cursor: last_cursor.clone(),
                        }))
                        .await;
                    return;
                }
            };

            let entries = match parse_lease_history_xread(&value, &stream_key) {
                Ok(e) => e,
                Err(detail) => {
                    let _ = tx
                        .send(Err(EngineError::Validation {
                            kind: ValidationKind::Corruption,
                            detail,
                        }))
                        .await;
                    continue;
                }
            };

            for (id_ms, id_seq, payload) in entries {
                let cursor = encode_valkey_cursor(id_ms, id_seq);
                last_cursor = cursor.clone();
                last_id = format!("{id_ms}-{id_seq}");
                let fields = decode_nul_field_blob(&payload);
                let event = match decode_lease_history_event(
                    &fields,
                    cursor,
                    id_ms,
                ) {
                    Ok(ev) => ev,
                    Err(detail) => {
                        let _ = tx
                            .send(Err(EngineError::Validation {
                                kind: ValidationKind::Corruption,
                                detail,
                            }))
                            .await;
                        continue;
                    }
                };
                // #282: per-subscriber filter gate; see sibling
                // comment in subscribe_signal_delivery_impl.
                if let Some(ref g) = filter_gate
                    && !g.admits(event.execution_id()).await
                {
                    continue;
                }
                if tx.send(Ok(event)).await.is_err() {
                    // Consumer dropped the stream.
                    return;
                }
            }
        }
    });

    Ok(Box::pin(ReceiverStream::new(rx)))
}

/// Parse an `XREAD BLOCK STREAMS <key> <id>` reply for a single
/// stream, returning `(ms, seq, fields_as_bytes)` per entry.
///
/// Stage A forwards the raw field map as a flat `FIELD\0VALUE\0...`
/// byte blob — consumers parse family-specific semantics Stage B will
/// document. Returns `Ok(vec![])` on `Nil` (BLOCK timeout / no new
/// entries).
fn parse_lease_history_xread(
    raw: &ferriskey::Value,
    stream_key: &str,
) -> Result<Vec<(u64, u64, bytes::Bytes)>, String> {
    use ferriskey::Value;
    let entries_val = match raw {
        Value::Nil => return Ok(Vec::new()),
        // RESP3: Map({stream_key: Map|Array(entries)})
        Value::Map(m) => {
            let mut found = None;
            for (k, v) in m {
                let key_ok = matches!(k, Value::BulkString(b) if b.as_ref() == stream_key.as_bytes())
                    || matches!(k, Value::SimpleString(s) if s == stream_key);
                if key_ok {
                    found = Some(v.clone());
                    break;
                }
            }
            match found {
                Some(v) => v,
                None => return Ok(Vec::new()),
            }
        }
        // RESP2: Array([[stream_key, [[id, fields], ...]], ...])
        Value::Array(arr) => {
            let mut found = None;
            for entry in arr {
                let Ok(Value::Array(pair)) = entry.as_ref() else {
                    continue;
                };
                if pair.len() != 2 {
                    continue;
                }
                let matches = match pair[0].as_ref() {
                    Ok(Value::BulkString(b)) => b.as_ref() == stream_key.as_bytes(),
                    Ok(Value::SimpleString(s)) => s == stream_key,
                    _ => false,
                };
                if !matches {
                    continue;
                }
                if let Ok(v) = pair[1].as_ref() {
                    found = Some(v.clone());
                }
                break;
            }
            match found {
                Some(v) => v,
                None => return Ok(Vec::new()),
            }
        }
        _ => return Err("subscribe_lease_history: unexpected XREAD reply shape".into()),
    };

    let entries = match entries_val {
        Value::Array(arr) => arr,
        Value::Map(m) => {
            // Each entry = (id, fields_value). Normalise to Array.
            m.into_iter()
                .map(|(k, v)| Ok::<Value, _>(Value::Array(vec![Ok(k), Ok(v)])))
                .collect()
        }
        _ => return Err("subscribe_lease_history: expected array of entries".into()),
    };

    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry
            .as_ref()
            .map_err(|e| format!("subscribe_lease_history: entry error: {e}"))?
            .clone();
        let pair = match entry {
            Value::Array(p) => p,
            _ => return Err("subscribe_lease_history: entry not an array".into()),
        };
        if pair.len() != 2 {
            return Err("subscribe_lease_history: entry pair != 2".into());
        }
        let id_str = match pair[0]
            .as_ref()
            .map_err(|e| format!("subscribe_lease_history: id value: {e}"))?
        {
            Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
            Value::SimpleString(s) => s.clone(),
            _ => return Err("subscribe_lease_history: id not a string".into()),
        };
        let (ms, seq) = parse_stream_id(&id_str)
            .ok_or_else(|| format!("subscribe_lease_history: bad stream id {id_str:?}"))?;

        // Fields: array of alternating [field, value, field, value, …]
        // or map; flatten to a NUL-delimited bytes blob for Stage A
        // opaque payload.
        let fields_val = pair[1]
            .as_ref()
            .map_err(|e| format!("subscribe_lease_history: fields: {e}"))?
            .clone();
        let payload = encode_fields_blob(&fields_val);
        out.push((ms, seq, payload));
    }
    Ok(out)
}

fn parse_stream_id(s: &str) -> Option<(u64, u64)> {
    let (ms, seq) = s.split_once('-')?;
    Some((ms.parse().ok()?, seq.parse().ok()?))
}

/// Serialise an XREAD entry's field map to a flat NUL-delimited bytes
/// blob. Stage A leaves the schema opaque; Stage B will publish a
/// family-specific Serde shape.
fn encode_fields_blob(v: &ferriskey::Value) -> bytes::Bytes {
    use ferriskey::Value;
    let mut buf: Vec<u8> = Vec::new();
    fn push_val(buf: &mut Vec<u8>, v: &Value) {
        match v {
            Value::BulkString(b) => buf.extend_from_slice(b),
            Value::SimpleString(s) => buf.extend_from_slice(s.as_bytes()),
            Value::Int(i) => buf.extend_from_slice(i.to_string().as_bytes()),
            _ => {}
        }
    }
    match v {
        Value::Array(arr) => {
            // ferriskey's XREAD adapter emits `FieldShape::Pairs`:
            // an outer Array where each element is an inner
            // Array([field, value]). Fall through to the flat RESP2
            // shape `[k, v, k, v]` if the first element is not a
            // pair array.
            let is_pairs = arr
                .first()
                .and_then(|r| r.as_ref().ok())
                .map(|v| matches!(v, Value::Array(_)))
                .unwrap_or(false);
            if is_pairs {
                for pair in arr {
                    let Ok(Value::Array(inner)) = pair.as_ref() else {
                        continue;
                    };
                    if inner.len() < 2 {
                        continue;
                    }
                    if let (Ok(k), Ok(vv)) = (inner[0].as_ref(), inner[1].as_ref()) {
                        push_val(&mut buf, k);
                        buf.push(0);
                        push_val(&mut buf, vv);
                        buf.push(0);
                    }
                }
            } else {
                let mut it = arr.iter();
                while let (Some(k), Some(vv)) = (it.next(), it.next()) {
                    if let (Ok(k), Ok(vv)) = (k.as_ref(), vv.as_ref()) {
                        push_val(&mut buf, k);
                        buf.push(0);
                        push_val(&mut buf, vv);
                        buf.push(0);
                    }
                }
            }
        }
        Value::Map(m) => {
            for (k, vv) in m {
                push_val(&mut buf, k);
                buf.push(0);
                push_val(&mut buf, vv);
                buf.push(0);
            }
        }
        _ => {}
    }
    bytes::Bytes::from(buf)
}

/// Parse the NUL-delimited `k\0v\0k\0v\0…` payload blob
/// `parse_lease_history_xread` produces back into a field map.
///
/// Non-UTF-8 keys/values are lossy-decoded — Lua producers only emit
/// ASCII field names + text values (uuids, ms timestamps, enum strings).
fn decode_nul_field_blob(
    blob: &bytes::Bytes,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut it = blob.split(|b| *b == 0u8);
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        if k.is_empty() && v.is_empty() {
            // Trailing NUL pair after the last entry.
            continue;
        }
        let k = String::from_utf8_lossy(k).into_owned();
        let v = String::from_utf8_lossy(v).into_owned();
        out.insert(k, v);
    }
    out
}

/// Decode a parsed `lease_history` XREAD entry into a
/// `LeaseHistoryEvent`. Producer shapes live in `lua/lease.lua`,
/// `lua/execution.lua`, `lua/helpers.lua` — acquired / renewed /
/// expired / reclaimed / revoked.
fn decode_lease_history_event(
    fields: &std::collections::HashMap<String, String>,
    cursor: ff_core::stream_subscribe::StreamCursor,
    id_ms: u64,
) -> Result<ff_core::stream_events::LeaseHistoryEvent, String> {
    use ff_core::stream_events::LeaseHistoryEvent;
    use ff_core::types::{ExecutionId, LeaseId, WorkerInstanceId};

    let event_name = fields
        .get("event")
        .ok_or_else(|| "lease_history: missing `event` field".to_string())?
        .as_str();

    let at = {
        // Prefer the Lua-stamped `ts`; fall back to the stream id's ms
        // component. `ts` may carry a floating-point formatting artifact
        // from `tostring(now_ms)` on some Lua versions, so parse loosely.
        let ts_ms = fields
            .get("ts")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(id_ms as i64);
        TimestampMs::from_millis(ts_ms)
    };

    let execution_id = fields
        .get("execution_id")
        .ok_or_else(|| "lease_history: missing `execution_id`".to_string())
        .and_then(|s| {
            ExecutionId::parse(s)
                .map_err(|e| format!("lease_history: bad execution_id `{s}`: {e}"))
        })?;

    let parse_lease = |key: &str| -> Option<LeaseId> {
        fields
            .get(key)
            .filter(|s| !s.is_empty())
            .and_then(|s| LeaseId::parse(s).ok())
    };
    let worker_instance = fields
        .get("worker_instance_id")
        .filter(|s| !s.is_empty())
        .map(|s| WorkerInstanceId::new(s.clone()));

    let event = match event_name {
        "acquired" => LeaseHistoryEvent::Acquired {
            cursor,
            execution_id,
            lease_id: parse_lease("lease_id"),
            worker_instance_id: worker_instance,
            at,
        },
        "renewed" => LeaseHistoryEvent::Renewed {
            cursor,
            execution_id,
            lease_id: parse_lease("lease_id"),
            worker_instance_id: worker_instance,
            at,
        },
        "expired" => LeaseHistoryEvent::Expired {
            cursor,
            execution_id,
            lease_id: parse_lease("lease_id"),
            prev_owner: worker_instance,
            at,
        },
        "reclaimed" => LeaseHistoryEvent::Reclaimed {
            cursor,
            execution_id,
            new_lease_id: parse_lease("new_lease_id").or_else(|| parse_lease("lease_id")),
            new_owner: worker_instance,
            at,
        },
        "revoked" => LeaseHistoryEvent::Revoked {
            cursor,
            execution_id,
            lease_id: parse_lease("lease_id"),
            revoked_by: fields
                .get("reason")
                .cloned()
                .unwrap_or_else(|| "operator".to_string()),
            at,
        },
        other => {
            return Err(format!(
                "lease_history: unknown event variant `{other}`"
            ));
        }
    };
    Ok(event)
}

/// Decode a parsed `signal_delivery` XREAD entry. Producer is
/// `lua/signal.lua::ff_deliver_signal` (see KEYS[15]).
fn decode_signal_delivery_event(
    fields: &std::collections::HashMap<String, String>,
    cursor: ff_core::stream_subscribe::StreamCursor,
    id_ms: u64,
) -> Result<ff_core::stream_events::SignalDeliveryEvent, String> {
    use ff_core::stream_events::{SignalDeliveryEffect, SignalDeliveryEvent};
    use ff_core::types::{ExecutionId, SignalId, WaitpointId};

    let execution_id = fields
        .get("execution_id")
        .ok_or_else(|| "signal_delivery: missing `execution_id`".to_string())
        .and_then(|s| {
            ExecutionId::parse(s)
                .map_err(|e| format!("signal_delivery: bad execution_id `{s}`: {e}"))
        })?;

    let signal_id = fields
        .get("signal_id")
        .ok_or_else(|| "signal_delivery: missing `signal_id`".to_string())
        .and_then(|s| {
            SignalId::parse(s).map_err(|e| format!("signal_delivery: bad signal_id `{s}`: {e}"))
        })?;

    let waitpoint_id = fields
        .get("waitpoint_id")
        .filter(|s| !s.is_empty())
        .and_then(|s| WaitpointId::parse(s).ok());

    let source_identity = fields
        .get("source_identity")
        .filter(|s| !s.is_empty())
        .cloned();

    let effect = fields
        .get("effect")
        .map(|s| SignalDeliveryEffect::from_wire(s))
        .unwrap_or(SignalDeliveryEffect::Satisfied);

    let at = fields
        .get("delivered_at_ms")
        .and_then(|s| s.parse::<i64>().ok())
        .map(TimestampMs::from_millis)
        .unwrap_or_else(|| TimestampMs::from_millis(id_ms as i64));

    Ok(SignalDeliveryEvent::new(
        cursor,
        execution_id,
        signal_id,
        waitpoint_id,
        source_identity,
        effect,
        at,
    ))
}

/// Detect Valkey errors indicating the Lua function library is not
/// loaded (post-failover cold replica, etc.). Relocated from
/// `ff-server::server::is_function_not_loaded` in Stage E2 alongside
/// `cancel_flow_header`'s FCALL-with-reload wrapper.
fn is_function_not_loaded_fk(e: &ferriskey::Error) -> bool {
    if matches!(e.kind(), ferriskey::ErrorKind::NoScriptError) {
        return true;
    }
    e.detail()
        .map(|d| {
            d.contains("Function not loaded")
                || d.contains("No matching function")
                || d.contains("function not found")
        })
        .unwrap_or(false)
        || e.to_string().contains("Function not loaded")
}

// ── Stream read implementations (RFC-012 Stage 1c tranche-4; #87) ──

/// Valkey XRANGE-backed stream reader. Mirrors the free-function body
/// that previously lived in `ff_sdk::task::read_stream` — moved here so
/// the `ferriskey::Client` parameter no longer leaks through the SDK's
/// public surface.
#[cfg(feature = "streaming")]
#[tracing::instrument(
    name = "ff.read_stream",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %execution_id,
        attempt_index = %attempt_index,
    )
)]
async fn read_stream_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    from: ff_core::contracts::StreamCursor,
    to: ff_core::contracts::StreamCursor,
    count_limit: u64,
) -> Result<ff_core::contracts::StreamFrames, EngineError> {
    use ff_core::contracts::{ReadFramesArgs, ReadFramesResult};

    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let keys = ff_script::functions::stream::StreamOpKeys { ctx: &ctx };

    // Lower the opaque cursor to the Valkey XRANGE marker at the
    // adapter edge. `ReadFramesArgs` is string-typed — the Lua ABI is
    // untouched.
    let args = ReadFramesArgs {
        execution_id: execution_id.clone(),
        attempt_index,
        from_id: from.into_wire_string(),
        to_id: to.into_wire_string(),
        count_limit,
    };

    let ReadFramesResult::Frames(f) =
        ff_script::functions::stream::ff_read_attempt_stream(client, &keys, &args)
            .await
            .map_err(transport_script)?;
    Ok(f)
}

/// Valkey XREAD BLOCK-backed stream tailer. See
/// [`read_stream_impl`] for the migration rationale.
///
/// Issues `XREAD BLOCK` on a **dedicated** ferriskey connection
/// obtained via [`ferriskey::Client::duplicate_connection`] and
/// drops that connection on return. This keeps the blocking read
/// off the shared multiplex socket so concurrent non-blocking
/// operations on the main client (e.g. `XADD` from
/// `append_frame`) never wait on head-of-line blocking. Fixes
/// GitHub issue #204.
#[cfg(feature = "streaming")]
#[tracing::instrument(
    name = "ff.tail_stream",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %execution_id,
        attempt_index = %attempt_index,
    )
)]
#[allow(clippy::too_many_arguments)]
async fn tail_stream_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
    after: ff_core::contracts::StreamCursor,
    block_ms: u64,
    count_limit: u64,
    visibility: TailVisibility,
) -> Result<ff_core::contracts::StreamFrames, EngineError> {
    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let stream_key = ctx.stream(attempt_index);
    let stream_meta_key = ctx.stream_meta(attempt_index);

    // Dedicated socket for the blocking read — dropped when this
    // function returns. Dialling a second connection fails only on
    // genuine transport problems (refused, TLS handshake, auth);
    // those are classified the same as any other transport error
    // via `transport_script`.
    let tail_client = client
        .duplicate_connection()
        .await
        .map_err(ff_script::error::ScriptError::from)
        .map_err(transport_script)?;

    let mut frames = ff_script::stream_tail::xread_block(
        &tail_client,
        &stream_key,
        &stream_meta_key,
        after.to_wire(),
        block_ms,
        count_limit,
    )
    .await
    .map_err(transport_script)?;

    // RFC-015 §6.1 server-side visibility filter. Applied post-XREAD
    // because `xread_block` is a Valkey primitive (not a Lua Function);
    // the filter is a cheap `mode` field check on each returned entry.
    // Frames written pre-RFC-015 have no `mode` field → treated as
    // `durable` (RFC-015 §8.1).
    if matches!(visibility, TailVisibility::ExcludeBestEffort) {
        frames.frames.retain(|f| {
            let mode = f.fields.get("mode").map(String::as_str).unwrap_or("durable");
            mode != "best_effort"
        });
    }

    Ok(frames)
}

// ── RFC-015 §6.3: read_summary ──
#[cfg(feature = "streaming")]
#[tracing::instrument(
    name = "ff.read_summary",
    skip_all,
    fields(
        backend = "valkey",
        execution_id = %execution_id,
        attempt_index = %attempt_index,
    )
)]
async fn read_summary_impl(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    attempt_index: AttemptIndex,
) -> Result<Option<SummaryDocument>, EngineError> {
    let partition = execution_partition(execution_id, partition_config);
    let ctx = ExecKeyContext::new(&partition, execution_id);
    let summary_key = ctx.stream_summary(attempt_index);

    // HGETALL decodes differently by protocol:
    //   - RESP2 → `Value::Array` of alternating keys/values.
    //   - RESP3 → `Value::Map` of `(key, value)` pairs.
    // ferriskey's default on Valkey 7.2 negotiates RESP3, so we must
    // accept both shapes (v0.6.0 regression: Array-only match always
    // returned `Ok(None)` on RESP3).
    let raw: ferriskey::Value = client
        .cmd("HGETALL")
        .arg(&summary_key)
        .execute()
        .await
        .map_err(transport_fk)?;

    fn value_to_string(v: ferriskey::Value) -> Option<String> {
        match v {
            ferriskey::Value::BulkString(b) => {
                Some(String::from_utf8_lossy(&b).into_owned())
            }
            ferriskey::Value::SimpleString(s) => Some(s),
            _ => None,
        }
    }

    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    match raw {
        ferriskey::Value::Array(arr) => {
            let mut it = arr.into_iter().filter_map(|r| r.ok().and_then(value_to_string));
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                map.insert(k, v);
            }
        }
        ferriskey::Value::Map(pairs) => {
            for (k, v) in pairs {
                if let (Some(k), Some(v)) = (value_to_string(k), value_to_string(v)) {
                    map.insert(k, v);
                }
            }
        }
        _ => {}
    }
    if map.is_empty() {
        return Ok(None);
    }
    let document_json = map
        .remove("document")
        .unwrap_or_else(|| "{}".to_owned())
        .into_bytes();
    let version: u64 = map
        .get("version")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if version == 0 {
        // No delta has been applied yet — the Hash only had metadata
        // stubs (shouldn't happen on the v0.6 write path, but a defensive
        // None keeps the `Option<SummaryDocument>` contract clean).
        return Ok(None);
    }
    let patch_kind = match map.get("patch_kind").map(String::as_str) {
        Some("json-merge-patch") => PatchKind::JsonMergePatch,
        _ => PatchKind::JsonMergePatch,
    };
    let last_updated_ms: u64 = map
        .get("last_updated_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let first_applied_ms: u64 = map
        .get("first_applied_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(last_updated_ms);

    Ok(Some(SummaryDocument::new(
        document_json,
        version,
        patch_kind,
        last_updated_ms,
        first_applied_ms,
    )))
}

// ── PR-7b Cluster 2b-B — flow_projector + retention_trimmer ───────────

/// `project_flow_summary` Valkey impl. Mirrors the scanner's pre-PR-7b
/// Rust-composed path: SRANDMEMBER sample, per-member cross-partition
/// HGET on `public_state`, derived `public_flow_state`, HSET summary,
/// conditional flow_index SREM once every member is observed terminal.
///
/// Returns `Ok(true)` when the summary hash was written, `Ok(false)`
/// when the flow had no members (or the core was already gone and the
/// index entry was pruned defensively).
async fn project_flow_summary_impl(
    client: &ferriskey::Client,
    partition: Partition,
    flow_id: &FlowId,
    now_ms: TimestampMs,
    partition_config: &PartitionConfig,
) -> Result<bool, EngineError> {
    const BATCH_SIZE: usize = 50;

    let tag = partition.hash_tag();
    let fid_string = flow_id.to_string();
    let fid_str = fid_string.as_str();
    let fidx = FlowIndexKeys::new(&partition);
    let flow_index_key = fidx.flow_index();
    let core_key = format!("ff:flow:{}:{}:core", tag, fid_str);
    let members_key = format!("ff:flow:{}:{}:members", tag, fid_str);
    let summary_key = format!("ff:flow:{}:{}:summary", tag, fid_str);
    let now_ms_i64 = now_ms.0;

    // Defensive prune: core missing means a retention purge / manual
    // delete has already removed the flow. Drop the index entry so
    // subsequent scans skip it.
    let core_exists: bool = client.exists(&core_key).await.map_err(transport_fk)?;
    if !core_exists {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(&flow_index_key)
            .arg(fid_str)
            .execute()
            .await
            .map_err(transport_fk)?;
        return Ok(false);
    }

    let true_total: u64 = client
        .cmd("SCARD")
        .arg(&members_key)
        .execute()
        .await
        .map_err(transport_fk)?;

    if true_total == 0 {
        return Ok(false);
    }

    let member_eids: Vec<String> = client
        .cmd("SRANDMEMBER")
        .arg(&members_key)
        .arg(BATCH_SIZE.to_string().as_str())
        .execute()
        .await
        .map_err(transport_fk)?;

    if member_eids.is_empty() {
        return Ok(false);
    }

    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut sampled: u32 = 0;

    for eid_str in &member_eids {
        let eid = match ExecutionId::parse(eid_str) {
            Ok(id) => id,
            Err(_) => {
                // Malformed member eid in flow members set — skip but
                // surface so a persistent upstream bug is visible.
                tracing::warn!(
                    flow_id = fid_str,
                    eid = eid_str.as_str(),
                    "project_flow_summary: malformed member execution_id in flow members"
                );
                continue;
            }
        };
        let member_partition = execution_partition(&eid, partition_config);
        let ctx_tag = member_partition.hash_tag();
        let member_core_key = format!("ff:exec:{}:{}:core", ctx_tag, eid_str);

        let ps: Option<String> = client
            .cmd("HGET")
            .arg(&member_core_key)
            .arg("public_state")
            .execute()
            .await
            .map_err(transport_fk)?;

        let state = ps.unwrap_or_else(|| "unknown".to_owned());
        *counts.entry(state).or_insert(0) += 1;
        sampled += 1;
    }

    let completed = *counts.get("completed").unwrap_or(&0);
    let skipped = *counts.get("skipped").unwrap_or(&0);
    let failed = *counts.get("failed").unwrap_or(&0);
    let cancelled = *counts.get("cancelled").unwrap_or(&0);
    let expired = *counts.get("expired").unwrap_or(&0);
    let active = *counts.get("active").unwrap_or(&0);
    let suspended = *counts.get("suspended").unwrap_or(&0);
    let waiting = *counts.get("waiting").unwrap_or(&0);
    let delayed = *counts.get("delayed").unwrap_or(&0);
    let rate_limited = *counts.get("rate_limited").unwrap_or(&0);
    let waiting_children = *counts.get("waiting_children").unwrap_or(&0);

    let terminal_count = completed + skipped + failed + cancelled + expired;
    let all_terminal = terminal_count == sampled && sampled > 0;

    let flow_state = if all_terminal {
        if failed > 0 || cancelled > 0 || expired > 0 {
            "failed"
        } else {
            "completed"
        }
    } else if active > 0 {
        "running"
    } else if suspended > 0 || delayed > 0 || rate_limited > 0 || waiting_children > 0 {
        "blocked"
    } else {
        "open"
    };

    let _: () = client
        .cmd("HSET")
        .arg(&summary_key)
        .arg("total_members").arg(true_total.to_string().as_str())
        .arg("sampled_members").arg(sampled.to_string().as_str())
        .arg("members_completed").arg(completed.to_string().as_str())
        .arg("members_failed").arg(failed.to_string().as_str())
        .arg("members_cancelled").arg(cancelled.to_string().as_str())
        .arg("members_expired").arg(expired.to_string().as_str())
        .arg("members_skipped").arg(skipped.to_string().as_str())
        .arg("members_active").arg(active.to_string().as_str())
        .arg("members_suspended").arg(suspended.to_string().as_str())
        .arg("members_waiting").arg(waiting.to_string().as_str())
        .arg("members_delayed").arg(delayed.to_string().as_str())
        .arg("members_rate_limited").arg(rate_limited.to_string().as_str())
        .arg("members_waiting_children").arg(waiting_children.to_string().as_str())
        .arg("public_flow_state").arg(flow_state)
        .arg("last_summary_update_at").arg(now_ms_i64.to_string().as_str())
        .execute()
        .await
        .map_err(transport_fk)?;

    // Prune the flow_index entry only when we've observed EVERY member
    // in this cycle and all of them are terminal. Sample-based pruning
    // would freeze the summary mid-flight for flows larger than the
    // sample size.
    if all_terminal && (sampled as u64) == true_total {
        let _: Option<i64> = client
            .cmd("SREM")
            .arg(&flow_index_key)
            .arg(fid_str)
            .execute()
            .await
            .map_err(transport_fk)?;
    }

    Ok(true)
}

/// `trim_retention` Valkey impl. Scans `ff:idx:{p:N}:lane:<lane>:terminal`
/// for executions scored (by completed_at) below the retention cutoff,
/// per-execution cascade-deletes all subordinate keys, then finally
/// drops `exec_core` + `policy` and the index entries. Returns the
/// number of executions actually purged (skipped ones — custom
/// retention not yet due — do not count).
async fn trim_retention_impl(
    backend: &ValkeyBackend,
    partition: Partition,
    lane_id: &LaneId,
    retention_ms: u64,
    now_ms: TimestampMs,
    batch_size: u32,
    filter: &ff_core::backend::ScannerFilter,
) -> Result<u32, EngineError> {
    let client = &backend.client;
    let idx = IndexKeys::new(&partition);
    let terminal_key = idx.lane_terminal(lane_id);
    let now_ms_u64 = now_ms.0.max(0) as u64;
    let cutoff = now_ms_u64.saturating_sub(retention_ms);

    let expired: Vec<String> = client
        .cmd("ZRANGEBYSCORE")
        .arg(&terminal_key)
        .arg("-inf")
        .arg(cutoff.to_string().as_str())
        .arg("LIMIT")
        .arg("0")
        .arg(batch_size.to_string().as_str())
        .execute()
        .await
        .map_err(transport_fk)?;

    if expired.is_empty() {
        return Ok(0);
    }

    let mut processed: u32 = 0;
    for eid_str in &expired {
        // Issue #122: honour per-candidate ScannerFilter (namespace /
        // instance tag). Filtered-out candidates are left in place so a
        // differently-scoped scanner cycle can reach them.
        if backend.scanner_skip_candidate(filter, eid_str).await {
            continue;
        }
        // false = custom retention not yet expired; do not count.
        // Log-and-continue on per-execution purge failures so one bad
        // entry can't abort the whole batch (pre-refactor behavior).
        match purge_retention_execution(
            client,
            &partition,
            &idx,
            eid_str,
            &terminal_key,
            now_ms_u64,
            retention_ms,
        )
        .await
        {
            Ok(true) => processed += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    partition = partition.index,
                    execution_id = %eid_str,
                    error = %e,
                    "retention purge failed; continuing batch",
                );
            }
        }
    }

    Ok(processed)
}

/// Cascading delete of all keys for one terminal execution. Returns
/// `Ok(true)` on purge, `Ok(false)` if a per-exec custom retention
/// override kept it alive. Subordinate keys are DELeted in chunks
/// FIRST; exec_core + policy come LAST so a transient mid-cascade
/// failure leaves the next pass enough state to rebuild the full
/// del_keys list.
async fn purge_retention_execution(
    client: &ferriskey::Client,
    partition: &Partition,
    idx: &IndexKeys,
    eid_str: &str,
    terminal_key: &str,
    now_ms: u64,
    default_retention_ms: u64,
) -> Result<bool, EngineError> {
    let tag = partition.hash_tag();
    let exec_core_key = format!("ff:exec:{}:{}:core", tag, eid_str);

    let fields: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(&exec_core_key)
        .arg("completed_at")
        .arg("total_attempt_count")
        .execute()
        .await
        .map_err(transport_fk)?;

    let completed_at: u64 = fields
        .first()
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let total_attempts: u32 = fields
        .get(1)
        .and_then(|v| v.as_ref())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if completed_at == 0 {
        // exec_core already gone — just clean the index entry.
        let _: u32 = client
            .cmd("ZREM")
            .arg(terminal_key)
            .arg(eid_str)
            .execute()
            .await
            .map_err(transport_fk)?;
        return Ok(true);
    }

    let policy_key = format!("ff:exec:{}:{}:policy", tag, eid_str);
    let retention_ms = read_retention_override_ms(client, &policy_key, default_retention_ms).await;

    if now_ms < completed_at + retention_ms {
        return Ok(false);
    }

    let mut del_keys: Vec<String> = Vec::with_capacity(16 + total_attempts as usize * 5);

    del_keys.push(format!("ff:exec:{}:{}:payload", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:result", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:tags", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:lease:current", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:lease:history", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:claim_grant", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:attempts", tag, eid_str));
    for i in 0..total_attempts {
        del_keys.push(format!("ff:attempt:{}:{}:{}", tag, eid_str, i));
        del_keys.push(format!("ff:attempt:{}:{}:{}:usage", tag, eid_str, i));
        del_keys.push(format!("ff:attempt:{}:{}:{}:policy", tag, eid_str, i));
        del_keys.push(format!("ff:stream:{}:{}:{}", tag, eid_str, i));
        del_keys.push(format!("ff:stream:{}:{}:{}:meta", tag, eid_str, i));
    }
    del_keys.push(format!("ff:exec:{}:{}:suspension:current", tag, eid_str));

    let deps_all_edges_key = format!("ff:exec:{}:{}:deps:all_edges", tag, eid_str);
    let dep_edge_ids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&deps_all_edges_key)
        .execute()
        .await
        .map_err(transport_fk)?;

    del_keys.push(format!("ff:exec:{}:{}:deps:meta", tag, eid_str));
    del_keys.push(format!("ff:exec:{}:{}:deps:unresolved", tag, eid_str));
    del_keys.push(deps_all_edges_key);
    for edge_id in &dep_edge_ids {
        del_keys.push(format!("ff:exec:{}:{}:dep:{}", tag, eid_str, edge_id));
    }

    let waitpoints_key = format!("ff:exec:{}:{}:waitpoints", tag, eid_str);
    let wp_ids: Vec<String> = client
        .cmd("SMEMBERS")
        .arg(&waitpoints_key)
        .execute()
        .await
        .map_err(transport_fk)?;

    del_keys.push(waitpoints_key);
    for wp_id_str in &wp_ids {
        del_keys.push(format!("ff:wp:{}:{}", tag, wp_id_str));
        del_keys.push(format!("ff:wp:{}:{}:signals", tag, wp_id_str));
        del_keys.push(format!("ff:wp:{}:{}:condition", tag, wp_id_str));
    }

    let signal_key = format!("ff:exec:{}:{}:signals", tag, eid_str);
    let sig_ids: Vec<String> = client
        .cmd("ZRANGE")
        .arg(&signal_key)
        .arg("0")
        .arg("-1")
        .execute()
        .await
        .map_err(transport_fk)?;

    del_keys.push(signal_key);
    for sig_id_str in &sig_ids {
        del_keys.push(format!("ff:signal:{}:{}", tag, sig_id_str));
        del_keys.push(format!("ff:signal:{}:{}:payload", tag, sig_id_str));
    }

    for chunk in del_keys.chunks(500) {
        let key_refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        let _: u32 = client
            .cmd("DEL")
            .arg(&key_refs)
            .execute()
            .await
            .map_err(transport_fk)?;
    }

    let _: u32 = client
        .cmd("DEL")
        .arg(&[exec_core_key.as_str(), policy_key.as_str()][..])
        .execute()
        .await
        .map_err(transport_fk)?;

    let _: u32 = client
        .cmd("ZREM")
        .arg(terminal_key)
        .arg(eid_str)
        .execute()
        .await
        .map_err(transport_fk)?;
    let all_exec_key = idx.all_executions();
    let _: u32 = client
        .cmd("SREM")
        .arg(&all_exec_key)
        .arg(eid_str)
        .execute()
        .await
        .map_err(transport_fk)?;

    Ok(true)
}

/// Read `policy.stream_policy.retention_ttl_ms` override from the
/// per-execution policy JSON; fall back to `default_retention_ms`
/// when unset, missing, or malformed. Retention overrides are
/// advisory — a transient read error returns the default rather
/// than propagating, because the next retention pass will retry.
async fn read_retention_override_ms(
    client: &ferriskey::Client,
    policy_key: &str,
    default_retention_ms: u64,
) -> u64 {
    let policy_json: Option<String> = match client
        .cmd("GET")
        .arg(policy_key)
        .execute()
        .await
    {
        Ok(v) => v,
        Err(_) => return default_retention_ms,
    };

    let json_str = match policy_json {
        Some(s) if !s.is_empty() => s,
        _ => return default_retention_ms,
    };

    let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => return default_retention_ms,
    };

    parsed
        .get("stream_policy")
        .and_then(|sp| sp.get("retention_ttl_ms"))
        .and_then(|v| v.as_u64())
        .unwrap_or(default_retention_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_policy_strings() {
        assert_eq!(
            cancel_policy_to_str(CancelFlowPolicy::FlowOnly),
            "flow_only"
        );
        assert_eq!(
            cancel_policy_to_str(CancelFlowPolicy::CancelAll),
            "cancel_all"
        );
        assert_eq!(
            cancel_policy_to_str(CancelFlowPolicy::CancelPending),
            "cancel_pending"
        );
    }

    /// `Duration::MAX.as_millis()` overflows `i64`. The pre-fix code used
    /// `as i64` which silently truncates and can wrap NEGATIVE. Post-fix
    /// should clamp to `i64::MAX` (via `saturating_add`) to match the PG +
    /// SQLite `create_waitpoint` behaviour.
    #[test]
    fn create_waitpoint_expires_in_max_clamps_instead_of_wrapping() {
        let now_ms: i64 = 1_700_000_000_000; // arbitrary positive "now"
        let got = waitpoint_expires_at_ms(now_ms, Duration::MAX);
        assert!(
            got > 0,
            "expires_at must not wrap negative on oversized expires_in (got {got})"
        );
        assert_eq!(got, i64::MAX, "expires_at must clamp to i64::MAX");
    }

    #[test]
    fn backend_config_valkey_shape() {
        let c = BackendConfig::valkey("localhost", 6379);
        // `BackendConnection` is `#[non_exhaustive]`; `if let`
        // matches the Valkey arm without tripping the
        // exhaustive-match / unreachable-pattern pair.
        let BackendConnection::Valkey(v) = &c.connection else {
            panic!("BackendConfig::valkey produced a non-Valkey connection");
        };
        assert_eq!(v.host, "localhost");
        assert_eq!(v.port, 6379);
    }

    // Dyn-safety smoke test: `Arc<dyn EngineBackend>` must hold for
    // ValkeyBackend. If a trait change breaks dyn-safety this fails
    // at compile time.
    #[allow(dead_code)]
    fn _dyn_compatible(b: Arc<ValkeyBackend>) -> Arc<dyn EngineBackend> {
        b
    }

    // ── timeouts.request wiring ─────────────────────────────────
    //
    // Exercises `BackendTimeouts.request` → `ClientBuilder::request_timeout`
    // via the `build_client` helper. Requires a live Valkey at
    // localhost:6379, so `#[ignore]`-gated:
    //   cargo test -p ff-backend-valkey --lib -- --ignored

    /// `timeouts.request = None` leaves ferriskey's default in
    /// place; `build_client` produces a working client.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn build_client_with_default_request_timeout() {
        let cfg = BackendConfig::valkey("127.0.0.1", 6379);
        let client = build_client(&cfg)
            .await
            .expect("build_client with default timeouts");
        // Non-blocking round-trip confirms the client is live.
        let _: ferriskey::Value = client
            .cmd("PING")
            .execute()
            .await
            .expect("PING on default-timeout client");
    }

    /// Smoke test the `Some(d)` arm: a configured `request_timeout`
    /// is accepted by the builder and the resulting client still
    /// completes round-trips within the budget. Does **not** assert
    /// the timeout fires on the wire — ferriskey treats blocking
    /// commands (BLPOP etc.) as `server_timeout +
    /// blocking_cmd_timeout_extension`, which masks a tight
    /// `request_timeout` when the server-side timeout dominates.
    /// The `request_timeout` → wire behaviour is covered by
    /// ferriskey's own test suite; here we only prove our wiring
    /// does not break the client.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn build_client_with_explicit_request_timeout_smoke() {
        let mut cfg = BackendConfig::valkey("127.0.0.1", 6379);
        cfg.timeouts.request = Some(Duration::from_secs(5));
        let client = build_client(&cfg)
            .await
            .expect("build_client with explicit request_timeout");
        let _: ferriskey::Value = client
            .cmd("PING")
            .execute()
            .await
            .expect("PING on explicit-timeout client");
    }

    // ── retry wiring ────────────────────────────────────────────
    //
    // Exercises `BackendRetry` → `ClientBuilder::retry_strategy`
    // via the `build_client` helper. Requires a live Valkey at
    // localhost:6379, so `#[ignore]`-gated:
    //   cargo test -p ff-backend-valkey --lib -- --ignored

    /// All-`None` `BackendRetry` (the default) skips
    /// `.retry_strategy(..)` on the builder; ferriskey's internal
    /// default stands and the client still dials.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn build_client_with_default_retry() {
        let cfg = BackendConfig::valkey("127.0.0.1", 6379);
        assert_eq!(cfg.retry, ff_core::backend::BackendRetry::default());
        let client = build_client(&cfg)
            .await
            .expect("build_client with default retry");
        let _: ferriskey::Value = client
            .cmd("PING")
            .execute()
            .await
            .expect("PING on default-retry client");
    }

    /// Any `Some` field on `BackendRetry` triggers the
    /// `.retry_strategy(..)` call path; the builder accepts the
    /// constructed `ConnectionRetryStrategy` and the resulting
    /// client still completes round-trips. Does **not** assert the
    /// retry curve fires on the wire (that's ferriskey's own test
    /// territory); this is a wiring smoke test.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn build_client_with_explicit_retry_smoke() {
        let mut cfg = BackendConfig::valkey("127.0.0.1", 6379);
        cfg.retry.number_of_retries = Some(3);
        cfg.retry.exponent_base = Some(2);
        cfg.retry.factor = Some(100);
        cfg.retry.jitter_percent = Some(20);
        let client = build_client(&cfg)
            .await
            .expect("build_client with explicit retry strategy");
        let _: ferriskey::Value = client
            .cmd("PING")
            .execute()
            .await
            .expect("PING on explicit-retry client");
    }

    // ── RFC-017 Stage A: backend_label + shutdown_prepare smoke ──
    //
    // Per `crates/ff-server/STAGE_A_SCOPE.md` §"Stage A CI gate":
    //   assert_eq!(valkey.backend_label(), "valkey");
    //   assert!(valkey.shutdown_prepare(Duration::from_secs(5)).await.is_ok());
    // Live-Valkey dependency; `#[ignore]`-gated like the sibling
    // live tests above. The MockBackend-backed parity tests in
    // `crates/ff-server/tests/parity_stage_a.rs` cover the non-live
    // path under the default `cargo test` invocation.

    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn backend_label_reports_valkey() {
        let cfg = BackendConfig::valkey("127.0.0.1", 6379);
        let client = build_client(&cfg)
            .await
            .expect("build_client for backend_label smoke");
        let backend = ValkeyBackend::from_client_and_partitions(
            client,
            PartitionConfig::default(),
        );
        assert_eq!(backend.backend_label(), "valkey");
    }

    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn shutdown_prepare_completes_within_grace() {
        let cfg = BackendConfig::valkey("127.0.0.1", 6379);
        let client = build_client(&cfg)
            .await
            .expect("build_client for shutdown_prepare smoke");
        let backend = ValkeyBackend::from_client_and_partitions(
            client,
            PartitionConfig::default(),
        );
        // Stage A is a no-op; must return Ok promptly well inside
        // the 5s grace. Stage B tightens this to "within grace when
        // loaded with 16 in-flight tails".
        backend
            .shutdown_prepare(Duration::from_secs(5))
            .await
            .expect("shutdown_prepare Ok on Stage A no-op impl");
    }

    // ── Issue #293: `with_embedded_scheduler` reachability ──────
    //
    // A `ValkeyBackend` built via `with_embedded_scheduler` routes
    // `claim_for_worker` through an internally-constructed
    // `ff_scheduler::Scheduler`, so direct `Arc<dyn EngineBackend>`
    // consumers (outside `ff-server`'s boot path) reach the trait
    // method instead of hitting `EngineError::Unavailable`. Live
    // Valkey at 127.0.0.1:6379 required; `#[ignore]`-gated like the
    // sibling live tests above.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn claim_for_worker_with_embedded_scheduler() {
        use ff_core::contracts::{ClaimForWorkerArgs, ClaimForWorkerOutcome};
        use ff_core::types::{LaneId, WorkerId, WorkerInstanceId};

        let cfg = BackendConfig::valkey("127.0.0.1", 6379);
        let metrics = Arc::new(ff_observability::Metrics::new());
        let backend = ValkeyBackend::with_embedded_scheduler(cfg, metrics)
            .await
            .expect("with_embedded_scheduler dials + wires scheduler");

        // Empty / absent lane ⇒ scheduler observes no eligible
        // execution and returns `NoWork`. The load-bearing assertion
        // is "not `Unavailable`": before #293's fix, the trait impl
        // returned `Unavailable { op: "claim_for_worker (scheduler
        // not wired on this ValkeyBackend)" }` for any lane.
        let args = ClaimForWorkerArgs::new(
            LaneId::new("ff-issue-293-empty-lane"),
            WorkerId::new("test-worker"),
            WorkerInstanceId::new("test-worker-instance"),
            std::collections::BTreeSet::new(),
            5_000,
        );
        let outcome = backend
            .claim_for_worker(args)
            .await
            .expect("claim_for_worker reaches the embedded scheduler");
        assert!(
            matches!(outcome, ClaimForWorkerOutcome::NoWork),
            "empty lane should yield NoWork, got {outcome:?}"
        );
    }
}
