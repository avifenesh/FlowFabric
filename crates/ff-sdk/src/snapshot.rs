//! Typed read-models that decouple consumers from FF's storage engine.
//!
//! **RFC-012 Stage 1c T3 + issue #160:** this module is a pure thin
//! forwarder onto the `EngineBackend` trait. The decoders
//! (`build_execution_snapshot`, `build_flow_snapshot`,
//! `build_edge_snapshot`) live in `ff_core::contracts::decode` and
//! the pipeline bodies that invoke them live on
//! [`ff_backend_valkey::ValkeyBackend`] as `EngineBackend` trait
//! methods. The five `describe_*` / `list_*_edges` functions here are
//! 3-line delegations to `self.backend`; `list_incoming_edges` /
//! `list_outgoing_edges` keep their `resolve_flow_id` step (the
//! trait's `list_edges` requires a `FlowId` argument) but route it
//! through [`EngineBackend::resolve_execution_flow_id`] plus a local
//! RFC-011 co-location cross-check.
//!
//! See issue #58 for the strategic context (engine-surface sealing
//! toward the Postgres backend port); issue #160 closed out the last
//! raw-client call sites so `FlowFabricWorker::client()` could be
//! deleted.
//!
//! Snapshot types (`ExecutionSnapshot`, `AttemptSummary`, `LeaseSummary`,
//! `FlowSnapshot`, `EdgeSnapshot`) live in [`ff_core::contracts`] so
//! non-SDK consumers (tests, REST server, alternate backends) share
//! them without depending on ff-sdk.

use ff_core::contracts::{
    EdgeDirection, EdgeSnapshot, ExecutionSnapshot, FlowSnapshot, ListFlowsPage, ListLanesPage,
};
use ff_core::partition::{execution_partition, flow_partition, PartitionKey};
use ff_core::types::{EdgeId, ExecutionId, FlowId, LaneId};

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

    /// List flows on a partition with cursor-based pagination
    /// (issue #185).
    ///
    /// `partition` is an opaque [`PartitionKey`] — typically obtained
    /// from a [`crate::ClaimGrant`] or from
    /// [`ff_core::partition::flow_partition`] when the caller already
    /// knows a `FlowId` on the partition it wants to enumerate.
    /// `cursor` is `None` on the first call and the previous page's
    /// `next_cursor` on subsequent calls; iteration terminates when
    /// the returned `next_cursor` is `None`.
    ///
    /// Thin forwarder onto
    /// [`EngineBackend::list_flows`](ff_core::engine_backend::EngineBackend::list_flows).
    pub async fn list_flows(
        &self,
        partition: PartitionKey,
        cursor: Option<FlowId>,
        limit: usize,
    ) -> Result<ListFlowsPage, SdkError> {
        Ok(self
            .backend_ref()
            .list_flows(partition, cursor, limit)
            .await?)
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
    ///
    /// Post-#160 this is a thin forwarder onto the bundled
    /// [`EngineBackend::describe_edge`](ff_core::engine_backend::EngineBackend::describe_edge)
    /// impl.
    pub async fn describe_edge(
        &self,
        flow_id: &FlowId,
        edge_id: &EdgeId,
    ) -> Result<Option<EdgeSnapshot>, SdkError> {
        Ok(self.backend_ref().describe_edge(flow_id, edge_id).await?)
    }

    /// List all outgoing dependency edges originating from an execution.
    ///
    /// Returns an empty `Vec` when the execution has no outgoing edges
    /// — including standalone executions not attached to any flow.
    ///
    /// # Reads
    ///
    /// 1. `EngineBackend::resolve_execution_flow_id` (single HGET on
    ///    `exec_core.flow_id` for the Valkey backend).
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

    /// Enumerate registered lanes with cursor-based pagination.
    ///
    /// Thin forwarder onto
    /// [`EngineBackend::list_lanes`](ff_core::engine_backend::EngineBackend::list_lanes).
    /// Lanes are global (not partition-scoped); the Valkey backend
    /// serves this from the `ff:idx:lanes` SET, sorts by lane name,
    /// and returns a `limit`-sized page starting after `cursor`
    /// (exclusive). Loop until [`ListLanesPage::next_cursor`] is
    /// `None` to read the full registry.
    pub async fn list_lanes(
        &self,
        cursor: Option<LaneId>,
        limit: usize,
    ) -> Result<ListLanesPage, SdkError> {
        Ok(self.backend_ref().list_lanes(cursor, limit).await?)
    }

    /// Resolve `exec_core.flow_id` via the trait and pin the RFC-011
    /// co-location invariant
    /// (`execution_partition(eid) == flow_partition(flow_id)`). A
    /// parsed-but-wrong flow_id would otherwise silently route the
    /// follow-up adjacency reads to the wrong partition and return
    /// bogus empty results. A mismatch surfaces as
    /// `SdkError::Engine(EngineError::Validation { kind: Corruption, .. })`.
    ///
    /// Returns `None` when the execution has no owning flow
    /// (standalone) or when exec_core is absent.
    async fn resolve_flow_id(&self, eid: &ExecutionId) -> Result<Option<FlowId>, SdkError> {
        let Some(flow_id) = self.backend_ref().resolve_execution_flow_id(eid).await? else {
            return Ok(None);
        };
        let exec_partition_index = execution_partition(eid, self.partition_config()).index;
        let flow_partition_index = flow_partition(&flow_id, self.partition_config()).index;
        if exec_partition_index != flow_partition_index {
            return Err(SdkError::from(
                ff_core::engine_error::EngineError::Validation {
                    kind: ff_core::engine_error::ValidationKind::Corruption,
                    detail: format!(
                        "list_edges: exec_core: flow_id: '{flow_id}' partition \
                         {flow_partition_index} does not match execution partition \
                         {exec_partition_index} (RFC-011 co-location violation; \
                         key corruption?)"
                    ),
                },
            ));
        }
        Ok(Some(flow_id))
    }
}
