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
    HandleKind, LeaseRenewal, PatchKind, PendingWaitpoint, ReclaimToken, ResumeSignal,
    StreamMode, SummaryDocument, TailVisibility, UsageDimensions, WaitpointHmac,
};
use ff_core::contracts::decode::{
    build_edge_snapshot, build_execution_snapshot, build_flow_snapshot,
};
use ff_core::contracts::{
    AdditionalWaitpointBinding, CancelFlowArgs, CancelFlowResult, ClaimResumedExecutionArgs,
    ClaimResumedExecutionResult, CompositeBody, CountKind, DeliverSignalArgs, DeliverSignalResult,
    EdgeDirection, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot, FlowStatus, FlowSummary,
    ListExecutionsPage, ListFlowsPage, ListLanesPage, ListSuspendedPage, ReportUsageResult,
    ResumeCondition, ResumePolicy, ResumeTarget, SignalMatcher, SuspendArgs, SuspendOutcome,
    SuspendOutcomeDetails, SuspendedExecutionEntry, WaitpointBinding,
};
use ff_core::engine_error::{StateKind, ValidationKind};
use ff_core::partition::PartitionKey;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{PartitionConfig, execution_partition, flow_partition};
use ff_core::types::{
    AttemptId, AttemptIndex, BudgetId, EdgeId, ExecutionId, FlowId, LaneId, LeaseEpoch, LeaseId,
    SignalId, TimestampMs, WaitpointId, WorkerInstanceId,
};
use ff_script::engine_error_ext::transport_script;
use ff_script::error::ScriptError;
use ff_script::functions::execution::ExecOpKeys;
use ff_script::functions::flow::{FlowStructOpKeys, ff_cancel_flow};
use ff_script::functions::signal::{SignalOpKeys, ff_claim_resumed_execution, ff_deliver_signal};
use ff_script::result::FcallResult;

pub mod backend_error;
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
}

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
    pub async fn connect_with_metrics(
        config: BackendConfig,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Result<Arc<dyn EngineBackend>, EngineError> {
        Self::connect_inner(config, Some(metrics), "connect_with_metrics").await
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
        let fields = handle_codec::HandleFields {
            execution_id,
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            lease_ttl_ms,
            lane_id,
            worker_instance_id,
        };
        handle_codec::encode_handle(&fields, kind)
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

/// Stage 1a cancel-flow FCALL wrapper. Only
/// [`CancelFlowWait::NoWait`] is supported at Stage 1a — the
/// dispatch+wait loop that [`CancelFlowWait::WaitTimeout`] /
/// [`CancelFlowWait::WaitIndefinite`] require lands in a
/// follow-up stage (today's ff-sdk cancel_flow HTTP path does the
/// wait client-side after the FCALL commits). Rejecting the
/// wait modes explicitly with [`EngineError::Unavailable`] lets
/// callers distinguish "backend won't do this yet" from a silent
/// fallback. See RFC-012 §3.1.1 for the cancel_flow policy matrix.
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
    wait: CancelFlowWait,
) -> Result<CancelFlowResult, EngineError> {
    match wait {
        CancelFlowWait::NoWait => {}
        CancelFlowWait::WaitTimeout(_) => {
            return Err(EngineError::Unavailable {
                op: "cancel_flow(wait=WaitTimeout)",
            });
        }
        CancelFlowWait::WaitIndefinite => {
            return Err(EngineError::Unavailable {
                op: "cancel_flow(wait=WaitIndefinite)",
            });
        }
        // `CancelFlowWait` is `#[non_exhaustive]`. Future wait
        // variants must be reviewed here explicitly; fall closed
        // with Unavailable so callers see a typed error instead of
        // silent fallback to NoWait.
        _ => {
            return Err(EngineError::Unavailable {
                op: "cancel_flow(wait=unknown)",
            });
        }
    }
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
    let expires_at =
        TimestampMs::from_millis(now_ms_timestamp().0 + expires_in.as_millis() as i64);

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
            // `observe_signals` / `claim_from_reclaim` routing.
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
    Ok(partial.complete(execution_id))
}

#[async_trait]
impl EngineBackend for ValkeyBackend {
    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        Err(EngineError::Unavailable { op: "claim" })
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

    async fn claim_from_reclaim(
        &self,
        _token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        Err(EngineError::Unavailable {
            op: "claim_from_reclaim",
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

    async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError> {
        describe_flow_impl(&self.client, &self.partition_config, id)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "describe_flow: HGETALL flow_core")
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
        cancel_flow_fcall(&self.client, &self.partition_config, id, policy, wait)
            .await
            .map_err(|e| {
                ff_core::engine_error::backend_context(e, "cancel_flow: FCALL ff_cancel_flow")
            })
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

    #[cfg(feature = "streaming")]
    async fn read_stream(
        &self,
        execution_id: &ExecutionId,
        attempt_index: AttemptIndex,
        from: ff_core::contracts::StreamCursor,
        to: ff_core::contracts::StreamCursor,
        count_limit: u64,
    ) -> Result<ff_core::contracts::StreamFrames, EngineError> {
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

    let raw: ferriskey::Value = client
        .cmd("HGETALL")
        .arg(&summary_key)
        .execute()
        .await
        .map_err(transport_fk)?;

    // HGETALL returns a flat array of alternating keys/values. Empty ⇒ no
    // summary (RFC-015 §6.3 returns `Ok(None)`).
    let entries: Vec<String> = match raw {
        ferriskey::Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| match v {
                Ok(ferriskey::Value::BulkString(b)) => {
                    Some(String::from_utf8_lossy(&b).into_owned())
                }
                Ok(ferriskey::Value::SimpleString(s)) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if entries.is_empty() {
        return Ok(None);
    }

    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut it = entries.into_iter();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        map.insert(k, v);
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
}
