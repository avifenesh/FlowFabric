// ff-engine: cross-partition dispatch and background scanners

pub mod budget;
pub mod dispatch;
pub mod scanner;

use std::sync::Arc;
use std::time::Duration;

use ff_core::partition::PartitionConfig;
use ff_core::types::LaneId;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use dispatch::PartitionRouter;
use scanner::ScannerRunner;
use scanner::attempt_timeout::AttemptTimeoutScanner;
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
    pub dependency_reconciler_interval: Duration,
    /// Flow summary projector interval. Default: 15s.
    pub flow_projector_interval: Duration,
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
            flow_projector_interval: Duration::from_secs(15),
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
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let num_partitions = config.partition_config.num_execution_partitions;
        let router = Arc::new(PartitionRouter::new(config.partition_config));

        let mut handles = Vec::new();

        // Lease expiry scanner
        let lease_scanner = Arc::new(LeaseExpiryScanner::new(config.lease_expiry_interval));
        handles.push(ScannerRunner::spawn(
            lease_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Delayed promoter
        let delayed_scanner = Arc::new(DelayedPromoter::new(
            config.delayed_promoter_interval,
            config.lanes.clone(),
        ));
        handles.push(ScannerRunner::spawn(
            delayed_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Index reconciler
        let reconciler = Arc::new(IndexReconciler::new(
            config.index_reconciler_interval,
            config.lanes.clone(),
        ));
        handles.push(ScannerRunner::spawn(
            reconciler,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Attempt timeout scanner
        let timeout_scanner = Arc::new(AttemptTimeoutScanner::new(
            config.attempt_timeout_interval,
            config.lanes.clone(),
        ));
        handles.push(ScannerRunner::spawn(
            timeout_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Suspension timeout scanner
        let suspension_scanner = Arc::new(SuspensionTimeoutScanner::new(
            config.suspension_timeout_interval,
        ));
        handles.push(ScannerRunner::spawn(
            suspension_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Pending waitpoint expiry scanner
        let pending_wp_scanner = Arc::new(PendingWaitpointExpiryScanner::new(
            config.pending_wp_expiry_interval,
        ));
        handles.push(ScannerRunner::spawn(
            pending_wp_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Retention trimmer
        let retention_scanner = Arc::new(RetentionTrimmer::new(
            config.retention_trimmer_interval,
            config.lanes.clone(),
        ));
        handles.push(ScannerRunner::spawn(
            retention_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Budget reset scanner (iterates budget partitions)
        let budget_reset = Arc::new(BudgetResetScanner::new(
            config.budget_reset_interval,
        ));
        handles.push(ScannerRunner::spawn(
            budget_reset,
            client.clone(),
            config.partition_config.num_budget_partitions,
            shutdown_rx.clone(),
        ));

        // Budget reconciler (iterates budget partitions)
        let budget_reconciler = Arc::new(BudgetReconciler::new(
            config.budget_reconciler_interval,
        ));
        handles.push(ScannerRunner::spawn(
            budget_reconciler,
            client.clone(),
            config.partition_config.num_budget_partitions,
            shutdown_rx.clone(),
        ));

        // Unblock scanner (iterates execution partitions, re-evaluates blocked)
        let unblock_scanner = Arc::new(UnblockScanner::new(
            config.unblock_interval,
            config.lanes.clone(),
            config.partition_config,
        ));
        handles.push(ScannerRunner::spawn(
            unblock_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Dependency reconciler (iterates execution partitions)
        let dep_reconciler = Arc::new(DependencyReconciler::new(
            config.dependency_reconciler_interval,
            config.lanes,
            config.partition_config,
        ));
        handles.push(ScannerRunner::spawn(
            dep_reconciler,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
        ));

        // Quota reconciler (iterates quota partitions)
        let quota_reconciler = Arc::new(QuotaReconciler::new(
            config.quota_reconciler_interval,
        ));
        handles.push(ScannerRunner::spawn(
            quota_reconciler,
            client.clone(),
            config.partition_config.num_quota_partitions,
            shutdown_rx.clone(),
        ));

        // Flow summary projector (iterates flow partitions)
        let flow_projector = Arc::new(FlowProjector::new(
            config.flow_projector_interval,
            config.partition_config,
        ));
        handles.push(ScannerRunner::spawn(
            flow_projector,
            client,
            config.partition_config.num_flow_partitions,
            shutdown_rx,
        ));

        tracing::info!(
            num_partitions,
            budget_partitions = config.partition_config.num_budget_partitions,
            quota_partitions = config.partition_config.num_quota_partitions,
            flow_partitions = config.partition_config.num_flow_partitions,
            "engine started with 13 scanners"
        );

        Self {
            router,
            shutdown_tx,
            handles,
        }
    }

    /// Signal all scanners to stop and wait for them to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        for handle in self.handles {
            let _ = handle.await;
        }
        tracing::info!("engine shutdown complete");
    }
}
