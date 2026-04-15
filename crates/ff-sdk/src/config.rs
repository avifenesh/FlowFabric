use ff_core::types::{LaneId, Namespace, WorkerId, WorkerInstanceId};

/// Configuration for a FlowFabric worker.
pub struct WorkerConfig {
    /// Valkey hostname. Default: `"localhost"`.
    pub host: String,
    /// Valkey port. Default: `6379`.
    pub port: u16,
    /// Enable TLS for the Valkey connection.
    pub tls: bool,
    /// Enable Valkey cluster mode.
    pub cluster: bool,
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
    /// Create a minimal config for a single-lane worker.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        worker_id: impl Into<String>,
        worker_instance_id: impl Into<String>,
        namespace: impl Into<String>,
        lane: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            tls: false,
            cluster: false,
            worker_id: WorkerId::new(worker_id),
            worker_instance_id: WorkerInstanceId::new(worker_instance_id),
            namespace: Namespace::new(namespace),
            lanes: vec![LaneId::new(lane)],
            capabilities: Vec::new(),
            lease_ttl_ms: 30_000,
            claim_poll_interval_ms: 1_000,
            max_concurrent_tasks: 1,
        }
    }

    /// Lease renewal interval: TTL / 3 (renew at 1/3 of TTL, leaving 2/3 margin).
    pub fn renewal_interval_ms(&self) -> u64 {
        self.lease_ttl_ms / 3
    }
}
