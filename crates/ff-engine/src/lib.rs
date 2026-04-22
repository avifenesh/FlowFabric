//! ff-engine: cross-partition dispatch and background scanners.

pub mod budget;
pub mod completion_listener;
pub mod partition_router;
pub mod scanner;
pub mod supervisor;

use std::sync::Arc;
use std::time::Duration;

use ff_core::partition::PartitionConfig;
use ff_core::types::LaneId;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use partition_router::PartitionRouter;
use supervisor::supervised_spawn;
use scanner::attempt_timeout::AttemptTimeoutScanner;
use scanner::execution_deadline::ExecutionDeadlineScanner;
use scanner::budget_reconciler::BudgetReconciler;
use scanner::budget_reset::BudgetResetScanner;
use scanner::delayed_promoter::DelayedPromoter;
use scanner::dependency_reconciler::DependencyReconciler;
use scanner::index_reconciler::IndexReconciler;
use scanner::lease_expiry::LeaseExpiryScanner;
use scanner::pending_wp_expiry::PendingWaitpointExpiryScanner;
use scanner::quota_reconciler::QuotaReconciler;
use scanner::retention_trimmer::RetentionTrimmer;
use scanner::suspension_timeout::SuspensionTimeoutScanner;
use scanner::flow_projector::FlowProjector;
use scanner::unblock::UnblockScanner;

/// Connection parameters for the completion listener's dedicated RESP3
/// client. Must match the deployment the dispatcher client connects to.
#[derive(Clone, Debug)]
pub struct CompletionListenerConfig {
    /// Seed addresses — `(host, port)` pairs. For standalone, one entry.
    /// For cluster, pass all configured seed nodes.
    pub addresses: Vec<(String, u16)>,
    /// Enable TLS (matches `ClientBuilder::tls_insecure` — the listener
    /// uses insecure TLS because it only handles completion metadata,
    /// never user payloads; if strict TLS is required, plumb a
    /// custom flag).
    pub tls: bool,
    /// Enable cluster mode.
    pub cluster: bool,
}

/// Engine configuration.
pub struct EngineConfig {
    pub partition_config: PartitionConfig,
    /// Lanes to scan for delayed/index operations. Phase 1: `["default"]`.
    pub lanes: Vec<LaneId>,
    /// Lease expiry scan interval. Default: 1.5s.
    pub lease_expiry_interval: Duration,
    /// Delayed promoter scan interval. Default: 750ms.
    pub delayed_promoter_interval: Duration,
    /// Index reconciler scan interval. Default: 45s.
    pub index_reconciler_interval: Duration,
    /// Attempt timeout scan interval. Default: 2s.
    pub attempt_timeout_interval: Duration,
    /// Suspension timeout scan interval. Default: 2s.
    pub suspension_timeout_interval: Duration,
    /// Pending waitpoint expiry scan interval. Default: 5s.
    pub pending_wp_expiry_interval: Duration,
    /// Retention trimmer scan interval. Default: 60s.
    pub retention_trimmer_interval: Duration,
    /// Budget reset scan interval. Default: 15s.
    pub budget_reset_interval: Duration,
    /// Budget reconciler scan interval. Default: 30s.
    pub budget_reconciler_interval: Duration,
    /// Quota reconciler scan interval. Default: 30s.
    pub quota_reconciler_interval: Duration,
    /// Unblock scanner interval. Default: 5s.
    pub unblock_interval: Duration,
    /// Dependency reconciler interval. Default: 15s.
    ///
    /// Post-Batch-C this scanner is a **safety net**, not the primary
    /// promotion path. The [`completion_listener`] SUBSCRIBEs to
    /// `ff:dag:completions` and dispatches dependency resolution
    /// synchronously with each completion — under normal operation,
    /// DAG latency is `~RTT × levels`, not `interval × levels`.
    ///
    /// The reconciler still runs as a catch-all for:
    ///   - messages missed during listener restart or reconnect;
    ///   - pre-Batch-C executions without `core.flow_id` stamped;
    ///   - operator-driven edge mutation that doesn't pass through
    ///     the terminal-transition publish path.
    ///
    /// 15s idle-scan cost is minimal. If the listener is disabled
    /// (`completion_listener` = None), drop this to 1s to preserve
    /// pre-Batch-C DAG latency behavior.
    ///
    /// [`completion_listener`]: Self::completion_listener
    pub dependency_reconciler_interval: Duration,

    /// Optional push-based DAG promotion listener (Batch C item 6).
    /// When `Some`, the engine spawns a [`completion_listener`] task
    /// that SUBSCRIBEs to `ff:dag:completions` on a dedicated RESP3
    /// client and dispatches dependency resolution per completion.
    ///
    /// `None` disables the listener entirely — the reconciler alone
    /// promotes. Useful for lightweight single-node deployments or
    /// test harnesses that don't care about DAG latency.
    ///
    /// [`completion_listener`]: crate::completion_listener
    pub completion_listener: Option<CompletionListenerConfig>,
    /// Flow summary projector interval. Default: 15s.
    ///
    /// Separate observability projection path — maintains the flow
    /// summary view, NOT on the DAG-completion latency path. Kept at
    /// 15s in this config; a change to that cadence is unrelated to
    /// dependency resolution.
    pub flow_projector_interval: Duration,
    /// Execution deadline scanner interval. Default: 5s.
    pub execution_deadline_interval: Duration,

    /// Cancel reconciler scanner interval. Default: 15s.
    ///
    /// Drains `ff_cancel_flow`'s per-partition `cancel_backlog` ZSET of
    /// flows owing async member cancels. Each cancelled flow gets a
    /// grace window (30s by default, set by ff-server) before the
    /// reconciler picks it up, so the live in-process dispatch isn't
    /// fought on the happy path.
    pub cancel_reconciler_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            partition_config: PartitionConfig::default(),
            lanes: vec![LaneId::new("default")],
            lease_expiry_interval: Duration::from_millis(1500),
            delayed_promoter_interval: Duration::from_millis(750),
            index_reconciler_interval: Duration::from_secs(45),
            attempt_timeout_interval: Duration::from_secs(2),
            suspension_timeout_interval: Duration::from_secs(2),
            pending_wp_expiry_interval: Duration::from_secs(5),
            retention_trimmer_interval: Duration::from_secs(60),
            budget_reset_interval: Duration::from_secs(15),
            budget_reconciler_interval: Duration::from_secs(30),
            quota_reconciler_interval: Duration::from_secs(30),
            unblock_interval: Duration::from_secs(5),
            dependency_reconciler_interval: Duration::from_secs(15),
            completion_listener: None,
            flow_projector_interval: Duration::from_secs(15),
            execution_deadline_interval: Duration::from_secs(5),
            cancel_reconciler_interval: Duration::from_secs(15),
        }
    }
}

/// The FlowFabric engine: partition routing + background scanners.
pub struct Engine {
    pub router: Arc<PartitionRouter>,
    shutdown_tx: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

impl Engine {
    /// Start the engine with the given config and Valkey client.
    ///
    /// Spawns background scanner tasks. Returns immediately.
    pub fn start(config: EngineConfig, client: ferriskey::Client) -> Self {
        Self::start_with_metrics(config, client, Arc::new(ff_observability::Metrics::new()))
    }

    /// PR-94: start the engine with a shared observability registry.
    ///
    /// Used by `ff-server` so scanner cycle metrics funnel into the
    /// same Prometheus registry exposed at `/metrics`. The no-arg
    /// [`Engine::start`] forwards here with a disabled shim, so direct
    /// callers (examples, tests) retain the previous behavior.
    pub fn start_with_metrics(
        config: EngineConfig,
        client: ferriskey::Client,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let num_partitions = config.partition_config.num_flow_partitions;
        let router = Arc::new(PartitionRouter::new(config.partition_config));

        let mut handles = Vec::new();

        // Lease expiry scanner
        let lease_scanner = Arc::new(LeaseExpiryScanner::new(config.lease_expiry_interval));
        handles.push(supervised_spawn(
            lease_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Delayed promoter
        let delayed_scanner = Arc::new(DelayedPromoter::new(
            config.delayed_promoter_interval,
            config.lanes.clone(),
        ));
        handles.push(supervised_spawn(
            delayed_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Index reconciler
        let reconciler = Arc::new(IndexReconciler::new(
            config.index_reconciler_interval,
            config.lanes.clone(),
        ));
        handles.push(supervised_spawn(
            reconciler,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Attempt timeout scanner
        let timeout_scanner = Arc::new(AttemptTimeoutScanner::new(
            config.attempt_timeout_interval,
            config.lanes.clone(),
        ));
        handles.push(supervised_spawn(
            timeout_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Suspension timeout scanner
        let suspension_scanner = Arc::new(SuspensionTimeoutScanner::new(
            config.suspension_timeout_interval,
        ));
        handles.push(supervised_spawn(
            suspension_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Pending waitpoint expiry scanner
        let pending_wp_scanner = Arc::new(PendingWaitpointExpiryScanner::new(
            config.pending_wp_expiry_interval,
        ));
        handles.push(supervised_spawn(
            pending_wp_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Retention trimmer
        let retention_scanner = Arc::new(RetentionTrimmer::new(
            config.retention_trimmer_interval,
            config.lanes.clone(),
        ));
        handles.push(supervised_spawn(
            retention_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Budget reset scanner (iterates budget partitions)
        let budget_reset = Arc::new(BudgetResetScanner::new(
            config.budget_reset_interval,
        ));
        handles.push(supervised_spawn(
            budget_reset,
            client.clone(),
            config.partition_config.num_budget_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Budget reconciler (iterates budget partitions)
        let budget_reconciler = Arc::new(BudgetReconciler::new(
            config.budget_reconciler_interval,
        ));
        handles.push(supervised_spawn(
            budget_reconciler,
            client.clone(),
            config.partition_config.num_budget_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Unblock scanner (iterates execution partitions, re-evaluates blocked)
        let unblock_scanner = Arc::new(UnblockScanner::new(
            config.unblock_interval,
            config.lanes.clone(),
            config.partition_config,
        ));
        handles.push(supervised_spawn(
            unblock_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Dependency reconciler (iterates execution partitions)
        let dep_reconciler = Arc::new(DependencyReconciler::new(
            config.dependency_reconciler_interval,
            config.lanes.clone(),
            config.partition_config,
        ));
        handles.push(supervised_spawn(
            dep_reconciler,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Quota reconciler (iterates quota partitions)
        let quota_reconciler = Arc::new(QuotaReconciler::new(
            config.quota_reconciler_interval,
        ));
        handles.push(supervised_spawn(
            quota_reconciler,
            client.clone(),
            config.partition_config.num_quota_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Flow summary projector (iterates flow partitions)
        let flow_projector = Arc::new(FlowProjector::new(
            config.flow_projector_interval,
            config.partition_config,
        ));
        handles.push(supervised_spawn(
            flow_projector,
            client.clone(),
            config.partition_config.num_flow_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Cancel reconciler (iterates flow partitions). Drains
        // cancel_backlog entries whose grace window has elapsed so a
        // process crash mid-dispatch can't leave flow members un-cancelled.
        let cancel_reconciler = Arc::new(scanner::cancel_reconciler::CancelReconciler::new(
            config.cancel_reconciler_interval,
            config.partition_config,
        ));
        handles.push(supervised_spawn(
            cancel_reconciler,
            client.clone(),
            config.partition_config.num_flow_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Execution deadline scanner (iterates execution partitions)
        let deadline_scanner = Arc::new(ExecutionDeadlineScanner::new(
            config.execution_deadline_interval,
            config.lanes,
        ));
        handles.push(supervised_spawn(
            deadline_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Completion listener (Batch C item 6 — push-based DAG promotion).
        // Optional: when Some, spawns a dedicated RESP3 client that
        // SUBSCRIBEs to ff:dag:completions and dispatches dependency
        // resolution per completion. See `completion_listener` module
        // docs for the design rationale.
        let listener_enabled = config.completion_listener.is_some();
        if let Some(listener_cfg) = config.completion_listener {
            handles.push(completion_listener::spawn_completion_listener(
                router.clone(),
                client,
                listener_cfg.addresses,
                listener_cfg.tls,
                listener_cfg.cluster,
                shutdown_rx,
            ));
        }

        let scanner_count = if listener_enabled { "15 scanners + completion listener" } else { "15 scanners" };
        tracing::info!(
            num_partitions,
            budget_partitions = config.partition_config.num_budget_partitions,
            quota_partitions = config.partition_config.num_quota_partitions,
            flow_partitions = config.partition_config.num_flow_partitions,
            "engine started with {scanner_count}"
        );

        Self {
            router,
            shutdown_tx,
            handles,
        }
    }

    /// Signal all scanners to stop and wait for them to finish.
    ///
    /// Waits up to 15 seconds for scanners to drain. If any scanner is
    /// blocked on a hung Valkey command, the timeout prevents shutdown
    /// from hanging indefinitely (Kubernetes SIGKILL safety).
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let join_all = async {
            for handle in self.handles {
                let _ = handle.await;
            }
        };
        match tokio::time::timeout(Duration::from_secs(15), join_all).await {
            Ok(()) => tracing::info!("engine shutdown complete"),
            Err(_) => tracing::warn!(
                "engine shutdown timed out after 15s, abandoning remaining scanners"
            ),
        }
    }
}
