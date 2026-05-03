//! Claim-grant cycle: find eligible executions and issue grants.
//!
//! The scheduler selects candidates from partition-local eligible sorted sets,
//! then atomically issues a claim grant via `FCALL ff_issue_claim_grant`.
//! The worker (ff-sdk) subsequently consumes the grant via `ff_claim_execution`.
//!
//! Phase 5: single lane, budget/quota pre-checks before grant issuance.
//! Candidates that fail budget/quota are blocked via ff_block_execution_for_admission.
//!
//! Reference: RFC-009 §12.7, RFC-010 §3.1, RFC-008 §1.7
//!
//! # Postgres variant (RFC-v0.7 Wave 5b)
//!
//! The admission pipeline is ported to Postgres as
//! [`ff_backend_postgres::scheduler::PostgresScheduler`]. It lives in
//! the backend crate — not as a `Scheduler` variant here — to keep
//! `ff-scheduler` off the sqlx dep graph. `ff-engine` / `ff-server`
//! branch at construction time on the concrete backend type.
//! See `crates/ff-backend-postgres/src/scheduler.rs` for the
//! pipeline + isolation notes.

use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{Partition, PartitionConfig, PartitionFamily, budget_partition, quota_partition};
use ff_core::types::{BudgetId, ExecutionId, LaneId, QuotaPolicyId, WorkerId, WorkerInstanceId};
use ff_script::retry::is_retryable_kind;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Short stable digest of a worker's caps CSV for per-event log lines.
/// Thin wrapper around the shared helper so call sites read as
/// "worker_caps_digest" locally while the algorithm lives in one place.
fn worker_caps_digest(csv: &str) -> String {
    ff_core::hash::fnv1a_xor8hex(csv)
}

/// A claim grant issued by the scheduler for a specific execution.
///
/// Re-exported from [`ff_core::contracts::ClaimGrant`]. Lives in
/// `ff-core` so `ff-scheduler` (issuer) and `ff-sdk` (consumer)
/// share one wire-level type without a cross-dep between them.
pub use ff_core::contracts::ClaimGrant;

/// A resume grant for a resumed (attempt_interrupted) execution.
///
/// Re-export of [`ff_core::contracts::ResumeGrant`] for symmetry
/// with [`ClaimGrant`]. `ff-scheduler` will be the canonical
/// producer once the Batch-C reclaim scanner lands; today only
/// test fixtures construct this type. Consumed by
/// `FlowFabricWorker::claim_from_resume_grant`.
pub use ff_core::contracts::ResumeGrant;

/// Budget check result from a cross-partition budget read.
#[derive(Debug)]
pub enum BudgetCheckResult {
    /// Budget is within limits — proceed.
    Ok,
    /// Budget hard limit breached. Contains (dimension, detail_string).
    HardBreach { dimension: String, detail: String },
}

/// Outcome of a quota admission check.
enum QuotaCheckOutcome {
    /// No quota attached to this execution.
    NoQuota,
    /// Quota admitted — carries context for release on subsequent failure.
    Admitted { tag: String, quota_id: String, eid: String },
    /// Quota denied — execution should be blocked.
    Blocked(String),
}

/// Cross-partition budget checker with per-cycle caching.
///
/// Reads budget usage/limits once per scan cycle (not per candidate).
/// This is MANDATORY for performance — without it, 50K blocked executions
/// would produce 50K budget reads per cycle.
pub struct BudgetChecker {
    /// Cached budget status: budget_id → BudgetCheckResult.
    /// Reset at the start of each scheduler cycle.
    cache: std::collections::HashMap<String, BudgetCheckResult>,
    config: PartitionConfig,
}

impl BudgetChecker {
    pub fn new(config: PartitionConfig) -> Self {
        Self {
            cache: std::collections::HashMap::new(),
            config,
        }
    }

    /// Check a budget by ID. Reads from Valkey on first call per budget,
    /// caches for subsequent candidates in the same cycle.
    ///
    /// Fail-closed: transport errors propagate as
    /// [`SchedulerError::ValkeyContext`] rather than being cached as
    /// `Ok`. Caching an Err would be wrong — a transient blip would then
    /// pin every candidate in this cycle to the same denial.  Successful
    /// results (including `HardBreach`) are still cached so 50K blocked
    /// candidates sharing a budget do one read per cycle, not 50K.
    pub async fn check_budget(
        &mut self,
        client: &ferriskey::Client,
        budget_id: &str,
    ) -> Result<&BudgetCheckResult, SchedulerError> {
        if self.cache.contains_key(budget_id) {
            return Ok(&self.cache[budget_id]);
        }

        // Compute real {b:M} partition tag from budget_id
        let (usage_key, limits_key) = match BudgetId::parse(budget_id) {
            Ok(bid) => {
                let partition = budget_partition(&bid, &self.config);
                let tag = partition.hash_tag();
                (
                    format!("ff:budget:{}:{}:usage", tag, budget_id),
                    format!("ff:budget:{}:{}:limits", tag, budget_id),
                )
            }
            Err(_) => {
                // Fallback for non-UUID budget IDs (test compat)
                (
                    format!("ff:budget:{{b:0}}:{}:usage", budget_id),
                    format!("ff:budget:{{b:0}}:{}:limits", budget_id),
                )
            }
        };

        let result =
            Self::read_and_check(client, &usage_key, &limits_key)
                .await
                .map_err(|source| SchedulerError::ValkeyContext {
                    source,
                    context: format!("budget_checker read {budget_id}"),
                })?;

        self.cache.insert(budget_id.to_owned(), result);
        Ok(&self.cache[budget_id])
    }

    /// Read budget usage and limits, compare each dimension.
    async fn read_and_check(
        client: &ferriskey::Client,
        usage_key: &str,
        limits_key: &str,
    ) -> Result<BudgetCheckResult, ferriskey::Error> {
        // Read all limit dimensions via hgetall (returns HashMap, not flat pairs)
        let limits: std::collections::HashMap<String, String> = client
            .hgetall(limits_key)
            .await?;

        // Parse hard limits
        for (field, limit_val) in &limits {
            if !field.starts_with("hard:") {
                continue;
            }
            let dimension = &field[5..]; // strip "hard:" prefix
            let limit: u64 = match limit_val.parse() {
                Ok(v) if v > 0 => v,
                _ => continue,
            };

            // Read current usage for this dimension. Fail-closed: a
            // transport error must NOT silently default the usage to 0
            // (which would make every breach invisible).  Absence is
            // still treated as 0 — the caller is expected to HSET
            // usage_key on every increment.
            let usage_str: Option<String> = client
                .cmd("HGET")
                .arg(usage_key)
                .arg(dimension)
                .execute()
                .await?;
            let usage: u64 = usage_str
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            if usage >= limit {
                return Ok(BudgetCheckResult::HardBreach {
                    dimension: dimension.to_owned(),
                    detail: format!("budget {}: {} {}/{}", usage_key, dimension, usage, limit),
                });
            }
        }

        Ok(BudgetCheckResult::Ok)
    }

    /// Clear the cache at the start of a new scheduler cycle.
    pub fn reset(&mut self) {
        self.cache.clear();
    }
}

/// Tunable scheduler behavior, separate from the topology-level
/// [`PartitionConfig`]. Lives in `ff-scheduler` because every field is a
/// scheduler-internal scan policy — none of it leaks into persisted keys,
/// Lua scripts, or cross-crate wire shapes (unlike `PartitionConfig`).
/// If a future RFC needs a unified knob surface we can lift this up; not
/// premature today.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    /// Maximum number of partitions to probe in a single
    /// [`Scheduler::claim_for_worker`] call before giving up and returning
    /// `Ok(None)`.
    ///
    /// **Trade-off:** smaller = lower worst-case no-hit latency per claim
    /// call (each probe is a ZRANGEBYSCORE round-trip, ~0.1ms LAN), larger
    /// = better fairness per call (more partitions seen before a worker
    /// yields). At the default of 32 with 256 partitions, any given
    /// partition is reached within `ceil(256/32) = 8` scheduling ticks —
    /// combined with the rotation cursor, that bounds worst-case
    /// starvation for a specific partition's head-of-queue execution.
    pub max_partitions_per_scan: u16,
    /// Duration a rotation cursor position stays stable before advancing.
    ///
    /// **Trade-off:** too short and tight-loop workers re-enter the same
    /// window on every tick (cursor never actually rotates relative to
    /// them); too long and slow-poll workers keep seeing the same cursor
    /// across many ticks (reducing fairness benefit). 250ms is a middle
    /// ground: tight-loop workers (sub-ms claim cycles) see a fresh
    /// window every ~250 ticks, 1s-poll workers see a fresh window every
    /// 4 ticks. Tune down if your workers all idle-poll >1s; tune up if
    /// you run a fleet of tight-loop claimers and want less cursor churn.
    pub rotation_window_ms: u64,
}

impl SchedulerConfig {
    /// Default scan budget: probe 32 partitions per claim call.
    /// See [`Self::max_partitions_per_scan`] for the latency/fairness
    /// rationale.
    pub const DEFAULT_MAX_PARTITIONS_PER_SCAN: u16 = 32;

    /// Default rotation window: advance the cursor every 250ms.
    /// See [`Self::rotation_window_ms`].
    pub const DEFAULT_ROTATION_WINDOW_MS: u64 = 250;
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_partitions_per_scan: Self::DEFAULT_MAX_PARTITIONS_PER_SCAN,
            rotation_window_ms: Self::DEFAULT_ROTATION_WINDOW_MS,
        }
    }
}

/// Iterate partitions `[start, start+1, ..., start+count-1] mod total`.
///
/// Factored out of [`Scheduler::claim_for_worker`] so the modular-wrap +
/// bounded-length contract has a dedicated unit test surface, independent
/// of Valkey. Called with `count <= total`; values are always distinct.
///
/// Returns an iterator (not a Vec) to keep the hot claim path allocation-
/// free — this runs once per claim tick per worker.
fn iter_partitions(total: u16, start: u16, count: u16) -> impl Iterator<Item = u16> {
    debug_assert!(total > 0);
    let count = count.min(total);
    (0..count).map(move |i| start.wrapping_add(i) % total)
}

/// Single-lane scheduler with budget/quota pre-checks.
///
/// Iterates execution partitions sequentially, picks the first eligible
/// execution (lowest priority score). Before issuing a claim grant:
/// 1. Check all attached budgets (cross-partition, cached per cycle)
/// 2. Check quota admission (cross-partition FCALL)
/// 3. If any check fails: block the candidate and try next
/// 4. If all pass: issue the claim grant
///
/// Scan bounding + rotation: each call probes at most
/// [`SchedulerConfig::max_partitions_per_scan`] partitions starting from
/// a rotation cursor that advances once per
/// [`SchedulerConfig::rotation_window_ms`]. The per-worker FNV jitter is
/// applied on top so different workers diverge within any given window.
pub struct Scheduler {
    client: ferriskey::Client,
    /// FF #511 Phase 2c: backend-agnostic routing for the 6 scheduler
    /// primitives that have trait coverage (`server_time_ms`,
    /// `read_quota_policy_limits`, `check_admission`,
    /// `block_execution_for_admission`, `release_admission`,
    /// `issue_claim_grant`). Budget HGETs stay on `client` for now —
    /// no trait primitive exists yet; Phase 2d or a future refactor
    /// can lift them.
    ///
    /// Held as `Weak` because Valkey/Postgres backends embed the
    /// Scheduler inside the backend `Arc`; a strong reference here
    /// would make an Arc-cycle and leak the backend. Every call-site
    /// upgrades to `Arc` for the duration of the await; upgrade
    /// failure (backend dropped mid-scheduler-tick) surfaces as a
    /// `SchedulerError::Config`.
    backend: std::sync::Weak<dyn ff_core::engine_backend::EngineBackend>,
    config: PartitionConfig,
    scheduler_config: SchedulerConfig,
    /// Packed rotation state: high 48 bits = last window epoch seen
    /// (`now_ms / rotation_window_ms`), low 16 bits = current cursor
    /// partition index. A single atomic keeps "advance cursor iff we're
    /// the first call in a new window" race-free without a Mutex.
    rotation_state: AtomicU64,
    /// PR-94: observability handle for claim_from_grant duration and
    /// budget/quota hit counters. Defaults to a fresh
    /// `ff_observability::Metrics::new()` — under the default build
    /// (`observability` feature off) that's the no-op shim; with the
    /// feature on it's a private real OTEL registry not shared with
    /// any scrape. Callers that want a shared registry plumb it in
    /// via [`Self::with_metrics`] / [`Self::with_config_and_metrics`].
    metrics: std::sync::Arc<ff_observability::Metrics>,
}

impl Scheduler {
    /// Upgrade the `Weak` backend ref to `Arc` for a single call.
    /// Returns `Err(SchedulerError::Config)` if the backend was
    /// dropped — mirrors a transport fault for caller-retry purposes.
    fn upgrade_backend(
        &self,
    ) -> Result<std::sync::Arc<dyn ff_core::engine_backend::EngineBackend>, SchedulerError> {
        self.backend.upgrade().ok_or_else(|| {
            SchedulerError::Config(
                "scheduler: backend Arc dropped (Weak upgrade failed); \
                 caller must rebuild the scheduler"
                    .to_owned(),
            )
        })
    }

    /// FF #511 Phase 2c: all constructors now require a trait-object
    /// backend alongside the client. The backend routes admission
    /// primitives; the client remains for Valkey-specific calls that
    /// have no trait coverage yet (budget HGETs). Callers with only a
    /// `ferriskey::Client` can wrap it in a `ValkeyBackend` via
    /// `ValkeyBackend::from_client_and_partitions` + `Arc::downgrade`.
    pub fn new(
        client: ferriskey::Client,
        backend: std::sync::Weak<dyn ff_core::engine_backend::EngineBackend>,
        config: PartitionConfig,
    ) -> Self {
        Self::with_config(client, backend, config, SchedulerConfig::default())
    }

    /// Construct a scheduler with an explicit [`SchedulerConfig`].
    pub fn with_config(
        client: ferriskey::Client,
        backend: std::sync::Weak<dyn ff_core::engine_backend::EngineBackend>,
        config: PartitionConfig,
        scheduler_config: SchedulerConfig,
    ) -> Self {
        Self::with_config_and_metrics(
            client,
            backend,
            config,
            scheduler_config,
            std::sync::Arc::new(ff_observability::Metrics::new()),
        )
    }

    /// PR-94: construct a scheduler with a shared observability registry.
    pub fn with_metrics(
        client: ferriskey::Client,
        backend: std::sync::Weak<dyn ff_core::engine_backend::EngineBackend>,
        config: PartitionConfig,
        metrics: std::sync::Arc<ff_observability::Metrics>,
    ) -> Self {
        Self::with_config_and_metrics(client, backend, config, SchedulerConfig::default(), metrics)
    }

    /// Construct a scheduler with an explicit
    /// [`SchedulerConfig`] AND a shared observability registry.
    pub fn with_config_and_metrics(
        client: ferriskey::Client,
        backend: std::sync::Weak<dyn ff_core::engine_backend::EngineBackend>,
        config: PartitionConfig,
        scheduler_config: SchedulerConfig,
        metrics: std::sync::Arc<ff_observability::Metrics>,
    ) -> Self {
        Self {
            client,
            backend,
            config,
            scheduler_config,
            rotation_state: AtomicU64::new(0),
            metrics,
        }
    }

    /// Return the current cursor for this call, advancing it if we're the
    /// first caller to observe a new rotation window. Pure compare-exchange
    /// on the packed atomic; no Mutex, no clock dep beyond `now_ms`.
    fn rotation_cursor(&self, now_ms: u64, num_partitions: u16) -> u16 {
        let window_ms = self.scheduler_config.rotation_window_ms.max(1);
        let step = self.scheduler_config.max_partitions_per_scan.max(1);
        let this_epoch = now_ms / window_ms;

        loop {
            let prev = self.rotation_state.load(Ordering::Relaxed);
            let prev_epoch = prev >> 16;
            let prev_cursor = (prev & 0xFFFF) as u16;

            if prev_epoch == this_epoch {
                return prev_cursor;
            }
            // New window: advance cursor by max_partitions_per_scan so
            // the next scan covers a fresh slice. Modular wrap on
            // num_partitions. Zero-partitions is a config bug; caller
            // guards it, but fall back to 0 to be safe.
            let new_cursor = if num_partitions == 0 {
                0
            } else {
                prev_cursor.wrapping_add(step) % num_partitions
            };
            let new_state = (this_epoch << 16) | u64::from(new_cursor);
            match self.rotation_state.compare_exchange(
                prev,
                new_state,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return new_cursor,
                // Lost race; another caller advanced. Re-read and use
                // whatever cursor now belongs to this epoch.
                Err(_) => continue,
            }
        }
    }

    /// Find an eligible execution and issue a claim grant.
    ///
    /// Iterates all execution partitions looking for the first partition
    /// with an eligible execution. Issues a claim grant via FCALL.
    ///
    /// `worker_capabilities` is sent to `ff_issue_claim_grant` as a sorted
    /// CSV (BTreeSet guarantees deterministic order). Executions whose
    /// `required_capabilities` are not a subset of this set are skipped
    /// (stay queued) via `ScriptError::CapabilityMismatch`.
    ///
    /// Returns `Ok(None)` if no eligible executions exist anywhere.
    /// Returns `Ok(Some(grant))` on success.
    /// Returns `Err` on Valkey errors.
    ///
    /// # Lane-FIFO: prior-run leftovers drain first
    ///
    /// Claim grants are issued in per-lane FIFO order over the lane's
    /// eligible queue. A smoke / dev script that reuses a lane across
    /// runs will observe the lane's leftover executions from earlier
    /// runs — failed, orphaned, or still-eligible — drain *before*
    /// the fresh submission is claimed. This surfaces as a confusing
    /// "wrong execution picked up" during iterative development.
    /// Smoke tests should either (a) use a fresh lane name per run
    /// (e.g. suffixing with a UUID or timestamp), or (b) call
    /// `cancel_flow` / `FLUSHDB` between runs to drain the lane.
    pub async fn claim_for_worker(
        &self,
        lane: &LaneId,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        worker_capabilities: &BTreeSet<String>,
        grant_ttl_ms: u64,
    ) -> Result<Option<ClaimGrant>, SchedulerError> {
        // PR-94: grant issuance latency histogram. Captures the full
        // cycle (candidate selection + budget/quota admission +
        // issuance FCALL) so an operator can spot slow paths without
        // having to correlate scheduler logs across partitions. Drop
        // guard records on *every* exit path (including ?-propagated
        // errors) so the histogram reflects wall-clock cost, not just
        // the happy path.
        struct ClaimTimer<'a> {
            start: std::time::Instant,
            lane: &'a str,
            metrics: &'a ff_observability::Metrics,
        }
        impl Drop for ClaimTimer<'_> {
            fn drop(&mut self) {
                self.metrics
                    .record_claim_from_grant(self.lane, self.start.elapsed());
            }
        }
        let _claim_timer = ClaimTimer {
            start: std::time::Instant::now(),
            lane: lane.as_str(),
            metrics: &self.metrics,
        };
        let num_partitions = self.config.num_flow_partitions;
        // Guard misconfiguration: a zero partition count would hit a
        // `% num_partitions` division-by-zero in the scan_start / jitter
        // computation below. The pre-bounded loop simply skipped on
        // `for offset in 0..0`; preserve that graceful no-op rather than
        // panicking. Config-returning (not Ok(None)) so an operator who
        // misconfigures gets a loud, actionable error — silent Ok(None)
        // would make every claim call look like an empty queue forever.
        if num_partitions == 0 {
            return Err(SchedulerError::Config(
                "num_flow_partitions must be > 0".to_owned(),
            ));
        }
        let mut budget_checker = BudgetChecker::new(self.config);

        // Jitter the partition scan start to avoid thundering-herd on
        // partition 0 when 100 workers all tick simultaneously. Seeded
        // from worker_instance_id so this worker hits a stable window
        // within a single scheduling cycle (still covers every partition),
        // and different workers naturally diverge. Uses the shared
        // ff_core::hash FNV-1a reducer — same helper powering ff-sdk's
        // PARTITION_SCAN_CHUNK cursor seed. Zero-modulus safe.
        let start_p: u16 = ff_core::hash::fnv1a_u16_mod(
            worker_instance_id.as_str(),
            num_partitions,
        );
        // BTreeSet iterates sorted → stable CSV for Lua subset match.
        // Ingress validation mirrors FlowFabricWorker::connect (ff-sdk):
        //   - `,` is the CSV delimiter — a token containing one would split
        //     mid-parse and could let a {"gpu"} worker appear to satisfy
        //     {"gpu,cuda"} (silent auth bypass).
        //   - Empty strings would inflate the CSV with leading / adjacent
        //     commas ("" → ",gpu" → [" "," gpu"]) and inflate the token
        //     count past CAPS_MAX_TOKENS for no semantic reason.
        //   - Non-printable / whitespace: "gpu " vs "gpu" or "gpu\n" vs
        //     "gpu" silently mis-routes. Require printable ASCII excluding
        //     space (0x21-0x7E) at ingress so typos fail loud.
        // Enforce ALL here so operator misconfig at the scheduler entry
        // point fails loud, symmetric with the SDK inline-claim path.
        // Bounds (#csv, #tokens) are enforced by the Lua side.
        for cap in worker_capabilities {
            if cap.is_empty() {
                return Err(SchedulerError::Config(
                    "capability token must not be empty".to_owned(),
                ));
            }
            if cap.contains(',') {
                return Err(SchedulerError::Config(format!(
                    "capability token may not contain ',' (CSV delimiter): {cap:?}"
                )));
            }
            // Reject ASCII control + whitespace (incl. Unicode whitespace);
            // allow non-ASCII printable UTF-8 so i18n cap names work. CSV
            // delimiter `,` is single-byte and never a UTF-8 continuation,
            // so multibyte UTF-8 is safe on the wire. Symmetric with
            // ff-sdk::FlowFabricWorker::connect.
            if cap.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(SchedulerError::Config(format!(
                    "capability token must not contain whitespace or control characters: {cap:?}"
                )));
            }
        }
        if worker_capabilities.len() > ff_core::policy::CAPS_MAX_TOKENS {
            return Err(SchedulerError::Config(format!(
                "capability set exceeds CAPS_MAX_TOKENS ({}): {}",
                ff_core::policy::CAPS_MAX_TOKENS,
                worker_capabilities.len()
            )));
        }
        let worker_caps_csv = worker_capabilities
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        // Stable digest used in per-mismatch logs so the full 4KB CSV
        // doesn't get echoed on every mismatch. See worker_caps_digest.
        let worker_caps_hash = worker_caps_digest(&worker_caps_csv);
        if worker_caps_csv.len() > ff_core::policy::CAPS_MAX_BYTES {
            return Err(SchedulerError::Config(format!(
                "capability CSV exceeds CAPS_MAX_BYTES ({}): {}",
                ff_core::policy::CAPS_MAX_BYTES,
                worker_caps_csv.len()
            )));
        }

        // ── Bounded scan with rotation cursor ──
        // Prior impl walked all `num_partitions` partitions on every call,
        // which at `num_flow_partitions = 256` meant a quiet cluster cost
        // 256 ZRANGEBYSCORE round-trips per claim tick per worker. The
        // bounded scan caps per-call work at `max_partitions_per_scan`
        // while the rotation cursor ensures every partition is still
        // visited within `ceil(total / max_partitions_per_scan)` ticks.
        //
        // Needs `now_ms` to compute the rotation window, so we snap server
        // time up front. One extra TIME round-trip for quiet-cluster no-hit
        // calls, but on hit paths the existing inner-loop `server_time_ms`
        // call is now redundant — we reuse this value.
        // FF #511 Phase 2c: route through the backend trait instead of
        // the raw ferriskey TIME. Transport-level failures surface as
        // `EngineError`; we map to `EngineContext` so callers backoff +
        // retry the same way they did pre-#511.
        let backend = self.upgrade_backend()?;
        let scan_now_ms = match backend.server_time_ms().await {
            Ok(t) => t,
            Err(e) => {
                return Err(SchedulerError::EngineContext { source: Box::new(e),
                    context: "scheduler: server_time_ms for rotation cursor".to_owned(),
                });
            }
        };
        let rotation_cursor = self.rotation_cursor(scan_now_ms, num_partitions);
        // Per-worker jitter stacks on the shared cursor: `start_p` diverges
        // different workers within one window; `rotation_cursor` drifts the
        // whole fleet across windows so no single partition is anyone's
        // permanent "partition 0".
        let scan_start = (start_p + rotation_cursor) % num_partitions;
        let scan_budget = self
            .scheduler_config
            .max_partitions_per_scan
            .min(num_partitions)
            .max(1);

        // Observability counters (RFC-plan item 3): one debug line per
        // call, not per partition, so a tight-loop worker doesn't flood
        // logs. `partitions_hit` is 0/1 in practice — the loop returns on
        // the first grant — but keeping it a counter means future
        // multi-grant variants (N per call) don't need the log format to
        // change.
        let mut partitions_visited: u16 = 0;
        let mut partitions_skipped: u16 = 0;
        let mut partitions_hit: u16 = 0;
        let call_start = std::time::Instant::now();

        for p_idx in iter_partitions(num_partitions, scan_start, scan_budget) {
            partitions_visited += 1;
            let partition = Partition {
                family: PartitionFamily::Execution,
                index: p_idx,
            };
            let idx = IndexKeys::new(&partition);
            let eligible_key = idx.lane_eligible(lane);

            // ZRANGEBYSCORE eligible -inf +inf LIMIT 0 1
            // Lowest score = highest priority candidate
            let candidates: Vec<String> = match self
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
            {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(
                        partition = p_idx,
                        error = %e,
                        "scheduler: ZRANGEBYSCORE eligible failed, skipping partition"
                    );
                    partitions_skipped += 1;
                    continue;
                }
            };

            let eid_str = match candidates.first() {
                Some(s) => s,
                None => {
                    // Empty partition — this is the hot path on a quiet
                    // cluster and the whole point of the bounded scan.
                    // Don't count as a "skip" (skip implies something
                    // went wrong); treat as a normal miss.
                    continue;
                }
            };

            // Parse the execution ID
            let eid = match ExecutionId::parse(eid_str) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        partition = p_idx,
                        execution_id = eid_str.as_str(),
                        error = %e,
                        "scheduler: invalid execution_id in eligible set, skipping"
                    );
                    partitions_skipped += 1;
                    continue;
                }
            };

            let exec_ctx = ExecKeyContext::new(&partition, &eid);
            let core_key = exec_ctx.core();
            let eid_s = eid.to_string();
            // Reuse the call-scoped `scan_now_ms` we read for the rotation
            // cursor. Semantics for block-detail timestamps are unchanged —
            // a block decision at time T is still written with a T in the
            // same call; the tiny staleness (<< 1 RTT) is dwarfed by the
            // usual scheduler→Valkey latency and saves one TIME round-trip
            // per candidate.
            let now_ms = scan_now_ms;

            // ── Capability pre-check (in-slot HGET, cheap) ──
            // Runs BEFORE quota admission so we never ZADD a quota slot
            // for an execution this worker can't actually claim. Without
            // this, on an unmatchable-top-of-zset, every scheduling tick
            // would ZADD {q:K}:admitted_set then ZREM it via
            // release_admission on the capability_mismatch reject — a
            // cross-slot write storm amplifying the mismatch loop.
            //
            // Lua `ff_issue_claim_grant` still does the authoritative
            // check; this is a fast-path short-circuit, not a substitute.
            // A narrow race exists where `required_capabilities` is
            // updated between our HGET and the FCALL — the FCALL is still
            // atomic and correct.
            let required_caps_csv: Option<String> = match self
                .client
                .cmd("HGET")
                .arg(&core_key)
                .arg("required_capabilities")
                .execute::<Option<String>>()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        partition = p_idx,
                        execution_id = eid_s.as_str(),
                        error = %e,
                        "scheduler: HGET required_capabilities failed, skipping candidate"
                    );
                    continue;
                }
            };
            if let Some(req) = required_caps_csv.as_deref()
                && !req.is_empty()
                && !ff_core::caps::matches_csv(req, worker_capabilities)
            {
                // Move this execution out of eligible into blocked_route
                // so we don't hot-loop on it every tick (RFC-009 §564). A
                // periodic sweep (scanner side) promotes blocked_route →
                // eligible when a worker with matching caps registers.
                // Logged with a hash digest, not the raw CSV, to keep
                // per-mismatch log volume bounded.
                tracing::info!(
                    partition = p_idx,
                    execution_id = eid_s.as_str(),
                    worker_id = worker_id.as_str(),
                    worker_caps_hash = worker_caps_hash.as_str(),
                    required = req,
                    "scheduler: capability mismatch, blocking execution off eligible"
                );
                self.block_candidate(
                    &partition, &idx, lane, &eid, &eligible_key,
                    "waiting_for_capable_worker",
                    "no connected worker satisfies required_capabilities",
                    now_ms,
                ).await;
                continue;
            }

            // ── Budget pre-check (cross-partition, cached per cycle) ──
            if let Some(block_detail) = self
                .check_budgets(&mut budget_checker, &exec_ctx, &core_key, &eid_s)
                .await?
            {
                // Budget breached — block candidate and try next
                self.block_candidate(
                    &partition, &idx, lane, &eid, &eligible_key,
                    "waiting_for_budget", &block_detail, now_ms,
                ).await;
                continue;
            }

            // ── Quota pre-check (cross-partition FCALL on {q:K}) ──
            let quota_admission = self
                .check_quota(&exec_ctx, &core_key, &eid_s, now_ms)
                .await?;
            match &quota_admission {
                QuotaCheckOutcome::Blocked(block_detail) => {
                    self.block_candidate(
                        &partition, &idx, lane, &eid, &eligible_key,
                        "waiting_for_quota", block_detail, now_ms,
                    ).await;
                    continue;
                }
                QuotaCheckOutcome::NoQuota | QuotaCheckOutcome::Admitted { .. } => {}
            }

            // ── All checks passed — issue claim grant ──
            let grant_key = exec_ctx.claim_grant();
            let _ = &core_key; // kept for any future direct read; unused post-trait-route

            // FF #511 Phase 2c: route through `backend.issue_claim_grant`.
            let caps_set: std::collections::BTreeSet<String> = worker_caps_csv
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            let args = ff_core::contracts::IssueClaimGrantArgs::new(
                eid.clone(),
                lane.clone(),
                worker_id.clone(),
                worker_instance_id.clone(),
                partition,
                caps_set,
                grant_ttl_ms,
                ff_core::types::TimestampMs(now_ms as i64),
            );

            let backend = self.upgrade_backend()?;
            let outcome = match backend.issue_claim_grant(args).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(
                        partition = p_idx,
                        execution_id = eid_s.as_str(),
                        error = %e,
                        "scheduler: issue_claim_grant transport error, trying next"
                    );
                    if let QuotaCheckOutcome::Admitted { tag, quota_id, eid } = &quota_admission {
                        self.release_admission(tag, quota_id, eid).await;
                    }
                    continue;
                }
            };

            // Trait's `IssueClaimGrantOutcome` only surfaces `Granted`
            // as success today; Lua-level rejects (capability mismatch,
            // grant_already_exists, etc.) come back on the outer `Err`
            // branch above. Pattern-match with a wildcard to stay
            // #[non_exhaustive]-robust without dropping into dead-code.
            let _ = outcome;

            partitions_hit += 1;
            tracing::debug!(
                partition = p_idx,
                execution_id = eid_s.as_str(),
                worker_instance_id = worker_instance_id.as_str(),
                start_p = scan_start,
                partitions_visited,
                partitions_skipped,
                partitions_hit,
                elapsed_ms = call_start.elapsed().as_millis() as u64,
                "scheduler: claim call completed (hit)"
            );
            return Ok(Some(ClaimGrant::new(
                eid,
                ff_core::partition::PartitionKey::from(&partition),
                grant_key.clone(),
                now_ms + grant_ttl_ms,
            )));
        }

        tracing::debug!(
            worker_instance_id = worker_instance_id.as_str(),
            start_p = scan_start,
            partitions_visited,
            partitions_skipped,
            partitions_hit,
            elapsed_ms = call_start.elapsed().as_millis() as u64,
            "scheduler: claim call completed (no hit)"
        );
        Ok(None)
    }

    /// Read budget_ids from exec_core and check each. Returns block detail
    /// string if any budget is breached, None if all pass.
    async fn check_budgets(
        &self,
        checker: &mut BudgetChecker,
        _exec_ctx: &ExecKeyContext,
        core_key: &str,
        _eid_s: &str,
    ) -> Result<Option<String>, SchedulerError> {
        // Read budget_ids from exec_core (comma-separated or JSON list)
        let budget_ids_str: Option<String> = self
            .client
            .cmd("HGET")
            .arg(core_key)
            .arg("budget_ids")
            .execute()
            .await?;

        let budget_ids_str = match budget_ids_str {
            Some(s) => s,
            None => return Ok(None),
        };
        if budget_ids_str.is_empty() {
            return Ok(None); // no budgets attached
        }

        // Parse comma-separated budget IDs
        for budget_id in budget_ids_str.split(',') {
            let budget_id = budget_id.trim();
            if budget_id.is_empty() {
                continue;
            }
            let result = checker.check_budget(&self.client, budget_id).await?;
            if let BudgetCheckResult::HardBreach { dimension, detail } = &result {
                // PR-94: budget hard-breach counter, labelled by
                // dimension so operators can distinguish "tokens"
                // breaches from "cost" breaches at a glance.
                self.metrics.inc_budget_hit(dimension);
                return Ok(Some(detail.clone()));
            }
        }

        Ok(None)
    }

    /// Check quota admission for the candidate.
    async fn check_quota(
        &self,
        _exec_ctx: &ExecKeyContext,
        core_key: &str,
        eid_s: &str,
        now_ms: u64,
    ) -> Result<QuotaCheckOutcome, SchedulerError> {
        // Read quota_policy_id from exec_core
        let quota_id_str: Option<String> = self
            .client
            .cmd("HGET")
            .arg(core_key)
            .arg("quota_policy_id")
            .execute()
            .await?;

        let quota_id_str = match quota_id_str {
            Some(s) => s,
            None => return Ok(QuotaCheckOutcome::NoQuota),
        };
        if quota_id_str.is_empty() {
            return Ok(QuotaCheckOutcome::NoQuota);
        }

        // Compute real {q:K} partition tag from quota_policy_id
        let tag = match QuotaPolicyId::parse(&quota_id_str) {
            Ok(qid) => {
                let partition = quota_partition(&qid, &self.config);
                partition.hash_tag()
            }
            Err(_) => "{q:0}".to_owned(), // fallback for non-UUID test IDs
        };

        let quota_def_key = format!("ff:quota:{}:{}", tag, quota_id_str);
        let window_key = format!("ff:quota:{}:{}:window:requests_per_window", tag, quota_id_str);
        let concurrency_key = format!("ff:quota:{}:{}:concurrency", tag, quota_id_str);
        let admitted_key = format!("ff:quota:{}:{}:admitted:{}", tag, quota_id_str, eid_s);
        let admitted_set_key = format!("ff:quota:{}:{}:admitted_set", tag, quota_id_str);

        // FF #511 Phase 2c: route the policy-limits read through the
        // trait. Parse failure on `quota_id_str` → treat as no-quota
        // (matches pre-#511 behaviour where invalid IDs fell through
        // to the unlimited branch).
        let Ok(qid) = QuotaPolicyId::parse(&quota_id_str) else {
            return Ok(QuotaCheckOutcome::NoQuota);
        };
        let backend = self.upgrade_backend()?;
        let limits = match backend.read_quota_policy_limits(&qid).await {
            Ok(Some(l)) => l,
            Ok(None) => return Ok(QuotaCheckOutcome::NoQuota),
            Err(e) => {
                return Err(SchedulerError::EngineContext { source: Box::new(e),
                    context: format!("check_quota: read_quota_policy_limits {qid}"),
                });
            }
        };
        let rate_limit = limits.max_requests_per_window;
        let window_secs = if limits.requests_per_window_seconds == 0 {
            60
        } else {
            limits.requests_per_window_seconds
        };
        let concurrency_cap = limits.active_concurrency_cap;
        let jitter_ms = limits.jitter_ms;

        if limits.is_unlimited() {
            return Ok(QuotaCheckOutcome::NoQuota);
        }

        // FF #511 Phase 2c: route through `backend.check_admission`.
        // The trait result enum maps 1:1 onto the pre-#511 Lua status
        // strings: Admitted/AlreadyAdmitted → QuotaCheckOutcome::Admitted,
        // RateExceeded → Blocked (rate), ConcurrencyExceeded → Blocked.
        // Unused inline keys stay in scope as dead references only —
        // remove them since the FCALL path is gone.
        let _ = (&window_key, &concurrency_key, &quota_def_key,
                 &admitted_key, &admitted_set_key);

        let eid_typed = match ExecutionId::parse(eid_s) {
            Ok(e) => e,
            Err(_) => {
                return Err(SchedulerError::Config(format!(
                    "check_quota: invalid execution_id {eid_s:?}"
                )));
            }
        };
        let args = ff_core::contracts::CheckAdmissionArgs {
            execution_id: eid_typed,
            now: ff_core::types::TimestampMs(now_ms as i64),
            window_seconds: window_secs,
            rate_limit,
            concurrency_cap,
            jitter_ms: if jitter_ms == 0 { None } else { Some(jitter_ms) },
        };

        match backend.check_admission(&qid, "default", args).await {
            Ok(result) => {
                use ff_core::contracts::CheckAdmissionResult;
                match result {
                    CheckAdmissionResult::Admitted | CheckAdmissionResult::AlreadyAdmitted => {
                        Ok(QuotaCheckOutcome::Admitted {
                            tag: tag.clone(),
                            quota_id: quota_id_str.clone(),
                            eid: eid_s.to_owned(),
                        })
                    }
                    CheckAdmissionResult::RateExceeded { .. } => {
                        self.metrics.inc_quota_hit("rate");
                        Ok(QuotaCheckOutcome::Blocked(format!(
                            "quota {}: rate limit {}/{} per {}s window",
                            quota_id_str, rate_limit, rate_limit, window_secs
                        )))
                    }
                    CheckAdmissionResult::ConcurrencyExceeded => {
                        self.metrics.inc_quota_hit("concurrency");
                        Ok(QuotaCheckOutcome::Blocked(format!(
                            "quota {}: concurrency cap {}",
                            quota_id_str, concurrency_cap
                        )))
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    quota_id = quota_id_str.as_str(),
                    error = %e,
                    "scheduler: check_admission failed, denying (fail-closed)"
                );
                Err(SchedulerError::EngineContext { source: Box::new(e),
                    context: format!("check_admission {quota_id_str}"),
                })
            }
        }
    }

    /// Block a candidate that failed budget/quota check.
    /// FCALL ff_block_execution_for_admission on {p:N}.
    #[allow(clippy::too_many_arguments)]
    async fn block_candidate(
        &self,
        partition: &Partition,
        _idx: &IndexKeys,
        lane: &LaneId,
        eid: &ExecutionId,
        _eligible_key: &str,
        block_reason: &str,
        blocking_detail: &str,
        now_ms: u64,
    ) {
        let eid_s = eid.to_string();
        let reason = match block_reason {
            "waiting_for_budget" => ff_core::contracts::BlockingReason::WaitingForBudget,
            "waiting_for_quota" => ff_core::contracts::BlockingReason::WaitingForQuota,
            "waiting_for_capable_worker" => {
                ff_core::contracts::BlockingReason::WaitingForCapableWorker
            }
            other => {
                tracing::warn!(
                    execution_id = eid_s,
                    reason = other,
                    "scheduler: unrecognised block reason, defaulting to WaitingForBudget"
                );
                ff_core::contracts::BlockingReason::WaitingForBudget
            }
        };

        let args = ff_core::contracts::BlockExecutionForAdmissionArgs::new(
            eid.clone(),
            lane.clone(),
            *partition,
            reason,
            if blocking_detail.is_empty() {
                None
            } else {
                Some(blocking_detail.to_owned())
            },
            ff_core::types::TimestampMs(now_ms as i64),
        );

        let Some(backend) = self.backend.upgrade() else {
            tracing::warn!(
                execution_id = eid_s,
                "scheduler: block_execution_for_admission skipped — backend dropped"
            );
            return;
        };
        match backend.block_execution_for_admission(args).await {
            Ok(ff_core::contracts::BlockExecutionForAdmissionOutcome::Blocked { .. }) => {
                tracing::info!(
                    execution_id = eid_s,
                    reason = block_reason,
                    "scheduler: candidate blocked by admission check"
                );
            }
            Ok(ff_core::contracts::BlockExecutionForAdmissionOutcome::LuaRejected { message }) => {
                // Logical reject (e.g. execution went terminal between
                // scheduler pick and block call); candidate loop moves on.
                tracing::warn!(
                    execution_id = eid_s,
                    reason = block_reason,
                    error = %message,
                    "scheduler: block_execution_for_admission rejected"
                );
            }
            Ok(_) => {
                // #[non_exhaustive] fallback.
                tracing::warn!(
                    execution_id = eid_s,
                    "scheduler: unknown BlockExecutionForAdmissionOutcome variant"
                );
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_s,
                    error = %e,
                    "scheduler: block_execution_for_admission transport failed"
                );
            }
        }
    }

    /// Release a previously-recorded quota admission slot.
    /// Called when issue_claim_grant fails after admission was
    /// recorded. FF #511 Phase 2c: trait-routed.
    async fn release_admission(
        &self,
        _tag: &str,
        quota_id: &str,
        eid_s: &str,
    ) {
        let Ok(qid) = QuotaPolicyId::parse(quota_id) else {
            tracing::warn!(
                execution_id = eid_s,
                quota_id,
                "scheduler: release_admission skipped — invalid quota_policy_id"
            );
            return;
        };
        let Ok(eid) = ExecutionId::parse(eid_s) else {
            tracing::warn!(
                execution_id = eid_s,
                quota_id,
                "scheduler: release_admission skipped — invalid execution_id"
            );
            return;
        };
        let args = ff_core::contracts::ReleaseAdmissionArgs::new(qid, eid);
        let Some(backend) = self.backend.upgrade() else {
            tracing::warn!(
                execution_id = eid_s,
                quota_id,
                "scheduler: release_admission skipped — backend dropped"
            );
            return;
        };
        match backend.release_admission(args).await {
            Ok(_) => {
                tracing::info!(
                    execution_id = eid_s,
                    quota_id,
                    "scheduler: released admission after claim failure"
                );
            }
            Err(e) => {
                tracing::warn!(
                    execution_id = eid_s,
                    quota_id,
                    error = %e,
                    "scheduler: release_admission failed \
                     (slot will expire via TTL)"
                );
            }
        }
    }
}

/// Errors from the scheduler.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// Valkey connection or command error (preserves ErrorKind for caller inspection).
    #[error("valkey: {0}")]
    Valkey(#[from] ferriskey::Error),
    /// Valkey error with additional context (preserves ErrorKind via #[source]).
    #[error("valkey ({context}): {source}")]
    ValkeyContext {
        #[source]
        source: ferriskey::Error,
        context: String,
    },
    /// FF #511 Phase 2c: trait-routed backend error (backend-agnostic).
    /// Carries the original `EngineError` so callers can classify by
    /// kind via `EngineError::kind`-ish matchers. Boxed because
    /// `EngineError` is large (clippy::result_large_err).
    #[error("engine ({context}): {source}")]
    EngineContext {
        #[source]
        source: Box<ff_core::engine_error::EngineError>,
        context: String,
    },
    /// Caller-supplied value failed ingress validation. NOT retryable — the
    /// caller must fix its input before retrying.
    #[error("config: {0}")]
    Config(String),
}

impl SchedulerError {
    /// Returns the underlying ferriskey ErrorKind, if this is a Valkey error.
    /// Matches `ScriptError::valkey_kind` so callers can treat both
    /// uniformly. (Note: `ServerError` exposed the same shape until #88
    /// renamed it to `ServerError::backend_kind` with a `BackendErrorKind`
    /// return; this scheduler-internal surface keeps the raw-ErrorKind
    /// name pending its own sealing.)
    pub fn valkey_kind(&self) -> Option<ferriskey::ErrorKind> {
        match self {
            Self::Valkey(e) | Self::ValkeyContext { source: e, .. } => Some(e.kind()),
            // EngineContext wraps a higher-level error that's already
            // classified (transport vs validation vs contention) —
            // callers that need retry-ability consult
            // `EngineError::is_retryable` instead. Return None here.
            Self::EngineContext { .. } => None,
            Self::Config(_) => None,
        }
    }

    /// Whether this error is safely retryable by a caller. Mirrors
    /// `ServerError::is_retryable` semantics.
    pub fn is_retryable(&self) -> bool {
        self.valkey_kind()
            .map(is_retryable_kind)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferriskey::ErrorKind;

    fn mk_fk_err(kind: ErrorKind) -> ferriskey::Error {
        ferriskey::Error::from((kind, "synthetic"))
    }

    #[test]
    fn scheduler_is_retryable_matches_kind_table() {
        assert!(SchedulerError::Valkey(mk_fk_err(ErrorKind::IoError)).is_retryable());
        assert!(SchedulerError::Valkey(mk_fk_err(ErrorKind::ClusterDown)).is_retryable());

        assert!(!SchedulerError::Valkey(mk_fk_err(ErrorKind::FatalReceiveError)).is_retryable());
        assert!(!SchedulerError::Valkey(mk_fk_err(ErrorKind::NoScriptError)).is_retryable());
        assert!(!SchedulerError::Valkey(mk_fk_err(ErrorKind::Moved)).is_retryable());
    }

    #[test]
    fn scheduler_valkey_context_is_retryable() {
        let err = SchedulerError::ValkeyContext {
            source: mk_fk_err(ErrorKind::BusyLoadingError),
            context: "HGET budget_ids".into(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn scheduler_valkey_kind_exposed() {
        let err = SchedulerError::Valkey(mk_fk_err(ErrorKind::TryAgain));
        assert_eq!(err.valkey_kind(), Some(ErrorKind::TryAgain));
    }

    // ── iter_partitions: regression test for the modular wrap + bounded
    // length contract. The fairness behaviour of the bounded scheduler
    // scan depends entirely on this helper returning exactly `count`
    // distinct partition indices starting at `start` and wrapping modulo
    // `total`. Tested in isolation so a bug here is distinguishable from
    // a bug in the Valkey-backed loop. ──

    #[test]
    fn iter_partitions_no_wrap() {
        // start=10, count=5, total=256 → 10..15, no wrap involved.
        let ps: Vec<u16> = iter_partitions(256, 10, 5).collect();
        assert_eq!(ps, vec![10, 11, 12, 13, 14]);
    }

    #[test]
    fn iter_partitions_wraps_modulo_total() {
        // start=254, count=5, total=256 → 254, 255, 0, 1, 2.
        let ps: Vec<u16> = iter_partitions(256, 254, 5).collect();
        assert_eq!(ps, vec![254, 255, 0, 1, 2]);
    }

    #[test]
    fn iter_partitions_count_capped_to_total() {
        // Asking for more than `total` yields exactly `total` distinct
        // partitions — never a duplicate, never more than the universe.
        let ps: Vec<u16> = iter_partitions(4, 1, 100).collect();
        assert_eq!(ps, vec![1, 2, 3, 0]);
    }

    #[test]
    fn iter_partitions_length_matches_count() {
        // Invariant: output length == min(count, total). The scan loop
        // upper-bounds round-trips on this, so regressing it would
        // silently re-introduce the 256-round-trip-per-tick bug.
        for start in [0u16, 1, 50, 255] {
            for count in [0u16, 1, 16, 32, 256] {
                let len = iter_partitions(256, start, count).count();
                assert_eq!(len, count.min(256) as usize);
            }
        }
    }

    // ── Fairness: the union of the partitions visited across
    // `ceil(total / max_partitions_per_scan)` successive scans, with the
    // rotation cursor advancing each scan, must cover every partition
    // exactly once. This is the contract the operator rustdoc promises
    // ("any given partition reached within 8 ticks at defaults"). ──
    #[test]
    fn fairness_full_coverage_in_ceil_total_over_budget_scans() {
        const TOTAL: u16 = 256;
        const BUDGET: u16 = SchedulerConfig::DEFAULT_MAX_PARTITIONS_PER_SCAN;
        let scans = TOTAL.div_ceil(BUDGET); // 8 at defaults

        // Simulate the same advance logic the live cursor performs: each
        // "scan" starts at the previous start + BUDGET (mod TOTAL). We
        // pin start to 0 for determinism; the per-worker FNV jitter is a
        // phase offset on top and doesn't change coverage.
        let mut union = std::collections::BTreeSet::new();
        let mut cursor: u16 = 0;
        for _ in 0..scans {
            for p in iter_partitions(TOTAL, cursor, BUDGET) {
                union.insert(p);
            }
            cursor = cursor.wrapping_add(BUDGET) % TOTAL;
        }

        assert_eq!(union.len(), TOTAL as usize, "every partition visited once");
        for p in 0..TOTAL {
            assert!(union.contains(&p), "missing partition {p}");
        }
    }

    #[test]
    fn fairness_full_coverage_with_phase_offset() {
        // Regression: the per-worker FNV phase must not change the
        // coverage property. Pick a non-zero start; we still cover the
        // whole universe in ceil(total/budget) scans.
        const TOTAL: u16 = 256;
        const BUDGET: u16 = SchedulerConfig::DEFAULT_MAX_PARTITIONS_PER_SCAN;
        let scans = TOTAL.div_ceil(BUDGET);

        let mut union = std::collections::BTreeSet::new();
        let mut cursor: u16 = 137; // arbitrary per-worker jitter
        for _ in 0..scans {
            for p in iter_partitions(TOTAL, cursor, BUDGET) {
                union.insert(p);
            }
            cursor = cursor.wrapping_add(BUDGET) % TOTAL;
        }
        assert_eq!(union.len(), TOTAL as usize);
    }

    #[test]
    fn scheduler_config_defaults_match_rustdoc() {
        let c = SchedulerConfig::default();
        assert_eq!(c.max_partitions_per_scan, 32);
        assert_eq!(c.rotation_window_ms, 250);
    }
}



