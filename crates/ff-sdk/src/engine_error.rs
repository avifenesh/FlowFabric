//! Compatibility shim preserving the `ff_sdk::engine_error` surface.
//!
//! **RFC-012 Stage 1a:** the `EngineError` enum and its sub-kinds
//! moved to `ff_core::engine_error`; the `From<ScriptError>` +
//! ScriptError-downcast helpers moved to
//! `ff_script::engine_error_ext`. This module re-exports both so
//! every `ff_sdk::EngineError` / `ff_sdk::engine_error::*` path used
//! by existing consumers (cairn-fabric, the SDK's own call sites)
//! continues to compile unchanged.
//!
//! The `enrich_dependency_conflict` helper is kept here (not moved)
//! because it performs a live `ferriskey::Client` round trip; that
//! dependency is ff-sdk–scoped.

pub use ff_core::engine_error::{
    BugKind, ConflictKind, ContentionKind, EngineError, StateKind, ValidationKind,
};
// `class` is backend-agnostic (returns `ErrorClass` which classifies
// any Transport whose source is a `ScriptError`; returns `Terminal`
// for non-ScriptError Transport). Re-exported unconditionally.
pub use ff_script::engine_error_ext::class;
// The three helpers below are Valkey-specific: `transport_script` /
// `transport_script_ref` name `ScriptError` (a Valkey-transport
// error), and `valkey_kind` returns `ferriskey::ErrorKind`. Gating
// them behind `valkey-default` keeps the `--no-default-features`
// public surface free of Valkey transport types, honouring the
// RFC-012 §1.3 agnosticism contract.
#[cfg(feature = "valkey-default")]
pub use ff_script::engine_error_ext::{transport_script, transport_script_ref, valkey_kind};

#[cfg(feature = "valkey-default")]
use std::collections::HashMap;

#[cfg(feature = "valkey-default")]
use ff_core::keys::FlowKeyContext;
#[cfg(feature = "valkey-default")]
use ff_core::partition::flow_partition;
#[cfg(feature = "valkey-default")]
use ff_core::types::{EdgeId, FlowId};
#[cfg(feature = "valkey-default")]
use ff_script::error::ScriptError;

/// Upgrade a bare `From<ScriptError>` translation of
/// `dependency_already_exists` into a fully-typed
/// [`ConflictKind::DependencyAlreadyExists`] by performing the
/// follow-up `HGETALL` on the edge hash.
///
/// Call sites that stage a dependency and receive
/// [`EngineError::Transport`] whose inner is
/// `ScriptError::DependencyAlreadyExists` use this helper to
/// upgrade the error before surfacing. On follow-up failure the
/// original `Transport` error is returned unchanged — the
/// strict-parse posture means a half-populated `Conflict` is
/// never constructed.
///
/// **RFC-012 Stage 1a:** converted from an inherent method on
/// `EngineError` to a free function because `EngineError` now lives in
/// ff-core (Rust disallows inherent impls on foreign types). The
/// signature and semantics are otherwise unchanged.
#[cfg(feature = "valkey-default")]
pub async fn enrich_dependency_conflict(
    client: &ferriskey::Client,
    partition_config: &ff_core::partition::PartitionConfig,
    flow_id: &FlowId,
    edge_id: &EdgeId,
) -> Result<EngineError, EngineError> {
    let partition = flow_partition(flow_id, partition_config);
    let ctx = FlowKeyContext::new(&partition, flow_id);
    let edge_key = ctx.edge(edge_id);

    let raw: HashMap<String, String> = match client
        .cmd("HGETALL")
        .arg(&edge_key)
        .execute()
        .await
    {
        Ok(raw) => raw,
        Err(transport) => {
            return Err(transport_script(ScriptError::Valkey(transport)));
        }
    };
    if raw.is_empty() {
        return Err(transport_script(ScriptError::DependencyAlreadyExists));
    }
    match crate::snapshot::build_edge_snapshot_public(flow_id, edge_id, &raw) {
        Ok(existing) => Ok(EngineError::Conflict(ConflictKind::DependencyAlreadyExists {
            existing,
        })),
        Err(_e) => Err(transport_script(ScriptError::DependencyAlreadyExists)),
    }
}
