use std::collections::HashMap;
#[cfg(feature = "insecure-direct-claim")]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "insecure-direct-claim")]
use std::sync::Arc;
use std::time::Duration;

use ferriskey::{Client, ClientBuilder, Value};
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::PartitionConfig;
use ff_core::types::*;
#[cfg(feature = "insecure-direct-claim")]
use tokio::sync::Semaphore;

use crate::config::WorkerConfig;
use crate::task::ClaimedTask;
use crate::SdkError;

/// FlowFabric worker — connects to Valkey, claims executions, and provides
/// the worker-facing API.
///
/// # Admission Control
///
/// **`claim_next()` bypasses budget and quota admission checks.** The SDK's
/// inline claim path reads the eligible ZSET directly and issues claim grants
/// without consulting budget (`{b:M}`) or quota (`{q:K}`) policies. In
/// production deployments, use the Scheduler (`ff-scheduler`) which enforces
/// admission control (budget breach check, quota sliding-window check,
/// concurrency cap) before issuing claim grants. The SDK's inline claim is
/// suitable for trusted workers, development, and testing.
///
/// # Usage
///
/// ```rust,ignore
/// use ff_sdk::{FlowFabricWorker, WorkerConfig};
///
/// let config = WorkerConfig::new("localhost", 6379, "w1", "w1-i1", "default", "main");
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
    client: Client,
    config: WorkerConfig,
    partition_config: PartitionConfig,
    /// Sorted, deduplicated, comma-separated capabilities — computed once
    /// from `config.capabilities` at connect time. Passed as ARGV[9] to
    /// `ff_issue_claim_grant` on every claim. BTreeSet sorting is critical:
    /// Lua's `ff_issue_claim_grant` relies on a stable CSV form for
    /// reproducible logs and tests.
    #[cfg(feature = "insecure-direct-claim")]
    worker_capabilities_csv: String,
    /// 8-hex FNV-1a digest of `worker_capabilities_csv`. Used in
    /// per-mismatch logs so the 4KB CSV never echoes on every reject
    /// during an incident. Full CSV logged once at connect-time WARN for
    /// cross-reference. Mirrors `ff-scheduler::claim::worker_caps_digest`.
    #[cfg(feature = "insecure-direct-claim")]
    worker_capabilities_hash: String,
    #[cfg(feature = "insecure-direct-claim")]
    lane_index: AtomicUsize,
    #[cfg(feature = "insecure-direct-claim")]
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
    #[cfg(feature = "insecure-direct-claim")]
    scan_cursor: AtomicUsize,
}

/// Number of partitions scanned per `claim_next()` poll. Keeps idle Valkey
/// load at O(PARTITION_SCAN_CHUNK) per worker-second instead of
/// O(num_execution_partitions).
#[cfg(feature = "insecure-direct-claim")]
const PARTITION_SCAN_CHUNK: usize = 32;

impl FlowFabricWorker {
    /// Connect to Valkey and prepare the worker.
    ///
    /// Establishes the ferriskey connection. Does NOT load the FlowFabric
    /// library — that is the server's responsibility (ff-server calls
    /// `ff_script::loader::ensure_library()` on startup). The SDK assumes
    /// the library is already loaded.
    pub async fn connect(config: WorkerConfig) -> Result<Self, SdkError> {
        if config.lanes.is_empty() {
            return Err(SdkError::Config("at least one lane is required".into()));
        }

        let mut builder = ClientBuilder::new()
            .host(&config.host, config.port)
            .connect_timeout(Duration::from_secs(10))
            .request_timeout(Duration::from_millis(5000));
        if config.tls {
            builder = builder.tls();
        }
        if config.cluster {
            builder = builder.cluster();
        }
        let client = builder.build()
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "failed to connect".into() })?;

        // Verify connectivity
        let pong: String = client
            .cmd("PING")
            .execute()
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "PING failed".into() })?;
        if pong != "PONG" {
            return Err(SdkError::Config(format!(
                "unexpected PING response: {pong}"
            )));
        }

        // Guard against two worker processes sharing the same
        // `worker_instance_id`. A duplicate instance would clobber each
        // other's lease_current/active_index entries and double-claim work.
        // SET NX on a liveness key with 2× lease TTL; if the key already
        // exists another process is live. The key auto-expires if this
        // process crashes without renewal, so a restart after a hard crash
        // just waits at most 2× lease_ttl_ms for the ghost entry to clear.
        //
        // Known limitations of this minimal scheme (documented for operators):
        //   1. **Startup-only, not runtime.** There is no heartbeat renewal
        //      path. After `2 × lease_ttl_ms` elapses the alive key expires
        //      naturally even while this worker is still running, and a
        //      second process with the same `worker_instance_id` launched
        //      later will successfully SET NX alongside the first. The check
        //      catches misconfiguration at boot; it does not fence duplicates
        //      that appear mid-lifetime. Production deployments should rely
        //      on the orchestrator (Kubernetes, systemd unit with
        //      `Restart=on-failure`, etc.) as the authoritative single-
        //      instance enforcer; this SET NX is belt-and-suspenders.
        //
        //   2. **Restart delay after a crash.** If a worker crashes
        //      ungracefully (SIGKILL, container OOM) and is restarted within
        //      `2 × lease_ttl_ms`, the alive key is still present and the
        //      new process exits with `SdkError::Config("duplicate
        //      worker_instance_id ...")`. Options for operators:
        //        - Wait `2 × lease_ttl_ms` (default 60s with the 30s TTL)
        //          before restarting.
        //        - Manually `DEL ff:worker:<instance_id>:alive` in Valkey to
        //          unblock the restart.
        //        - Use a fresh `worker_instance_id` for the restart (the
        //          orchestrator should already do this per-Pod).
        //
        //   3. **No graceful cleanup on shutdown.** There is no explicit
        //      `disconnect()` call that DELs the alive key. On clean
        //      `SIGTERM` the key lingers until its TTL expires. A follow-up
        //      can add `FlowFabricWorker::disconnect(self)` for callers that
        //      want to skip the restart-delay window.
        let alive_key = format!("ff:worker:{}:alive", config.worker_instance_id);
        let alive_ttl_ms = (config.lease_ttl_ms.saturating_mul(2)).max(1_000);
        let set_result: Option<String> = client
            .cmd("SET")
            .arg(&alive_key)
            .arg("1")
            .arg("NX")
            .arg("PX")
            .arg(alive_ttl_ms.to_string().as_str())
            .execute()
            .await
            .map_err(|e| SdkError::ValkeyContext {
                source: e,
                context: "SET NX worker alive key".into(),
            })?;
        if set_result.is_none() {
            return Err(SdkError::Config(format!(
                "duplicate worker_instance_id '{}': another process already holds {alive_key}",
                config.worker_instance_id
            )));
        }

        // Read partition config from Valkey (set by ff-server on startup).
        // Falls back to defaults if key doesn't exist (e.g. SDK-only testing).
        let partition_config = read_partition_config(&client).await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "ff:config:partitions not found, using defaults"
                );
                PartitionConfig::default()
            });

        #[cfg(feature = "insecure-direct-claim")]
        let max_tasks = config.max_concurrent_tasks.max(1);
        #[cfg(feature = "insecure-direct-claim")]
        let concurrency_semaphore = Arc::new(Semaphore::new(max_tasks));

        tracing::info!(
            worker_id = %config.worker_id,
            instance_id = %config.worker_instance_id,
            lanes = ?config.lanes.iter().map(|l| l.as_str()).collect::<Vec<_>>(),
            "FlowFabricWorker connected"
        );

        #[cfg(feature = "insecure-direct-claim")]
        let scan_cursor_init = scan_cursor_seed(
            config.worker_instance_id.as_str(),
            partition_config.num_execution_partitions.max(1) as usize,
        );

        // Sort + dedupe capabilities into a stable CSV. BTreeSet both sorts
        // and deduplicates in one pass; string joining happens once here.
        //
        // Ingress validation mirrors Scheduler::claim_for_worker (ff-scheduler):
        //   - `,` is the CSV delimiter; a token containing one would split
        //     mid-parse and could let a {"gpu"} worker appear to satisfy
        //     {"gpu,cuda"} (silent auth bypass).
        //   - Empty strings would produce leading/adjacent commas on the
        //     wire and inflate token count for no semantic reason.
        //   - Non-printable / whitespace chars: `"gpu "` vs `"gpu"` or
        //     `"gpu\n"` vs `"gpu"` produce silent mismatches that are
        //     miserable to debug. Reject anything outside printable ASCII
        //     excluding space (`'!'..='~'`) at ingress so a typo fails
        //     loudly at connect instead of silently mis-routing forever.
        // Reject at boot so operator misconfig is loud, symmetric with the
        // scheduler path.
        #[cfg(feature = "insecure-direct-claim")]
        for cap in &config.capabilities {
            if cap.is_empty() {
                return Err(SdkError::Config(
                    "capability token must not be empty".into(),
                ));
            }
            if cap.contains(',') {
                return Err(SdkError::Config(format!(
                    "capability token may not contain ',' (CSV delimiter): {cap:?}"
                )));
            }
            // Reject ASCII control bytes (0x00-0x1F, 0x7F) and any ASCII
            // whitespace (space, tab, LF, CR, FF, VT). UTF-8 printable
            // characters above 0x7F are ALLOWED so i18n caps like
            // "东京-gpu" can be used. The CSV wire form is byte-safe for
            // multibyte UTF-8 because `,` is always a single byte and
            // never part of a multibyte continuation (only 0x80-0xBF are
            // continuations, ',' is 0x2C).
            if cap.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(SdkError::Config(format!(
                    "capability token must not contain whitespace or control characters: {cap:?}"
                )));
            }
        }
        #[cfg(feature = "insecure-direct-claim")]
        let worker_capabilities_csv: String = {
            let set: std::collections::BTreeSet<&str> = config
                .capabilities
                .iter()
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .collect();
            if set.len() > ff_core::policy::CAPS_MAX_TOKENS {
                return Err(SdkError::Config(format!(
                    "capability set exceeds CAPS_MAX_TOKENS ({}): {}",
                    ff_core::policy::CAPS_MAX_TOKENS,
                    set.len()
                )));
            }
            let csv = set.into_iter().collect::<Vec<_>>().join(",");
            if csv.len() > ff_core::policy::CAPS_MAX_BYTES {
                return Err(SdkError::Config(format!(
                    "capability CSV exceeds CAPS_MAX_BYTES ({}): {}",
                    ff_core::policy::CAPS_MAX_BYTES,
                    csv.len()
                )));
            }
            csv
        };

        // Short stable digest of the sorted caps CSV, computed once so
        // per-mismatch logs carry a stable identifier instead of the 4KB
        // CSV. Shared helper — ff-scheduler uses the same one for its
        // own per-mismatch logs, so cross-component log lines are
        // diffable against each other.
        #[cfg(feature = "insecure-direct-claim")]
        let worker_capabilities_hash = ff_core::hash::fnv1a_xor8hex(&worker_capabilities_csv);

        // Full CSV logged once at connect so per-mismatch logs (which
        // carry only the 8-hex hash) can be cross-referenced by ops.
        #[cfg(feature = "insecure-direct-claim")]
        if !worker_capabilities_csv.is_empty() {
            tracing::info!(
                worker_instance_id = %config.worker_instance_id,
                worker_caps_hash = %worker_capabilities_hash,
                worker_caps = %worker_capabilities_csv,
                "worker connected with capabilities (full CSV — mismatch logs use hash only)"
            );
        }

        // Non-authoritative advertisement of caps for operator visibility
        // (CLI introspection, dashboards). The AUTHORITATIVE source for
        // scheduling decisions is ARGV[9] on each claim — Lua reads ONLY
        // that, never this string. Lossy here is correctness-safe.
        //
        // Storage: a single STRING key holding the sorted CSV. Rationale:
        //   * **Atomic overwrite.** `SET` is a single command — a concurrent
        //     reader can never observe a transient empty value (the prior
        //     DEL+SADD pair had that window).
        //   * **Crash cleanup without refresh loop.** The alive-key SET NX
        //     is startup-only (see §1 above); there's no periodic renew
        //     to piggy-back on, so a TTL on caps would independently
        //     expire mid-flight and hide a live worker's caps from ops
        //     tools. Instead we drop the TTL: each reconnect overwrites;
        //     a crashed worker leaves a stale CSV until a new process
        //     with the same worker_instance_id boots (which triggers
        //     `duplicate worker_instance_id` via alive-key guard anyway —
        //     the orchestrator allocates a new id, and operators can DEL
        //     the stale caps key if they care).
        //   * **Empty caps = DEL.** A restart from {gpu} to {} clears the
        //     advertisement rather than leaving stale data.
        // Cluster-safe advertisement: the per-worker caps STRING lives at
        // `ff:worker:{id}:caps` (lands on whatever slot CRC16 puts it on),
        // and the INSTANCE ID is SADD'd to the global workers-index SET
        // `ff:idx:workers` (single slot). The unblock scanner's cluster
        // enumeration uses SMEMBERS on the index + per-member GET on each
        // caps key, instead of `SCAN MATCH ff:worker:*:caps` (which in
        // cluster mode only scans the shard the SCAN lands on and misses
        // workers whose key hashes elsewhere). Pattern mirrors Batch A
        // `budget_policies_index` / `flow_index` / `deps_all_edges`:
        // operations stay atomic per command, the index is the
        // cluster-wide enumeration surface.
        #[cfg(feature = "insecure-direct-claim")]
        {
            let caps_key = ff_core::keys::worker_caps_key(&config.worker_instance_id);
            let index_key = ff_core::keys::workers_index_key();
            let instance_id = config.worker_instance_id.to_string();
            if worker_capabilities_csv.is_empty() {
                // No caps advertised. DEL the per-worker caps string AND
                // SREM from the index so the scanner doesn't GET an empty
                // string for a worker that never declares caps.
                let _ = client
                    .cmd("DEL")
                    .arg(&caps_key)
                    .execute::<Option<i64>>()
                    .await;
                if let Err(e) = client
                    .cmd("SREM")
                    .arg(&index_key)
                    .arg(&instance_id)
                    .execute::<Option<i64>>()
                    .await
                {
                    tracing::warn!(error = %e, key = %index_key, instance = %instance_id,
                        "SREM workers-index failed; continuing (non-authoritative)");
                }
            } else {
                // Atomic overwrite of the caps STRING (one-command). Then
                // SADD to the index (idempotent — re-running connect for
                // the same id is a no-op at SADD level). The per-worker
                // caps key is written BEFORE the index SADD so that when
                // the scanner observes the id in the index, the caps key
                // is guaranteed to resolve to a non-stale CSV (the reverse
                // order would leak an index entry pointing at a stale or
                // empty caps key during a narrow window).
                if let Err(e) = client
                    .cmd("SET")
                    .arg(&caps_key)
                    .arg(&worker_capabilities_csv)
                    .execute::<Option<String>>()
                    .await
                {
                    tracing::warn!(error = %e, key = %caps_key,
                        "SET worker caps advertisement failed; continuing");
                }
                if let Err(e) = client
                    .cmd("SADD")
                    .arg(&index_key)
                    .arg(&instance_id)
                    .execute::<Option<i64>>()
                    .await
                {
                    tracing::warn!(error = %e, key = %index_key, instance = %instance_id,
                        "SADD workers-index failed; continuing");
                }
            }
        }

        Ok(Self {
            client,
            config,
            partition_config,
            #[cfg(feature = "insecure-direct-claim")]
            worker_capabilities_csv,
            #[cfg(feature = "insecure-direct-claim")]
            worker_capabilities_hash,
            #[cfg(feature = "insecure-direct-claim")]
            lane_index: AtomicUsize::new(0),
            #[cfg(feature = "insecure-direct-claim")]
            concurrency_semaphore,
            #[cfg(feature = "insecure-direct-claim")]
            scan_cursor: AtomicUsize::new(scan_cursor_init),
        })
    }

    /// Get a reference to the underlying ferriskey client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get the worker config.
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    /// Get the server-published partition config this worker bound to at
    /// `connect()`. Callers need this when computing partition hash-tags
    /// for direct-client reads (e.g. ff-sdk::read_stream) to stay aligned
    /// with the server's `num_execution_partitions` — using
    /// `PartitionConfig::default()` assumes 256 partitions and silently
    /// misses data on deployments with any other value.
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
    /// **This bypasses budget/quota admission control.** Use the Scheduler
    /// (`ff-scheduler`) for production deployments. Enable with:
    /// `ff-sdk = { ..., features = ["insecure-direct-claim"] }`
    ///
    /// # `None` semantics
    ///
    /// `Ok(None)` means **no work was found in the partition window this
    /// poll covered**, not "the cluster is idle". Each call scans a chunk
    /// of [`PARTITION_SCAN_CHUNK`] partitions starting at the rolling
    /// `scan_cursor`; the cursor advances by that chunk size on every
    /// invocation, so a worker covers every partition exactly once every
    /// `ceil(num_execution_partitions / PARTITION_SCAN_CHUNK)` polls.
    ///
    /// Callers should treat `None` as "poll again soon" (typically after
    /// `config.claim_poll_interval_ms`) rather than "sleep for a long
    /// time". Backing off too aggressively on `None` can starve workers
    /// when work lives on partitions outside the current window.
    ///
    /// Returns `Err` on Valkey errors or script failures.
    #[cfg(feature = "insecure-direct-claim")]
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
        let num_partitions = self.partition_config.num_execution_partitions as usize;
        if num_partitions == 0 {
            return Ok(None);
        }
        let chunk = PARTITION_SCAN_CHUNK.min(num_partitions);
        let start = self.scan_cursor.fetch_add(chunk, Ordering::Relaxed) % num_partitions;

        for step in 0..chunk {
            let partition_idx = ((start + step) % num_partitions) as u16;
            let partition = ff_core::partition::Partition {
                family: ff_core::partition::PartitionFamily::Execution,
                index: partition_idx,
            };
            let idx = IndexKeys::new(&partition);
            let eligible_key = idx.lane_eligible(&lane_id);

            // ZRANGEBYSCORE to get the highest-priority eligible execution.
            // Score format: -(priority * 1_000_000_000_000) + created_at_ms
            // ZRANGEBYSCORE with "-inf" "+inf" LIMIT 0 1 gives lowest score = highest priority.
            let result: Value = self
                .client
                .cmd("ZRANGEBYSCORE")
                .arg(&eligible_key)
                .arg("-inf")
                .arg("+inf")
                .arg("LIMIT")
                .arg("0")
                .arg("1")
                .execute()
                .await
                .map_err(|e| SdkError::ValkeyContext { source: e, context: "ZRANGEBYSCORE failed".into() })?;

            let execution_id_str = match extract_first_array_string(&result) {
                Some(s) => s,
                None => continue, // No eligible executions on this partition
            };

            let execution_id = ExecutionId::parse(&execution_id_str).map_err(|e| {
                SdkError::Script(ff_script::error::ScriptError::Parse(format!(
                    "bad execution_id in eligible set: {e}"
                )))
            })?;

            // Step 1: Issue claim grant
            let grant_result = self
                .issue_claim_grant(&execution_id, &lane_id, &partition, &idx)
                .await;

            match grant_result {
                Ok(()) => {}
                Err(SdkError::Script(ff_script::error::ScriptError::CapabilityMismatch(
                    ref missing,
                ))) => {
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
                    self.block_route(&execution_id, &lane_id, &partition, &idx).await;
                    continue;
                }
                Err(SdkError::Script(ref e)) if is_retryable_claim_error(e) => {
                    tracing::debug!(
                        execution_id = %execution_id,
                        error = %e,
                        "claim grant failed (retryable), trying next partition"
                    );
                    continue;
                }
                Err(e) => return Err(e),
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
                Err(SdkError::Script(ff_script::error::ScriptError::UseClaimResumedExecution)) => {
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
                        Err(SdkError::Script(ref e2)) if is_retryable_claim_error(e2) => {
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
                Err(SdkError::Script(ref e)) if is_retryable_claim_error(e) => {
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

    #[cfg(feature = "insecure-direct-claim")]
    async fn issue_claim_grant(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
        idx: &IndexKeys,
    ) -> Result<(), SdkError> {
        let ctx = ExecKeyContext::new(partition, execution_id);

        // KEYS (3): exec_core, claim_grant_key, eligible_zset
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.claim_grant(),
            idx.lane_eligible(lane_id),
        ];

        // ARGV (9): eid, worker_id, worker_instance_id, lane_id,
        //           capability_hash, grant_ttl_ms, route_snapshot_json,
        //           admission_summary, worker_capabilities_csv (sorted)
        let args: Vec<String> = vec![
            execution_id.to_string(),
            self.config.worker_id.to_string(),
            self.config.worker_instance_id.to_string(),
            lane_id.to_string(),
            String::new(), // capability_hash
            "5000".to_owned(), // grant_ttl_ms (5 seconds)
            String::new(), // route_snapshot_json
            String::new(), // admission_summary
            self.worker_capabilities_csv.clone(), // sorted CSV
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_issue_claim_grant", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        crate::task::parse_success_result(&raw, "ff_issue_claim_grant")
    }

    /// Move an execution from the lane's eligible ZSET into its
    /// blocked_route ZSET via `ff_block_execution_for_admission`. Called
    /// after a `CapabilityMismatch` reject — without this, the inline
    /// direct-claim path would re-pick the same top-of-zset every tick
    /// (same pattern the scheduler's block_candidate handles). The
    /// engine's unblock scanner periodically promotes blocked_route
    /// back to eligible once a worker with matching caps registers.
    ///
    /// Best-effort: transport or logical rejects (e.g. the execution
    /// already went terminal between pick and block) are logged and the
    /// outer loop simply `continue`s to the next partition. Parity with
    /// ff-scheduler::Scheduler::block_candidate.
    #[cfg(feature = "insecure-direct-claim")]
    async fn block_route(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
        idx: &IndexKeys,
    ) {
        let ctx = ExecKeyContext::new(partition, execution_id);
        let core_key = ctx.core();
        let eligible_key = idx.lane_eligible(lane_id);
        let blocked_key = idx.lane_blocked_route(lane_id);
        let eid_s = execution_id.to_string();
        let now_ms = TimestampMs::now().0.to_string();

        let keys: [&str; 3] = [&core_key, &eligible_key, &blocked_key];
        let argv: [&str; 4] = [
            &eid_s,
            "waiting_for_capable_worker",
            "no connected worker satisfies required_capabilities",
            &now_ms,
        ];

        match self
            .client
            .fcall::<Value>("ff_block_execution_for_admission", &keys, &argv)
            .await
        {
            Ok(v) => {
                // Parse Lua result so a logical reject (e.g. execution
                // went terminal mid-flight) is visible — same fix we
                // applied to ff-scheduler's block_candidate.
                if let Err(e) = crate::task::parse_success_result(&v, "ff_block_execution_for_admission") {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %e,
                        "SDK block_route: Lua rejected; eligible ZSET unchanged, next poll \
                         will re-evaluate"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "SDK block_route: transport failure; eligible ZSET unchanged"
                );
            }
        }
    }

    #[cfg(feature = "insecure-direct-claim")]
    async fn claim_execution(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
        _now: TimestampMs,
    ) -> Result<ClaimedTask, SdkError> {
        let ctx = ExecKeyContext::new(partition, execution_id);
        let idx = IndexKeys::new(partition);

        // Pre-read total_attempt_count from exec_core to derive next attempt index.
        // The Lua uses total_attempt_count as the new index and dynamically builds
        // the attempt key from the hash tag, so KEYS[6-8] are placeholders, but
        // we pass the correct index for documentation/debugging.
        let total_str: Option<String> = self.client
            .cmd("HGET")
            .arg(ctx.core())
            .arg("total_attempt_count")
            .execute()
            .await
            .unwrap_or(None);
        let next_idx = total_str
            .as_deref()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let att_idx = AttemptIndex::new(next_idx);

        let lease_id = LeaseId::new().to_string();
        let attempt_id = AttemptId::new().to_string();
        let renew_before_ms = self.config.lease_ttl_ms * 2 / 3;

        // KEYS (14): must match lua/execution.lua ff_claim_execution positional order
        let keys: Vec<String> = vec![
            ctx.core(),                                    // 1  exec_core
            ctx.claim_grant(),                             // 2  claim_grant
            idx.lane_eligible(lane_id),                    // 3  eligible_zset
            idx.lease_expiry(),                            // 4  lease_expiry_zset
            idx.worker_leases(&self.config.worker_instance_id), // 5  worker_leases
            ctx.attempt_hash(att_idx),                     // 6  attempt_hash (placeholder)
            ctx.attempt_usage(att_idx),                    // 7  attempt_usage (placeholder)
            ctx.attempt_policy(att_idx),                   // 8  attempt_policy (placeholder)
            ctx.attempts(),                                // 9  attempts_zset
            ctx.lease_current(),                           // 10 lease_current
            ctx.lease_history(),                           // 11 lease_history
            idx.lane_active(lane_id),                      // 12 active_index
            idx.attempt_timeout(),                         // 13 attempt_timeout_zset
            idx.execution_deadline(),                      // 14 execution_deadline_zset
        ];

        // ARGV (12): must match lua/execution.lua ff_claim_execution positional order
        let args: Vec<String> = vec![
            execution_id.to_string(),                      // 1  execution_id
            self.config.worker_id.to_string(),             // 2  worker_id
            self.config.worker_instance_id.to_string(),    // 3  worker_instance_id
            lane_id.to_string(),                           // 4  lane
            String::new(),                                 // 5  capability_hash
            lease_id.clone(),                              // 6  lease_id
            self.config.lease_ttl_ms.to_string(),          // 7  lease_ttl_ms
            renew_before_ms.to_string(),                   // 8  renew_before_ms
            attempt_id.clone(),                            // 9  attempt_id
            "{}".to_owned(),                               // 10 attempt_policy_json
            String::new(),                                 // 11 attempt_timeout_ms
            String::new(),                                 // 12 execution_deadline_at
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_claim_execution", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        // Parse claim result: {1, "OK", lease_id, lease_epoch, attempt_index,
        //                      attempt_id, attempt_type, lease_expires_at}
        let arr = match &raw {
            Value::Array(arr) => arr,
            _ => {
                return Err(SdkError::Script(ff_script::error::ScriptError::Parse(
                    "ff_claim_execution: expected Array".into(),
                )));
            }
        };

        let status_code = match arr.first() {
            Some(Ok(Value::Int(n))) => *n,
            _ => {
                return Err(SdkError::Script(ff_script::error::ScriptError::Parse(
                    "ff_claim_execution: bad status code".into(),
                )));
            }
        };

        if status_code != 1 {
            let err_field_str = |idx: usize| -> String {
                arr.get(idx)
                    .and_then(|v| match v {
                        Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                        Ok(Value::SimpleString(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            };
            let error_code = {
                let s = err_field_str(1);
                if s.is_empty() { "unknown".to_owned() } else { s }
            };
            let detail = err_field_str(2);

            return Err(SdkError::Script(
                ff_script::error::ScriptError::from_code_with_detail(&error_code, &detail)
                    .unwrap_or_else(|| {
                        ff_script::error::ScriptError::Parse(format!(
                            "ff_claim_execution: {error_code}"
                        ))
                    }),
            ));
        }

        // Extract fields from success response
        let field_str = |idx: usize| -> String {
            arr.get(idx + 2) // skip status_code and "OK"
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    Ok(Value::Int(n)) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };

        // Lua returns: ok(lease_id, epoch, expires_at, attempt_id, attempt_index, attempt_type)
        // Positions:       0         1       2           3            4              5
        let lease_id = LeaseId::parse(&field_str(0))
            .unwrap_or_else(|_| LeaseId::new());
        let lease_epoch = LeaseEpoch::new(field_str(1).parse().unwrap_or(1));
        // field_str(2) is expires_at — skip it (lease timing managed by renewal)
        let attempt_id = AttemptId::parse(&field_str(3))
            .unwrap_or_else(|_| AttemptId::new());
        let attempt_index = AttemptIndex::new(field_str(4).parse().unwrap_or(0));

        // Read execution payload and metadata
        let (input_payload, execution_kind, tags) = self
            .read_execution_context(execution_id, partition)
            .await?;

        Ok(ClaimedTask::new(
            self.client.clone(),
            self.partition_config,
            execution_id.clone(),
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            self.config.lease_ttl_ms,
            lane_id.clone(),
            self.config.worker_instance_id.clone(),
            input_payload,
            execution_kind,
            tags,
        ))
    }

    /// Consume a [`ReclaimGrant`] and transition the granted
    /// `attempt_interrupted` execution into a `started` state on this
    /// worker. Symmetric partner to `claim_from_grant` for the resume
    /// path.
    ///
    /// The grant must have been issued to THIS worker (matching
    /// `worker_id` at grant time). A mismatch returns
    /// `Err(Script(InvalidClaimGrant))`. The grant is consumed
    /// atomically by `ff_claim_resumed_execution`; a second call with
    /// the same grant also returns `InvalidClaimGrant`.
    ///
    /// # Errors
    ///
    /// * `ScriptError::InvalidClaimGrant` — grant missing, consumed,
    ///   or `worker_id` mismatch.
    /// * `ScriptError::ClaimGrantExpired` — grant TTL elapsed.
    /// * `ScriptError::NotAResumedExecution` — `attempt_state` is not
    ///   `attempt_interrupted`.
    /// * `ScriptError::ExecutionNotLeaseable` — `lifecycle_phase` is
    ///   not `runnable`.
    /// * `ScriptError::ExecutionNotFound` — core key missing.
    /// * `SdkError::Valkey` / `SdkError::ValkeyContext` — transport.
    ///
    /// [`ReclaimGrant`]: ff_core::contracts::ReclaimGrant
    pub async fn claim_from_reclaim_grant(
        &self,
        grant: ff_core::contracts::ReclaimGrant,
    ) -> Result<ClaimedTask, SdkError> {
        // Grant carries partition + lane_id so no round-trip is needed
        // to resolve them before the FCALL.
        self.claim_resumed_execution(
            &grant.execution_id,
            &grant.lane_id,
            &grant.partition,
        )
        .await
    }

    /// Low-level resume claim. Invokes `ff_claim_resumed_execution`
    /// and returns a `ClaimedTask` bound to the resumed attempt.
    ///
    /// Previously gated behind `insecure-direct-claim`; ungated so the
    /// public [`claim_from_reclaim_grant`] entry point can reuse it.
    /// The method stays private — external callers use
    /// `claim_from_reclaim_grant`.
    ///
    /// [`claim_from_reclaim_grant`]: FlowFabricWorker::claim_from_reclaim_grant
    async fn claim_resumed_execution(
        &self,
        execution_id: &ExecutionId,
        lane_id: &LaneId,
        partition: &ff_core::partition::Partition,
    ) -> Result<ClaimedTask, SdkError> {
        let ctx = ExecKeyContext::new(partition, execution_id);
        let idx = IndexKeys::new(partition);

        // Pre-read current_attempt_index for the existing attempt hash key.
        // This is load-bearing: KEYS[6] must point to the real attempt hash.
        let att_idx_str: Option<String> = self.client
            .cmd("HGET")
            .arg(ctx.core())
            .arg("current_attempt_index")
            .execute()
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "read attempt_index".into() })?;
        let att_idx = AttemptIndex::new(
            att_idx_str.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
        );

        let lease_id = LeaseId::new().to_string();

        // KEYS (11): must match lua/signal.lua ff_claim_resumed_execution
        let keys: Vec<String> = vec![
            ctx.core(),                                             // 1  exec_core
            ctx.claim_grant(),                                      // 2  claim_grant
            idx.lane_eligible(lane_id),                             // 3  eligible_zset
            idx.lease_expiry(),                                     // 4  lease_expiry_zset
            idx.worker_leases(&self.config.worker_instance_id),     // 5  worker_leases
            ctx.attempt_hash(att_idx),                              // 6  existing_attempt_hash
            ctx.lease_current(),                                    // 7  lease_current
            ctx.lease_history(),                                    // 8  lease_history
            idx.lane_active(lane_id),                               // 9  active_index
            idx.attempt_timeout(),                                  // 10 attempt_timeout_zset
            idx.execution_deadline(),                               // 11 execution_deadline_zset
        ];

        // ARGV (8): must match lua/signal.lua ff_claim_resumed_execution
        let args: Vec<String> = vec![
            execution_id.to_string(),                               // 1  execution_id
            self.config.worker_id.to_string(),                      // 2  worker_id
            self.config.worker_instance_id.to_string(),             // 3  worker_instance_id
            lane_id.to_string(),                                    // 4  lane
            String::new(),                                          // 5  capability_hash
            lease_id.clone(),                                       // 6  lease_id
            self.config.lease_ttl_ms.to_string(),                   // 7  lease_ttl_ms
            String::new(),                                          // 8  remaining_attempt_timeout_ms
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_claim_resumed_execution", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        // Parse result — same format as ff_claim_execution:
        // {1, "OK", lease_id, lease_epoch, expires_at, attempt_id, attempt_index, attempt_type}
        let arr = match &raw {
            Value::Array(arr) => arr,
            _ => {
                return Err(SdkError::Script(ff_script::error::ScriptError::Parse(
                    "ff_claim_resumed_execution: expected Array".into(),
                )));
            }
        };

        let status_code = match arr.first() {
            Some(Ok(Value::Int(n))) => *n,
            _ => {
                return Err(SdkError::Script(ff_script::error::ScriptError::Parse(
                    "ff_claim_resumed_execution: bad status code".into(),
                )));
            }
        };

        if status_code != 1 {
            let err_field_str = |idx: usize| -> String {
                arr.get(idx)
                    .and_then(|v| match v {
                        Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                        Ok(Value::SimpleString(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            };
            let error_code = {
                let s = err_field_str(1);
                if s.is_empty() { "unknown".to_owned() } else { s }
            };
            let detail = err_field_str(2);

            return Err(SdkError::Script(
                ff_script::error::ScriptError::from_code_with_detail(&error_code, &detail)
                    .unwrap_or_else(|| {
                        ff_script::error::ScriptError::Parse(format!(
                            "ff_claim_resumed_execution: {error_code}"
                        ))
                    }),
            ));
        }

        let field_str = |idx: usize| -> String {
            arr.get(idx + 2)
                .and_then(|v| match v {
                    Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                    Ok(Value::SimpleString(s)) => Some(s.clone()),
                    Ok(Value::Int(n)) => Some(n.to_string()),
                    _ => None,
                })
                .unwrap_or_default()
        };

        let lease_id = LeaseId::parse(&field_str(0))
            .unwrap_or_else(|_| LeaseId::new());
        let lease_epoch = LeaseEpoch::new(field_str(1).parse().unwrap_or(1));
        let attempt_index = AttemptIndex::new(field_str(4).parse().unwrap_or(0));
        let attempt_id = AttemptId::parse(&field_str(3))
            .unwrap_or_else(|_| AttemptId::new());

        let (input_payload, execution_kind, tags) = self
            .read_execution_context(execution_id, partition)
            .await?;

        Ok(ClaimedTask::new(
            self.client.clone(),
            self.partition_config,
            execution_id.clone(),
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
            self.config.lease_ttl_ms,
            lane_id.clone(),
            self.config.worker_instance_id.clone(),
            input_payload,
            execution_kind,
            tags,
        ))
    }

    /// Read payload + execution_kind + tags from exec_core. Previously
    /// gated behind `insecure-direct-claim`; now shared by the
    /// feature-gated inline claim path and the public
    /// `claim_from_reclaim_grant` entry point.
    async fn read_execution_context(
        &self,
        execution_id: &ExecutionId,
        partition: &ff_core::partition::Partition,
    ) -> Result<(Vec<u8>, String, HashMap<String, String>), SdkError> {
        let ctx = ExecKeyContext::new(partition, execution_id);

        // Read payload
        let payload: Option<String> = self
            .client
            .get(&ctx.payload())
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "GET payload failed".into() })?;
        let input_payload = payload.unwrap_or_default().into_bytes();

        // Read execution_kind from core
        let kind: Option<String> = self
            .client
            .hget(&ctx.core(), "execution_kind")
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "HGET execution_kind failed".into() })?;
        let execution_kind = kind.unwrap_or_default();

        // Read tags
        let tags: HashMap<String, String> = self
            .client
            .hgetall(&ctx.tags())
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "HGETALL tags".into() })?;

        Ok((input_payload, execution_kind, tags))
    }

    // ── Phase 3: Signal delivery ──

    /// Deliver a signal to a suspended execution's waitpoint.
    ///
    /// The engine atomically records the signal, evaluates the resume condition,
    /// and optionally transitions the execution from `suspended` to `runnable`.
    pub async fn deliver_signal(
        &self,
        execution_id: &ExecutionId,
        waitpoint_id: &WaitpointId,
        signal: crate::task::Signal,
    ) -> Result<crate::task::SignalOutcome, SdkError> {
        let partition = ff_core::partition::execution_partition(execution_id, &self.partition_config);
        let ctx = ExecKeyContext::new(&partition, execution_id);
        let idx = IndexKeys::new(&partition);

        let signal_id = ff_core::types::SignalId::new();
        let now = TimestampMs::now();

        // Pre-read lane_id from exec_core — the execution may be on any lane,
        // not necessarily one of this worker's configured lanes.
        let lane_str: Option<String> = self
            .client
            .hget(&ctx.core(), "lane_id")
            .await
            .map_err(|e| SdkError::ValkeyContext { source: e, context: "HGET lane_id".into() })?;
        let lane_id = LaneId::new(lane_str.unwrap_or_else(|| "default".to_owned()));

        // KEYS (14): exec_core, wp_condition, wp_signals_stream,
        //            exec_signals_zset, signal_hash, signal_payload,
        //            idem_key, waitpoint_hash, suspension_current,
        //            eligible_zset, suspended_zset, delayed_zset,
        //            suspension_timeout_zset, hmac_secrets
        let idem_key = if let Some(ref ik) = signal.idempotency_key {
            ctx.signal_dedup(waitpoint_id, ik)
        } else {
            ctx.noop() // must share {p:N} hash tag for cluster mode
        };
        let keys: Vec<String> = vec![
            ctx.core(),                                    // 1
            ctx.waitpoint_condition(waitpoint_id),         // 2
            ctx.waitpoint_signals(waitpoint_id),           // 3
            ctx.exec_signals(),                            // 4
            ctx.signal(&signal_id),                        // 5
            ctx.signal_payload(&signal_id),                // 6
            idem_key,                                      // 7
            ctx.waitpoint(waitpoint_id),                   // 8
            ctx.suspension_current(),                      // 9
            idx.lane_eligible(&lane_id),                   // 10
            idx.lane_suspended(&lane_id),                  // 11
            idx.lane_delayed(&lane_id),                    // 12
            idx.suspension_timeout(),                      // 13
            idx.waitpoint_hmac_secrets(),                  // 14
        ];

        let payload_str = signal
            .payload
            .as_ref()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_default();

        // ARGV (18): signal_id, execution_id, waitpoint_id, signal_name,
        //            signal_category, source_type, source_identity,
        //            payload, payload_encoding, idempotency_key,
        //            correlation_id, target_scope, created_at,
        //            dedup_ttl_ms, resume_delay_ms, signal_maxlen,
        //            max_signals_per_execution, waitpoint_token
        let args: Vec<String> = vec![
            signal_id.to_string(),                           // 1
            execution_id.to_string(),                        // 2
            waitpoint_id.to_string(),                        // 3
            signal.signal_name,                              // 4
            signal.signal_category,                          // 5
            signal.source_type,                              // 6
            signal.source_identity,                          // 7
            payload_str,                                     // 8
            "json".to_owned(),                               // 9 payload_encoding
            signal.idempotency_key.unwrap_or_default(),      // 10
            String::new(),                                   // 11 correlation_id
            "waitpoint".to_owned(),                          // 12 target_scope
            now.to_string(),                                 // 13 created_at
            "86400000".to_owned(),                           // 14 dedup_ttl_ms
            "0".to_owned(),                                  // 15 resume_delay_ms
            "1000".to_owned(),                               // 16 signal_maxlen
            "10000".to_owned(),                              // 17 max_signals
            // WIRE BOUNDARY — raw token must reach Lua unredacted. Do NOT
            // use ToString/Display (those are redacted for log safety);
            // .as_str() is the explicit opt-in that gets the secret bytes.
            signal.waitpoint_token.as_str().to_owned(),      // 18 waitpoint_token
        ];

        let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let raw: Value = self
            .client
            .fcall("ff_deliver_signal", &key_refs, &arg_refs)
            .await
            .map_err(SdkError::Valkey)?;

        crate::task::parse_signal_result(&raw)
    }

    #[cfg(feature = "insecure-direct-claim")]
    fn next_lane(&self) -> LaneId {
        let idx = self.lane_index.fetch_add(1, Ordering::Relaxed) % self.config.lanes.len();
        self.config.lanes[idx].clone()
    }
}

#[cfg(feature = "insecure-direct-claim")]
fn is_retryable_claim_error(err: &ff_script::error::ScriptError) -> bool {
    use ff_core::error::ErrorClass;
    matches!(
        err.class(),
        ErrorClass::Retryable | ErrorClass::Informational
    )
}

/// Initial offset for [`FlowFabricWorker::scan_cursor`]. Hashes the worker
/// instance id with FNV-1a to place distinct worker processes on different
/// partition windows from their first poll. Zero is valid for single-worker
/// clusters but spreads work in multi-worker deployments.
#[cfg(feature = "insecure-direct-claim")]
fn scan_cursor_seed(worker_instance_id: &str, num_partitions: usize) -> usize {
    if num_partitions == 0 {
        return 0;
    }
    (ff_core::hash::fnv1a_u64(worker_instance_id.as_bytes()) as usize) % num_partitions
}

#[cfg(feature = "insecure-direct-claim")]
fn extract_first_array_string(value: &Value) -> Option<String> {
    match value {
        Value::Array(arr) if !arr.is_empty() => match &arr[0] {
            Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
            Ok(Value::SimpleString(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Read partition config from Valkey's `ff:config:partitions` hash.
/// Returns Err if the key doesn't exist or can't be read.
async fn read_partition_config(client: &Client) -> Result<PartitionConfig, SdkError> {
    let key = ff_core::keys::global_config_partitions();
    let fields: HashMap<String, String> = client
        .hgetall(&key)
        .await
        .map_err(|e| SdkError::ValkeyContext { source: e, context: format!("HGETALL {key}") })?;

    if fields.is_empty() {
        return Err(SdkError::Config(
            "ff:config:partitions not found in Valkey".into(),
        ));
    }

    let parse = |field: &str, default: u16| -> u16 {
        fields
            .get(field)
            .and_then(|v| v.parse().ok())
            .filter(|&n: &u16| n > 0)
            .unwrap_or(default)
    };

    Ok(PartitionConfig {
        num_execution_partitions: parse("num_execution_partitions", 256),
        num_flow_partitions: parse("num_flow_partitions", 64),
        num_budget_partitions: parse("num_budget_partitions", 32),
        num_quota_partitions: parse("num_quota_partitions", 32),
    })
}

#[cfg(all(test, feature = "insecure-direct-claim"))]
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
