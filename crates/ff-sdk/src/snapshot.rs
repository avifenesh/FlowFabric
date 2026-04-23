//! Typed read-models that decouple consumers from FF's storage engine.
//!
//! **RFC-012 Stage 1c T3:** after T3 this module is thin forwarders.
//! The decoders (`build_execution_snapshot`, `build_flow_snapshot`,
//! `build_edge_snapshot`) live in `ff_core::contracts::decode` and
//! the pipeline bodies that invoke them live on
//! [`ff_backend_valkey::ValkeyBackend`] as `EngineBackend` trait
//! methods. The three `describe_*` functions here are now 3-line
//! delegations to `self.backend`; `list_incoming_edges` /
//! `list_outgoing_edges` keep their `resolve_flow_id` step (the
//! trait's `list_edges` requires a `FlowId` argument) and then call
//! the trait method.
//!
//! See issue #58 for the strategic context (engine-surface sealing
//! toward the Postgres backend port).
//!
//! Snapshot types (`ExecutionSnapshot`, `AttemptSummary`, `LeaseSummary`,
//! `FlowSnapshot`, `EdgeSnapshot`) live in [`ff_core::contracts`] so
//! non-SDK consumers (tests, REST server, alternate backends) share
//! them without depending on ff-sdk.

use std::collections::HashMap;

use ff_core::contracts::{EdgeDirection, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot};
use ff_core::keys::ExecKeyContext;
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::{EdgeId, ExecutionId, FlowId};

use crate::SdkError;
use crate::worker::FlowFabricWorker;

impl FlowFabricWorker {
    /// Read a typed snapshot of one execution. See
    /// [`ExecutionSnapshot`] for field semantics.
    ///
    /// Post-T3 this is a thin forwarder onto the bundled
    /// [`EngineBackend::describe_execution`](ff_core::engine_backend::EngineBackend::describe_execution)
    /// impl; the HGETALL pipeline body and strict-parse decoder both
    /// live in `ff-backend-valkey` / `ff-core` so alternate backends
    /// can serve the same call shape. Parse failures surface as
    /// `SdkError::Engine(EngineError::Validation { kind: Corruption, .. })`.
    pub async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, SdkError> {
        Ok(self.backend_ref().describe_execution(id).await?)
    }

    /// Read a typed snapshot of one flow. See [`FlowSnapshot`] for
    /// field semantics.
    ///
    /// Post-T3 this is a thin forwarder onto the bundled
    /// [`EngineBackend::describe_flow`](ff_core::engine_backend::EngineBackend::describe_flow)
    /// impl.
    pub async fn describe_flow(&self, id: &FlowId) -> Result<Option<FlowSnapshot>, SdkError> {
        Ok(self.backend_ref().describe_flow(id).await?)
    }

    /// Read a typed snapshot of one dependency edge.
    ///
    /// Takes both `flow_id` and `edge_id`: the edge hash is stored
    /// under the flow's partition
    /// (`ff:flow:{fp:N}:<flow_id>:edge:<edge_id>`) and FF does not
    /// maintain a global `edge_id -> flow_id` index. The caller
    /// already knows the flow from the staging call result or the
    /// consumer's own metadata.
    ///
    /// Returns `Ok(None)` when the edge hash is absent (never staged,
    /// or staged under a different flow).
    pub async fn describe_edge(
        &self,
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, SdkError> {
        let partition = flow_partition(flow_id, self.partition_config());
        let ctx = ff_core::keys::FlowKeyContext::new(&partition, flow_id);
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

        ff_core::contracts::decode::build_edge_snapshot(flow_id, edge_id, &raw)
            .map(Some)
            .map_err(SdkError::from)
    }

    /// List all outgoing dependency edges originating from an execution.
    ///
    /// Returns an empty `Vec` when the execution has no outgoing edges
    /// — including standalone executions not attached to any flow.
    ///
    /// # Reads
    ///
    /// 1. `HGET exec_core flow_id` (via [`Self::resolve_flow_id`]).
    /// 2. `EngineBackend::list_edges` on the resolved flow (SMEMBERS
    ///    + pipelined HGETALL on the flow's partition).
    ///
    /// Ordering is unspecified — the adjacency set is an unordered
    /// SET. Callers that need deterministic order should sort by
    /// [`EdgeSnapshot::edge_id`] or `created_at`.
    pub async fn list_outgoing_edges(
        &self,
        upstream_eid: &ExecutionId,
    ) -> Result<Vec<EdgeSnapshot>, SdkError> {
        let Some(flow_id) = self.resolve_flow_id(upstream_eid).await? else {
            return Ok(Vec::new());
        };
        Ok(self
            .backend_ref()
            .list_edges(
                &flow_id,
                EdgeDirection::Outgoing {
                    from_node: upstream_eid.clone(),
                },
            )
            .await?)
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
        Ok(self
            .backend_ref()
            .list_edges(
                &flow_id,
                EdgeDirection::Incoming {
                    to_node: downstream_eid.clone(),
                },
            )
            .await?)
    }

    /// `HGET exec_core.flow_id` and parse to a [`FlowId`]. `None` when
    /// the exec_core hash is absent OR the flow_id field is empty
    /// (standalone execution).
    ///
    /// Also pins the RFC-011 co-location invariant
    /// (`execution_partition(eid) == flow_partition(flow_id)`) — a
    /// parsed-but-wrong flow_id would otherwise silently route the
    /// follow-up adjacency reads to the wrong partition and return
    /// bogus empty results. A mismatch surfaces as
    /// `SdkError::Engine(EngineError::Validation { kind: Corruption, .. })`.
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
        let flow_id = FlowId::parse(&raw).map_err(|e| {
            SdkError::from(ff_core::engine_error::EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::Corruption,
                detail: format!(
                    "list_edges: exec_core: flow_id: '{raw}' is not a valid UUID \
                     (key corruption?): {e}"
                ),
            })
        })?;
        let flow_partition_index = flow_partition(&flow_id, self.partition_config()).index;
        if exec_partition.index != flow_partition_index {
            return Err(SdkError::from(
                ff_core::engine_error::EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::Corruption,
                    detail: format!(
                        "list_edges: exec_core: flow_id: '{flow_id}' partition \
                         {flow_partition_index} does not match execution partition {} \
                         (RFC-011 co-location violation; key corruption?)",
                        exec_partition.index
                    ),
                },
            ));
        }
        Ok(Some(flow_id))
    }
}
