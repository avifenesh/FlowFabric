use ff_core::backend::BackendConfig;
use ff_core::types::{LaneId, Namespace, WorkerId, WorkerInstanceId};

/// Configuration for a FlowFabric worker.
///
/// **RFC-012 Stage 1c tranche 1.** Valkey-specific connection
/// parameters (`host`, `port`, `tls`, `cluster`) moved to the nested
/// [`BackendConfig`] field, which also carries
/// [`BackendTimeouts`](ff_core::backend::BackendTimeouts) and
/// [`BackendRetry`](ff_core::backend::BackendRetry) policy. Build one
/// with [`BackendConfig::valkey`] for the common standalone case; for
/// TLS / cluster / tuned retry, construct the
/// [`ValkeyConnection`](ff_core::backend::ValkeyConnection) and
/// [`BackendConfig`] fields directly.
///
/// Worker-policy fields (`lease_ttl_ms`, `claim_poll_interval_ms`,
/// capability set, lane list, identity) stay on `WorkerConfig` —
/// those are orthogonal to the storage backend choice.
///
/// `WorkerConfig::new` was removed in this stage (pre-1.0 clean
/// break); construct via struct literal.
pub struct WorkerConfig {
    /// Backend connection + shared timeouts / retry policy.
    pub backend: BackendConfig,
    /// Logical worker identity (e.g., "gpu-worker-pool-1").
    pub worker_id: WorkerId,
    /// Concrete worker process/runtime instance identity (e.g., container ID).
    pub worker_instance_id: WorkerInstanceId,
    /// Namespace this worker operates in.
    pub namespace: Namespace,
    /// Lanes this worker claims work from.
    pub lanes: Vec<LaneId>,
    /// Capabilities this worker advertises for routing.
    pub capabilities: Vec<String>,
    /// Lease TTL in milliseconds. Default: 30,000 (30s).
    pub lease_ttl_ms: u64,
    /// Interval between claim attempts when idle, in milliseconds. Default: 1,000 (1s).
    pub claim_poll_interval_ms: u64,
    /// Maximum concurrent tasks. Default: 1.
    pub max_concurrent_tasks: usize,
}

impl WorkerConfig {
    /// Lease renewal interval: TTL / 3 (renew at 1/3 of TTL, leaving 2/3 margin).
    pub fn renewal_interval_ms(&self) -> u64 {
        self.lease_ttl_ms / 3
    }
}
