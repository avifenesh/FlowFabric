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
    HandleKind, LeaseRenewal, PendingWaitpoint, ReclaimToken, ResumeSignal, UsageDimensions,
    WaitpointSpec,
};
use ff_core::contracts::decode::{
    build_edge_snapshot, build_execution_snapshot, build_flow_snapshot,
};
use ff_core::contracts::{
    CancelFlowArgs, CancelFlowResult, EdgeDirection, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot,
    ReportUsageResult,
};
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
use ff_script::functions::flow::{FlowStructOpKeys, ff_cancel_flow};
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
    pub async fn connect(config: BackendConfig) -> Result<Arc<dyn EngineBackend>, EngineError> {
        // `BackendConnection` is `#[non_exhaustive]` for future
        // backends; the compiler treats the pattern as refutable,
        // hence `let ... else`. Today only `Valkey` exists; a
        // non-Valkey BackendConnection handed to `ValkeyBackend`
        // surfaces as `EngineError::Unavailable` so callers get a
        // typed error rather than a panic.
        let BackendConnection::Valkey(v) = config.connection.clone() else {
            return Err(EngineError::Unavailable {
                op: "ValkeyBackend::connect (non-Valkey BackendConnection)",
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
        })
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

/// Pipeline two `HGETALL`s (exec_core + tags) on the execution's
/// partition and decode via [`build_execution_snapshot`]. `Ok(None)`
/// when exec_core is absent. Decode failures surface as
/// `EngineError::Validation { kind: Corruption, .. }`.
///
/// Mirrors the pre-T3 ff-sdk pipeline shape: the two keys share
/// `{fp:N}` so cluster mode routes them to the same slot.
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

/// Single `HGETALL flow_core` + decode via [`build_flow_snapshot`].
/// `Ok(None)` when flow_core is absent. Decode failures surface as
/// `EngineError::Validation { kind: Corruption, .. }`.
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
    build_flow_snapshot(id.clone(), &raw).map(Some)
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
    let total_str: Option<String> = client
        .hget(&wp_cond_key, "total_matchers")
        .await
        .map_err(transport_fk)?;
    let total: usize = total_str
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut signal_ids: Vec<SignalId> = Vec::new();
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

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.stream(f.attempt_index),
        ctx.stream_meta(f.attempt_index),
    ];

    // ARGV (13): execution_id, attempt_index, lease_id, lease_epoch,
    //            frame_type, ts, payload, encoding, correlation_id,
    //            source, retention_maxlen, attempt_id, max_payload_bytes
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

    // ok(entry_id, frame_count) — `fields` is 0-indexed past status.
    let stream_id = parsed.field_str(0);
    let frame_count: u64 = parsed.field_str(1).parse().unwrap_or(0);

    Ok(AppendFrameOutcome {
        stream_id,
        frame_count,
    })
}

/// Round-7 — `create_waitpoint` FCALL body. Migrated from
/// `ff_sdk::task::ClaimedTask::create_pending_waitpoint`. 4 KEYS, 5
/// ARGV. Mints a fresh `WaitpointId` client-side and returns the
/// server-assigned HMAC token.
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
        let f = handle_codec::decode_handle(handle)?;
        renew_impl(&self.client, &self.partition_config, &f).await
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
    }

    async fn append_frame(
        &self,
        handle: &Handle,
        frame: Frame,
    ) -> Result<AppendFrameOutcome, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        append_frame_impl(&self.client, &self.partition_config, &f, frame).await
    }

    async fn complete(&self, handle: &Handle, payload: Option<Vec<u8>>) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        complete_impl(&self.client, &self.partition_config, &f, payload).await
    }

    async fn fail(
        &self,
        handle: &Handle,
        reason: FailureReason,
        classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        fail_impl(
            &self.client,
            &self.partition_config,
            &f,
            reason,
            classification,
        )
        .await
    }

    async fn cancel(&self, handle: &Handle, reason: &str) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        cancel_impl(&self.client, &self.partition_config, &f, reason).await
    }

    async fn suspend(
        &self,
        _handle: &Handle,
        _waitpoints: Vec<WaitpointSpec>,
        _timeout: Option<Duration>,
    ) -> Result<Handle, EngineError> {
        Err(EngineError::Unavailable { op: "suspend" })
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
    }

    async fn observe_signals(
        &self,
        handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        observe_signals_impl(&self.client, &self.partition_config, &f).await
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
        delay_impl(&self.client, &self.partition_config, &f, delay_until).await
    }

    async fn wait_children(&self, handle: &Handle) -> Result<(), EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        wait_children_impl(&self.client, &self.partition_config, &f).await
    }

    async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        describe_execution_impl(&self.client, &self.partition_config, id).await
    }

    async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, EngineError> {
        describe_flow_impl(&self.client, &self.partition_config, id).await
    }

    async fn list_edges(
        &self,
        flow_id: &FlowId,
        direction: EdgeDirection,
    ) -> Result<Vec<EdgeSnapshot>, EngineError> {
        list_edges_impl(&self.client, &self.partition_config, flow_id, direction).await
    }

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        cancel_flow_fcall(&self.client, &self.partition_config, id, policy, wait).await
    }

    async fn report_usage(
        &self,
        handle: &Handle,
        budget: &BudgetId,
        dimensions: UsageDimensions,
    ) -> Result<ReportUsageResult, EngineError> {
        let f = handle_codec::decode_handle(handle)?;
        report_usage_impl(&self.client, &self.partition_config, &f, budget, dimensions).await
    }
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
