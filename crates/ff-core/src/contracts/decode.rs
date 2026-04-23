// `EngineError` is ~200 bytes; the decoder and its helpers return
// `Result<_, EngineError>` throughout to match the
// [`crate::engine_backend::EngineBackend::list_edges`] contract. The
// variant size is a cross-crate design point (see ff-backend-valkey's
// crate-level allow for the same rationale); a future PR can box the
// large `Conflict`/`Transport` variants. Module-local allow to
// contain the exception to this one file.
#![allow(clippy::result_large_err)]

//! Canonical decoders for engine-owned hash shapes.
//!
//! RFC-012 Stage 1c (T2): the edge-hash decoder lives here so every
//! `EngineBackend` implementation — not just `ff-backend-valkey` —
//! shares one strict-parse posture and one error surface
//! ([`EngineError::Validation { kind: Corruption, .. }`]). ff-sdk's
//! snapshot module historically owned this code and surfaced
//! `SdkError::Config`; the pre-migration wrapper still maps to that
//! shape so public ff-sdk callers see no behavior change while the
//! engine-side decoder moves.
//!
//! Stage 1c T3 adds [`build_execution_snapshot`] and
//! [`build_flow_snapshot`] alongside the edge decoder: every
//! engine-owned hash shape now parses through one canonical strict-parse
//! surface, freeing `ff-backend-valkey` to implement
//! `describe_execution` / `describe_flow` against the trait and letting
//! ff-sdk collapse its snapshot module into thin trait forwarders.
//!
//! [`EngineError::Validation { kind: Corruption, .. }`]: crate::engine_error::EngineError::Validation

use std::collections::{BTreeMap, HashMap};

use crate::contracts::{
    AttemptSummary, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot, LeaseSummary,
};
use crate::engine_error::{EngineError, ValidationKind};
use crate::state::PublicState;
use crate::types::{
    AttemptId, AttemptIndex, EdgeId, ExecutionId, FlowId, LaneId, LeaseEpoch, Namespace,
    TimestampMs, WaitpointId, WorkerInstanceId,
};

/// FF-owned fields on the flow-scoped `edge:<edge_id>` hash.
///
/// An HGETALL field outside this set signals on-disk corruption or
/// protocol drift — see [`build_edge_snapshot`]'s unknown-field
/// sweep. Kept `pub` so test fixtures / diagnostic tooling can share
/// the canonical list instead of hard-coding duplicates.
pub const EDGE_KNOWN_FIELDS: &[&str] = &[
    "edge_id",
    "flow_id",
    "upstream_execution_id",
    "downstream_execution_id",
    "dependency_kind",
    "satisfaction_condition",
    "data_passing_ref",
    "edge_state",
    "created_at",
    "created_by",
];

/// Assemble an [`EdgeSnapshot`] from the raw HGETALL field map.
///
/// Mirrors the pre-T2 ff-sdk free-fn: every validation gate (unknown
/// fields, missing required fields, identity cross-check against the
/// caller-supplied `flow_id`/`edge_id`) returns the same diagnostic
/// shape, just routed through [`EngineError::Validation`] with
/// [`ValidationKind::Corruption`] instead of `SdkError::Config`. The
/// pre-migration ff-sdk wrapper re-maps to `SdkError::Config` for
/// public-API parity; direct backend callers read the
/// `EngineError::Validation` payload.
///
/// `flow_id` + `edge_id` are the caller's expected identities. The
/// decoder verifies both are present and match the stored values; a
/// mismatch or absence surfaces as `Corruption` because it indicates
/// a wrong-key read or an on-disk drift.
pub fn build_edge_snapshot(
    flow_id: &FlowId,
    edge_id: &EdgeId,
    raw: &HashMap<String, String>,
) -> Result<EdgeSnapshot, EngineError> {
    // Unknown-field sweep — reject eagerly so a future FF rename that
    // landed a new field surfaces as an explicit parse failure rather
    // than silently dropping data.
    for k in raw.keys() {
        if !EDGE_KNOWN_FIELDS.contains(&k.as_str()) {
            return Err(corruption(
                "edge_snapshot: edge_hash",
                None,
                &format!("has unexpected field '{k}' (protocol drift or corruption?)"),
            ));
        }
    }

    // edge_id cross-check.
    let stored_edge_id_str = required(raw, "edge_snapshot: edge_hash", "edge_id")?;
    if stored_edge_id_str != edge_id.to_string() {
        return Err(corruption(
            "edge_snapshot: edge_hash",
            Some("edge_id"),
            &format!(
                "'{stored_edge_id_str}' does not match requested edge_id \
                 '{edge_id}' (key corruption or wrong-key read?)"
            ),
        ));
    }

    // flow_id cross-check.
    let stored_flow_id_str = required(raw, "edge_snapshot: edge_hash", "flow_id")?;
    if stored_flow_id_str != flow_id.to_string() {
        return Err(corruption(
            "edge_snapshot: edge_hash",
            Some("flow_id"),
            &format!(
                "'{stored_flow_id_str}' does not match requested flow_id \
                 '{flow_id}' (key corruption or wrong-key read?)"
            ),
        ));
    }

    let upstream_execution_id = parse_eid(raw, "upstream_execution_id")?;
    let downstream_execution_id = parse_eid(raw, "downstream_execution_id")?;

    let dependency_kind = required(raw, "edge_snapshot: edge_hash", "dependency_kind")?;
    let satisfaction_condition =
        required(raw, "edge_snapshot: edge_hash", "satisfaction_condition")?;

    // data_passing_ref is stored as "" when the stager passed None.
    // Treat empty as absent rather than surfacing an empty String.
    let data_passing_ref = raw
        .get("data_passing_ref")
        .filter(|s| !s.is_empty())
        .cloned();

    let edge_state = required(raw, "edge_snapshot: edge_hash", "edge_state")?;

    let created_at = parse_ts_required(raw, "edge_snapshot: edge_hash", "created_at")?;
    let created_by = required(raw, "edge_snapshot: edge_hash", "created_by")?;

    Ok(EdgeSnapshot::new(
        edge_id.clone(),
        flow_id.clone(),
        upstream_execution_id,
        downstream_execution_id,
        dependency_kind,
        satisfaction_condition,
        data_passing_ref,
        edge_state,
        created_at,
        created_by,
    ))
}

/// Format a `Corruption` detail string in the
/// `"<context>: <field?>: <message>"` shape documented on
/// [`ValidationKind::Corruption`].
fn corruption(context: &str, field: Option<&str>, message: &str) -> EngineError {
    let detail = match field {
        Some(f) => format!("{context}: {f}: {message}"),
        None => format!("{context}: {message}"),
    };
    EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail,
    }
}

/// Fetch a required non-empty string field, emitting a `Corruption`
/// error when the field is absent or empty.
fn required(
    raw: &HashMap<String, String>,
    context: &str,
    field: &str,
) -> Result<String, EngineError> {
    raw.get(field)
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or_else(|| {
            corruption(
                context,
                Some(field),
                "is missing or empty (key corruption?)",
            )
        })
}

/// Parse a ms-timestamp field that must be present.
fn parse_ts_required(
    raw: &HashMap<String, String>,
    context: &str,
    field: &str,
) -> Result<TimestampMs, EngineError> {
    let s = required(raw, context, field)?;
    let ms: i64 = s.parse().map_err(|e| {
        corruption(
            context,
            Some(field),
            &format!("is not a valid ms timestamp ('{s}'): {e}"),
        )
    })?;
    Ok(TimestampMs::from_millis(ms))
}

/// Parse a required ExecutionId field.
fn parse_eid(raw: &HashMap<String, String>, field: &str) -> Result<ExecutionId, EngineError> {
    let s = required(raw, "edge_snapshot: edge_hash", field)?;
    ExecutionId::parse(&s).map_err(|e| {
        corruption(
            "edge_snapshot: edge_hash",
            Some(field),
            &format!("'{s}' is not a valid ExecutionId (key corruption?): {e}"),
        )
    })
}

// ═══════════════════════════════════════════════════════════════════════
// execution decoder (describe_execution)
// ═══════════════════════════════════════════════════════════════════════

/// Assemble an [`ExecutionSnapshot`] from the raw HGETALL field maps.
///
/// `core` is the HGETALL of `exec_core`, `tags_raw` the HGETALL of the
/// sibling tags hash (which may be empty for executions created without
/// tags). Every parse failure surfaces as
/// [`EngineError::Validation { kind: Corruption, .. }`] — fields that
/// FCALLs write atomically are strict-required, while fields that clear
/// on transition (`blocking_reason`, `current_attempt_id`, etc.) are
/// treated as absent when empty.
pub fn build_execution_snapshot(
    execution_id: ExecutionId,
    core: &HashMap<String, String>,
    tags_raw: HashMap<String, String>,
) -> Result<Option<ExecutionSnapshot>, EngineError> {
    let ctx = "describe_execution: exec_core";

    let public_state = parse_public_state(opt_str(core, "public_state").unwrap_or(""))?;

    // `LaneId::try_new` validates non-empty + ASCII-printable + <= 64 bytes.
    // Exec_core writes a LaneId that already passed these invariants at
    // ingress; a read that fails validation here signals on-disk
    // corruption — surface it rather than silently constructing an
    // invalid LaneId that would mis-partition downstream.
    let lane_id = LaneId::try_new(opt_str(core, "lane_id").unwrap_or("")).map_err(|e| {
        corruption(
            ctx,
            Some("lane_id"),
            &format!("fails LaneId validation (key corruption?): {e}"),
        )
    })?;

    let namespace_str = opt_str(core, "namespace").unwrap_or("").to_owned();
    let namespace = Namespace::new(namespace_str);

    let flow_id = opt_str(core, "flow_id")
        .filter(|s| !s.is_empty())
        .map(|s| {
            FlowId::parse(s).map_err(|e| {
                corruption(
                    ctx,
                    Some("flow_id"),
                    &format!("is not a valid UUID (key corruption?): {e}"),
                )
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
    let created_at = parse_ts(core, ctx, "created_at")?.ok_or_else(|| {
        corruption(
            ctx,
            Some("created_at"),
            "is missing or empty (key corruption?)",
        )
    })?;
    let last_mutation_at = parse_ts(core, ctx, "last_mutation_at")?.ok_or_else(|| {
        corruption(
            ctx,
            Some("last_mutation_at"),
            "is missing or empty (key corruption?)",
        )
    })?;

    let total_attempt_count: u32 =
        parse_u32_strict(core, ctx, "total_attempt_count")?.unwrap_or(0);

    let current_attempt = build_attempt_summary(core)?;
    let current_lease = build_lease_summary(core)?;

    let current_waitpoint = opt_str(core, "current_waitpoint_id")
        .filter(|s| !s.is_empty())
        .map(|s| {
            WaitpointId::parse(s).map_err(|e| {
                corruption(
                    ctx,
                    Some("current_waitpoint_id"),
                    &format!("is not a valid UUID (key corruption?): {e}"),
                )
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
) -> Result<Option<TimestampMs>, EngineError> {
    match opt_str(map, field).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => {
            let ms: i64 = raw.parse().map_err(|e| {
                corruption(
                    context,
                    Some(field),
                    &format!("is not a valid ms timestamp ('{raw}'): {e}"),
                )
            })?;
            Ok(Some(TimestampMs::from_millis(ms)))
        }
    }
}

/// Strictly parse a `u32` field. Returns `Ok(None)` when the field is
/// absent or empty (a valid pre-write state), `Err` when the value is
/// present but unparseable (on-disk corruption).
fn parse_u32_strict(
    map: &HashMap<String, String>,
    context: &str,
    field: &str,
) -> Result<Option<u32>, EngineError> {
    match opt_str(map, field).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => Ok(Some(raw.parse().map_err(|e| {
            corruption(
                context,
                Some(field),
                &format!("is not a valid u32 ('{raw}'): {e}"),
            )
        })?)),
    }
}

/// Strictly parse a `u64` field. Semantics mirror [`parse_u32_strict`].
fn parse_u64_strict(
    map: &HashMap<String, String>,
    context: &str,
    field: &str,
) -> Result<Option<u64>, EngineError> {
    match opt_str(map, field).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => Ok(Some(raw.parse().map_err(|e| {
            corruption(
                context,
                Some(field),
                &format!("is not a valid u64 ('{raw}'): {e}"),
            )
        })?)),
    }
}

fn parse_public_state(raw: &str) -> Result<PublicState, EngineError> {
    // exec_core stores the snake_case literal (e.g. "waiting"). PublicState's
    // Deserialize accepts the JSON-quoted form, so wrap + delegate.
    let quoted = format!("\"{raw}\"");
    serde_json::from_str(&quoted).map_err(|e| {
        corruption(
            "describe_execution: exec_core",
            Some("public_state"),
            &format!("'{raw}' is not a known public state: {e}"),
        )
    })
}

fn build_attempt_summary(
    core: &HashMap<String, String>,
) -> Result<Option<AttemptSummary>, EngineError> {
    let ctx = "describe_execution: exec_core";
    let attempt_id_str = match opt_str(core, "current_attempt_id").filter(|s| !s.is_empty()) {
        None => return Ok(None),
        Some(s) => s,
    };
    let attempt_id = AttemptId::parse(attempt_id_str).map_err(|e| {
        corruption(
            ctx,
            Some("current_attempt_id"),
            &format!("is not a valid UUID: {e}"),
        )
    })?;
    // When `current_attempt_id` is set, `current_attempt_index` MUST be
    // set too — lua/execution.lua writes both atomically in
    // `ff_claim_execution`. A missing index while the id is populated
    // is corruption, not a valid intermediate state.
    let attempt_index = parse_u32_strict(core, ctx, "current_attempt_index")?.ok_or_else(|| {
        corruption(
            ctx,
            Some("current_attempt_index"),
            "is missing while current_attempt_id is set (key corruption?)",
        )
    })?;
    Ok(Some(AttemptSummary::new(
        attempt_id,
        AttemptIndex::new(attempt_index),
    )))
}

fn build_lease_summary(
    core: &HashMap<String, String>,
) -> Result<Option<LeaseSummary>, EngineError> {
    let ctx = "describe_execution: exec_core";
    // A lease is "held" when the worker_instance_id field is populated
    // AND lease_expires_at is set. Both clear together on revoke/expire
    // (see clear_lease_and_indexes in lua/helpers.lua).
    let wid_str = match opt_str(core, "current_worker_instance_id").filter(|s| !s.is_empty()) {
        None => return Ok(None),
        Some(s) => s,
    };
    let expires_at = match parse_ts(core, ctx, "lease_expires_at")? {
        None => return Ok(None),
        Some(ts) => ts,
    };
    // A lease is only "held" if the epoch is present too — lua/helpers.lua
    // sets/clears epoch atomically with wid + expires_at. Parse strictly
    // and require it: a missing epoch alongside a live wid is corruption.
    let epoch = parse_u64_strict(core, ctx, "current_lease_epoch")?.ok_or_else(|| {
        corruption(
            ctx,
            Some("current_lease_epoch"),
            "is missing while current_worker_instance_id is set (key corruption?)",
        )
    })?;
    Ok(Some(LeaseSummary::new(
        LeaseEpoch::new(epoch),
        WorkerInstanceId::new(wid_str.to_owned()),
        expires_at,
    )))
}

// ═══════════════════════════════════════════════════════════════════════
// flow decoder (describe_flow)
// ═══════════════════════════════════════════════════════════════════════

/// FF-owned snake_case fields on flow_core. Any HGETALL field NOT in
/// this set AND matching the `^[a-z][a-z0-9_]*\.` namespaced-tag shape
/// is surfaced on [`FlowSnapshot::tags`]. Fields that are neither FF-
/// owned nor namespaced (unexpected shapes) are surfaced as a
/// `Corruption` error so on-disk corruption or protocol drift fails loud.
pub const FLOW_CORE_KNOWN_FIELDS: &[&str] = &[
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

/// Assemble a [`FlowSnapshot`] from the raw HGETALL field map.
///
/// Cross-checks the stored `flow_id` against the caller's expected id.
/// Unknown fields that match the `^[a-z][a-z0-9_]*\.` namespaced-tag
/// shape are routed to `tags`; any other unknown field surfaces as
/// `Corruption`.
pub fn build_flow_snapshot(
    flow_id: FlowId,
    raw: &HashMap<String, String>,
    edge_groups: Vec<crate::contracts::EdgeGroupSnapshot>,
) -> Result<FlowSnapshot, EngineError> {
    let ctx = "describe_flow: flow_core";

    // flow_id cross-check — corruption or wrong-key read.
    let stored_flow_id_str = opt_str(raw, "flow_id")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| corruption(ctx, Some("flow_id"), "is missing or empty (key corruption?)"))?;
    if stored_flow_id_str != flow_id.to_string() {
        return Err(corruption(
            ctx,
            Some("flow_id"),
            &format!(
                "'{stored_flow_id_str}' does not match requested flow_id \
                 '{flow_id}' (key corruption or wrong-key read?)"
            ),
        ));
    }

    let namespace_str = opt_str(raw, "namespace")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            corruption(ctx, Some("namespace"), "is missing or empty (key corruption?)")
        })?;
    let namespace = Namespace::new(namespace_str.to_owned());

    let flow_kind = opt_str(raw, "flow_kind")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            corruption(ctx, Some("flow_kind"), "is missing or empty (key corruption?)")
        })?
        .to_owned();

    let public_flow_state = opt_str(raw, "public_flow_state")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            corruption(
                ctx,
                Some("public_flow_state"),
                "is missing or empty (key corruption?)",
            )
        })?
        .to_owned();

    let graph_revision = parse_u64_strict(raw, ctx, "graph_revision")?
        .ok_or_else(|| corruption(ctx, Some("graph_revision"), "is missing (key corruption?)"))?;
    let node_count = parse_u32_strict(raw, ctx, "node_count")?
        .ok_or_else(|| corruption(ctx, Some("node_count"), "is missing (key corruption?)"))?;
    let edge_count = parse_u32_strict(raw, ctx, "edge_count")?
        .ok_or_else(|| corruption(ctx, Some("edge_count"), "is missing (key corruption?)"))?;

    let created_at = parse_ts(raw, ctx, "created_at")?.ok_or_else(|| {
        corruption(
            ctx,
            Some("created_at"),
            "is missing or empty (key corruption?)",
        )
    })?;
    let last_mutation_at = parse_ts(raw, ctx, "last_mutation_at")?.ok_or_else(|| {
        corruption(
            ctx,
            Some("last_mutation_at"),
            "is missing or empty (key corruption?)",
        )
    })?;

    let cancelled_at = parse_ts(raw, ctx, "cancelled_at")?;
    let cancel_reason = opt_str(raw, "cancel_reason")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let cancellation_policy = opt_str(raw, "cancellation_policy")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    // Route unknown fields: namespaced-prefix (e.g. `cairn.task_id`) →
    // tags; anything else → corruption.
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in raw {
        if FLOW_CORE_KNOWN_FIELDS.contains(&k.as_str()) {
            continue;
        }
        if is_namespaced_tag_key(k) {
            tags.insert(k.clone(), v.clone());
        } else {
            return Err(corruption(
                ctx,
                None,
                &format!(
                    "has unexpected field '{k}' — not an FF field and not a namespaced \
                     tag (lowercase-alphanumeric-prefix + '.')"
                ),
            ));
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
        edge_groups,
    ))
}

/// Match the namespaced-tag shape `^[a-z][a-z0-9_]*\.` documented on
/// [`ExecutionSnapshot::tags`] / [`FlowSnapshot::tags`]. Kept inline
/// (no regex dependency) — the shape is tight enough to hand-check.
pub(crate) fn is_namespaced_tag_key(k: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::partition::PartitionConfig;

    fn fid() -> FlowId {
        FlowId::new()
    }

    fn eids_for_flow(f: &FlowId) -> (ExecutionId, ExecutionId) {
        let cfg = PartitionConfig::default();
        (
            ExecutionId::for_flow(f, &cfg),
            ExecutionId::for_flow(f, &cfg),
        )
    }

    fn minimal_edge_hash(
        flow: &FlowId,
        edge: &EdgeId,
        up: &ExecutionId,
        down: &ExecutionId,
    ) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("edge_id".into(), edge.to_string());
        m.insert("flow_id".into(), flow.to_string());
        m.insert("upstream_execution_id".into(), up.to_string());
        m.insert("downstream_execution_id".into(), down.to_string());
        m.insert("dependency_kind".into(), "success_only".into());
        m.insert("satisfaction_condition".into(), "all_required".into());
        m.insert("data_passing_ref".into(), String::new());
        m.insert("edge_state".into(), "pending".into());
        m.insert("created_at".into(), "1234".into());
        m.insert("created_by".into(), "engine".into());
        m
    }

    #[test]
    fn round_trips_all_fields() {
        let f = fid();
        let edge = EdgeId::new();
        let (up, down) = eids_for_flow(&f);
        let raw = minimal_edge_hash(&f, &edge, &up, &down);
        let snap = build_edge_snapshot(&f, &edge, &raw).unwrap();
        assert_eq!(snap.edge_id, edge);
        assert_eq!(snap.flow_id, f);
        assert_eq!(snap.upstream_execution_id, up);
        assert_eq!(snap.downstream_execution_id, down);
        assert_eq!(snap.dependency_kind, "success_only");
        assert_eq!(snap.satisfaction_condition, "all_required");
        assert!(snap.data_passing_ref.is_none());
        assert_eq!(snap.edge_state, "pending");
        assert_eq!(snap.created_at.0, 1234);
        assert_eq!(snap.created_by, "engine");
    }

    #[test]
    fn data_passing_ref_round_trips_when_set() {
        let f = fid();
        let edge = EdgeId::new();
        let (up, down) = eids_for_flow(&f);
        let mut raw = minimal_edge_hash(&f, &edge, &up, &down);
        raw.insert("data_passing_ref".into(), "ref://blob-42".into());
        let snap = build_edge_snapshot(&f, &edge, &raw).unwrap();
        assert_eq!(snap.data_passing_ref.as_deref(), Some("ref://blob-42"));
    }

    fn expect_corruption(err: EngineError) -> String {
        match err {
            EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail,
            } => detail,
            other => panic!("expected Validation::Corruption, got {other:?}"),
        }
    }

    #[test]
    fn unknown_field_fails_loud() {
        let f = fid();
        let edge = EdgeId::new();
        let (up, down) = eids_for_flow(&f);
        let mut raw = minimal_edge_hash(&f, &edge, &up, &down);
        raw.insert("bogus_future_field".into(), "v".into());
        let detail = expect_corruption(build_edge_snapshot(&f, &edge, &raw).unwrap_err());
        assert!(detail.contains("bogus_future_field"), "{detail}");
    }

    #[test]
    fn flow_id_mismatch_fails_loud() {
        let f = fid();
        let other = fid();
        let edge = EdgeId::new();
        let (up, down) = eids_for_flow(&f);
        let raw = minimal_edge_hash(&other, &edge, &up, &down);
        let detail = expect_corruption(build_edge_snapshot(&f, &edge, &raw).unwrap_err());
        assert!(detail.contains("flow_id"), "{detail}");
        assert!(detail.contains("does not match"), "{detail}");
    }

    #[test]
    fn edge_id_mismatch_fails_loud() {
        let f = fid();
        let edge = EdgeId::new();
        let other_edge = EdgeId::new();
        let (up, down) = eids_for_flow(&f);
        let raw = minimal_edge_hash(&f, &other_edge, &up, &down);
        let detail = expect_corruption(build_edge_snapshot(&f, &edge, &raw).unwrap_err());
        assert!(detail.contains("edge_id"), "{detail}");
        assert!(detail.contains("does not match"), "{detail}");
    }

    #[test]
    fn missing_required_fields_fail_loud() {
        for want in [
            "edge_id",
            "flow_id",
            "upstream_execution_id",
            "downstream_execution_id",
            "dependency_kind",
            "satisfaction_condition",
            "edge_state",
            "created_at",
            "created_by",
        ] {
            let f = fid();
            let edge = EdgeId::new();
            let (up, down) = eids_for_flow(&f);
            let mut raw = minimal_edge_hash(&f, &edge, &up, &down);
            raw.remove(want);
            let err = build_edge_snapshot(&f, &edge, &raw)
                .err()
                .unwrap_or_else(|| panic!("missing {want} should fail"));
            let detail = expect_corruption(err);
            assert!(detail.contains(want), "detail for {want}: {detail}");
        }
    }

    #[test]
    fn malformed_created_at_fails_loud() {
        let f = fid();
        let edge = EdgeId::new();
        let (up, down) = eids_for_flow(&f);
        let mut raw = minimal_edge_hash(&f, &edge, &up, &down);
        raw.insert("created_at".into(), "not-a-number".into());
        let detail = expect_corruption(build_edge_snapshot(&f, &edge, &raw).unwrap_err());
        assert!(detail.contains("created_at"), "{detail}");
    }

    #[test]
    fn malformed_upstream_eid_fails_loud() {
        let f = fid();
        let edge = EdgeId::new();
        let (up, down) = eids_for_flow(&f);
        let mut raw = minimal_edge_hash(&f, &edge, &up, &down);
        raw.insert("upstream_execution_id".into(), "not-an-execution-id".into());
        let detail = expect_corruption(build_edge_snapshot(&f, &edge, &raw).unwrap_err());
        assert!(detail.contains("upstream_execution_id"), "{detail}");
    }

    // ─── ExecutionSnapshot (describe_execution) ───────────────────────

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

    fn expect_corruption_field<F>(err: EngineError, pred: F)
    where
        F: FnOnce(&str) -> bool,
    {
        let detail = expect_corruption(err);
        assert!(pred(&detail), "detail did not match predicate: {detail}");
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
        expect_corruption_field(err, |d| d.contains("public_state"));
    }

    #[test]
    fn invalid_lane_id_fails_loud() {
        let mut core = minimal_core("waiting");
        core.insert("lane_id".to_owned(), "lane\nbroken".to_owned());
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        expect_corruption_field(err, |d| d.contains("lane_id"));
    }

    #[test]
    fn missing_required_timestamps_fail_loud() {
        for want in ["created_at", "last_mutation_at"] {
            let mut core = minimal_core("waiting");
            core.remove(want);
            let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
            expect_corruption_field(err, |d| d.contains(want));
        }
    }

    #[test]
    fn malformed_total_attempt_count_fails_loud() {
        let mut core = minimal_core("waiting");
        core.insert("total_attempt_count".to_owned(), "not-a-number".to_owned());
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        expect_corruption_field(err, |d| d.contains("total_attempt_count"));
    }

    #[test]
    fn attempt_id_without_index_fails_loud() {
        let mut core = minimal_core("active");
        core.insert(
            "current_attempt_id".to_owned(),
            AttemptId::new().to_string(),
        );
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        expect_corruption_field(err, |d| d.contains("current_attempt_index"));
    }

    #[test]
    fn lease_without_epoch_fails_loud() {
        let mut core = minimal_core("active");
        core.insert(
            "current_worker_instance_id".to_owned(),
            "w-inst-1".to_owned(),
        );
        core.insert("lease_expires_at".to_owned(), "9000".to_owned());
        let err = build_execution_snapshot(eid(), &core, HashMap::new()).unwrap_err();
        expect_corruption_field(err, |d| d.contains("current_lease_epoch"));
    }

    #[test]
    fn lease_summary_requires_both_wid_and_expires_at() {
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

    // ─── FlowSnapshot (describe_flow) ─────────────────────────────────

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
        let snap = build_flow_snapshot(f.clone(), &minimal_flow_core(&f, "open"), Vec::new()).unwrap();
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
        let snap = build_flow_snapshot(f, &core, Vec::new()).unwrap();
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
        let snap = build_flow_snapshot(f, &core, Vec::new()).unwrap();
        assert_eq!(snap.tags.len(), 3);
        let keys: Vec<_> = snap.tags.keys().cloned().collect();
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
        let f = fid();
        let mut core = minimal_flow_core(&f, "open");
        core.insert("bogus_future_field".to_owned(), "v".to_owned());
        let err = build_flow_snapshot(f, &core, Vec::new()).unwrap_err();
        expect_corruption_field(err, |d| d.contains("bogus_future_field"));
    }

    #[test]
    fn missing_required_flow_fields_fail_loud() {
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
            let err = build_flow_snapshot(f, &core, Vec::new()).err().unwrap_or_else(|| {
                panic!("field {want} should fail but build_flow_snapshot returned Ok")
            });
            expect_corruption_field(err, |d| d.contains(want));
        }
    }

    #[test]
    fn empty_required_strings_fail_loud() {
        for want in ["flow_id", "namespace", "flow_kind", "public_flow_state"] {
            let f = fid();
            let mut core = minimal_flow_core(&f, "open");
            core.insert(want.to_owned(), String::new());
            let err = build_flow_snapshot(f, &core, Vec::new()).err().unwrap_or_else(|| {
                panic!("empty {want} should fail but build_flow_snapshot returned Ok")
            });
            expect_corruption_field(err, |d| d.contains(want));
        }
    }

    #[test]
    fn flow_snapshot_flow_id_mismatch_fails_loud() {
        let requested = fid();
        let other = fid();
        let core = minimal_flow_core(&other, "open");
        let err = build_flow_snapshot(requested, &core, Vec::new()).unwrap_err();
        expect_corruption_field(err, |d| d.contains("flow_id") && d.contains("does not match"));
    }

    #[test]
    fn malformed_counter_fails_loud() {
        let f = fid();
        let mut core = minimal_flow_core(&f, "open");
        core.insert("graph_revision".to_owned(), "not-a-number".to_owned());
        let err = build_flow_snapshot(f, &core, Vec::new()).unwrap_err();
        expect_corruption_field(err, |d| d.contains("graph_revision"));
    }

    #[test]
    fn namespaced_tag_matcher_boundaries() {
        assert!(is_namespaced_tag_key("cairn.task_id"));
        assert!(is_namespaced_tag_key("a.b"));
        assert!(is_namespaced_tag_key("ab_12.field"));
        assert!(!is_namespaced_tag_key("cairn_task_id"));
        assert!(!is_namespaced_tag_key("Cairn.task"));
        assert!(!is_namespaced_tag_key("1cairn.task"));
        assert!(!is_namespaced_tag_key(""));
        assert!(!is_namespaced_tag_key(".x"));
        assert!(!is_namespaced_tag_key("caIrn.task"));
    }
}
