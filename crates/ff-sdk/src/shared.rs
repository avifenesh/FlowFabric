//! `SharedFfBackend` — factory that amortises a single dialed
//! [`ValkeyBackend`] (and its embedded multiplexed `ferriskey::Client`)
//! across multiple logical [`FlowFabricWorker`] instances in the same
//! process (issue #174).
//!
//! The motivating use case is apalis's "shared connections" hook from
//! `MakeShared` (issue #51) and cairn-fabric multi-queue consumers
//! that want one TCP socket serving N worker loops: without this
//! factory, each [`FlowFabricWorker::connect`] call dials its own
//! `ferriskey::Client`, so a process running 4 lane-specialised
//! workers holds 4 sockets into Valkey for write-surface ops even
//! though the backend is stateless.
//!
//! Today this factory collapses the `Arc<ValkeyBackend>` allocation
//! (and therefore the `CompletionBackend` subscriber surface) into
//! one. The worker's embedded hot-path `ferriskey::Client` is still
//! redialed per worker by [`FlowFabricWorker::connect_with`] — Stage
//! 1c / 1d of RFC-012 remove that embedded client entirely, at which
//! point the `Arc<ValkeyBackend>` handed in here becomes the one and
//! only client the worker talks through.
//!
//! # Example
//!
//! ```rust,ignore
//! use ff_core::backend::BackendConfig;
//! use ff_core::types::{LaneId, Namespace, WorkerId, WorkerInstanceId};
//! use ff_sdk::{SharedFfBackend, WorkerConfig};
//!
//! # async fn run() -> Result<(), ff_sdk::SdkError> {
//! let shared = SharedFfBackend::connect(
//!     BackendConfig::valkey("127.0.0.1", 6379),
//! ).await?;
//!
//! let cfg_a = WorkerConfig {
//!     backend: BackendConfig::valkey("127.0.0.1", 6379),
//!     worker_id: WorkerId::new("w-a"),
//!     worker_instance_id: WorkerInstanceId::new("w-a-1"),
//!     namespace: Namespace::new("default"),
//!     lanes: vec![LaneId::new("lane-a")],
//!     capabilities: Vec::new(),
//!     lease_ttl_ms: 30_000,
//!     claim_poll_interval_ms: 1_000,
//!     max_concurrent_tasks: 1,
//! };
//! let cfg_b = WorkerConfig {
//!     worker_id: WorkerId::new("w-b"),
//!     worker_instance_id: WorkerInstanceId::new("w-b-1"),
//!     lanes: vec![LaneId::new("lane-b")],
//!     ..cfg_a.clone()
//! };
//!
//! let worker_a = shared.worker(cfg_a).await?;
//! let worker_b = shared.worker(cfg_b).await?;
//! // `worker_a` and `worker_b` share the same `Arc<ValkeyBackend>` —
//! // one allocation, one completion-subscription surface, one
//! // backend trait-object for Stage 1c/1d hot-path migration.
//! # Ok(()) }
//! ```

use std::sync::Arc;

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;

use crate::SdkError;
use crate::config::WorkerConfig;
use crate::worker::FlowFabricWorker;

/// Factory that holds a single dialed [`ValkeyBackend`] and stamps
/// out [`FlowFabricWorker`] instances that all share the same
/// backend allocation.
///
/// Direct answer to apalis's `MakeShared` pattern from issue #51 and
/// cairn-fabric's multi-queue "one socket, N worker loops" need.
///
/// See the module-level docs for the motivating example and the
/// precise scope of sharing at the current RFC-012 stage.
#[derive(Clone)]
pub struct SharedFfBackend {
    backend: Arc<ValkeyBackend>,
}

impl SharedFfBackend {
    /// Dial a Valkey node and wrap the resulting [`ValkeyBackend`] in
    /// a shareable factory.
    ///
    /// One call = one dialed `ferriskey::Client` + one
    /// `Arc<ValkeyBackend>`. Each subsequent [`Self::worker`] call
    /// clones the `Arc` — no additional dial.
    pub async fn connect(config: BackendConfig) -> Result<Self, SdkError> {
        let backend = ValkeyBackend::connect_concrete(config).await?;
        Ok(Self { backend })
    }

    /// Build a [`FlowFabricWorker`] that forwards trait ops through
    /// this factory's shared [`ValkeyBackend`]. The same `Arc` is
    /// handed in as both the [`EngineBackend`] and
    /// [`CompletionBackend`] trait-object views — one allocation,
    /// two views.
    ///
    /// `config.backend` is still used by the underlying
    /// [`FlowFabricWorker::connect_with`] to dial the worker's own
    /// embedded `ferriskey::Client` today (RFC-012 Stage 1b holdover
    /// — the shared `ValkeyBackend` only covers the migrated
    /// per-task trait ops). Stage 1d removes the embedded client, at
    /// which point the shared `Arc` becomes the sole transport.
    ///
    /// [`EngineBackend`]: ff_core::engine_backend::EngineBackend
    /// [`CompletionBackend`]: ff_core::completion_backend::CompletionBackend
    pub async fn worker(&self, config: WorkerConfig) -> Result<FlowFabricWorker, SdkError> {
        let backend: Arc<dyn ff_core::engine_backend::EngineBackend> = self.backend.clone();
        let completion: Arc<dyn ff_core::completion_backend::CompletionBackend> =
            self.backend.clone();
        FlowFabricWorker::connect_with(config, backend, Some(completion)).await
    }

    /// Borrow the shared [`ValkeyBackend`] as its concrete type.
    /// Primarily for integration tests that assert sharing via
    /// [`Arc::strong_count`]; consumers should prefer
    /// [`Self::worker`] for the public path.
    pub fn backend_arc(&self) -> &Arc<ValkeyBackend> {
        &self.backend
    }
}
