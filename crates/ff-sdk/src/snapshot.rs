//! Typed read-models that decouple consumers from FF's storage engine.
//!
//! Today the only call here is [`FlowFabricWorker::describe_execution`],
//! which replaces the HGETALL-on-exec_core pattern downstream consumers
//! rely on with a typed snapshot. The engine remains free to rename
//! hash fields or restructure keys under this surface — see issue #58
//! for the strategic context (decoupling ahead of a Postgres backend
//! port).
//!
//! Snapshot types (`ExecutionSnapshot`, `AttemptSummary`, `LeaseSummary`,
//! `FlowSnapshot`) live in [`ff_core::contracts`] so non-SDK consumers
//! (tests, REST server, future alternate backends) can share them
//! without depending on ff-sdk.

use std::collections::{BTreeMap, HashMap};

use ff_core::contracts::decode::build_edge_snapshot as build_edge_snapshot_core;
use ff_core::contracts::{
    AttemptSummary, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot, LeaseSummary,
};
use ff_core::keys::{ExecKeyContext, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptId, AttemptIndex, EdgeId, ExecutionId, FlowId, LaneId, LeaseEpoch, Namespace,
    TimestampMs, WaitpointId, WorkerInstanceId,
};

use crate::SdkError;
use crate::worker::FlowFabricWorker;

impl FlowFabricWorker {
    /// Read a typed snapshot of one execution.
    ///
    /// Returns `Ok(None)` when no execution exists at `id` (exec_core
    /// hash absent). Returns `Ok(Some(snapshot))` on success. Errors
    /// propagate Valkey transport faults and decode failures.
    ///
    /// # Consistency
    ///
    /// The snapshot is assembled from two pipelined `HGETALL`s — one
    /// for `exec_core`, one for the sibling tags hash — issued in a
    /// single round trip against the partition holding the execution.
    /// The two reads share a hash-tag so they always land on the same
    /// Valkey slot in cluster mode. They are NOT MULTI/EXEC-atomic:
    /// a concurrent FCALL that HSETs both keys can interleave, and a
    /// caller may observe exec_core fields from epoch `N+1` alongside
    /// tags from epoch `N` (or vice versa). This matches the
    /// last-write-wins-per-field semantics every existing HGETALL
    /// consumer already assumes.
    ///
    /// # Field semantics
    ///
    /// * `public_state` is the engine-maintained derived label written
    ///   atomically by every state-mutating FCALL. Parsed from the
    ///   snake_case string stored on exec_core.
    /// * `blocking_reason` / `blocking_detail` — `None` when the
    ///   exec_core field is the empty string (cleared on transition).
    /// * `current_attempt` — `None` before the first claim (exec_core
    ///   `current_attempt_id` empty).
    /// * `current_lease` — `None` when no lease is held (typical for
    ///   terminal, suspended, or pre-claim executions).
    /// * `current_waitpoint` — `None` unless an active suspension has
    ///   pinned a waitpoint id.
    /// * `tags` — empty map if the tags hash is absent (common for
    ///   executions created without `tags_json`).
    pub async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, SdkError> {
        let partition = execution_partition(id, self.partition_config());
        let ctx = ExecKeyContext::new(&partition, id);
        let core_key = ctx.core();
        let tags_key = ctx.tags();

        // Pipeline the two HGETALLs in one round trip. The two keys
        // share `{fp:N}` so cluster mode routes them to the same slot.
        let mut pipe = self.client().pipeline();
        let core_slot = pipe
            .cmd::<HashMap<String, String>>("HGETALL")
            .arg(&core_key)
            .finish();
        let tags_slot = pipe
            .cmd::<HashMap<String, String>>("HGETALL")
            .arg(&tags_key)
            .finish();
        pipe.execute().await.map_err(|e| crate::backend_context(e, "describe_execution: pipeline HGETALL exec_core + tags"))?;

        let core = core_slot.value().map_err(|e| crate::backend_context(e, "describe_execution: decode HGETALL exec_core"))?;
        if core.is_empty() {
            return Ok(None);
        }
        let tags_raw = tags_slot.value().map_err(|e| crate::backend_context(e, "describe_execution: decode HGETALL tags"))?;

        build_execution_snapshot(id.clone(), &core, tags_raw)
    }
}

/// Assemble an [`ExecutionSnapshot`] from the raw HGETALL field maps.
///
/// Kept as a free function so a future unit test can feed synthetic
/// maps without a live Valkey.
fn build_execution_snapshot(
    execution_id: ExecutionId,
    core: &HashMap<String, String>,
    tags_raw: HashMap<String, String>,
) -> Result<Option<ExecutionSnapshot>, SdkError> {
    let public_state = parse_public_state(opt_str(core, "public_state").unwrap_or(""))?;

    // `LaneId::try_new` validates non-empty + ASCII-printable + <= 64 bytes.
    // Exec_core writes a LaneId that already passed these invariants at
    // ingress; a read that fails validation here signals on-disk
    // corruption — surface it rather than silently constructing an
    // invalid LaneId that would mis-partition downstream.
    let lane_id =
        LaneId::try_new(opt_str(core, "lane_id").unwrap_or("")).map_err(|e| SdkError::Config {
            context: "describe_execution: exec_core".into(),
            field: Some("lane_id".into()),
            message: format!("fails LaneId validation (key corruption?): {e}"),
        })?;

    let namespace_str = opt_str(core, "namespace").unwrap_or("").to_owned();
    let namespace = Namespace::new(namespace_str);

    let flow_id = opt_str(core, "flow_id")
        .filter(|s| !s.is_empty())
        .map(|s| {
            FlowId::parse(s).map_err(|e| SdkError::Config {
                context: "describe_execution: exec_core".into(),
                field: Some("flow_id".into()),
                message: format!("is not a valid UUID (key corruption?): {e}"),
            })
        })
        .transpose()?;

    let blocking_reason = opt_str(core, "blocking_reason")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let blocking_detail = opt_str(core, "blocking_detail")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // created_at + last_mutation_at are engine-maintained invariants
    // (lua/execution.lua writes both on create; every mutating FCALL
    // updates last_mutation_at). Missing values indicate on-disk
    // corruption, not a valid pre-create state — fail loudly.
    let created_at =
        parse_ts(core, "describe_execution: exec_core", "created_at")?.ok_or_else(|| {
            SdkError::Config {
                context: "describe_execution: exec_core".into(),
                field: Some("created_at".into()),
                message: "is missing or empty (key corruption?)".into(),
            }
        })?;
    let last_mutation_at = parse_ts(core, "describe_execution: exec_core", "last_mutation_at")?
        .ok_or_else(|| SdkError::Config {
            context: "describe_execution: exec_core".into(),
            field: Some("last_mutation_at".into()),
            message: "is missing or empty (key corruption?)".into(),
        })?;

    let total_attempt_count: u32 =
        parse_u32_strict(core, "describe_execution: exec_core", "total_attempt_count")?
            .unwrap_or(0);

    let current_attempt = build_attempt_summary(core)?;
    let current_lease = build_lease_summary(core)?;

    let current_waitpoint = opt_str(core, "current_waitpoint_id")
        .filter(|s| !s.is_empty())
        .map(|s| {
            WaitpointId::parse(s).map_err(|e| SdkError::Config {
                context: "describe_execution: exec_core".into(),
                field: Some("current_waitpoint_id".into()),
                message: format!("is not a valid UUID (key corruption?): {e}"),
            })
        })
        .transpose()?;

    let tags: BTreeMap<String, String> = tags_raw.into_iter().collect();

    Ok(Some(ExecutionSnapshot::new(
        execution_id,
        flow_id,
        lane_id,
        namespace,
        public_state,
        blocking_reason,
        blocking_detail,
        current_attempt,
        current_lease,
        current_waitpoint,
        created_at,
        last_mutation_at,
        total_attempt_count,
        tags,
    )))
}

fn opt_str<'a>(map: &'a HashMap<String, String>, field: &str) -> Option<&'a str> {
    map.get(field).map(String::as_str)
}

/// Strictly parse a ms-timestamp field. `Ok(None)` when absent/empty,
/// `Err` on unparseable content. `context` names both the calling
/// FCALL and the hash (e.g. `"describe_execution: exec_core"`) so
/// error messages point to the exact source of corruption.
fn parse_ts(
    map: &HashMap<String, String>,
    context: &str,
    field: &str,
) -> Result<Option<TimestampMs>, SdkError> {
    match opt_str(map, field).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => {
            let ms: i64 = raw.parse().map_err(|e| SdkError::Config {
                context: context.to_owned(),
                field: Some(field.to_owned()),
                message: format!("is not a valid ms timestamp ('{raw}'): {e}"),
            })?;
            Ok(Some(TimestampMs::from_millis(ms)))
        }
    }
}

/// Strictly parse a `u32` field. Returns `Ok(None)` when the field is
/// absent or empty (a valid pre-write state), `Err` when the value is
/// present but unparseable (on-disk corruption), `Ok(Some(v))` otherwise.
/// `context` is the caller/hash prefix used in error messages.
fn parse_u32_strict(
    map: &HashMap<String, String>,
    context: &str,
    field: &str,
) -> Result<Option<u32>, SdkError> {
    match opt_str(map, field).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => Ok(Some(raw.parse().map_err(|e| SdkError::Config {
            context: context.to_owned(),
            field: Some(field.to_owned()),
            message: format!("is not a valid u32 ('{raw}'): {e}"),
        })?)),
    }
}

/// Strictly parse a `u64` field. Semantics mirror [`parse_u32_strict`].
fn parse_u64_strict(
    map: &HashMap<String, String>,
    context: &str,
    field: &str,
) -> Result<Option<u64>, SdkError> {
    match opt_str(map, field).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => Ok(Some(raw.parse().map_err(|e| SdkError::Config {
            context: context.to_owned(),
            field: Some(field.to_owned()),
            message: format!("is not a valid u64 ('{raw}'): {e}"),
        })?)),
    }
}

fn parse_public_state(raw: &str) -> Result<PublicState, SdkError> {
    // exec_core stores the snake_case literal (e.g. "waiting"). PublicState's
    // Deserialize accepts the JSON-quoted form, so wrap + delegate.
    let quoted = format!("\"{raw}\"");
    serde_json::from_str(&quoted).map_err(|e| SdkError::Config {
        context: "describe_execution: exec_core".into(),
        field: Some("public_state".into()),
        message: format!("'{raw}' is not a known public state: {e}"),
    })
}

fn build_attempt_summary(
    core: &HashMap<String, String>,
) -> Result<Option<AttemptSummary>, SdkError> {
    let attempt_id_str = match opt_str(core, "current_attempt_id").filter(|s| !s.is_empty()) {
        None => return Ok(None),
        Some(s) => s,
    };
    let attempt_id = AttemptId::parse(attempt_id_str).map_err(|e| SdkError::Config {
        context: "describe_execution: exec_core".into(),
        field: Some("current_attempt_id".into()),
        message: format!("is not a valid UUID: {e}"),
    })?;
    // When `current_attempt_id` is set, `current_attempt_index` MUST be
    // set too — lua/execution.lua writes both atomically in
    // `ff_claim_execution`. A missing index while the id is populated
    // is corruption, not a valid intermediate state.
    let attempt_index = parse_u32_strict(
        core,
        "describe_execution: exec_core",
        "current_attempt_index",
    )?
    .ok_or_else(|| SdkError::Config {
        context: "describe_execution: exec_core".into(),
        field: Some("current_attempt_index".into()),
        message: "is missing while current_attempt_id is set (key corruption?)".into(),
    })?;
    Ok(Some(AttemptSummary::new(
        attempt_id,
        AttemptIndex::new(attempt_index),
    )))
}

fn build_lease_summary(core: &HashMap<String, String>) -> Result<Option<LeaseSummary>, SdkError> {
    // A lease is "held" when the worker_instance_id field is populated
    // AND lease_expires_at is set. Both clear together on revoke/expire
    // (see clear_lease_and_indexes in lua/helpers.lua).
    let wid_str = match opt_str(core, "current_worker_instance_id").filter(|s| !s.is_empty()) {
        None => return Ok(None),
        Some(s) => s,
    };
    let expires_at = match parse_ts(core, "describe_execution: exec_core", "lease_expires_at")? {
        None => return Ok(None),
        Some(ts) => ts,
    };
    // A lease is only "held" if the epoch is present too — lua/helpers.lua
    // sets/clears epoch atomically with wid + expires_at. Parse strictly
    // and require it: a missing epoch alongside a live wid is corruption.
    let epoch = parse_u64_strict(core, "describe_execution: exec_core", "current_lease_epoch")?
        .ok_or_else(|| SdkError::Config {
            context: "describe_execution: exec_core".into(),
            field: Some("current_lease_epoch".into()),
            message: "is missing while current_worker_instance_id is set (key corruption?)".into(),
        })?;
    Ok(Some(LeaseSummary::new(
        LeaseEpoch::new(epoch),
        WorkerInstanceId::new(wid_str.to_owned()),
        expires_at,
    )))
}

// ═══════════════════════════════════════════════════════════════════════
// describe_flow (issue #58.2)
// ═══════════════════════════════════════════════════════════════════════

/// FF-owned snake_case fields on flow_core. Any HGETALL field NOT in
/// this set AND matching the `^[a-z][a-z0-9_]*\.` namespaced-tag shape
/// is surfaced on [`FlowSnapshot::tags`]. Fields that are neither FF-
/// owned nor namespaced (unexpected shapes) are surfaced as a `Config`
/// error so on-disk corruption or protocol drift fails loud.
const FLOW_CORE_KNOWN_FIELDS: &[&str] = &[
    // flow_id is consumed + validated up-front in build_flow_snapshot but
    // must still be listed here so the unknown-field sweep downstream
    // doesn't flag it as corruption.
    "flow_id",
    "flow_kind",
    "namespace",
    "public_flow_state",
    "graph_revision",
    "node_count",
    "edge_count",
    "created_at",
    "last_mutation_at",
    "cancelled_at",
    "cancel_reason",
    "cancellation_policy",
];

impl FlowFabricWorker {
    /// Read a typed snapshot of one flow.
    ///
    /// Returns `Ok(None)` when no flow exists at `id` (flow_core hash
    /// absent). Returns `Ok(Some(snapshot))` on success. Errors
    /// propagate Valkey transport faults and decode failures.
    ///
    /// # Consistency
    ///
    /// The snapshot is assembled from a single `HGETALL flow_core`. No
    /// second key is pipelined: unlike `describe_execution`, flow tags
    /// live inline on `flow_core` under the namespaced-prefix
    /// convention (see [`FlowSnapshot::tags`]). A single round trip is
    /// sufficient and reflects last-write-wins-per-field semantics
    /// under concurrent FCALLs — identical to every existing HGETALL
    /// consumer.
    ///
    /// # Field semantics
    ///
    /// * `public_flow_state` — engine-written literal (`open`,
    ///   `running`, `blocked`, `cancelled`, `completed`, `failed`).
    ///   Surfaced as `String` because FF has no typed enum yet.
    /// * `cancelled_at` / `cancel_reason` / `cancellation_policy` —
    ///   populated only after `cancel_flow`. `None` for live flows
    ///   and for pre-cancel_flow-persistence-era flows.
    /// * `tags` — any flow_core field matching `^[a-z][a-z0-9_]*\.`
    ///   (the namespaced-tag convention). FF's own fields stay in
    ///   snake_case without dots, so there's no collision. Fields
    ///   that match neither shape are treated as corruption and fail
    ///   loud.
    pub async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, SdkError> {
        let partition = flow_partition(id, self.partition_config());
        let ctx = FlowKeyContext::new(&partition, id);
        let core_key = ctx.core();

        let raw: HashMap<String, String> = self
            .client()
            .cmd("HGETALL")
            .arg(&core_key)
            .execute()
            .await
            .map_err(|e| crate::backend_context(e, "describe_flow: HGETALL flow_core"))?;

        if raw.is_empty() {
            return Ok(None);
        }

        build_flow_snapshot(id.clone(), &raw).map(Some)
    }
}

/// Assemble a [`FlowSnapshot`] from the raw HGETALL field map.
///
/// Kept as a free function so a future unit test can feed synthetic
/// maps without a live Valkey.
fn build_flow_snapshot(
    flow_id: FlowId,
    raw: &HashMap<String, String>,
) -> Result<FlowSnapshot, SdkError> {
    // flow_id is engine-written at create time (lua/flow.lua). Validate
    // it matches the requested FlowId — a disagreement means either
    // on-disk corruption or a caller accidentally reading the wrong key.
    let stored_flow_id_str = opt_str(raw, "flow_id")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("flow_id".into()),
            message: "is missing or empty (key corruption?)".into(),
        })?;
    if stored_flow_id_str != flow_id.to_string() {
        return Err(SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("flow_id".into()),
            message: format!(
                "'{stored_flow_id_str}' does not match requested flow_id \
                 '{flow_id}' (key corruption or wrong-key read?)"
            ),
        });
    }

    // namespace and flow_kind are engine-written at create time; absent
    // or empty values indicate on-disk corruption.
    let namespace_str = opt_str(raw, "namespace")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("namespace".into()),
            message: "is missing or empty (key corruption?)".into(),
        })?;
    let namespace = Namespace::new(namespace_str.to_owned());

    let flow_kind = opt_str(raw, "flow_kind")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("flow_kind".into()),
            message: "is missing or empty (key corruption?)".into(),
        })?
        .to_owned();

    let public_flow_state = opt_str(raw, "public_flow_state")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("public_flow_state".into()),
            message: "is missing or empty (key corruption?)".into(),
        })?
        .to_owned();

    // graph_revision / node_count / edge_count are engine-maintained
    // counters; missing values indicate corruption (ff_create_flow
    // writes "0" to all three). Parse strictly.
    let graph_revision = parse_u64_strict(raw, "describe_flow: flow_core", "graph_revision")?
        .ok_or_else(|| SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("graph_revision".into()),
            message: "is missing (key corruption?)".into(),
        })?;
    let node_count =
        parse_u32_strict(raw, "describe_flow: flow_core", "node_count")?.ok_or_else(|| {
            SdkError::Config {
                context: "describe_flow: flow_core".into(),
                field: Some("node_count".into()),
                message: "is missing (key corruption?)".into(),
            }
        })?;
    let edge_count =
        parse_u32_strict(raw, "describe_flow: flow_core", "edge_count")?.ok_or_else(|| {
            SdkError::Config {
                context: "describe_flow: flow_core".into(),
                field: Some("edge_count".into()),
                message: "is missing (key corruption?)".into(),
            }
        })?;

    // created_at + last_mutation_at are engine-maintained; absent values
    // indicate corruption (ff_create_flow writes both).
    let created_at = parse_ts(raw, "describe_flow: flow_core", "created_at")?.ok_or_else(|| {
        SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("created_at".into()),
            message: "is missing or empty (key corruption?)".into(),
        }
    })?;
    let last_mutation_at = parse_ts(raw, "describe_flow: flow_core", "last_mutation_at")?
        .ok_or_else(|| SdkError::Config {
            context: "describe_flow: flow_core".into(),
            field: Some("last_mutation_at".into()),
            message: "is missing or empty (key corruption?)".into(),
        })?;

    let cancelled_at = parse_ts(raw, "describe_flow: flow_core", "cancelled_at")?;
    let cancel_reason = opt_str(raw, "cancel_reason")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let cancellation_policy = opt_str(raw, "cancellation_policy")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Route unknown fields: namespaced-prefix (e.g. `cairn.task_id`) →
    // tags; anything else → corruption. This keeps FF's snake_case
    // field additions distinct from consumer metadata without a second
    // HGETALL on a non-existent tags hash.
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in raw {
        if FLOW_CORE_KNOWN_FIELDS.contains(&k.as_str()) {
            continue;
        }
        if is_namespaced_tag_key(k) {
            tags.insert(k.clone(), v.clone());
        } else {
            return Err(SdkError::Config {
                context: "describe_flow: flow_core".into(),
                field: None,
                message: format!(
                    "has unexpected field '{k}' — not an FF field and not a namespaced \
                     tag (lowercase-alphanumeric-prefix + '.')"
                ),
            });
        }
    }

    Ok(FlowSnapshot::new(
        flow_id,
        flow_kind,
        namespace,
        public_flow_state,
        graph_revision,
        node_count,
        edge_count,
        created_at,
        last_mutation_at,
        cancelled_at,
        cancel_reason,
        cancellation_policy,
        tags,
    ))
}

/// Match the namespaced-tag shape `^[a-z][a-z0-9_]*\.` documented on
/// [`ExecutionSnapshot::tags`] / [`FlowSnapshot::tags`]. Kept inline
/// (no regex dependency) — the shape is tight enough to hand-check.
fn is_namespaced_tag_key(k: &str) -> bool {
    let mut chars = k.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut saw_dot = false;
    for c in chars {
        if c == '.' {
            saw_dot = true;
            break;
        }
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return false;
        }
    }
    saw_dot
}

// ═══════════════════════════════════════════════════════════════════════
// describe_edge / list_*_edges (issue #58.3)
// ═══════════════════════════════════════════════════════════════════════
//
// RFC-012 Stage 1c T2: the `EDGE_KNOWN_FIELDS` list and the
// `build_edge_snapshot` decoder moved to
// `ff_core::contracts::decode`. Parse failures now surface as
// `EngineError::Validation { kind: Corruption, .. }` — callers that
// match on `SdkError` see them wrapped as
// `SdkError::Engine(EngineError::Validation(..))` via the crate-root
// `From<EngineError> for SdkError` impl. The pipeline shape
// (SMEMBERS + pipelined HGETALL) is unchanged; only the decoder
// location moved so non-SDK backends can reuse it.

impl FlowFabricWorker {
    /// Read a typed snapshot of one dependency edge.
    ///
    /// Takes both `flow_id` and `edge_id`: the edge hash is stored under
    /// the flow's partition (`ff:flow:{fp:N}:<flow_id>:edge:<edge_id>`)
    /// and FF does not maintain a global `edge_id -> flow_id` index.
    /// The caller already knows the flow from the staging call result
    /// or the consumer's own metadata.
    ///
    /// Returns `Ok(None)` when the edge hash is absent (never staged,
    /// or staged under a different flow). Returns `Ok(Some(snapshot))`
    /// on success.
    ///
    /// # Consistency
    ///
    /// Single `HGETALL` — the flow-scoped edge hash is written once at
    /// staging time and never mutated (per-execution resolution state
    /// lives on a separate `dep:<edge_id>` hash), so a single round
    /// trip is authoritative.
    pub async fn describe_edge(
        &self,
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, SdkError> {
        let partition = flow_partition(flow_id, self.partition_config());
        let ctx = FlowKeyContext::new(&partition, flow_id);
        let edge_key = ctx.edge(edge_id);

        let raw: HashMap<String, String> = self
            .client()
            .cmd("HGETALL")
            .arg(&edge_key)
            .execute()
            .await
            .map_err(|e| crate::backend_context(e, "describe_edge: HGETALL edge_hash"))?;

        if raw.is_empty() {
            return Ok(None);
        }

        build_edge_snapshot_core(flow_id, edge_id, &raw)
            .map(Some)
            .map_err(SdkError::from)
    }

    /// List all outgoing dependency edges originating from an execution.
    ///
    /// Returns an empty `Vec` when the execution has no outgoing edges
    /// (including the case where the execution is standalone, i.e. not
    /// attached to any flow — such executions cannot participate in
    /// dependency edges).
    ///
    /// # Reads
    ///
    /// 1. `HGET exec_core flow_id` — resolve the flow owning the
    ///    adjacency set. Missing or empty flow_id returns an empty Vec.
    /// 2. `SMEMBERS` on the flow-scoped `out:<upstream_eid>` set.
    /// 3. One pipelined round trip issuing one `HGETALL` per edge_id.
    ///
    /// Ordering is unspecified — the adjacency set is an unordered SET.
    /// Callers that need deterministic order should sort by
    /// [`EdgeSnapshot::edge_id`] (or `created_at`) themselves.
    pub async fn list_outgoing_edges(
        &self,
        upstream_eid: &ExecutionId,
    ) -> Result<Vec<EdgeSnapshot>, SdkError> {
        let Some(flow_id) = self.resolve_flow_id(upstream_eid).await? else {
            return Ok(Vec::new());
        };
        let partition = flow_partition(&flow_id, self.partition_config());
        let ctx = FlowKeyContext::new(&partition, &flow_id);
        self.list_edges_from_set(
            &ctx.outgoing(upstream_eid),
            &flow_id,
            upstream_eid,
            AdjacencySide::Outgoing,
        )
        .await
    }

    /// List all incoming dependency edges landing on an execution.
    /// See [`list_outgoing_edges`] for the read shape.
    pub async fn list_incoming_edges(
        &self,
        downstream_eid: &ExecutionId,
    ) -> Result<Vec<EdgeSnapshot>, SdkError> {
        let Some(flow_id) = self.resolve_flow_id(downstream_eid).await? else {
            return Ok(Vec::new());
        };
        let partition = flow_partition(&flow_id, self.partition_config());
        let ctx = FlowKeyContext::new(&partition, &flow_id);
        self.list_edges_from_set(
            &ctx.incoming(downstream_eid),
            &flow_id,
            downstream_eid,
            AdjacencySide::Incoming,
        )
        .await
    }

    /// `HGET exec_core.flow_id` and parse to a [`FlowId`]. `None` when
    /// the exec_core hash is absent OR the flow_id field is empty
    /// (standalone execution).
    ///
    /// Also pins the RFC-011 co-location invariant
    /// (`execution_partition(eid) == flow_partition(flow_id)`) — a
    /// parsed-but-wrong flow_id would otherwise silently route the
    /// follow-up adjacency reads to the wrong partition and return
    /// bogus empty results. A mismatch surfaces as `SdkError::Config`.
    async fn resolve_flow_id(&self, eid: &ExecutionId) -> Result<Option<FlowId>, SdkError> {
        let exec_partition = execution_partition(eid, self.partition_config());
        let ctx = ExecKeyContext::new(&exec_partition, eid);
        let raw: Option<String> = self
            .client()
            .cmd("HGET")
            .arg(ctx.core())
            .arg("flow_id")
            .execute()
            .await
            .map_err(|e| crate::backend_context(e, "list_edges: HGET exec_core.flow_id"))?;
        let Some(raw) = raw.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let flow_id = FlowId::parse(&raw).map_err(|e| SdkError::Config {
            context: "list_edges: exec_core".into(),
            field: Some("flow_id".into()),
            message: format!("'{raw}' is not a valid UUID (key corruption?): {e}"),
        })?;
        let flow_partition_index = flow_partition(&flow_id, self.partition_config()).index;
        if exec_partition.index != flow_partition_index {
            return Err(SdkError::Config {
                context: "list_edges: exec_core".into(),
                field: Some("flow_id".into()),
                message: format!(
                    "'{flow_id}' partition {flow_partition_index} does not match \
                     execution partition {} (RFC-011 co-location violation; key corruption?)",
                    exec_partition.index
                ),
            });
        }
        Ok(Some(flow_id))
    }

    /// Shared body for `list_incoming_edges` / `list_outgoing_edges`:
    /// SMEMBERS + pipelined HGETALL.
    ///
    /// `subject_eid` + `side` pin the expected endpoint on each
    /// returned `EdgeSnapshot`: an adjacency SET entry whose edge
    /// hash points at a different upstream (for `Outgoing` listings)
    /// or downstream (for `Incoming`) is treated as corruption and
    /// surfaced as `SdkError::Config`, matching the strict-parse
    /// posture elsewhere in this module.
    async fn list_edges_from_set(
        &self,
        adj_key: &str,
        flow_id: &FlowId,
        subject_eid: &ExecutionId,
        side: AdjacencySide,
    ) -> Result<Vec<EdgeSnapshot>, SdkError> {
        let edge_id_strs: Vec<String> = self
            .client()
            .cmd("SMEMBERS")
            .arg(adj_key)
            .execute()
            .await
            .map_err(|e| crate::backend_context(e, "list_edges: SMEMBERS adj_set"))?;
        if edge_id_strs.is_empty() {
            return Ok(Vec::new());
        }

        // Parse edge ids first so a corrupt adjacency entry fails loud
        // before we spend a round trip on it.
        let mut edge_ids: Vec<EdgeId> = Vec::with_capacity(edge_id_strs.len());
        for raw in &edge_id_strs {
            let parsed = EdgeId::parse(raw).map_err(|e| SdkError::Config {
                context: "list_edges: adjacency_set".into(),
                field: Some("edge_id".into()),
                message: format!("'{raw}' is not a valid EdgeId (key corruption?): {e}"),
            })?;
            edge_ids.push(parsed);
        }

        let partition = flow_partition(flow_id, self.partition_config());
        let ctx = FlowKeyContext::new(&partition, flow_id);

        let mut pipe = self.client().pipeline();
        let slots: Vec<_> = edge_ids
            .iter()
            .map(|eid| {
                pipe.cmd::<HashMap<String, String>>("HGETALL")
                    .arg(ctx.edge(eid))
                    .finish()
            })
            .collect();
        pipe.execute().await.map_err(|e| crate::backend_context(e, "list_edges: pipeline HGETALL edges"))?;

        let mut out: Vec<EdgeSnapshot> = Vec::with_capacity(edge_ids.len());
        for (edge_id, slot) in edge_ids.iter().zip(slots) {
            let raw = slot.value().map_err(|e| crate::backend_context(e, "list_edges: decode HGETALL edge_hash"))?;
            if raw.is_empty() {
                // Adjacency SET referenced an edge_hash that no longer
                // exists. FF does not delete edge hashes today (staging
                // is write-once), so this is corruption — fail loud.
                return Err(SdkError::Config {
                    context: "list_edges: adjacency_set".into(),
                    field: None,
                    message: format!(
                        "refers to edge_id '{edge_id}' but its edge_hash is absent \
                         (key corruption?)"
                    ),
                });
            }
            let snap = build_edge_snapshot_core(flow_id, edge_id, &raw).map_err(SdkError::from)?;
            // Cross-check: the edge hash's endpoint on the listed side
            // must match the execution we're listing for. A mismatch
            // means the adjacency SET and edge hash disagree (e.g. a
            // stale or corrupted SET entry) — refuse to silently
            // return an edge the caller did not ask about.
            let endpoint = match side {
                AdjacencySide::Outgoing => &snap.upstream_execution_id,
                AdjacencySide::Incoming => &snap.downstream_execution_id,
            };
            if endpoint != subject_eid {
                return Err(SdkError::Config {
                    context: "list_edges: adjacency_set".into(),
                    field: None,
                    message: format!(
                        "for execution '{subject_eid}' (side={side:?}) contains edge \
                         '{edge_id}' whose stored endpoint is '{endpoint}' \
                         (adjacency/edge-hash drift?)"
                    ),
                });
            }
            out.push(snap);
        }
        Ok(out)
    }
}

/// Which side of an adjacency SET the subject execution lives on.
/// Used by [`list_edges_from_set`] to cross-check the returned edge's
/// stored endpoint against the listing's subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdjacencySide {
    /// Outgoing: subject is the `upstream_execution_id` on each edge.
    Outgoing,
    /// Incoming: subject is the `downstream_execution_id` on each edge.
    Incoming,
}

/// Crate-visible thin adapter over
/// [`ff_core::contracts::decode::build_edge_snapshot`] for
/// [`crate::engine_error::enrich_dependency_conflict`]. Returns the
/// native [`ff_core::engine_error::EngineError`] so the caller can
/// route parse failures into the same `EngineError`-shaped surface
/// it already uses. Kept as a named indirection so the one non-SDK
/// internal user (`enrich_dependency_conflict`) has a stable call
/// site; new callers should invoke the ff-core module directly.
#[allow(dead_code, clippy::result_large_err)]
pub(crate) fn build_edge_snapshot_public(
    flow_id: &FlowId,
    edge_id: &EdgeId,
    raw: &HashMap<String, String>,
) -> Result<EdgeSnapshot, ff_core::engine_error::EngineError> {
    build_edge_snapshot_core(flow_id, edge_id, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::partition::PartitionConfig;
    use ff_core::types::FlowId;

    fn eid() -> ExecutionId {
        let config = PartitionConfig::default();
        ExecutionId::for_flow(&FlowId::new(), &config)
    }

    fn minimal_core(public_state: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("public_state".to_owned(), public_state.to_owned());
        m.insert("lane_id".to_owned(), "default".to_owned());
        m.insert("namespace".to_owned(), "ns".to_owned());
        m.insert("created_at".to_owned(), "1000".to_owned());
        m.insert("last_mutation_at".to_owned(), "2000".to_owned());
        m.insert("total_attempt_count".to_owned(), "0".to_owned());
        m
    }

    #[test]
    fn waiting_exec_no_attempt_no_lease_no_tags() {
        let snap = build_execution_snapshot(eid(), &minimal_core("waiting"), HashMap::new())
            .unwrap()
            .expect("should build");
        assert_eq!(snap.public_state, PublicState::Waiting);
        assert!(snap.current_attempt.is_none());
        assert!(snap.current_lease.is_none());
        assert!(snap.current_waitpoint.is_none());
        assert_eq!(snap.tags.len(), 0);
        assert_eq!(snap.created_at.0, 1000);
        assert_eq!(snap.last_mutation_at.0, 2000);
        assert!(snap.flow_id.is_none());
        assert!(snap.blocking_reason.is_none());
    }

    #[test]
    fn tags_flow_through_sorted() {
        let mut tags = HashMap::new();
        tags.insert("cairn.task_id".to_owned(), "t-1".to_owned());
        tags.insert("cairn.project".to_owned(), "proj".to_owned());
        let snap = build_execution_snapshot(eid(), &minimal_core("waiting"), tags)
            .unwrap()
            .unwrap();
        let keys: Vec<_> = snap.tags.keys().cloned().collect();
        assert_eq!(
            keys,
            vec!["cairn.project".to_owned(), "cairn.task_id".to_owned()]
        );
    }

    #[test]
    fn invalid_public_state_fails_loud() {
        let err =
            build_execution_snapshot(eid(), &minimal_core("bogus"), HashMap::new()).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert_eq!(field.as_deref(), Some("public_state"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn invalid_lane_id_fails_loud() {
        // LaneId::try_new rejects non-printable bytes. Simulate on-disk
        // corruption by stamping a lane_id with an embedded \n.
        let mut core = minimal_core("waiting");
        core.insert("lane_id".to_owned(), "lane\nbroken".to_owned());
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert_eq!(field.as_deref(), Some("lane_id"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_timestamps_fail_loud() {
        for want in ["created_at", "last_mutation_at"] {
            let mut core = minimal_core("waiting");
            core.remove(want);
            let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
            match err {
                SdkError::Config {
                    field,
                    message: msg,
                    ..
                } => {
                    assert_eq!(field.as_deref(), Some(want), "msg for {want}: {msg}");
                }
                other => panic!("expected Config for {want}, got {other:?}"),
            }
        }
    }

    #[test]
    fn malformed_total_attempt_count_fails_loud() {
        let mut core = minimal_core("waiting");
        core.insert("total_attempt_count".to_owned(), "not-a-number".to_owned());
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert_eq!(field.as_deref(), Some("total_attempt_count"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn attempt_id_without_index_fails_loud() {
        // current_attempt_id set but current_attempt_index absent =>
        // corruption, since lua/execution.lua writes both atomically.
        let mut core = minimal_core("active");
        core.insert(
            "current_attempt_id".to_owned(),
            AttemptId::new().to_string(),
        );
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert_eq!(
                    field.as_deref(),
                    Some("current_attempt_index"),
                    "msg: {msg}"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn lease_without_epoch_fails_loud() {
        // wid + expires_at present but epoch missing => corruption.
        let mut core = minimal_core("active");
        core.insert(
            "current_worker_instance_id".to_owned(),
            "w-inst-1".to_owned(),
        );
        core.insert("lease_expires_at".to_owned(), "9000".to_owned());
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert_eq!(field.as_deref(), Some("current_lease_epoch"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn lease_summary_requires_both_wid_and_expires_at() {
        // Only wid → no lease (lease expired + cleared lease_expires_at
        // but wid not yet rewritten is not a real state; defensive).
        let mut core = minimal_core("active");
        core.insert(
            "current_worker_instance_id".to_owned(),
            "w-inst-1".to_owned(),
        );
        let snap = build_execution_snapshot(eid(), &core, HashMap::new())
            .unwrap()
            .unwrap();
        assert!(snap.current_lease.is_none());

        core.insert("lease_expires_at".to_owned(), "9000".to_owned());
        core.insert("current_lease_epoch".to_owned(), "3".to_owned());
        let snap = build_execution_snapshot(eid(), &core, HashMap::new())
            .unwrap()
            .unwrap();
        let lease = snap.current_lease.expect("lease present");
        assert_eq!(lease.lease_epoch, LeaseEpoch::new(3));
        assert_eq!(lease.expires_at.0, 9000);
        assert_eq!(lease.worker_instance_id.as_str(), "w-inst-1");
    }

    // ─── FlowSnapshot (describe_flow) ───

    fn fid() -> FlowId {
        FlowId::new()
    }

    fn minimal_flow_core(id: &FlowId, state: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("flow_id".to_owned(), id.to_string());
        m.insert("flow_kind".to_owned(), "dag".to_owned());
        m.insert("namespace".to_owned(), "ns".to_owned());
        m.insert("public_flow_state".to_owned(), state.to_owned());
        m.insert("graph_revision".to_owned(), "0".to_owned());
        m.insert("node_count".to_owned(), "0".to_owned());
        m.insert("edge_count".to_owned(), "0".to_owned());
        m.insert("created_at".to_owned(), "1000".to_owned());
        m.insert("last_mutation_at".to_owned(), "1000".to_owned());
        m
    }

    #[test]
    fn open_flow_round_trips() {
        let f = fid();
        let snap = build_flow_snapshot(f.clone(), &minimal_flow_core(&f, "open")).unwrap();
        assert_eq!(snap.flow_id, f);
        assert_eq!(snap.flow_kind, "dag");
        assert_eq!(snap.namespace.as_str(), "ns");
        assert_eq!(snap.public_flow_state, "open");
        assert_eq!(snap.graph_revision, 0);
        assert_eq!(snap.node_count, 0);
        assert_eq!(snap.edge_count, 0);
        assert_eq!(snap.created_at.0, 1000);
        assert_eq!(snap.last_mutation_at.0, 1000);
        assert!(snap.cancelled_at.is_none());
        assert!(snap.cancel_reason.is_none());
        assert!(snap.cancellation_policy.is_none());
        assert!(snap.tags.is_empty());
    }

    #[test]
    fn cancelled_flow_surfaces_cancel_fields() {
        let f = fid();
        let mut core = minimal_flow_core(&f, "cancelled");
        core.insert("cancelled_at".to_owned(), "2000".to_owned());
        core.insert("cancel_reason".to_owned(), "operator".to_owned());
        core.insert("cancellation_policy".to_owned(), "cancel_all".to_owned());
        let snap = build_flow_snapshot(f, &core).unwrap();
        assert_eq!(snap.public_flow_state, "cancelled");
        assert_eq!(snap.cancelled_at.unwrap().0, 2000);
        assert_eq!(snap.cancel_reason.as_deref(), Some("operator"));
        assert_eq!(snap.cancellation_policy.as_deref(), Some("cancel_all"));
    }

    #[test]
    fn namespaced_tags_routed_to_tags_map() {
        let f = fid();
        let mut core = minimal_flow_core(&f, "open");
        core.insert("cairn.task_id".to_owned(), "t-1".to_owned());
        core.insert("cairn.project".to_owned(), "proj".to_owned());
        core.insert("operator.label".to_owned(), "v".to_owned());
        let snap = build_flow_snapshot(f, &core).unwrap();
        assert_eq!(snap.tags.len(), 3);
        let keys: Vec<_> = snap.tags.keys().cloned().collect();
        // BTreeMap keeps them sorted.
        assert_eq!(
            keys,
            vec![
                "cairn.project".to_owned(),
                "cairn.task_id".to_owned(),
                "operator.label".to_owned()
            ]
        );
    }

    #[test]
    fn unknown_flat_field_fails_loud() {
        // A future FF field rename or on-disk drift lands a non-
        // namespaced unknown key. Don't silently bucket it.
        let f = fid();
        let mut core = minimal_flow_core(&f, "open");
        core.insert("bogus_future_field".to_owned(), "v".to_owned());
        let err = build_flow_snapshot(f, &core).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert!(
                    field.is_none(),
                    "expected whole-object error, got field={field:?}"
                );
                assert!(msg.contains("bogus_future_field"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_fields_fail_loud() {
        for want in [
            "flow_id",
            "namespace",
            "flow_kind",
            "public_flow_state",
            "graph_revision",
            "node_count",
            "edge_count",
            "created_at",
            "last_mutation_at",
        ] {
            let f = fid();
            let mut core = minimal_flow_core(&f, "open");
            core.remove(want);
            let err = build_flow_snapshot(f, &core).err().unwrap_or_else(|| {
                panic!("field {want} should fail but build_flow_snapshot returned Ok")
            });
            match err {
                SdkError::Config {
                    field,
                    message: msg,
                    ..
                } => {
                    assert_eq!(field.as_deref(), Some(want), "msg for {want}: {msg}");
                }
                other => panic!("expected Config for {want}, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_required_strings_fail_loud() {
        // opt_str + .filter(|s| !s.is_empty()) must treat an empty
        // value the same as a missing one for strict-parsed fields.
        for want in ["flow_id", "namespace", "flow_kind", "public_flow_state"] {
            let f = fid();
            let mut core = minimal_flow_core(&f, "open");
            core.insert(want.to_owned(), String::new());
            let err = build_flow_snapshot(f, &core).err().unwrap_or_else(|| {
                panic!("empty {want} should fail but build_flow_snapshot returned Ok")
            });
            match err {
                SdkError::Config {
                    field,
                    message: msg,
                    ..
                } => {
                    assert_eq!(field.as_deref(), Some(want), "msg for {want}: {msg}");
                }
                other => panic!("expected Config for {want}, got {other:?}"),
            }
        }
    }

    #[test]
    fn flow_id_mismatch_fails_loud() {
        // flow_core.flow_id disagreeing with the requested FlowId is
        // corruption or a wrong-key read — must surface as Config.
        let requested = fid();
        let other = fid();
        let core = minimal_flow_core(&other, "open");
        let err = build_flow_snapshot(requested, &core).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert_eq!(field.as_deref(), Some("flow_id"), "msg: {msg}");
                assert!(msg.contains("does not match"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn malformed_counter_fails_loud() {
        let f = fid();
        let mut core = minimal_flow_core(&f, "open");
        core.insert("graph_revision".to_owned(), "not-a-number".to_owned());
        let err = build_flow_snapshot(f, &core).unwrap_err();
        match err {
            SdkError::Config {
                field,
                message: msg,
                ..
            } => {
                assert_eq!(field.as_deref(), Some("graph_revision"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    // ─── EdgeSnapshot (describe_edge) ───
    //
    // RFC-012 Stage 1c T2: the edge-hash decoder moved to
    // `ff_core::contracts::decode`. The unit-level coverage for
    // unknown-field / missing-field / mismatch / malformed-number
    // paths lives alongside the decoder at ff-core; the ff-sdk
    // higher-level describe_edge + list_*_edges integration coverage
    // lives in `crates/ff-test/tests/describe_edge_api.rs`.

    #[test]
    fn namespaced_tag_matcher_boundaries() {
        // Positive
        assert!(is_namespaced_tag_key("cairn.task_id"));
        assert!(is_namespaced_tag_key("a.b"));
        assert!(is_namespaced_tag_key("ab_12.field"));
        // Negative: no dot
        assert!(!is_namespaced_tag_key("cairn_task_id"));
        // Negative: uppercase prefix
        assert!(!is_namespaced_tag_key("Cairn.task"));
        // Negative: leading digit
        assert!(!is_namespaced_tag_key("1cairn.task"));
        // Negative: empty
        assert!(!is_namespaced_tag_key(""));
        // Negative: dot at position 0
        assert!(!is_namespaced_tag_key(".x"));
        // Negative: uppercase in prefix
        assert!(!is_namespaced_tag_key("caIrn.task"));
    }
}
