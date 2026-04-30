use ff_core::backend::BackendConfig;
use ff_core::partition::PartitionConfig;
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
    ///
    /// **v0.13 ergonomics fix (cairn, feedback_sdk_reclaim_ergonomics
    /// Finding 2):** this field is only consumed by
    /// [`FlowFabricWorker::connect`] (the URL-based Valkey-native
    /// entry point that dials a fresh `ferriskey::Client`).
    /// [`FlowFabricWorker::connect_with`] — the backend-agnostic
    /// entry point that takes a pre-built `Arc<dyn EngineBackend>` —
    /// ignores it entirely. Pre-v0.13 this field was required and
    /// `connect_with` callers had to supply a placeholder
    /// `BackendConfig::valkey(...)` just to satisfy the struct
    /// literal; the SC-10 `incident-remediation` example surfaced
    /// this as a rough edge.
    ///
    /// * `Some(cfg)` + [`FlowFabricWorker::connect`]: dial using
    ///   `cfg` (unchanged behaviour).
    /// * `None` + [`FlowFabricWorker::connect`]: rejected with
    ///   [`SdkError::Config`]. The URL-based path needs a
    ///   `BackendConfig` to dial.
    /// * `None` + [`FlowFabricWorker::connect_with`]: clean —
    ///   the injected backend is authoritative.
    /// * `Some(cfg)` + [`FlowFabricWorker::connect_with`]:
    ///   accepted but logs a WARN (`cfg` is ignored — the
    ///   injected backend is authoritative).
    ///
    /// [`FlowFabricWorker::connect`]: crate::FlowFabricWorker::connect
    /// [`FlowFabricWorker::connect_with`]: crate::FlowFabricWorker::connect_with
    /// [`SdkError::Config`]: crate::SdkError::Config
    pub backend: Option<BackendConfig>,
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
    /// Override for the server-published partition config.
    ///
    /// v0.12 PR-6: closes the follow-up flagged in
    /// [`FlowFabricWorker::connect_with`]'s pre-PR-6 rustdoc
    /// ("callers needing a non-default `PartitionConfig` under
    /// non-Valkey backends use `connect` (Valkey) or override
    /// post-construction through a future `WorkerConfig` field").
    ///
    /// * `None` (default) — `connect_with` uses
    ///   [`PartitionConfig::default()`] (256 / 32 / 32);
    ///   `connect` ignores this field and reads
    ///   `ff:config:partitions` from Valkey as before.
    /// * `Some(cfg)` — `connect_with` binds the worker to `cfg`
    ///   directly; `connect` still prefers Valkey's published
    ///   hash (this override is a `connect_with`-only knob, since
    ///   Valkey's `ff:config:partitions` is authoritative when
    ///   present).
    ///
    /// Consumers using a PG / SQLite backend whose deployment
    /// uses a non-default `num_flow_partitions` (e.g. 512) must
    /// set this — otherwise `describe_execution` + partition-
    /// keyed claim paths compute the wrong partition index and
    /// silently miss data.
    ///
    /// [`FlowFabricWorker::connect_with`]: crate::FlowFabricWorker::connect_with
    pub partition_config: Option<PartitionConfig>,
}

impl WorkerConfig {
    /// Lease renewal interval: TTL / 3 (renew at 1/3 of TTL, leaving 2/3 margin).
    pub fn renewal_interval_ms(&self) -> u64 {
        self.lease_ttl_ms / 3
    }
}
