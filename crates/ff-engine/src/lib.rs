//! ff-engine: cross-partition dispatch and background scanners.

pub mod budget;
pub mod completion_listener;
pub mod partition_router;
pub mod scanner;
pub mod supervisor;

use std::sync::Arc;
use std::time::Duration;

use ff_core::backend::ScannerFilter;
use ff_core::completion_backend::CompletionStream;
use ff_core::engine_backend::EngineBackend;
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
    /// promotion path. When a [`CompletionStream`] is handed to
    /// `start_with_completions`, push-based dispatch drives DAG
    /// promotion synchronously with each completion — under normal
    /// operation DAG latency is `~RTT × levels`, not `interval × levels`.
    ///
    /// The reconciler still runs as a catch-all for:
    ///   - messages missed during subscriber restart or reconnect;
    ///   - pre-Batch-C executions without `core.flow_id` stamped;
    ///   - operator-driven edge mutation that doesn't pass through
    ///     the terminal-transition publish path.
    ///
    /// 15s idle-scan cost is minimal. If the push dispatch loop is
    /// disabled (engine started via `start`/`start_with_metrics`
    /// without a stream), drop this to 1s to preserve pre-Batch-C
    /// DAG latency behavior.
    pub dependency_reconciler_interval: Duration,
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

    /// RFC-016 Stage C sibling-cancel dispatcher interval. Default: 1s.
    ///
    /// Drains the per-flow-partition `pending_cancel_groups` SET,
    /// populated by `ff_resolve_dependency` whenever an AnyOf/Quorum
    /// edge group fires terminal under `OnSatisfied::CancelRemaining`.
    /// For each indexed group the dispatcher issues per-sibling
    /// `ff_cancel_execution` with `FailureReason::sibling_quorum_{
    /// satisfied,impossible}`, then atomically SREM+clear via
    /// `ff_drain_sibling_cancel_group`.
    ///
    /// A short default (1s) minimises the window between quorum
    /// satisfaction and sibling termination — this is the user-facing
    /// latency floor for "kill the losers" workflows. Bump only if a
    /// deployment's steady-state pending-set depth is observed to
    /// backlog under the 1s cadence; Stage C's §4.2 benchmark gates
    /// the release against the p99 ≤ 500 ms SLO at n=100 (§4.2 of
    /// the RFC).
    pub edge_cancel_dispatcher_interval: Duration,

    /// RFC-016 Stage D sibling-cancel reconciler interval. Default: 10s.
    ///
    /// Safety-net scanner for Invariant Q6: if the engine crashed
    /// between `ff_resolve_dependency`'s SADD to `pending_cancel_groups`
    /// and the dispatcher's `ff_drain_sibling_cancel_group`, this
    /// reconciler detects the orphan tuple and finalises via
    /// `ff_reconcile_sibling_cancel_group`. It runs at a deliberately
    /// slower cadence than the dispatcher (10s vs 1s) so the dispatcher
    /// owns the happy path and the reconciler only cleans up
    /// crash-recovery residue. The reconciler MUST NOT fight the
    /// dispatcher — it no-ops whenever siblings are still non-terminal.
    pub edge_cancel_reconciler_interval: Duration,

    /// Per-consumer scanner filter (issue #122).
    ///
    /// Applied by every execution-shaped scanner (lease_expiry,
    /// attempt_timeout, execution_deadline, suspension_timeout,
    /// pending_wp_expiry, delayed_promoter, dependency_reconciler,
    /// cancel_reconciler, unblock, index_reconciler,
    /// retention_trimmer) to restrict the candidate set to
    /// executions owned by this consumer. The four non-execution
    /// scanners (budget_reconciler, budget_reset, quota_reconciler,
    /// flow_projector) accept the filter for API uniformity but do
    /// not apply it — their domains are not per-execution.
    ///
    /// Default: [`ScannerFilter::default`] — no filtering,
    /// pre-#122 behaviour. Multi-tenant deployments that share a
    /// single Valkey keyspace across two FlowFabric instances set
    /// this (paired with
    /// [`CompletionBackend::subscribe_completions_filtered`]) for
    /// mutual isolation.
    ///
    /// [`CompletionBackend::subscribe_completions_filtered`]: ff_core::completion_backend::CompletionBackend::subscribe_completions_filtered
    pub scanner_filter: ScannerFilter,
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
            execution_deadline_interval: Duration::from_secs(5),
            cancel_reconciler_interval: Duration::from_secs(15),
            edge_cancel_dispatcher_interval: Duration::from_secs(1),
            edge_cancel_reconciler_interval: Duration::from_secs(10),
            scanner_filter: ScannerFilter::default(),
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
    /// Start the engine with the given config and backend.
    ///
    /// Spawns background scanner tasks. Returns immediately.
    ///
    /// `backend` must be a backend whose [`EngineBackend::as_any`]
    /// downcasts to a supported concrete type. In v0.12 (PR-7a) that
    /// is [`ff_backend_valkey::ValkeyBackend`] only; v0.13 (PR-7b)
    /// will trait-ify the scanner surface and accept any
    /// `EngineBackend` implementation. Passing a non-Valkey backend
    /// today panics immediately inside this constructor (before any
    /// scanner tasks are spawned) — by design; the cairn embedding
    /// path that motivated this signature goes through Valkey until
    /// the trait-ification lands.
    pub fn start(config: EngineConfig, backend: Arc<dyn EngineBackend>) -> Self {
        // Construct a fresh metrics handle here so direct callers
        // (examples, tests) don't need to. Under the default build
        // (`observability` feature off) this is the no-op shim. With
        // the feature on the handle is a real OTEL registry but — by
        // design — one that nothing else shares; it's only useful for
        // tests that want to exercise scanner cycle recording in
        // isolation. Production code uses
        // [`Self::start_with_metrics`] to plumb the same handle
        // through the HTTP /metrics route.
        Self::start_with_metrics(config, backend, Arc::new(ff_observability::Metrics::new()))
    }

    /// PR-94: start the engine with a shared observability registry.
    ///
    /// Used by `ff-server` so scanner cycle metrics funnel into the
    /// same Prometheus registry exposed at `/metrics`. Under the
    /// `observability` feature (flipped via the same feature on
    /// `ff-server` / `ff-engine`), the handle records into an OTEL
    /// `MeterProvider`; otherwise the shim no-ops.
    pub fn start_with_metrics(
        config: EngineConfig,
        backend: Arc<dyn EngineBackend>,
        metrics: Arc<ff_observability::Metrics>,
    ) -> Self {
        Self::start_internal(config, backend, metrics, None)
    }

    /// Start the engine with a shared observability registry and a
    /// completion stream for push-based DAG promotion (issue #90).
    ///
    /// The stream is typically produced by
    /// [`ff_core::completion_backend::CompletionBackend::subscribe_completions`].
    /// The engine spawns a dispatch loop that drains the stream and
    /// fires `ff_resolve_dependency` per completion, reducing DAG
    /// latency from `interval × levels` to `~RTT × levels`. The
    /// `dependency_reconciler` scanner remains as a safety net for
    /// completions missed during subscriber reconnect windows.
    ///
    /// # Backend parameter (v0.12 PR-7a)
    ///
    /// `backend` is an `Arc<dyn EngineBackend>` so the public
    /// constructor signature is backend-agnostic — cairn and other
    /// consumers can sequence their engine construction around this
    /// shape without waiting for the scanner trait-ification. Runtime
    /// support in v0.12 is still Valkey-only: the scanner supervisors
    /// today speak ferriskey, so this constructor downcasts
    /// `backend.as_any()` to [`ff_backend_valkey::ValkeyBackend`] and
    /// reaches for the embedded `ferriskey::Client`. v0.13 (PR-7b)
    /// will trait-ify each scanner onto `EngineBackend` and retire
    /// the downcast — the public signature stays stable through that
    /// transition, so consumers wiring up the v0.12 shape today
    /// don't change their call site.
    ///
    /// # Panics
    ///
    /// Panics if `backend.as_any().downcast_ref::<ValkeyBackend>()`
    /// is `None`. The only in-tree caller (`ff-server`) constructs a
    /// `ValkeyBackend` before calling; external consumers that want
    /// non-Valkey support must wait for PR-7b.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use ff_core::engine_backend::EngineBackend;
    /// # async fn ex(
    /// #     cfg: ff_engine::EngineConfig,
    /// #     metrics: Arc<ff_observability::Metrics>,
    /// #     stream: ff_core::completion_backend::CompletionStream,
    /// #     backend: Arc<dyn EngineBackend>, // e.g. from ValkeyBackend::connect
    /// # ) {
    /// // Valkey backend: works today (v0.12 PR-7a).
    /// let engine = ff_engine::Engine::start_with_completions(
    ///     cfg, backend, metrics, stream,
    /// );
    /// # let _ = engine;
    /// # }
    /// ```
    pub fn start_with_completions(
        config: EngineConfig,
        backend: Arc<dyn EngineBackend>,
        metrics: Arc<ff_observability::Metrics>,
        completions: CompletionStream,
    ) -> Self {
        Self::start_internal(config, backend, metrics, Some(completions))
    }

    fn start_internal(
        config: EngineConfig,
        backend: Arc<dyn EngineBackend>,
        metrics: Arc<ff_observability::Metrics>,
        completions: Option<CompletionStream>,
    ) -> Self {
        // v0.12 PR-7a transitional downcast. Scanners today still
        // speak ferriskey directly; until PR-7b trait-ifies them,
        // reach in for the embedded client. Only in-tree consumer is
        // ff-server which always constructs a `ValkeyBackend`.
        let client = match backend
            .as_any()
            .downcast_ref::<ff_backend_valkey::ValkeyBackend>()
        {
            Some(vb) => vb.client().clone(),
            None => panic!(
                "Engine::start_* in v0.12 requires ValkeyBackend \
                 (got backend_label={:?}); non-Valkey support lands \
                 in v0.13 (PR-7b).",
                backend.backend_label(),
            ),
        };
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let num_partitions = config.partition_config.num_flow_partitions;
        let router = Arc::new(PartitionRouter::new(config.partition_config));

        let mut handles = Vec::new();

        let scanner_filter = config.scanner_filter.clone();

        // Lease expiry scanner
        let lease_scanner = Arc::new(LeaseExpiryScanner::with_filter_and_backend(
            config.lease_expiry_interval,
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            lease_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Delayed promoter
        let delayed_scanner = Arc::new(DelayedPromoter::with_filter_and_backend(
            config.delayed_promoter_interval,
            config.lanes.clone(),
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            delayed_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Index reconciler
        let reconciler = Arc::new(IndexReconciler::with_filter_and_backend(
            config.index_reconciler_interval,
            config.lanes.clone(),
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            reconciler,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Attempt timeout scanner
        let timeout_scanner = Arc::new(AttemptTimeoutScanner::with_filter_and_backend(
            config.attempt_timeout_interval,
            config.lanes.clone(),
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            timeout_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Suspension timeout scanner
        let suspension_scanner = Arc::new(SuspensionTimeoutScanner::with_filter_and_backend(
            config.suspension_timeout_interval,
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            suspension_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Pending waitpoint expiry scanner
        let pending_wp_scanner = Arc::new(PendingWaitpointExpiryScanner::with_filter_and_backend(
            config.pending_wp_expiry_interval,
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            pending_wp_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Retention trimmer
        let retention_scanner = Arc::new(RetentionTrimmer::with_filter_and_backend(
            config.retention_trimmer_interval,
            config.lanes.clone(),
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            retention_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Budget reset scanner (iterates budget partitions).
        // Filter is accepted but not applied (budget partitions don't
        // carry the per-execution namespace / instance_tag shape).
        let budget_reset = Arc::new(BudgetResetScanner::with_filter_and_backend(
            config.budget_reset_interval,
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            budget_reset,
            client.clone(),
            config.partition_config.num_budget_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Budget reconciler (iterates budget partitions). Filter
        // accepted but not applied — see BudgetReconciler::with_filter
        // rustdoc.
        let budget_reconciler = Arc::new(BudgetReconciler::with_filter(
            config.budget_reconciler_interval,
            scanner_filter.clone(),
        ));
        handles.push(supervised_spawn(
            budget_reconciler,
            client.clone(),
            config.partition_config.num_budget_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Unblock scanner (iterates execution partitions, re-evaluates blocked)
        let unblock_scanner = Arc::new(UnblockScanner::with_filter_and_backend(
            config.unblock_interval,
            config.lanes.clone(),
            config.partition_config,
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            unblock_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Dependency reconciler (iterates execution partitions)
        let dep_reconciler = Arc::new(DependencyReconciler::with_filter_and_backend(
            config.dependency_reconciler_interval,
            config.lanes.clone(),
            config.partition_config,
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            dep_reconciler,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Quota reconciler (iterates quota partitions). Filter
        // accepted but not applied — see QuotaReconciler::with_filter
        // rustdoc.
        let quota_reconciler = Arc::new(QuotaReconciler::with_filter(
            config.quota_reconciler_interval,
            scanner_filter.clone(),
        ));
        handles.push(supervised_spawn(
            quota_reconciler,
            client.clone(),
            config.partition_config.num_quota_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Flow summary projector (iterates flow partitions). Filter
        // accepted but not applied — see FlowProjector::with_filter
        // rustdoc.
        let flow_projector = Arc::new(FlowProjector::with_filter(
            config.flow_projector_interval,
            config.partition_config,
            scanner_filter.clone(),
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
        let cancel_reconciler = Arc::new(scanner::cancel_reconciler::CancelReconciler::with_filter_and_backend(
            config.cancel_reconciler_interval,
            config.partition_config,
            scanner_filter.clone(),
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            cancel_reconciler,
            client.clone(),
            config.partition_config.num_flow_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // RFC-016 Stage C: sibling-cancel dispatcher. Iterates flow
        // partitions, drains `pending_cancel_groups` SET via
        // `ff_drain_sibling_cancel_group`, issues per-sibling
        // `ff_cancel_execution` with sibling_quorum reasons.
        let edge_cancel_dispatcher = Arc::new(
            scanner::edge_cancel_dispatcher::EdgeCancelDispatcher::with_filter_metrics_and_backend(
                config.edge_cancel_dispatcher_interval,
                config.partition_config,
                scanner_filter.clone(),
                metrics.clone(),
                backend.clone(),
            ),
        );
        handles.push(supervised_spawn(
            edge_cancel_dispatcher,
            client.clone(),
            config.partition_config.num_flow_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // RFC-016 Stage D: sibling-cancel reconciler. Crash-recovery
        // safety net for Invariant Q6 — finalises tuples in
        // `pending_cancel_groups` whose dispatcher drain was interrupted
        // by an engine crash. Runs at a slower cadence than the
        // dispatcher so it never fights the happy path.
        let edge_cancel_reconciler = Arc::new(
            scanner::edge_cancel_reconciler::EdgeCancelReconciler::with_filter_metrics_and_backend(
                config.edge_cancel_reconciler_interval,
                scanner_filter.clone(),
                metrics.clone(),
                backend.clone(),
            ),
        );
        handles.push(supervised_spawn(
            edge_cancel_reconciler,
            client.clone(),
            config.partition_config.num_flow_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Execution deadline scanner (iterates execution partitions)
        let deadline_scanner = Arc::new(ExecutionDeadlineScanner::with_filter_and_backend(
            config.execution_deadline_interval,
            config.lanes,
            scanner_filter,
            backend.clone(),
        ));
        handles.push(supervised_spawn(
            deadline_scanner,
            client.clone(),
            num_partitions,
            shutdown_rx.clone(),
            metrics.clone(),
        ));

        // Completion dispatch loop (Batch C item 6 — push-based DAG
        // promotion; backend-agnostic since issue #90). Optional: when
        // a stream is provided, spawn a task that drains
        // `CompletionPayload`s and fires dependency resolution per
        // completion. See `completion_listener` module docs.
        let listener_enabled = completions.is_some();
        if let Some(stream) = completions {
            handles.push(completion_listener::spawn_dispatch_loop(
                backend.clone(),
                stream,
                shutdown_rx,
            ));
        }

        let scanner_count = if listener_enabled { "17 scanners + completion dispatch" } else { "17 scanners" };
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
