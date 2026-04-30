//! Partition-aware dispatch for cross-partition operations.
//!
//! The PartitionRouter resolves an ExecutionId to its partition and provides
//! key contexts for Valkey operations. For Phase 1, a single ferriskey::Client
//! connection is shared across all partitions (the client handles cluster routing
//! internally via hash tags).

use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{
    Partition, PartitionConfig, PartitionFamily, execution_partition,
};
use ff_core::types::ExecutionId;

/// Routes execution operations to the correct partition.
///
/// In a Valkey Cluster deployment, the ferriskey client handles slot-level
/// routing transparently — all keys for a partition share the same `{p:N}`
/// hash tag, so they land on the same shard. The router's job is partition
/// computation and key context construction, not connection selection.
pub struct PartitionRouter {
    config: PartitionConfig,
}

impl PartitionRouter {
    pub fn new(config: PartitionConfig) -> Self {
        Self { config }
    }

    /// Resolve an execution ID to its partition.
    pub fn partition_for(&self, eid: &ExecutionId) -> Partition {
        execution_partition(eid, &self.config)
    }

    /// Build an ExecKeyContext for the given execution.
    pub fn exec_keys(&self, eid: &ExecutionId) -> ExecKeyContext {
        let partition = self.partition_for(eid);
        ExecKeyContext::new(&partition, eid)
    }

    /// Build IndexKeys for a given partition index.
    pub fn index_keys(&self, partition_index: u16) -> IndexKeys {
        let partition = Partition {
            family: PartitionFamily::Execution,
            index: partition_index,
        };
        IndexKeys::new(&partition)
    }

    /// The partition config.
    pub fn config(&self) -> &PartitionConfig {
        &self.config
    }

    /// Total number of flow partitions.
    ///
    /// Post-RFC-011: exec keys co-locate with their parent flow's partition
    /// under hash-tag routing, so this count governs exec routing too.
    /// There is no separate `num_execution_partitions`.
    pub fn num_flow_partitions(&self) -> u16 {
        self.config.num_flow_partitions
    }
}

// ── Cluster 4 relocation (PR-7b) ────────────────────────────────────
//
// The pre-PR-7b `dispatch_dependency_resolution` +
// `dispatch_via_postgres` free functions + `is_child_skipped_result`
// helper lived here. They have been moved behind
// `EngineBackend::cascade_completion`:
//
// - Valkey: `ff_backend_valkey::cascade::run_cascade`
// - Postgres: `PostgresBackend::cascade_completion` →
//   `ff_backend_postgres::dispatch::dispatch_completion`
//
// The single caller, `completion_listener::spawn_dispatch_loop`, now
// trait-routes via `Arc<dyn EngineBackend>`.

