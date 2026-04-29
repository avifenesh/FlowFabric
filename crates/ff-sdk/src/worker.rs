use std::collections::HashMap;
#[cfg(feature = "direct-valkey-claim")]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// RFC-023 Phase 1a (§4.4 item 10b): ferriskey imports scoped behind
// `valkey-default` so the module compiles under
// `--no-default-features, features = ["sqlite"]`.
#[cfg(feature = "valkey-default")]
use ferriskey::Client;
use ff_core::partition::PartitionConfig;
use ff_core::types::*;
use tokio::sync::Semaphore;

use crate::config::WorkerConfig;
use crate::task::ClaimedTask;
use crate::SdkError;

/// FlowFabric worker — connects to Valkey, claims executions, and provides
/// the worker-facing API.
///
/// # Admission control
///
/// `claim_next()` lives behind the `direct-valkey-claim` feature flag and
/// **bypasses the scheduler's admission controls**: it reads the eligible
/// ZSET directly and mints its own claim grant without consulting budget
/// (`{b:M}`) or quota (`{q:K}`) policies. Default-off. Intended for
/// benchmarks, tests, and single-tenant development where the scheduler
/// hop is measurement noise, not for production.
///
/// For production deployments, consume scheduler-issued grants via
/// [`FlowFabricWorker::claim_from_grant`] — the scheduler enforces
/// budget breach, quota sliding-window, concurrency cap, and
/// capability-match checks before issuing grants.
///
/// # Usage
///
/// ```rust,ignore
/// use ff_core::backend::BackendConfig;
/// use ff_core::types::{LaneId, Namespace, WorkerId, WorkerInstanceId};
/// use ff_sdk::{FlowFabricWorker, WorkerConfig};
///
/// let config = WorkerConfig {
///     backend: BackendConfig::valkey("localhost", 6379),
///     worker_id: WorkerId::new("w1"),
///     worker_instance_id: WorkerInstanceId::new("w1-i1"),
///     namespace: Namespace::new("default"),
///     lanes: vec![LaneId::new("main")],
///     capabilities: Vec::new(),
///     lease_ttl_ms: 30_000,
///     claim_poll_interval_ms: 1_000,
///     max_concurrent_tasks: 1,
///     partition_config: None,
/// };
/// let worker = FlowFabricWorker::connect(config).await?;
///
/// loop {
///     if let Some(task) = worker.claim_next().await? {
///         // Process task...
///         task.complete(Some(b"result".to_vec())).await?;
///     } else {
///         tokio::time::sleep(Duration::from_secs(1)).await;
///     }
/// }
/// ```
pub struct FlowFabricWorker {
    /// RFC-023 Phase 1a (§4.4 item 10c): `ferriskey::Client` is
    /// `valkey-default`-gated **and** `Option` wrapped. Under
    /// sqlite-only features the field is absent. Under
    /// `valkey-default` the field is `Some(client)` when the worker
    /// was built via [`Self::connect`] (the Valkey-bundled entry
    /// point that dials), and `None` when it was built via
    /// [`Self::connect_with`] (the backend-agnostic entry point per
    /// §4.4 item 10e — no ferriskey round-trip). Claim/signal hot
    /// paths (all `valkey-default`-gated per §4.4 item 10f) expect
    /// `Some`; a claim call against a `connect_with`-built worker
    /// panics with a clear message until the backend-agnostic SDK
    /// worker-loop RFC lands (tracked in §8).
    #[cfg(feature = "valkey-default")]
    #[allow(dead_code)]
    client: Option<Client>,
    config: WorkerConfig,
    partition_config: PartitionConfig,
    /// 8-hex FNV-1a digest of the sorted capabilities CSV. Used in
    /// per-mismatch logs so the 4KB CSV never echoes on every reject
    /// during an incident. For [`Self::connect`]-built workers, the
    /// full CSV is additionally logged once at connect-time WARN via
    /// `valkey_preamble::run` for cross-reference; [`Self::connect_with`]-
    /// built workers compute the hash without the companion CSV log.
    /// Mirrors `ff-scheduler::claim::worker_caps_digest`.
    #[cfg(feature = "direct-valkey-claim")]
    worker_capabilities_hash: String,
    #[cfg(feature = "direct-valkey-claim")]
    lane_index: AtomicUsize,
    /// Concurrency cap for in-flight tasks. Permits are acquired or
    /// transferred by [`claim_next`] (feature-gated),
    /// [`claim_from_grant`] (always available), and
    /// [`claim_from_reclaim_grant`], transferred to the returned
    /// [`ClaimedTask`], and released on task complete/fail/cancel/drop.
    /// Holds `max_concurrent_tasks` permits total.
    ///
    /// [`claim_next`]: FlowFabricWorker::claim_next
    /// [`claim_from_grant`]: FlowFabricWorker::claim_from_grant
    /// [`claim_from_reclaim_grant`]: FlowFabricWorker::claim_from_reclaim_grant
    #[cfg_attr(not(feature = "valkey-default"), allow(dead_code))]
    concurrency_semaphore: Arc<Semaphore>,
    /// Rolling offset for chunked partition scans. Each poll advances the
    /// cursor by `PARTITION_SCAN_CHUNK`, so over `ceil(num_partitions /
    /// chunk)` polls every partition is covered. The initial value is
    /// derived from `worker_instance_id` so idle workers spread their
    /// scans across different partitions from the first poll onward.
    ///
    /// Overflow: on 64-bit targets `usize` is `u64` — overflow after
    /// ~2^64 polls (billions of years at any realistic rate). On 32-bit
    /// targets (wasm32, i686) `usize` is `u32` and wraps after ~4 years
    /// at 1 poll/sec — acceptable; on wrap, the modulo preserves
    /// correctness because the sequence simply restarts a new cycle.
    #[cfg(feature = "direct-valkey-claim")]
    scan_cursor: AtomicUsize,
    /// The [`EngineBackend`] the Stage-1b trait forwarders route
    /// through.
    ///
    /// **RFC-012 Stage 1b.** Always populated:
    /// [`FlowFabricWorker::connect`] now wraps the worker's own
    /// `ferriskey::Client` in a `ValkeyBackend` via
    /// `ValkeyBackend::from_client_and_partitions`, and
    /// [`FlowFabricWorker::connect_with`] replaces that default with
    /// the caller-supplied `Arc<dyn EngineBackend>`. The
    /// [`FlowFabricWorker::backend`] accessor still returns
    /// `Option<&Arc<dyn EngineBackend>>` for API stability — Stage 1c
    /// narrows the return type once consumers have migrated.
    ///
    /// Hot paths (claim, deliver_signal, admin queries) still use the
    /// embedded `ferriskey::Client` directly at Stage 1b; Stage 1c
    /// migrates them through this field, and Stage 1d removes the
    /// embedded client.
    backend: Arc<dyn ff_core::engine_backend::EngineBackend>,
    /// Optional handle to the same underlying backend viewed as a
    /// [`CompletionBackend`](ff_core::completion_backend::CompletionBackend).
    /// Populated by [`Self::connect`] from the bundled
    /// `ValkeyBackend` (which implements the trait); supplied by the
    /// caller on [`Self::connect_with`] as an explicit
    /// `Option<Arc<dyn CompletionBackend>>` — `None` means "this
    /// backend does not support push-based completion" (e.g. a future
    /// Postgres backend without LISTEN/NOTIFY, or a test mock). Cairn
    /// and other completion-subscription consumers reach this through
    /// [`Self::completion_backend`].
    completion_backend_handle:
        Option<Arc<dyn ff_core::completion_backend::CompletionBackend>>,
}

/// Number of partitions scanned per `claim_next()` poll. Keeps idle Valkey
/// load at O(PARTITION_SCAN_CHUNK) per worker-second instead of
/// O(num_flow_partitions).
#[cfg(feature = "direct-valkey-claim")]
const PARTITION_SCAN_CHUNK: usize = 32;

impl FlowFabricWorker {
    /// RFC-023 Phase 1a helper: borrow the embedded `ferriskey::Client`,
    /// panicking with a clear message if the worker was built via
    /// [`Self::connect_with`] (which does not dial Valkey). Every
    /// Valkey-specific claim/signal method funnels through this accessor
    /// so the panic site is uniform and traceable.
    ///
    /// A backend-agnostic claim/signal API is deferred per §8 breaking-
    /// change disclosure; until that lands, valkey-default consumers
    /// must use [`Self::connect`] if they intend to drive the Valkey
    /// claim/signal loop.
    #[cfg(feature = "valkey-default")]
    #[inline]
    #[allow(dead_code)]
    fn valkey_client(&self) -> &Client {
        self.client.as_ref().expect(
            "FlowFabricWorker was built via connect_with (no Valkey dial) \
             but a Valkey-specific claim/signal method was invoked. \
             Use FlowFabricWorker::connect to dial Valkey, or drive the \
             backend directly through the trait surface via .backend().",
        )
    }
}

impl FlowFabricWorker {
    /// Connect to Valkey and prepare the worker.
    ///
    /// Establishes the ferriskey connection. Does NOT load the FlowFabric
    /// library — that is the server's responsibility (ff-server calls
    /// `ff_script::loader::ensure_library()` on startup). The SDK assumes
    /// the library is already loaded.
    ///
    /// # Smoke / dev scripts: rotate `WorkerInstanceId`
    ///
    /// The SDK writes a SET-NX liveness sentinel keyed on the worker's
    /// `WorkerInstanceId`. When a smoke / dev script reuses the same
    /// `WorkerInstanceId` across restarts, subsequent runs trap behind
    /// the prior run's SET-NX until the liveness key's TTL (≈ 2× the
    /// configured lease TTL) expires — the worker appears stuck and
    /// claims nothing. Iterative scripts should synthesise a fresh
    /// `WorkerInstanceId` per process (e.g. `WorkerInstanceId::new()`
    /// or embed a UUID/timestamp) rather than hard-coding a stable
    /// value. Production workers that cleanly shut down release the
    /// key; only crashed / kill -9'd processes hit this trap.
    ///
    /// **RFC-023 Phase 1a (§4.4 item 10d, v0.12.0):** this Valkey-bundled
    /// entry point is `#[cfg(feature = "valkey-default")]`-gated.
    /// Consumers on the sqlite-only feature set must use
    /// [`FlowFabricWorker::connect_with`] and drive the backend directly
    /// through the trait surface.
    #[cfg(feature = "valkey-default")]
    pub async fn connect(config: WorkerConfig) -> Result<Self, SdkError> {
        if config.lanes.is_empty() {
            return Err(SdkError::Config {
                context: "worker_config".into(),
                field: None,
                message: "at least one lane is required".into(),
            });
        }

        // Build the ferriskey client from the nested `BackendConfig`.
        // Delegates to `ff_backend_valkey::build_client` so host/port +
        // TLS + cluster + `BackendTimeouts::request` + `BackendRetry`
        // wiring lives in exactly one place (RFC-012 Stage 1c tranche 1).
        let client = ff_backend_valkey::build_client(&config.backend).await?;

        // v0.12 PR-6: the Valkey-specific preamble (PING, alive-key
        // SET-NX, `ff:config:partitions` HGETALL, caps ingress +
        // sorted-dedup CSV, caps STRING + workers-index writes) lives
        // in `crate::valkey_preamble`. Extracted byte-for-byte from
        // the pre-PR-6 inline body; the write order is observable
        // from scheduler-side reads (unblock scanner:
        // SMEMBERS ff:idx:workers → GET ff:worker:{id}:caps) so
        // preservation is load-bearing. See `valkey_preamble::run`.
        let crate::valkey_preamble::PreambleOutput {
            partition_config,
            #[cfg(feature = "direct-valkey-claim")]
            capabilities_csv: _worker_capabilities_csv,
            #[cfg(feature = "direct-valkey-claim")]
            capabilities_hash: worker_capabilities_hash,
        } = crate::valkey_preamble::run(&client, &config).await?;

        let max_tasks = config.max_concurrent_tasks.max(1);
        let concurrency_semaphore = Arc::new(Semaphore::new(max_tasks));

        tracing::info!(
            worker_id = %config.worker_id,
            instance_id = %config.worker_instance_id,
            lanes = ?config.lanes.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            "FlowFabricWorker connected"
        );

        #[cfg(feature = "direct-valkey-claim")]
        let scan_cursor_init = scan_cursor_seed(
            config.worker_instance_id.as_str(),
            partition_config.num_flow_partitions.max(1) as usize,
        );

        // RFC-012 Stage 1b: wrap the dialed client in a ValkeyBackend
        // so `ClaimedTask`'s trait forwarders have something to call.
        // `from_client_and_partitions` reuses the already-dialed client
        // — no second connection. Share the concrete
        // `Arc<ValkeyBackend>` across the two trait objects — one
        // allocation, both accessors yield identity-equivalent handles.
        let valkey_backend: Arc<ff_backend_valkey::ValkeyBackend> =
            ff_backend_valkey::ValkeyBackend::from_client_and_partitions(
                client.clone(),
                partition_config,
            );
        let backend: Arc<dyn ff_core::engine_backend::EngineBackend> = valkey_backend.clone();
        let completion_backend_handle: Option<
            Arc<dyn ff_core::completion_backend::CompletionBackend>,
        > = Some(valkey_backend);

        Ok(Self {
            client: Some(client),
            config,
            partition_config,
            #[cfg(feature = "direct-valkey-claim")]
            worker_capabilities_hash,
            #[cfg(feature = "direct-valkey-claim")]
            lane_index: AtomicUsize::new(0),
            concurrency_semaphore,
            #[cfg(feature = "direct-valkey-claim")]
            scan_cursor: AtomicUsize::new(scan_cursor_init),
            backend,
            completion_backend_handle,
        })
    }

    /// Store pre-built [`EngineBackend`] and (optional)
    /// [`CompletionBackend`] handles on the worker. Builds the worker
    /// via the legacy [`FlowFabricWorker::connect`] path first (so the
    /// embedded `ferriskey::Client` that the Stage 1b non-migrated hot
    /// paths still use is dialed), then replaces the default
    /// `ValkeyBackend` wrapper with the caller-supplied trait objects.
    ///
    /// The `completion` argument is explicit: 0.3.3 previously accepted
    /// only `backend` and `completion_backend()` silently returned
    /// `None` on this path because `Arc<dyn EngineBackend>` cannot be
    /// upcast to `Arc<dyn CompletionBackend>` without loss of
    /// trait-object identity. 0.3.4 lets the caller decide.
    ///
    /// - `Some(arc)` — caller supplies a completion backend.
    ///   [`Self::completion_backend`] returns `Some(clone)`.
    /// - `None` — this backend does not support push-based completion
    ///   (future Postgres backend without LISTEN/NOTIFY, test mocks).
    ///   [`Self::completion_backend`] returns `None`.
    ///
    /// When the underlying backend implements both traits (as
    /// `ValkeyBackend` does), pass the same `Arc` twice — the two
    /// trait-object views share one allocation:
    ///
    /// ```rust,ignore
    /// use std::sync::Arc;
    /// use ff_backend_valkey::ValkeyBackend;
    /// use ff_sdk::{FlowFabricWorker, WorkerConfig};
    ///
    /// # async fn doc(worker_config: WorkerConfig,
    /// #              backend_config: ff_backend_valkey::BackendConfig)
    /// #     -> Result<(), ff_sdk::SdkError> {
    /// // Valkey (completion supported):
    /// let valkey = Arc::new(ValkeyBackend::connect(backend_config).await?);
    /// let worker = FlowFabricWorker::connect_with(
    ///     worker_config,
    ///     valkey.clone(),
    ///     Some(valkey),
    /// ).await?;
    /// # Ok(()) }
    /// ```
    ///
    /// Backend without completion support:
    ///
    /// ```rust,ignore
    /// let worker = FlowFabricWorker::connect_with(
    ///     worker_config,
    ///     backend,
    ///     None,
    /// ).await?;
    /// ```
    ///
    /// **Stage 1b + Round-7 scope — what the injected backend covers
    /// today.** The injected backend currently covers these per-task
    /// `ClaimedTask` ops: `update_progress` / `resume_signals` /
    /// `delay_execution` / `move_to_waiting_children` / `complete` /
    /// `cancel` / `fail` / `create_pending_waitpoint` /
    /// `append_frame` / `report_usage`. A mock backend therefore sees
    /// that portion of the worker's per-task write surface. Lease
    /// renewal also routes through `backend.renew(&handle)`. Round-7
    /// (#135/#145) closed the four trait-shape gaps tracked by #117,
    /// but `suspend` still reaches the embedded `ferriskey::Client`
    /// directly via `ff_suspend_execution` — this is the deferred
    /// suspend per RFC-012 §R7.6.1, pending Stage 1d input-shape
    /// work. `claim_next` / `claim_from_grant` /
    /// `claim_from_reclaim_grant` / `deliver_signal` / admin queries
    /// are Stage 1c hot-path work. Stage 1d removes the embedded
    /// client entirely.
    ///
    /// Today's constructor is therefore NOT yet a drop-in way to swap
    /// in a non-Valkey backend — it requires a reachable Valkey node
    /// for `suspend` plus the remaining hot-path ops. Tests that
    /// exercise only the migrated per-task ops can run fully against
    /// a mock backend.
    ///
    /// [`EngineBackend`]: ff_core::engine_backend::EngineBackend
    /// [`CompletionBackend`]: ff_core::completion_backend::CompletionBackend
    pub async fn connect_with(
        config: WorkerConfig,
        backend: Arc<dyn ff_core::engine_backend::EngineBackend>,
        completion: Option<Arc<dyn ff_core::completion_backend::CompletionBackend>>,
    ) -> Result<Self, SdkError> {
        // RFC-023 Phase 1a (§4.4 item 10e, v0.12.0): direct
        // backend-agnostic construction. No `Self::connect` preamble,
        // no ferriskey round-trips (no PING, no alive-key SET-NX, no
        // `ff:config:partitions` HGETALL). Callers needing a
        // non-default `PartitionConfig` under non-Valkey backends set
        // [`WorkerConfig::partition_config`] (v0.12 PR-6 closed the
        // original follow-up flagged here).
        //
        // Lane-empty validation (the one check `connect` did before
        // any Valkey work) is hoisted here so every entry point
        // refuses an empty lane list.
        if config.lanes.is_empty() {
            return Err(SdkError::Config {
                context: "worker_config".into(),
                field: None,
                message: "at least one lane is required".into(),
            });
        }

        let max_tasks = config.max_concurrent_tasks.max(1);
        let concurrency_semaphore = Arc::new(Semaphore::new(max_tasks));
        // v0.12 PR-6: honor the optional `WorkerConfig::partition_config`
        // override. `None` keeps the pre-PR-6 default shape (256 / 32 /
        // 32); `Some(cfg)` lets non-Valkey deployments with a custom
        // `num_flow_partitions` bind correctly instead of silently
        // missing data via the wrong partition index.
        let partition_config = config.partition_config.unwrap_or_default();

        #[cfg(feature = "direct-valkey-claim")]
        let scan_cursor_init = scan_cursor_seed(
            config.worker_instance_id.as_str(),
            partition_config.num_flow_partitions.max(1) as usize,
        );

        // Build the capabilities CSV / hash inline for the
        // direct-valkey-claim path. The validation mirrors `connect`'s
        // ingress so the two entry points refuse the same malformed
        // tokens.
        #[cfg(feature = "direct-valkey-claim")]
        for cap in &config.capabilities {
            if cap.is_empty() {
                return Err(SdkError::Config {
                    context: "worker_config".into(),
                    field: Some("capabilities".into()),
                    message: "capability token must not be empty".into(),
                });
            }
            if cap.contains(',') {
                return Err(SdkError::Config {
                    context: "worker_config".into(),
                    field: Some("capabilities".into()),
                    message: format!(
                        "capability token may not contain ',' (CSV delimiter): {cap:?}"
                    ),
                });
            }
            if cap.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(SdkError::Config {
                    context: "worker_config".into(),
                    field: Some("capabilities".into()),
                    message: format!(
                        "capability token must not contain whitespace or control \
                         characters: {cap:?}"
                    ),
                });
            }
        }
        #[cfg(feature = "direct-valkey-claim")]
        let worker_capabilities_hash = {
            let set: std::collections::BTreeSet<&str> = config
                .capabilities
                .iter()
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .collect();
            let csv: String = set.into_iter().collect::<Vec<_>>().join(",");
            ff_core::hash::fnv1a_xor8hex(&csv)
        };

        Ok(Self {
            #[cfg(feature = "valkey-default")]
            client: None,
            config,
            partition_config,
            #[cfg(feature = "direct-valkey-claim")]
            worker_capabilities_hash,
            #[cfg(feature = "direct-valkey-claim")]
            lane_index: AtomicUsize::new(0),
            concurrency_semaphore,
            #[cfg(feature = "direct-valkey-claim")]
            scan_cursor: AtomicUsize::new(scan_cursor_init),
            backend,
            completion_backend_handle: completion,
        })
    }

    /// Borrow the `EngineBackend` this worker forwards Stage-1b trait
    /// ops through.
    ///
    /// **RFC-012 Stage 1b.** Always returns `Some(&self.backend)` —
    /// the `Option` wrapper is retained for API stability with the
    /// Stage-1a shape. Stage 1c narrows the return type to
    /// `&Arc<dyn EngineBackend>`.
    pub fn backend(&self) -> Option<&Arc<dyn ff_core::engine_backend::EngineBackend>> {
        Some(&self.backend)
    }

    /// Crate-internal direct borrow of the backend. The public
    /// [`Self::backend`] still returns `Option` for API stability
    /// (Stage 1b holdover). Snapshot trait-forwarders in
    /// [`crate::snapshot`] need an un-wrapped reference.
    #[cfg_attr(not(feature = "valkey-default"), allow(dead_code))]
    pub(crate) fn backend_ref(
        &self,
    ) -> &Arc<dyn ff_core::engine_backend::EngineBackend> {
        &self.backend
    }

    /// Handle to the completion-event subscription backend, for
    /// consumers that need to observe execution completions (DAG
    /// reconcilers, tenant-isolated subscribers).
    ///
    /// Returns `Some` when the worker was built through
    /// [`Self::connect`] on the default `valkey-default` feature
    /// (the bundled `ValkeyBackend` implements
    /// [`CompletionBackend`](ff_core::completion_backend::CompletionBackend)),
    /// or via [`Self::connect_with`] with a `Some(..)` completion
    /// handle. Returns `None` when the caller passed `None` to
    /// [`Self::connect_with`] — i.e. the backend does not support
    /// push-based completion streams (future Postgres without
    /// LISTEN/NOTIFY, test mocks).
    ///
    /// The returned handle shares the same underlying allocation as
    /// [`Self::backend`]; calls through it (e.g.
    /// `subscribe_completions_filtered`) hit the same connection
    /// the worker itself uses.
    pub fn completion_backend(
        &self,
    ) -> Option<Arc<dyn ff_core::completion_backend::CompletionBackend>> {
        self.completion_backend_handle.clone()
    }

    /// Get the worker config.
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    /// Get the server-published partition config this worker bound to at
    /// `connect()`. Exposed so consumers that mint custom
    /// [`ExecutionId`]s (e.g. for `describe_execution` lookups on ids
    /// produced outside this worker) stay aligned with the server's
    /// `num_flow_partitions` — using `PartitionConfig::default()`
    /// assumes 256 partitions and silently misses data on deployments
    /// with any other value.
    pub fn partition_config(&self) -> &ff_core::partition::PartitionConfig {
        &self.partition_config
    }

    /// Attempt to claim the next eligible execution.
    ///
    /// Phase 1 simplified claim flow:
    /// 1. Pick a lane (round-robin across configured lanes)
    /// 2. Issue a claim grant via `ff_issue_claim_grant` on the execution's partition
    /// 3. Claim the execution via `ff_claim_execution`
    /// 4. Read execution payload + tags
    /// 5. Return a [`ClaimedTask`] with auto lease renewal
    ///
    /// Gated behind the `direct-valkey-claim` feature — bypasses the
    /// scheduler's budget / quota / capability admission checks. Enable
    /// with `ff-sdk = { ..., features = ["direct-valkey-claim"] }` when
    /// the scheduler hop would be measurement noise (benches) or when
    /// the test harness needs a deterministic worker-local path. Prefer
    /// the scheduler-routed HTTP claim path in production.
    ///
    /// # `None` semantics
    ///
    /// `Ok(None)` means **no work was found in the partition window this
    /// poll covered**, not "the cluster is idle". Each call scans a chunk
    /// of [`PARTITION_SCAN_CHUNK`] partitions starting at the rolling
    /// `scan_cursor`; the cursor advances by that chunk size on every
    /// invocation, so a worker covers every partition exactly once every
    /// `ceil(num_flow_partitions / PARTITION_SCAN_CHUNK)` polls.
    ///
    /// Callers should treat `None` as "poll again soon" (typically after
    /// `config.claim_poll_interval_ms`) rather than "sleep for a long
    /// time". Backing off too aggressively on `None` can starve workers
    /// when work lives on partitions outside the current window.
    ///
    /// Returns `Err` on Valkey errors or script failures.
    #[cfg(feature = "direct-valkey-claim")]
    pub async fn claim_next(&self) -> Result<Option<ClaimedTask>, SdkError> {
        // Enforce max_concurrent_tasks: try to acquire a semaphore permit.
        // try_acquire returns immediately — if no permits available, the worker
        // is at capacity and should not claim more work.
        let permit = match self.concurrency_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return Ok(None), // At capacity — no claim attempted
        };

        let lane_id = self.next_lane();
        let now = TimestampMs::now();

        // Phase 1: We scan eligible executions directly by reading the eligible
        // ZSET across execution partitions. In production the scheduler
        // (ff-scheduler) would handle this. For Phase 1, the SDK does a
        // simplified inline claim.
        //
        // Chunked scan: each poll covers at most PARTITION_SCAN_CHUNK
        // partitions starting at a rolling offset. This keeps idle Valkey
        // load at O(chunk) per worker-second instead of O(num_partitions),
        // and the worker-instance-seeded initial cursor spreads concurrent
        // workers across different partition windows.
        let num_partitions = self.partition_config.num_flow_partitions as usize;
        if num_partitions == 0 {
            return Ok(None);
        }
        let chunk = PARTITION_SCAN_CHUNK.min(num_partitions);
        let start = self.scan_cursor.fetch_add(chunk, Ordering::Relaxed) % num_partitions;

        // Hoist the sorted/deduped capability set out of the loop — the
        // per-partition iteration reused a fresh BTreeSet on every
        // step pre-fix, adding O(n log n) alloc/sort to the scanner
        // hot path on every tick. Computed once per `claim_next` call.
        let worker_capabilities: std::collections::BTreeSet<String> = self
            .config
            .capabilities
            .iter()
            .cloned()
            .collect();

        for step in 0..chunk {
            let partition_idx = ((start + step) % num_partitions) as u16;
            let partition = ff_core::partition::Partition {
                family: ff_core::partition::PartitionFamily::Execution,
                index: partition_idx,
            };

            // v0.12 PR-5: trait-routed scanner primitive. Replaces the
            // pre-PR-5 `ZRANGEBYSCORE` inline; the Valkey backend fires
            // the identical command byte-for-byte (see
            // `ff_backend_valkey::scan_eligible_executions_impl`).
            let scan_args = ff_core::contracts::ScanEligibleArgs::new(
                lane_id.clone(),
                partition,
                1,
            );
            let candidates = self.backend.scan_eligible_executions(scan_args).await?;
            let execution_id = match candidates.into_iter().next() {
                Some(id) => id,
                None => continue, // No eligible executions on this partition
            };

            // Step 1: Issue claim grant (v0.12 PR-5: trait-routed).
            let grant_args = ff_core::contracts::IssueClaimGrantArgs::new(
                execution_id.clone(),
                lane_id.clone(),
                self.config.worker_id.clone(),
                self.config.worker_instance_id.clone(),
                partition,
                worker_capabilities.clone(),
                5_000, // grant_ttl_ms
                now,
            );
            let grant_result = self.backend.issue_claim_grant(grant_args).await;

            match grant_result {
                Ok(_) => {}
                Err(ref boxed)
                    if matches!(
                        boxed,
                        crate::EngineError::Validation {
                            kind: crate::ValidationKind::CapabilityMismatch,
                            ..
                        }
                    ) =>
                {
                    let missing = match boxed {
                        crate::EngineError::Validation { detail, .. } => detail.clone(),
                        _ => unreachable!(),
                    };
                    // Block-on-mismatch (RFC-009 §7.5) — parity with
                    // ff-scheduler's Scheduler::claim_for_worker. Without
                    // this, the inline-direct-claim path would hot-loop
                    // on an unclaimable top-of-zset (every tick picks the
                    // same execution, wastes an FCALL, logs, releases,
                    // repeats). The scheduler-side unblock scanner
                    // promotes blocked_route executions back to eligible
                    // when a worker with matching caps registers.
                    tracing::info!(
                        execution_id = %execution_id,
                        worker_id = %self.config.worker_id,
                        worker_caps_hash = %self.worker_capabilities_hash,
                        missing = %missing,
                        "capability mismatch, blocking execution off eligible (SDK inline claim)"
                    );
                    // v0.12 PR-5: trait-routed block_route. Swallow
                    // typed outcomes + transport faults (best-effort
                    // semantic preserved from pre-PR-5 behaviour —
                    // see `BlockRouteOutcome::LuaRejected` rustdoc).
                    let block_args = ff_core::contracts::BlockRouteArgs::new(
                        execution_id.clone(),
                        lane_id.clone(),
                        partition,
                        "waiting_for_capable_worker".to_owned(),
                        "no connected worker satisfies required_capabilities".to_owned(),
                        now,
                    );
                    match self.backend.block_route(block_args).await {
                        Ok(ff_core::contracts::BlockRouteOutcome::Blocked { .. }) => {}
                        Ok(ff_core::contracts::BlockRouteOutcome::LuaRejected { message }) => {
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %message,
                                "SDK block_route: Lua rejected; eligible ZSET unchanged, next \
                                 poll will re-evaluate"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                execution_id = %execution_id,
                                error = %e,
                                "SDK block_route: transport failure; eligible ZSET unchanged"
                            );
                        }
                    }
                    continue;
                }
                Err(ref e) if is_retryable_claim_error(e) => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        error = %e,
                        "claim grant failed (retryable), trying next partition"
                    );
                    continue;
                }
                Err(e) => return Err(SdkError::from(e)),
            }

            // Step 2: Claim the execution
            match self
                .claim_execution(&execution_id, &lane_id, &partition, now)
                .await
            {
                Ok(mut task) => {
                    // Transfer concurrency permit to the task. When the task is
                    // completed/failed/cancelled/dropped the permit returns to
                    // the semaphore, allowing another claim.
                    task.set_concurrency_permit(permit);
                    return Ok(Some(task));
                }
                Err(SdkError::Engine(ref boxed))
                    if matches!(
                        **boxed,
                        crate::EngineError::Contention(
                            crate::ContentionKind::UseClaimResumedExecution
                        )
                    ) =>
                {
                    // Execution was resumed from suspension — attempt_interrupted.
                    // ff_claim_execution rejects this; use ff_claim_resumed_execution
                    // which reuses the existing attempt instead of creating a new one.
                    tracing::debug!(
                        execution_id = %execution_id,
                        "execution is resumed, using claim_resumed path"
                    );
                    match self
                        .claim_resumed_execution(&execution_id, &lane_id, &partition)
                        .await
                    {
                        Ok(mut task) => {
                            task.set_concurrency_permit(permit);
                            return Ok(Some(task));
                        }
                        Err(SdkError::Engine(ref e2)) if is_retryable_claim_error(e2) => {
                            tracing::debug!(
                                execution_id = %execution_id,
                                error = %e2,
                                "claim_resumed failed (retryable), trying next partition"
                            );
                            continue;
                        }
                        Err(e2) => return Err(e2),
                    }
                }
                Err(SdkError::Engine(ref e)) if is_retryable_claim_error(e) => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        error = %e,
                        "claim execution failed (retryable), trying next partition"
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        // No eligible work found on any partition
        Ok(None)
    }

    /// Low-level claim of a granted execution. Routes through
    /// [`EngineBackend::claim_execution`](ff_core::engine_backend::EngineBackend::claim_execution)
    /// — the trait-level grant-consumer method landed in v0.12 PR-4 —
    /// and returns a `ClaimedTask` with auto lease renewal.
    ///
    /// Pre-PR-4 the SDK fired the `ff_claim_execution` FCALL directly
    /// against `valkey_client()` and hand-parsed the Lua response shape.
    /// The body now collapses to `backend.claim_execution(args)` +
    /// `backend.read_execution_context(...)` + `ClaimedTask::new(...)`;
    /// the Valkey-specific FCALL plumbing lives behind the trait in
    /// `ff_backend_valkey::claim_execution_impl`.
    ///
    /// As of v0.12 PR-5.5 this helper + `claim_from_grant` are no
    /// longer `valkey-default`-gated: the backend mints the `Handle`
    /// at claim time and `ClaimedTask::new` caches it, so no
    /// Valkey-specific handle synthesis is required on this path. The
    /// `EngineBackend::claim_execution` default impl returns
    /// `Err(Unavailable)` on PG/SQLite today (grant-consumer surface
    /// is Valkey-only until the PG/SQLite grant-consumer RFC lands);
    /// the compile surface is fully agnostic.
    ///
    /// [`claim_from_grant`]: FlowFabricWorker::claim_from_grant
    async fn claim_execution(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
        now: TimestampMs,
    ) -> Result<ClaimedTask, SdkError> {
        // v0.12 PR-5.5 retry-path fix — pre-read the **total attempt
        // counter**, not the current-attempt pointer. The fresh-claim
        // path mints a new attempt row whose index is the counter's
        // current value (the backend's Lua 5920 / PG `ff_claim_execution`
        // / SQLite `claim_impl` all consult this same counter to compute
        // `next_att_idx`). Pre-PR-5.5 this call went inline as `HGET
        // {exec}:core total_attempt_count` on the Valkey client; the
        // first PR-5.5 landing mistakenly routed it to
        // `read_current_attempt_index`, which returns the *pointer* at
        // the previously-leased attempt — on a retry-of-a-retry that
        // still named the terminal-failed prior attempt and the new
        // claim collided with it. `read_total_attempt_count` wraps
        // the same byte-for-byte HGET on Valkey and provides the JSONB
        // / json_extract equivalents on PG / SQLite. See
        // `EngineBackend::read_total_attempt_count` rustdoc for the
        // pointer-vs-counter distinction.
        let att_idx = self
            .backend
            .read_total_attempt_count(execution_id)
            .await
            .map_err(|e| SdkError::Engine(Box::new(e)))?;

        let args = ff_core::contracts::ClaimExecutionArgs::new(
            execution_id.clone(),
            self.config.worker_id.clone(),
            self.config.worker_instance_id.clone(),
            lane_id.clone(),
            LeaseId::new(),
            self.config.lease_ttl_ms,
            AttemptId::new(),
            att_idx,
            "{}".to_owned(),
            None,
            None,
            now,
        );

        // `ClaimExecutionResult` is `#[non_exhaustive]` (v0.12 PR-4)
        // so let-binding requires an explicit wildcard arm even though
        // `Claimed` is the only variant today. A future additive variant
        // surfaces here as a typed `ScriptError::Parse` — loud, routed
        // through the existing SDK error path, and never silently
        // dropped.
        let claimed = match self.backend.claim_execution(args).await? {
            ff_core::contracts::ClaimExecutionResult::Claimed(c) => c,
            other => {
                return Err(SdkError::from(ff_script::error::ScriptError::Parse {
                    fcall: "ff_claim_execution".into(),
                    execution_id: Some(execution_id.to_string()),
                    message: format!(
                        "unexpected ClaimExecutionResult variant: {other:?}"
                    ),
                }));
            }
        };

        // Read execution payload and metadata
        let (input_payload, execution_kind, tags) = self
            .read_execution_context(execution_id, partition)
            .await?;

        Ok(ClaimedTask::new(
            self.backend.clone(),
            self.partition_config,
            claimed.handle,
            execution_id.clone(),
            claimed.attempt_index,
            claimed.attempt_id,
            claimed.lease_id,
            claimed.lease_epoch,
            self.config.lease_ttl_ms,
            lane_id.clone(),
            self.config.worker_instance_id.clone(),
            input_payload,
            execution_kind,
            tags,
        ))
    }

    /// Consume a [`ClaimGrant`] and claim the granted execution on
    /// this worker. The intended production entry point: pair with
    /// [`ff_scheduler::Scheduler::claim_for_worker`] to flow
    /// scheduler-issued grants into the SDK without enabling the
    /// `direct-valkey-claim` feature (which bypasses budget/quota
    /// admission control).
    ///
    /// The worker's concurrency semaphore is checked BEFORE the FCALL
    /// so a saturated worker does not consume the grant: the grant
    /// stays valid for its remaining TTL and the caller can either
    /// release it back to the scheduler or retry after some other
    /// in-flight task completes.
    ///
    /// On success the returned [`ClaimedTask`] holds a concurrency
    /// permit that releases automatically on
    /// `complete`/`fail`/`cancel`/drop — same contract as
    /// `claim_next`.
    ///
    /// # Arguments
    ///
    /// * `lane` — the lane the grant was issued for. Must match what
    ///   was passed to `Scheduler::claim_for_worker`; the Lua FCALL
    ///   uses it to look up `lane_eligible`, `lane_active`, and the
    ///   `worker_leases` index slot.
    /// * `grant` — the [`ClaimGrant`] returned by the scheduler.
    ///
    /// # Errors
    ///
    /// * [`SdkError::WorkerAtCapacity`] — `max_concurrent_tasks`
    ///   permits all held. Retryable; the grant is untouched.
    /// * `ScriptError::InvalidClaimGrant` — grant missing, consumed,
    ///   or `worker_id` mismatch (wrapped in [`SdkError::Engine`]).
    /// * `ScriptError::ClaimGrantExpired` — grant TTL elapsed
    ///   (wrapped in [`SdkError::Engine`]).
    /// * `ScriptError::CapabilityMismatch` — execution's required
    ///   capabilities not a subset of this worker's caps (wrapped in
    ///   [`SdkError::Engine`]). Surfaced post-grant if a race
    ///   between grant issuance and caps change allows it.
    /// * `ScriptError::Parse` — `ff_claim_execution` returned an
    ///   unexpected shape (wrapped in [`SdkError::Engine`]).
    /// * [`SdkError::Backend`] / [`SdkError::BackendContext`] —
    ///   transport error during the FCALL or the
    ///   `read_execution_context` follow-up.
    ///
    /// [`ClaimGrant`]: ff_core::contracts::ClaimGrant
    /// [`ff_scheduler::Scheduler::claim_for_worker`]: https://docs.rs/ff-scheduler
    ///
    /// # Backend coverage (v0.12 PR-5.5)
    ///
    /// Method ungated across backends. The Valkey backend handles the
    /// grant fully. Postgres + SQLite backends return
    /// [`EngineError::Unavailable`](ff_core::engine_error::EngineError::Unavailable)
    /// from [`EngineBackend::claim_execution`] today — grants on those
    /// backends flow through the scheduler-routed [`claim_via_server`]
    /// path (the PG/SQLite scheduler lives outside the `EngineBackend`
    /// trait in this release). See
    /// `project_claim_from_grant_pg_sqlite_gap.md` for motivation and
    /// planned follow-up.
    ///
    /// [`claim_via_server`]: FlowFabricWorker::claim_via_server
    /// [`EngineBackend::claim_execution`]: ff_core::engine_backend::EngineBackend::claim_execution
    pub async fn claim_from_grant(
        &self,
        lane: LaneId,
        grant: ff_core::contracts::ClaimGrant,
    ) -> Result<ClaimedTask, SdkError> {
        // Semaphore check FIRST. If the worker is saturated we must
        // surface the condition to the caller without touching the
        // grant — silently returning Ok(None) (as claim_next does)
        // would drop a grant the scheduler has already committed work
        // to issuing, wasting the slot until its TTL elapses.
        let permit = self
            .concurrency_semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| SdkError::WorkerAtCapacity)?;

        let now = TimestampMs::now();
        let partition = grant.partition().map_err(|e| SdkError::Config {
            context: "claim_from_grant".to_owned(),
            field: Some("partition_key".to_owned()),
            message: e.to_string(),
        })?;
        let mut task = match self
            .claim_execution(&grant.execution_id, &lane, &partition, now)
            .await
        {
            Ok(task) => task,
            Err(SdkError::Engine(ref boxed))
                if matches!(
                    **boxed,
                    crate::EngineError::Contention(
                        crate::ContentionKind::UseClaimResumedExecution
                    )
                ) =>
            {
                // Execution was resumed from suspension — attempt_interrupted.
                // ff_claim_execution rejects this; use ff_claim_resumed_execution
                // which reuses the existing attempt instead of creating a new one.
                // Mirrors the fallback inside `claim_next` so HTTP-routed callers
                // (`claim_via_server` → `claim_from_grant`) get the same transparent
                // re-claim behavior.
                tracing::debug!(
                    execution_id = %grant.execution_id,
                    "execution is resumed, using claim_resumed path"
                );
                self.claim_resumed_execution(&grant.execution_id, &lane, &partition)
                    .await?
            }
            Err(e) => return Err(e),
        };
        task.set_concurrency_permit(permit);
        Ok(task)
    }

    /// Scheduler-routed claim: POST the server's
    /// `/v1/workers/{id}/claim`, then chain to
    /// [`Self::claim_from_grant`].
    ///
    /// Batch C item 2 PR-B. This is the production entry point —
    /// budget + quota + capability admission run server-side inside
    /// `ff_scheduler::Scheduler::claim_for_worker`. Callers don't
    /// enable the `direct-valkey-claim` feature.
    ///
    /// Returns `Ok(None)` when the server says no eligible execution
    /// (HTTP 204). Callers typically back off by
    /// `config.claim_poll_interval_ms` and try again, same cadence
    /// as the direct-claim path's `Ok(None)`.
    ///
    /// The `admin` client is the established HTTP surface
    /// (`FlowFabricAdminClient`) reused here so workers don't keep a
    /// second reqwest client around. Build once at worker boot and
    /// hand in by reference on every claim.
    pub async fn claim_via_server(
        &self,
        admin: &crate::FlowFabricAdminClient,
        lane: &LaneId,
        grant_ttl_ms: u64,
    ) -> Result<Option<ClaimedTask>, SdkError> {
        let req = crate::admin::ClaimForWorkerRequest {
            worker_id: self.config.worker_id.to_string(),
            lane_id: lane.to_string(),
            worker_instance_id: self.config.worker_instance_id.to_string(),
            capabilities: self.config.capabilities.clone(),
            grant_ttl_ms,
        };
        let Some(resp) = admin.claim_for_worker(req).await? else {
            return Ok(None);
        };
        let grant = resp.into_grant()?;
        self.claim_from_grant(lane.clone(), grant).await.map(Some)
    }

    /// Consume a [`ResumeGrant`] and transition the granted
    /// `attempt_interrupted` execution into a `started` state on this
    /// worker. Symmetric partner to [`claim_from_grant`] for the
    /// resume path.
    ///
    /// **Renamed from `claim_from_reclaim_grant` (RFC-024 PR-B+C).**
    /// The new `claim_from_reclaim_grant` method lands with PR-G and
    /// dispatches to [`EngineBackend::reclaim_execution`] for the
    /// lease-reclaim path (distinct semantic, distinct grant type).
    ///
    /// The grant must have been issued to THIS worker (matching
    /// `worker_id` at grant time). A mismatch returns
    /// `Err(Script(InvalidClaimGrant))`. The grant is consumed
    /// atomically by `ff_claim_resumed_execution`; a second call with
    /// the same grant also returns `InvalidClaimGrant`.
    ///
    /// # Concurrency
    ///
    /// The worker's concurrency semaphore is checked BEFORE the FCALL
    /// (same contract as [`claim_from_grant`]). Reclaim does NOT
    /// assume pre-existing capacity on this worker — a reclaim can
    /// land on a fresh worker instance that just came up after a
    /// crash/restart and is picking up a previously-interrupted
    /// execution. If the worker is saturated, the grant stays valid
    /// for its remaining TTL and the caller can release it or retry.
    ///
    /// On success the returned [`ClaimedTask`] holds a concurrency
    /// permit that releases automatically on
    /// `complete`/`fail`/`cancel`/drop.
    ///
    /// # Errors
    ///
    /// * [`SdkError::WorkerAtCapacity`] — `max_concurrent_tasks`
    ///   permits all held. Retryable; the grant is untouched (no
    ///   FCALL was issued, so `ff_claim_resumed_execution` did not
    ///   atomically consume the grant key).
    /// * `ScriptError::InvalidClaimGrant` — grant missing, consumed,
    ///   or `worker_id` mismatch.
    /// * `ScriptError::ClaimGrantExpired` — grant TTL elapsed.
    /// * `ScriptError::NotAResumedExecution` — `attempt_state` is not
    ///   `attempt_interrupted`.
    /// * `ScriptError::ExecutionNotLeaseable` — `lifecycle_phase` is
    ///   not `runnable`.
    /// * `ScriptError::ExecutionNotFound` — core key missing.
    /// * [`SdkError::Backend`] / [`SdkError::BackendContext`] —
    ///   transport.
    ///
    /// [`ResumeGrant`]: ff_core::contracts::ResumeGrant
    /// [`claim_from_grant`]: FlowFabricWorker::claim_from_grant
    pub async fn claim_from_resume_grant(
        &self,
        grant: ff_core::contracts::ResumeGrant,
    ) -> Result<ClaimedTask, SdkError> {
        // Semaphore check FIRST — same load-bearing ordering as
        // `claim_from_grant`. If the worker is saturated, surface
        // WorkerAtCapacity without firing the FCALL; the FCALL is an
        // atomic consume on the grant key, so calling it past-
        // saturation would destroy the grant while leaving no
        // permit to attach to the returned `ClaimedTask`.
        let permit = self
            .concurrency_semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| SdkError::WorkerAtCapacity)?;

        // Grant carries partition + lane_id so no round-trip is needed
        // to resolve them before the FCALL.
        let partition = grant.partition().map_err(|e| SdkError::Config {
            context: "claim_from_resume_grant".to_owned(),
            field: Some("partition_key".to_owned()),
            message: e.to_string(),
        })?;
        let mut task = self
            .claim_resumed_execution(
                &grant.execution_id,
                &grant.lane_id,
                &partition,
            )
            .await?;
        task.set_concurrency_permit(permit);
        Ok(task)
    }

    /// Consume a [`ReclaimGrant`] to mint a fresh attempt for a
    /// lease-expired / lease-revoked execution (RFC-024 §3.4).
    ///
    /// Backend-agnostic. Routes through
    /// [`EngineBackend::reclaim_execution`] on whatever backend the
    /// worker was connected with (Valkey via `connect` / `connect_with`,
    /// Postgres / SQLite via `connect_with`). Distinct from
    /// [`claim_from_resume_grant`]: reclaim creates a NEW attempt row
    /// and bumps `lease_reclaim_count` (`HandleKind::Reclaimed`), while
    /// resume re-uses the existing attempt under
    /// `ff_claim_resumed_execution` (`HandleKind::Resumed`).
    ///
    /// # Return shape
    ///
    /// Returns the raw [`ReclaimExecutionOutcome`] so consumers match
    /// on the four outcomes (`Claimed(Handle)`, `NotReclaimable`,
    /// `ReclaimCapExceeded`, `GrantNotFound`) and decide their
    /// dispatch. The `Claimed` variant carries a [`Handle`] whose
    /// `kind` is [`HandleKind::Reclaimed`]; downstream ops (complete,
    /// fail, renew, append_frame, …) take the handle directly via the
    /// `EngineBackend` trait.
    ///
    /// This contrasts with [`claim_from_resume_grant`], which wraps
    /// the handle in a [`ClaimedTask`] with a concurrency-permit and
    /// auto-lease-renewal loop. Those affordances are
    /// `valkey-default`-gated today (they depend on the bundled
    /// `ferriskey::Client` + `lease_ttl_ms` renewal timer). The
    /// reclaim surface is intentionally narrower so it compiles under
    /// `--no-default-features, features = ["sqlite"]` and consumers
    /// can drive the reclaim flow on any backend.
    ///
    /// # Feature compatibility
    ///
    /// No cfg-gate. Compiles + runs under every feature set ff-sdk
    /// supports (including sqlite-only). Verified by the compile-time
    /// type assertion
    /// `worker_claim_from_reclaim_grant_is_backend_agnostic_at_type_level`
    /// in `crates/ff-sdk/tests/rfc024_sdk.rs`, which pins the method's
    /// full signature under the default feature set and is paralleled
    /// by `sqlite_only_compile_surface_tests` in this file for the
    /// `--no-default-features, features = ["sqlite"]` compile anchor on
    /// the rest of the backend-agnostic surface.
    ///
    /// # worker_capabilities
    ///
    /// `ReclaimExecutionArgs::worker_capabilities` is NOT part of the
    /// Lua FCALL (the reclaim Lua validates grant consumption via
    /// `grant.worker_id == args.worker_id` only — see
    /// `crates/ff-script/src/flowfabric.lua:3088` and RFC-024 §4.4).
    /// Capability matching happens at grant-issuance time (see
    /// [`FlowFabricAdminClient::issue_reclaim_grant`]
    /// — in the `ff-sdk::admin` module, `valkey-default`-gated).
    ///
    /// # Errors
    ///
    /// * [`SdkError::Engine`] — the backend's `reclaim_execution`
    ///   returned an [`EngineError`] (transport fault, validation
    ///   failure, `Unavailable` from a backend that does not
    ///   implement RFC-024 — currently only pre-RFC out-of-tree
    ///   backends).
    ///
    /// [`ReclaimGrant`]: ff_core::contracts::ReclaimGrant
    /// [`ReclaimExecutionOutcome`]: ff_core::contracts::ReclaimExecutionOutcome
    /// [`Handle`]: ff_core::backend::Handle
    /// [`HandleKind::Reclaimed`]: ff_core::backend::HandleKind::Reclaimed
    /// [`EngineBackend::reclaim_execution`]: ff_core::engine_backend::EngineBackend::reclaim_execution
    /// [`EngineError`]: ff_core::engine_error::EngineError
    /// [`claim_from_resume_grant`]: FlowFabricWorker::claim_from_resume_grant
    /// [`FlowFabricAdminClient::issue_reclaim_grant`]: crate::admin::FlowFabricAdminClient::issue_reclaim_grant
    pub async fn claim_from_reclaim_grant(
        &self,
        grant: ff_core::contracts::ReclaimGrant,
        args: ff_core::contracts::ReclaimExecutionArgs,
    ) -> Result<ff_core::contracts::ReclaimExecutionOutcome, SdkError> {
        // `ReclaimGrant` is accepted as a parameter so the call-site
        // shape matches RFC-024 §3.4 (consumer receives a grant from
        // `issue_reclaim_grant` and feeds it + args into
        // `claim_from_reclaim_grant`). The grant metadata is
        // already embedded in the backend's server-side store
        // (Valkey `claim_grant` hash, PG/SQLite `ff_claim_grant`
        // table) keyed by (execution_id, grant_key) — the trait
        // method looks it up from `args.execution_id`.
        //
        // The overlap between `ReclaimGrant` and
        // `ReclaimExecutionArgs` is (execution_id, lane_id);
        // mismatched grant/args is a consumer-side bug (grant-for-A
        // + args-for-B), and silently forwarding lets the SDK act
        // on the args execution. Reject the mismatch up front so
        // the misuse surfaces at the SDK boundary instead of
        // succeeding against the wrong execution. The grant's
        // `expires_at_ms` is validated against wall-clock now so
        // an already-expired grant is rejected without a backend
        // round-trip (the backend also enforces expiry, but the
        // SDK-side check gives a crisper error and preserves the
        // reclaim-budget / lease slot).
        // `TimestampMs` is an `i64` (Unix-epoch ms); cast to `u64` for
        // comparison against `ReclaimGrant::expires_at_ms`. Clamp
        // negatives to 0 so a pre-epoch system clock (vanishingly
        // unlikely, but representable) doesn't wrap to a future u64.
        let now_ms: u64 = ff_core::types::TimestampMs::now().0.max(0) as u64;
        validate_reclaim_grant_against_args(&grant, &args, now_ms)?;
        self.backend
            .reclaim_execution(args)
            .await
            .map_err(|e| SdkError::Engine(Box::new(e)))
    }

    /// Low-level resume claim. Forwards through
    /// [`EngineBackend::claim_resumed_execution`](ff_core::engine_backend::EngineBackend::claim_resumed_execution)
    /// — the trait-level trigger surface landed in issue #150 — and
    /// returns a [`ClaimedTask`] bound to the resumed attempt.
    ///
    /// The method stays private; external callers use
    /// [`claim_from_resume_grant`].
    ///
    /// [`claim_from_resume_grant`]: FlowFabricWorker::claim_from_resume_grant
    async fn claim_resumed_execution(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
    ) -> Result<ClaimedTask, SdkError> {
        // v0.12 PR-3 — pre-read current_attempt_index via the trait
        // rather than an inline `HGET` on the Valkey client. Load-bearing:
        // the backend's KEYS[6] (Valkey) / `ff_attempt` PK tuple (PG/SQLite)
        // must target the real existing attempt hash/row, and the backend
        // takes the index verbatim from
        // `ClaimResumedExecutionArgs::current_attempt_index`.
        let att_idx = self
            .backend
            .read_current_attempt_index(execution_id)
            .await
            .map_err(|e| SdkError::Engine(Box::new(e)))?;

        let args = ff_core::contracts::ClaimResumedExecutionArgs {
            execution_id: execution_id.clone(),
            worker_id: self.config.worker_id.clone(),
            worker_instance_id: self.config.worker_instance_id.clone(),
            lane_id: lane_id.clone(),
            lease_id: LeaseId::new(),
            lease_ttl_ms: self.config.lease_ttl_ms,
            current_attempt_index: att_idx,
            remaining_attempt_timeout_ms: None,
            now: TimestampMs::now(),
        };

        let ff_core::contracts::ClaimResumedExecutionResult::Claimed(claimed) =
            self.backend.claim_resumed_execution(args).await?;

        let (input_payload, execution_kind, tags) = self
            .read_execution_context(execution_id, partition)
            .await?;

        Ok(ClaimedTask::new(
            self.backend.clone(),
            self.partition_config,
            claimed.handle,
            execution_id.clone(),
            claimed.attempt_index,
            claimed.attempt_id,
            claimed.lease_id,
            claimed.lease_epoch,
            self.config.lease_ttl_ms,
            lane_id.clone(),
            self.config.worker_instance_id.clone(),
            input_payload,
            execution_kind,
            tags,
        ))
    }

    /// Read payload + execution_kind + tags from exec_core.
    ///
    /// As of v0.12 PR-1 this forwards through
    /// [`EngineBackend::read_execution_context`](ff_core::engine_backend::EngineBackend::read_execution_context)
    /// rather than issuing direct GET/HGET/HGETALL against Valkey. The
    /// outer `valkey-default` gate + `(&ExecutionId, &Partition)`
    /// signature are preserved; hot-path decoupling (ungating this
    /// helper + its call sites in `claim_execution` and
    /// `claim_resumed_execution`) is PR-4/PR-5 scope per the v0.12
    /// agnostic-SDK plan.
    async fn read_execution_context(
        &self,
        execution_id: &ExecutionId,
        _partition: &ff_core::partition::Partition,
    ) -> Result<(Vec<u8>, String, HashMap<String, String>), SdkError> {
        let ctx = self.backend.read_execution_context(execution_id).await?;
        Ok((ctx.input_payload, ctx.execution_kind, ctx.tags))
    }

    // ── Phase 3: Signal delivery ──

    /// Deliver a signal to a suspended execution's waitpoint.
    ///
    /// The engine atomically records the signal, evaluates the resume condition,
    /// and optionally transitions the execution from `suspended` to `runnable`.
    ///
    /// Forwards through
    /// [`EngineBackend::deliver_signal`](ff_core::engine_backend::EngineBackend::deliver_signal)
    /// — the trait-level trigger surface landed in issue #150.
    ///
    /// Backend-agnostic as of v0.12 PR-3. Compiles + runs under every
    /// feature set ff-sdk supports (including
    /// `--no-default-features --features sqlite`); pinned by
    /// `sqlite_only_compile_surface_tests::deliver_signal_addressable_under_sqlite_only`.
    pub async fn deliver_signal(
        &self,
        execution_id: &ExecutionId,
        waitpoint_id: &WaitpointId,
        signal: crate::task::Signal,
    ) -> Result<crate::task::SignalOutcome, SdkError> {
        let args = ff_core::contracts::DeliverSignalArgs {
            execution_id: execution_id.clone(),
            waitpoint_id: waitpoint_id.clone(),
            signal_id: ff_core::types::SignalId::new(),
            signal_name: signal.signal_name,
            signal_category: signal.signal_category,
            source_type: signal.source_type,
            source_identity: signal.source_identity,
            payload: signal.payload,
            payload_encoding: Some("json".to_owned()),
            correlation_id: None,
            idempotency_key: signal.idempotency_key,
            target_scope: "waitpoint".to_owned(),
            created_at: Some(TimestampMs::now()),
            dedup_ttl_ms: None,
            resume_delay_ms: None,
            max_signals_per_execution: None,
            signal_maxlen: None,
            waitpoint_token: signal.waitpoint_token,
            now: TimestampMs::now(),
        };

        let result = self.backend.deliver_signal(args).await?;
        Ok(match result {
            ff_core::contracts::DeliverSignalResult::Accepted { signal_id, effect } => {
                if effect == "resume_condition_satisfied" {
                    crate::task::SignalOutcome::TriggeredResume { signal_id }
                } else {
                    crate::task::SignalOutcome::Accepted { signal_id, effect }
                }
            }
            ff_core::contracts::DeliverSignalResult::Duplicate { existing_signal_id } => {
                crate::task::SignalOutcome::Duplicate {
                    existing_signal_id: existing_signal_id.to_string(),
                }
            }
        })
    }

    #[cfg(feature = "direct-valkey-claim")]
    fn next_lane(&self) -> LaneId {
        let idx = self.lane_index.fetch_add(1, Ordering::Relaxed) % self.config.lanes.len();
        self.config.lanes[idx].clone()
    }
}

#[cfg(feature = "direct-valkey-claim")]
fn is_retryable_claim_error(err: &crate::EngineError) -> bool {
    use ff_core::error::ErrorClass;
    matches!(
        ff_script::engine_error_ext::class(err),
        ErrorClass::Retryable | ErrorClass::Informational
    )
}

/// Initial offset for [`FlowFabricWorker::scan_cursor`]. Hashes the worker
/// instance id with FNV-1a to place distinct worker processes on different
/// partition windows from their first poll. Zero is valid for single-worker
/// clusters but spreads work in multi-worker deployments.
#[cfg(feature = "direct-valkey-claim")]
fn scan_cursor_seed(worker_instance_id: &str, num_partitions: usize) -> usize {
    if num_partitions == 0 {
        return 0;
    }
    (ff_core::hash::fnv1a_u64(worker_instance_id.as_bytes()) as usize) % num_partitions
}

/// Cross-check the [`ReclaimGrant`] handed back by
/// `issue_reclaim_grant` against the [`ReclaimExecutionArgs`] the
/// consumer is about to dispatch. Catches the grant-for-A + args-for-B
/// misuse at the SDK boundary before a backend round-trip (PR #407
/// review F1).
///
/// The overlap between the two types is `(execution_id, lane_id)`;
/// `partition_key` / `grant_key` live only on the grant and are
/// verified server-side. The grant's `expires_at_ms` is also
/// validated against `now_ms` so an expired grant fails fast without
/// burning a backend call (the backend enforces expiry too, but the
/// SDK-side check gives a crisper, typed error).
///
/// [`ReclaimGrant`]: ff_core::contracts::ReclaimGrant
/// [`ReclaimExecutionArgs`]: ff_core::contracts::ReclaimExecutionArgs
fn validate_reclaim_grant_against_args(
    grant: &ff_core::contracts::ReclaimGrant,
    args: &ff_core::contracts::ReclaimExecutionArgs,
    now_ms: u64,
) -> Result<(), SdkError> {
    if grant.execution_id != args.execution_id {
        return Err(SdkError::Config {
            context: "claim_from_reclaim_grant".to_owned(),
            field: Some("execution_id".to_owned()),
            message: format!(
                "grant.execution_id ({}) does not match args.execution_id ({})",
                grant.execution_id, args.execution_id
            ),
        });
    }
    if grant.lane_id != args.lane_id {
        return Err(SdkError::Config {
            context: "claim_from_reclaim_grant".to_owned(),
            field: Some("lane_id".to_owned()),
            message: format!(
                "grant.lane_id ({}) does not match args.lane_id ({})",
                grant.lane_id.as_str(),
                args.lane_id.as_str()
            ),
        });
    }
    if grant.expires_at_ms <= now_ms {
        return Err(SdkError::Config {
            context: "claim_from_reclaim_grant".to_owned(),
            field: Some("expires_at_ms".to_owned()),
            message: format!(
                "grant expired: expires_at_ms={} now_ms={}",
                grant.expires_at_ms, now_ms
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod reclaim_grant_validation_tests {
    //! Unit tests for `validate_reclaim_grant_against_args` — the
    //! SDK-side cross-check that catches grant/args mismatch (PR #407
    //! review F1). No Valkey / backend required: the helper is pure.
    use super::validate_reclaim_grant_against_args;
    use crate::SdkError;
    use ff_core::contracts::{ReclaimExecutionArgs, ReclaimGrant};
    use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
    use ff_core::types::{
        AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseId, WorkerId, WorkerInstanceId,
    };

    const EXEC_A: &str = "{fp:7}:00000000-0000-4000-8000-000000000001";
    const EXEC_B: &str = "{fp:7}:00000000-0000-4000-8000-000000000002";

    fn exec(s: &str) -> ExecutionId {
        ExecutionId::parse(s).expect("valid execution id")
    }

    fn grant_for(execution_id: ExecutionId, lane: &str, expires_at_ms: u64) -> ReclaimGrant {
        ReclaimGrant::new(
            execution_id,
            PartitionKey::from(&Partition { family: PartitionFamily::Flow, index: 7 }),
            "reclaim:grant:abc".to_owned(),
            expires_at_ms,
            LaneId::new(lane),
        )
    }

    fn args_for(execution_id: ExecutionId, lane: &str) -> ReclaimExecutionArgs {
        ReclaimExecutionArgs::new(
            execution_id,
            WorkerId::new("w1"),
            WorkerInstanceId::new("w1-i1"),
            LaneId::new(lane),
            None,
            LeaseId::new(),
            30_000,
            AttemptId::new(),
            "{}".to_owned(),
            None,
            WorkerInstanceId::new("w1-i0"),
            AttemptIndex::new(0),
        )
    }

    #[test]
    fn accepts_matching_grant_and_args() {
        let g = grant_for(exec(EXEC_A), "main", 2_000);
        let a = args_for(exec(EXEC_A), "main");
        assert!(validate_reclaim_grant_against_args(&g, &a, 1_000).is_ok());
    }

    #[test]
    fn rejects_mismatched_execution_id() {
        let g = grant_for(exec(EXEC_A), "main", 2_000);
        let a = args_for(exec(EXEC_B), "main");
        let err = validate_reclaim_grant_against_args(&g, &a, 1_000)
            .expect_err("expected mismatched execution_id to fail");
        match err {
            SdkError::Config { field, .. } => assert_eq!(field.as_deref(), Some("execution_id")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_mismatched_lane_id() {
        let g = grant_for(exec(EXEC_A), "main", 2_000);
        let a = args_for(exec(EXEC_A), "other");
        let err = validate_reclaim_grant_against_args(&g, &a, 1_000)
            .expect_err("expected mismatched lane_id to fail");
        match err {
            SdkError::Config { field, .. } => assert_eq!(field.as_deref(), Some("lane_id")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_expired_grant() {
        // expires_at_ms == now_ms is rejected (strict `<=`) so the
        // server's TTL enforcement window can't race us.
        let g = grant_for(exec(EXEC_A), "main", 1_000);
        let a = args_for(exec(EXEC_A), "main");
        let err = validate_reclaim_grant_against_args(&g, &a, 1_000)
            .expect_err("expected expired grant to fail");
        match err {
            SdkError::Config { field, .. } => assert_eq!(field.as_deref(), Some("expires_at_ms")),
            other => panic!("expected Config error, got {other:?}"),
        }

        // Also rejected when already past expiry.
        let err2 = validate_reclaim_grant_against_args(&g, &a, 5_000)
            .expect_err("expected past-expiry grant to fail");
        match err2 {
            SdkError::Config { field, .. } => assert_eq!(field.as_deref(), Some("expires_at_ms")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod completion_accessor_type_tests {
    //! Type-level compile check that
    //! [`FlowFabricWorker::completion_backend`] returns an
    //! `Option<Arc<dyn CompletionBackend>>`. No Valkey required —
    //! the assertion is at the function-pointer type level and the
    //! #[test] body exists solely so the compiler elaborates it.
    use super::FlowFabricWorker;
    use ff_core::completion_backend::CompletionBackend;
    use std::sync::Arc;

    #[test]
    fn completion_backend_accessor_signature() {
        // If this line compiles, the public accessor returns the
        // advertised type. The function is never called (no live
        // worker), so no I/O happens.
        let _f: fn(&FlowFabricWorker) -> Option<Arc<dyn CompletionBackend>> =
            FlowFabricWorker::completion_backend;
    }
}

/// RFC-023 Phase 1a §4.4 item 10g compile-only anchor. Parallels
/// `completion_accessor_type_tests` but fires under the sqlite-only
/// feature set: proves `FlowFabricWorker::connect_with` is callable
/// and returns the advertised type under `--no-default-features,
/// features = ["sqlite"]`. The matching `§9` CI cell (`cargo check
/// -p ff-sdk --no-default-features --features sqlite`) executes this
/// compile check mechanically so future PRs that accidentally reach
/// outside the `valkey-default` gate fail the build.
#[cfg(all(test, not(feature = "valkey-default")))]
mod sqlite_only_compile_surface_tests {
    use super::FlowFabricWorker;
    use ff_core::completion_backend::CompletionBackend;
    use ff_core::engine_backend::EngineBackend;
    use std::sync::Arc;

    #[test]
    fn addressable_surface_under_sqlite_only() {
        // Type-level proof that the backend-agnostic accessors are
        // reachable without the `valkey-default` feature. None of
        // these are called; the assignment targets pin the signature.
        let _a: fn(
            &FlowFabricWorker,
        ) -> Option<&Arc<dyn EngineBackend>> = FlowFabricWorker::backend;
        let _b: fn(
            &FlowFabricWorker,
        ) -> Option<Arc<dyn CompletionBackend>> =
            FlowFabricWorker::completion_backend;
        let _c: fn(&FlowFabricWorker) -> &crate::config::WorkerConfig =
            FlowFabricWorker::config;
        let _d: fn(&FlowFabricWorker) -> &ff_core::partition::PartitionConfig =
            FlowFabricWorker::partition_config;
    }

    /// v0.12 PR-1 compile anchor — the new
    /// [`EngineBackend::read_execution_context`] trait method MUST be
    /// addressable under `--no-default-features --features sqlite`. A
    /// direct fn-pointer cast is awkward under `#[async_trait]`
    /// (lifetime-generic fn items don't coerce to `fn` pointers), so
    /// we take the next-best compile proof: exercise a generic that
    /// names the method through the trait bound. A bodyless call would
    /// require a concrete backend; the generic keeps the test pure
    /// compile-time.
    #[test]
    fn read_execution_context_addressable_under_sqlite_only() {
        use ff_core::contracts::ExecutionContext;
        use ff_core::engine_error::EngineError;
        use ff_core::types::ExecutionId;

        #[allow(dead_code)]
        async fn _pin<B: EngineBackend + ?Sized>(
            b: &B,
            id: &ExecutionId,
        ) -> Result<ExecutionContext, EngineError> {
            b.read_execution_context(id).await
        }
    }

    /// v0.12 PR-2 compile anchor — `ClaimedTask` as a type MUST be
    /// addressable under `--no-default-features --features sqlite`
    /// (the `task` module is no longer `valkey-default`-gated at the
    /// module level). As of v0.12 PR-5.5 the `impl ClaimedTask { ... }`
    /// block is likewise ungated: the backend mints the `Handle` at
    /// claim time and `cloned_handle` just clones the cached field, so
    /// no Valkey-specific synthesis is required. This anchor pins the
    /// struct path + its field types through a generic-over-T wrapper
    /// so a compile-time lookup of `ClaimedTask` is exercised
    /// mechanically under the sqlite-only feature set.
    #[test]
    fn claimed_task_type_addressable_under_sqlite_only() {
        use crate::task::ClaimedTask;

        #[allow(dead_code)]
        fn _pin<T>(_: std::marker::PhantomData<T>) -> std::marker::PhantomData<ClaimedTask> {
            std::marker::PhantomData
        }
    }

    /// v0.12 PR-6 compile anchor — ungating `admin` + `snapshot`
    /// modules at module level must not re-introduce ferriskey
    /// symbols under `--no-default-features --features sqlite`.
    /// Pins that [`FlowFabricAdminClient::new`] and a representative
    /// snapshot forwarder are addressable without the
    /// `valkey-default` feature.
    #[test]
    fn admin_and_snapshot_addressable_under_sqlite_only() {
        use crate::admin::FlowFabricAdminClient;
        use crate::SdkError;
        use ff_core::contracts::ExecutionSnapshot;
        use ff_core::types::ExecutionId;

        // `FlowFabricAdminClient::new` is `fn(impl Into<String>)` —
        // can't fn-pointer-coerce directly. A no-op call with a
        // concrete `&str` pins the method addressably under the
        // sqlite-only feature set (the error return-type is the
        // same `SdkError` the rest of the module returns).
        #[allow(dead_code)]
        fn _pin_admin_new() -> Result<FlowFabricAdminClient, SdkError> {
            FlowFabricAdminClient::new("http://anchor")
        }

        // Snapshot method is `async`, so coerce via the same
        // trait-bound pattern as `read_execution_context` above.
        #[allow(dead_code)]
        async fn _pin_describe(
            w: &FlowFabricWorker,
            id: &ExecutionId,
        ) -> Result<Option<ExecutionSnapshot>, SdkError> {
            w.describe_execution(id).await
        }
    }

    /// v0.13 SC-10 compile anchor — the backend-agnostic admin facade
    /// (`FlowFabricAdminClient::connect_with` + embedded-transport
    /// methods) MUST be addressable under
    /// `--no-default-features --features sqlite`. Without this anchor,
    /// a regression that re-introduces a ferriskey symbol on the
    /// embedded admin path would only surface on downstream
    /// sqlite-only consumers.
    #[test]
    fn admin_facade_addressable_under_sqlite_only() {
        use crate::admin::{
            ClaimForWorkerRequest, ClaimForWorkerResponse, FlowFabricAdminClient,
            IssueReclaimGrantRequest, IssueReclaimGrantResponse, RotateWaitpointSecretRequest,
            RotateWaitpointSecretResponse,
        };
        use crate::SdkError;
        use std::sync::Arc;

        // `connect_with` is infallible; fn-pointer coerce pins the
        // signature under the sqlite-only feature set.
        let _ctor: fn(Arc<dyn EngineBackend>) -> FlowFabricAdminClient =
            FlowFabricAdminClient::connect_with;

        // Each async method is generic over `&self` lifetimes under
        // `#[async_trait]`-style expansion, so pin via a wrapper fn.
        #[allow(dead_code)]
        async fn _pin_claim(
            c: &FlowFabricAdminClient,
            req: ClaimForWorkerRequest,
        ) -> Result<Option<ClaimForWorkerResponse>, SdkError> {
            c.claim_for_worker(req).await
        }
        #[allow(dead_code)]
        async fn _pin_reclaim(
            c: &FlowFabricAdminClient,
            id: &str,
            req: IssueReclaimGrantRequest,
        ) -> Result<IssueReclaimGrantResponse, SdkError> {
            c.issue_reclaim_grant(id, req).await
        }
        #[allow(dead_code)]
        async fn _pin_rotate(
            c: &FlowFabricAdminClient,
            req: RotateWaitpointSecretRequest,
        ) -> Result<RotateWaitpointSecretResponse, SdkError> {
            c.rotate_waitpoint_secret(req).await
        }
    }

    /// v0.12 PR-3 compile anchor — the new
    /// [`EngineBackend::read_current_attempt_index`] trait method MUST
    /// be addressable under `--no-default-features --features sqlite`.
    /// Mirrors the PR-1 `read_execution_context` anchor: takes a
    /// generic over the trait bound so `#[async_trait]` lifetime
    /// elision doesn't block an `fn`-pointer coercion.
    #[test]
    fn read_current_attempt_index_addressable_under_sqlite_only() {
        use ff_core::engine_error::EngineError;
        use ff_core::types::{AttemptIndex, ExecutionId};

        #[allow(dead_code)]
        async fn _pin<B: EngineBackend + ?Sized>(
            b: &B,
            id: &ExecutionId,
        ) -> Result<AttemptIndex, EngineError> {
            b.read_current_attempt_index(id).await
        }
    }

    /// v0.12 PR-5.5 retry-path-fix compile anchor — the new
    /// [`EngineBackend::read_total_attempt_count`] trait method MUST
    /// be addressable under `--no-default-features --features sqlite`.
    /// Mirrors the PR-3 `read_current_attempt_index` anchor.
    #[test]
    fn read_total_attempt_count_addressable_under_sqlite_only() {
        use ff_core::engine_error::EngineError;
        use ff_core::types::{AttemptIndex, ExecutionId};

        #[allow(dead_code)]
        async fn _pin<B: EngineBackend + ?Sized>(
            b: &B,
            id: &ExecutionId,
        ) -> Result<AttemptIndex, EngineError> {
            b.read_total_attempt_count(id).await
        }
    }

    /// v0.12 PR-4 compile anchor — the new
    /// [`EngineBackend::claim_execution`] trait method MUST be
    /// addressable under `--no-default-features --features sqlite`.
    /// Mirrors the PR-3 `read_current_attempt_index` anchor: takes a
    /// generic over the trait bound so `#[async_trait]` lifetime
    /// elision doesn't block a plain fn-pointer coercion.
    ///
    /// The method has an `Err(Unavailable)` default impl; PG + SQLite
    /// backends don't override it today (grants are Valkey-only until
    /// the PG/SQLite grant-consumer RFC lands). The anchor pins the
    /// trait-surface signature — not a runtime call — so the compile
    /// check passes cleanly on every feature set.
    #[test]
    fn claim_execution_addressable_under_sqlite_only() {
        use ff_core::contracts::{ClaimExecutionArgs, ClaimExecutionResult};
        use ff_core::engine_error::EngineError;

        #[allow(dead_code)]
        async fn _pin<B: EngineBackend + ?Sized>(
            b: &B,
            args: ClaimExecutionArgs,
        ) -> Result<ClaimExecutionResult, EngineError> {
            b.claim_execution(args).await
        }
    }

    /// v0.12 PR-5 compile anchor — the three new scanner-primitive
    /// trait methods (`scan_eligible_executions`, `issue_claim_grant`,
    /// `block_route`) MUST be addressable under
    /// `--no-default-features --features sqlite`. Each has an
    /// `Err(Unavailable)` default impl; PG + SQLite backends don't
    /// override (the scheduler-routed `claim_for_worker` path is the
    /// supported PG/SQLite entry point). The anchor pins the trait-
    /// surface signatures — not runtime calls — so the compile check
    /// passes cleanly on every feature set.
    #[test]
    fn scan_eligible_executions_addressable_under_sqlite_only() {
        use ff_core::contracts::ScanEligibleArgs;
        use ff_core::engine_error::EngineError;
        use ff_core::types::ExecutionId;

        #[allow(dead_code)]
        async fn _pin<B: EngineBackend + ?Sized>(
            b: &B,
            args: ScanEligibleArgs,
        ) -> Result<Vec<ExecutionId>, EngineError> {
            b.scan_eligible_executions(args).await
        }
    }

    #[test]
    fn issue_claim_grant_addressable_under_sqlite_only() {
        use ff_core::contracts::{IssueClaimGrantArgs, IssueClaimGrantOutcome};
        use ff_core::engine_error::EngineError;

        #[allow(dead_code)]
        async fn _pin<B: EngineBackend + ?Sized>(
            b: &B,
            args: IssueClaimGrantArgs,
        ) -> Result<IssueClaimGrantOutcome, EngineError> {
            b.issue_claim_grant(args).await
        }
    }

    #[test]
    fn block_route_addressable_under_sqlite_only() {
        use ff_core::contracts::{BlockRouteArgs, BlockRouteOutcome};
        use ff_core::engine_error::EngineError;

        #[allow(dead_code)]
        async fn _pin<B: EngineBackend + ?Sized>(
            b: &B,
            args: BlockRouteArgs,
        ) -> Result<BlockRouteOutcome, EngineError> {
            b.block_route(args).await
        }
    }

    /// v0.12 PR-3 compile anchor — `FlowFabricWorker::deliver_signal`
    /// MUST be addressable under `--no-default-features --features sqlite`
    /// (ungated in PR-3 — the body is pure
    /// `self.backend.deliver_signal(...)` trait dispatch).
    #[test]
    fn deliver_signal_addressable_under_sqlite_only() {
        use crate::task::{Signal, SignalOutcome};
        use crate::SdkError;
        use ff_core::types::{ExecutionId, WaitpointId};
        use std::future::Future;
        use std::pin::Pin;

        // `deliver_signal` is `async fn`, so its item signature bakes
        // in a hidden lifetime and opaque return future. Take an
        // `fn`-pointer to an explicit wrapper that names the return
        // type through the trait — same shape as the PR-1 anchor
        // (`read_execution_context_addressable_under_sqlite_only`).
        #[allow(dead_code)]
        fn _pin<'a>(
            w: &'a FlowFabricWorker,
            id: &'a ExecutionId,
            wp: &'a WaitpointId,
            s: Signal,
        ) -> Pin<Box<dyn Future<Output = Result<SignalOutcome, SdkError>> + Send + 'a>> {
            Box::pin(w.deliver_signal(id, wp, s))
        }
    }

    /// v0.12 PR-5.5 compile anchor — `claim_from_grant` MUST be
    /// addressable under `--no-default-features --features sqlite`
    /// (ungated in PR-5.5). PG / SQLite return `EngineError::Unavailable`
    /// from the underlying trait method today, so the call path is
    /// compile-reachable but runtime-unavailable. The anchor pins the
    /// signature; a future PR wiring real PG/SQLite grant-consumer
    /// bodies flips the runtime behaviour without touching this test.
    #[test]
    fn claim_from_grant_addressable_under_sqlite_only() {
        use crate::task::ClaimedTask;
        use crate::SdkError;
        use ff_core::contracts::ClaimGrant;
        use ff_core::types::LaneId;
        use std::future::Future;
        use std::pin::Pin;

        #[allow(dead_code)]
        fn _pin<'a>(
            w: &'a FlowFabricWorker,
            lane: LaneId,
            grant: ClaimGrant,
        ) -> Pin<Box<dyn Future<Output = Result<ClaimedTask, SdkError>> + Send + 'a>> {
            Box::pin(w.claim_from_grant(lane, grant))
        }
    }

    /// v0.12 PR-5.5 compile anchor — `claim_via_server` MUST be
    /// addressable under `--no-default-features --features sqlite`.
    /// The scheduler-routed path is the supported PG/SQLite claim
    /// entry point per `project_claim_from_grant_pg_sqlite_gap.md`;
    /// pinning the signature here prevents a future PR from
    /// accidentally re-gating it behind `valkey-default`.
    #[test]
    fn claim_via_server_addressable_under_sqlite_only() {
        use crate::admin::FlowFabricAdminClient;
        use crate::task::ClaimedTask;
        use crate::SdkError;
        use ff_core::types::LaneId;
        use std::future::Future;
        use std::pin::Pin;

        #[allow(dead_code)]
        fn _pin<'a>(
            w: &'a FlowFabricWorker,
            admin: &'a FlowFabricAdminClient,
            lane: &'a LaneId,
            grant_ttl_ms: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Option<ClaimedTask>, SdkError>> + Send + 'a>>
        {
            Box::pin(w.claim_via_server(admin, lane, grant_ttl_ms))
        }
    }

    /// v0.12 PR-5.5 compile anchor — `ClaimedTask::{complete, fail,
    /// cancel}` MUST be addressable under `--no-default-features
    /// --features sqlite`. The `impl ClaimedTask` block is now
    /// module-level ungated (PR-5.5); the terminal ops route through
    /// `EngineBackend::{complete, fail, cancel}` which are core trait
    /// methods (no `streaming` / `suspension` / `budget` gate).
    #[test]
    fn claimed_task_terminal_ops_addressable_under_sqlite_only() {
        use crate::task::{ClaimedTask, FailOutcome};
        use crate::SdkError;
        use std::future::Future;
        use std::pin::Pin;

        #[allow(dead_code)]
        fn _pin_complete(
            t: ClaimedTask,
            payload: Option<Vec<u8>>,
        ) -> Pin<Box<dyn Future<Output = Result<(), SdkError>> + Send>> {
            Box::pin(t.complete(payload))
        }

        #[allow(dead_code)]
        fn _pin_fail<'a>(
            t: ClaimedTask,
            reason: &'a str,
            error_category: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<FailOutcome, SdkError>> + Send + 'a>> {
            Box::pin(t.fail(reason, error_category))
        }

        #[allow(dead_code)]
        fn _pin_cancel<'a>(
            t: ClaimedTask,
            reason: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), SdkError>> + Send + 'a>> {
            Box::pin(t.cancel(reason))
        }
    }
}

#[cfg(all(test, feature = "direct-valkey-claim"))]
mod scan_cursor_tests {
    use super::scan_cursor_seed;

    #[test]
    fn stable_for_same_input() {
        assert_eq!(scan_cursor_seed("w1", 256), scan_cursor_seed("w1", 256));
    }

    #[test]
    fn distinct_for_different_ids() {
        assert_ne!(scan_cursor_seed("w1", 256), scan_cursor_seed("w2", 256));
    }

    #[test]
    fn bounded_by_partition_count() {
        for i in 0..100 {
            assert!(scan_cursor_seed(&format!("w{i}"), 256) < 256);
        }
    }

    #[test]
    fn zero_partitions_returns_zero() {
        assert_eq!(scan_cursor_seed("w1", 0), 0);
    }
}
