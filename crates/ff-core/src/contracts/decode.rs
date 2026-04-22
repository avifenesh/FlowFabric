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
//! Only the edge decoder lives here today. Future tranches may
//! migrate `build_execution_snapshot` / `build_flow_snapshot` to this
//! module; they currently live in `ff-sdk::snapshot` because their
//! pipelines are not yet trait-shaped.
//!
//! [`EngineError::Validation { kind: Corruption, .. }`]: crate::engine_error::EngineError::Validation

use std::collections::HashMap;

use crate::contracts::EdgeSnapshot;
use crate::engine_error::{EngineError, ValidationKind};
use crate::types::{EdgeId, ExecutionId, FlowId, TimestampMs};

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
}
