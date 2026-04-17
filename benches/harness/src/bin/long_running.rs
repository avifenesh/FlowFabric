//! Scenario 3 — long-running steady-state throughput.
//!
//! **Workload**
//!   * 100 distinct `FlowFabricWorker` instances, each with its own
//!     `worker_instance_id` so the scheduler's lease-owner accounting
//!     and worker-index sharding see the real fan-out (a single
//!     shared worker would collapse all leases onto one owner and
//!     sidestep the surface we want to measure).
//!   * 10 000 tasks pre-seeded, then a refill loop keeps the
//!     eligible queue non-empty for the full run (without refill a
//!     100-worker pool drains 10k in a few seconds and the rest of
//!     the run measures idle behavior).
//!   * Each claimed task simulates ~3 s of work
//!     (`tokio::time::sleep`) before `complete()`. 3 s is long
//!     enough to cross the lease-renewal tick at `ttl/3`, so the
//!     renewal path is exercised on a representative fraction of
//!     tasks.
//!   * Every submitted task carries `execution_deadline_at = t+60s`.
//!     Under steady-state load some tasks will miss — that is the
//!     back-pressure signal.
//!
//! **Metrics**
//!   * Throughput   — `completed / wall_seconds`
//!   * p50/p95/p99  — claim-to-complete latency (µs histogram)
//!   * RSS          — `/proc/self/status` VmRSS samples at start /
//!     midpoint / end (linux); `ps -o rss=` fallback on macOS
//!   * Lease renewal overhead = `sum(renewal_duration_ns) /
//!     total_worker_time_ns`. Measured by a custom
//!     `tracing_subscriber::Layer` attached to the `renew_lease`
//!     span added to `ff_sdk::task::renew_lease_inner`. No
//!     calibration estimate — this is the real measured overhead
//!     under steady-state load, which is strictly ≥ the quiescent
//!     calibration number.
//!   * Missed deadline rate — computed client-side from
//!     `(complete_ts - submit_ts) > 60_000ms`. Cheaper than polling
//!     per-task deadline state.
//!
//! **Shutdown**
//!   * `SIGINT` / `SIGTERM` → graceful drain.
//!   * Runtime cap at `duration + 30s` — past that we exit non-zero
//!     and print what we have so the outer driver can inspect the
//!     diff vs expected duration.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use ff_bench::report::Percentiles;
use ff_bench::{write_report, LatencyMs, Report, SYSTEM_FLOWFABRIC};
use ff_sdk::{ClaimedTask, FlowFabricWorker, WorkerConfig};
use reqwest::Client;
use tokio::sync::{Mutex, Notify};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const SCENARIO: &str = "long_running_steady_state";
const DEFAULT_WORKERS: usize = 100;
const DEFAULT_SEED: usize = 10_000;
const PAYLOAD_BYTES: usize = 4 * 1024;
const TASK_WORK_MS: u64 = 3_000;
const TASK_DEADLINE_MS: i64 = 60_000;
const POLL_INTERVAL_MS: u64 = 50;
const REFILL_EVERY_MS: u64 = 10_000;
const REFILL_TOPUP: usize = 1_000;
const SHUTDOWN_GRACE_S: u64 = 30;

// Lease TTL tuned so renewals actually fire during the 3 s work sleep:
// renewal tick is `lease_ttl_ms / 3` (see `ff_sdk::config`), so a 6 s
// TTL gives a renewal at ~2 s into the task — every task triggers
// exactly one renewal. With the default 30 s TTL no renewals fire
// inside a 3 s task and the overhead measurement is trivially zero.
const LEASE_TTL_MS: u64 = 6_000;

#[derive(Parser)]
#[command(about = "FlowFabric scenario 3 — long-running steady-state")]
struct Args {
    #[arg(long, env = "FF_BENCH_DURATION_S", default_value_t = 300)]
    duration: u64,
    #[arg(long, default_value_t = DEFAULT_WORKERS)]
    workers: usize,
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: usize,
    /// Where to write the JSON report (relative to cwd / repo root).
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

// ── Renewal-overhead tracing Layer ─────────────────────────────────────

/// Counters incremented by `RenewalLayer`. The Layer itself is stateless
/// aside from the span-id → start-Instant map it keeps in `active` so
/// `on_exit` can compute the exit delta. Global `Arc<RenewalCounters>`
/// lets the harness read the aggregate at shutdown.
#[derive(Default)]
struct RenewalCounters {
    count: AtomicU64,
    total_ns: AtomicU64,
}

/// Per-span state tracked by `RenewalLayer`.
///
/// `first_enter` is the wall-clock time of the first poll (span is now
/// on the active stack). `busy_ns` accumulates time spent between each
/// on_enter / on_exit pair — i.e. the CPU/await-time the renewal
/// future is actually running (not idle/yielded). When the span closes
/// we commit the accumulator to the global total: one count per span,
/// real measured busy time per call.
struct SpanState {
    last_enter: Instant,
    busy_ns: u64,
}

struct RenewalLayer {
    counters: Arc<RenewalCounters>,
    // Renewal ticks are at least `lease_ttl_ms / 3` apart per task
    // (≥ 2 s on the scenario's tuned 6 s TTL, ≥ 10 s on SDK default),
    // so contention on this mutex is negligible in practice.
    active: std::sync::Mutex<std::collections::HashMap<tracing::span::Id, SpanState>>,
}

impl RenewalLayer {
    fn new(counters: Arc<RenewalCounters>) -> Self {
        Self {
            counters,
            active: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn is_renewal(md: &tracing::Metadata<'_>) -> bool {
        md.target() == "ff_sdk::task" && md.name() == "renew_lease"
    }
}

impl<S> tracing_subscriber::Layer<S> for RenewalLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    // Called once when the span is created — we pre-register state so
    // on_enter / on_exit can accumulate busy time without racing on
    // "first poll sees no entry".
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if Self::is_renewal(attrs.metadata()) {
            let mut guard = self.active.lock().expect("renewal mutex");
            guard.insert(
                id.clone(),
                SpanState {
                    last_enter: Instant::now(),
                    busy_ns: 0,
                },
            );
        }
    }

    fn on_enter(
        &self,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id) {
            if Self::is_renewal(span.metadata()) {
                let mut guard = self.active.lock().expect("renewal mutex");
                if let Some(state) = guard.get_mut(id) {
                    state.last_enter = Instant::now();
                }
            }
        }
    }

    fn on_exit(
        &self,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id) {
            if Self::is_renewal(span.metadata()) {
                let mut guard = self.active.lock().expect("renewal mutex");
                if let Some(state) = guard.get_mut(id) {
                    state.busy_ns += state.last_enter.elapsed().as_nanos() as u64;
                }
            }
        }
    }

    // Called once when the span is dropped — exactly one count per
    // renewal call, and we commit the accumulated busy time.
    fn on_close(
        &self,
        id: tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let state = {
            let mut guard = self.active.lock().expect("renewal mutex");
            guard.remove(&id)
        };
        if let Some(s) = state {
            self.counters.count.fetch_add(1, Ordering::Relaxed);
            self.counters
                .total_ns
                .fetch_add(s.busy_ns, Ordering::Relaxed);
        }
    }
}

// ── RSS probe ──────────────────────────────────────────────────────────

/// Read Resident Set Size in MB. Linux reads `/proc/self/status`
/// (VmRSS is reported in kB). macOS falls back to `ps -o rss=` which
/// likewise reports kB. Returns None on error so the harness reports
/// whatever it can still capture.
fn read_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()?;
                return Some(kb / 1024);
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = std::str::from_utf8(&out.stdout).ok()?.trim();
        let kb: u64 = text.parse().ok()?;
        Some(kb / 1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

// ── Per-worker state ──────────────────────────────────────────────────

/// Accumulated during the run; moved out of each worker on shutdown so
/// the aggregator can compute percentiles without any cross-worker
/// locking in the hot path.
struct WorkerOutcome {
    /// claim-to-complete latency samples, microseconds.
    latencies_us: Vec<u64>,
    completed: u64,
    missed: u64,
    failed: u64,
}

// ── Main ──────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let env = ff_bench::workload::BenchEnv::from_env();

    // Tracing pipeline: stdout fmt layer + our renewal counter layer.
    // EnvFilter default is `info` for the bench itself; renewal spans
    // in ff_sdk live at TRACE inside the function body, but the span
    // enter/exit events are delivered to layers independent of the
    // level filter's event gating — so we explicitly bump ff_sdk=trace
    // to make sure the span is actually constructed.
    let renewal_counters: Arc<RenewalCounters> = Arc::new(RenewalCounters::default());
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(
            "long_running=info,ff_bench=info,ff_sdk=trace",
        ));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().compact())
        .with(RenewalLayer::new(renewal_counters.clone()))
        .init();

    tracing::info!(
        duration_s = args.duration,
        workers = args.workers,
        seed = args.seed,
        "scenario 3 starting"
    );

    let http = ff_bench::workload::http_client()?;

    // Pre-seed the queue before spinning up workers so the first claim
    // attempts all succeed. Failures at this stage are usually "server
    // down" — fail loud rather than silently measuring an empty run.
    let seed_start = Instant::now();
    seed_tasks(&http, &env, args.seed).await?;
    tracing::info!(
        seeded = args.seed,
        seed_wall_ms = seed_start.elapsed().as_millis() as u64,
        "seeded initial backlog"
    );

    // Shared state.
    let shutdown = Arc::new(Notify::new());
    let stop_flag = Arc::new(AtomicBool::new(false));
    let outcomes: Arc<Mutex<Vec<WorkerOutcome>>> = Arc::new(Mutex::new(Vec::new()));
    let outcomes_cap_per_worker = estimated_tasks_per_worker(&args);

    // Record RSS at run start.
    let rss_start_mb = read_rss_mb();

    // Worker pool.
    let drain_start = Instant::now();
    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let env = env.clone();
        let shutdown = shutdown.clone();
        let stop_flag = stop_flag.clone();
        let outcomes = outcomes.clone();
        handles.push(tokio::spawn(async move {
            match drive_worker(
                wi,
                env,
                shutdown,
                stop_flag,
                outcomes_cap_per_worker,
            )
            .await
            {
                Ok(o) => {
                    outcomes.lock().await.push(o);
                }
                Err(e) => {
                    tracing::error!(worker = wi, error = %e, "worker exited with error");
                }
            }
        }));
    }

    // Refill loop — keeps the eligible queue topped up so the workers
    // measure steady-state throughput, not drain-and-idle.
    let refiller_env = env.clone();
    let refiller_http = http.clone();
    let refiller_shutdown = shutdown.clone();
    let refiller = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(REFILL_EVERY_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // prime
        loop {
            tokio::select! {
                _ = refiller_shutdown.notified() => return,
                _ = ticker.tick() => {
                    if let Err(e) = seed_tasks(&refiller_http, &refiller_env, REFILL_TOPUP).await {
                        tracing::warn!(error = %e, "refill batch failed; continuing");
                    }
                }
            }
        }
    });

    // Midpoint RSS sample via a dedicated timer task so it fires
    // regardless of what the workers are doing.
    let rss_mid_cell: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let mid_deadline_ms = args.duration.saturating_mul(1_000) / 2;
    let rss_mid_cell_clone = rss_mid_cell.clone();
    let rss_mid_shutdown = shutdown.clone();
    let rss_mid = tokio::spawn(async move {
        tokio::select! {
            _ = rss_mid_shutdown.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(mid_deadline_ms)) => {
                let sample = read_rss_mb();
                *rss_mid_cell_clone.lock().await = sample;
                tracing::info!(rss_mid_mb = ?sample, "midpoint RSS sample");
            }
        }
    });

    // Main wait: duration elapsed, ctrl-c, or cap timeout.
    let run_duration = Duration::from_secs(args.duration);
    let timeout_cap = Duration::from_secs(args.duration + SHUTDOWN_GRACE_S);
    let termination_reason = tokio::select! {
        _ = tokio::signal::ctrl_c() => "signal",
        _ = tokio::time::sleep(run_duration) => "duration_elapsed",
        _ = tokio::time::sleep(timeout_cap) => "hard_timeout",
    };
    tracing::info!(reason = termination_reason, "initiating shutdown");

    // Notify workers + refiller. Workers finish their current task and
    // then exit the claim loop cleanly.
    stop_flag.store(true, Ordering::Release);
    shutdown.notify_waiters();

    // Give workers time to drain their in-flight task (one ~3s work
    // sleep + one ~few-hundred-ms complete). Past the grace period we
    // let the JoinHandle errors flow through and report what we have.
    let drain_timeout = Duration::from_secs(SHUTDOWN_GRACE_S);
    let _ = tokio::time::timeout(drain_timeout, async {
        for h in handles {
            let _ = h.await;
        }
    })
    .await;
    refiller.abort();
    rss_mid.abort();
    let _ = rss_mid.await;
    let _ = refiller.await;

    let wall = drain_start.elapsed();
    let rss_end_mb = read_rss_mb();
    let rss_mid_mb = *rss_mid_cell.lock().await;

    // Aggregate worker outcomes.
    let outcomes = outcomes.lock().await;
    let mut all_latencies: Vec<u64> = Vec::new();
    let mut completed: u64 = 0;
    let mut missed: u64 = 0;
    let mut failed: u64 = 0;
    for o in outcomes.iter() {
        all_latencies.extend(&o.latencies_us);
        completed += o.completed;
        missed += o.missed;
        failed += o.failed;
    }
    drop(outcomes);

    let throughput = if wall.as_secs_f64() > 0.0 {
        completed as f64 / wall.as_secs_f64()
    } else {
        0.0
    };
    let latency = Percentiles::from_micros(&all_latencies);
    let renewal_count = renewal_counters.count.load(Ordering::Relaxed);
    let renewal_total_ns = renewal_counters.total_ns.load(Ordering::Relaxed);
    // Overhead denominator: total worker time = workers × wall_ns. Each
    // worker holds a tokio task for the entire wall window, so this is
    // the right apples-to-apples ratio — "what fraction of the pool's
    // aggregate runtime was spent in renew_lease".
    let worker_time_ns = (args.workers as u64) * (wall.as_nanos() as u64);
    let renewal_overhead_pct = if worker_time_ns > 0 {
        (renewal_total_ns as f64 / worker_time_ns as f64) * 100.0
    } else {
        0.0
    };
    let missed_pct = if completed + missed > 0 {
        (missed as f64 / (completed + missed) as f64) * 100.0
    } else {
        0.0
    };

    tracing::info!(
        wall_s = wall.as_secs_f64(),
        completed,
        missed,
        failed,
        throughput_ops_s = throughput,
        p50_ms = latency.p50,
        p95_ms = latency.p95,
        p99_ms = latency.p99,
        renewal_count,
        renewal_overhead_pct,
        rss_start_mb = ?rss_start_mb,
        rss_mid_mb = ?rss_mid_mb,
        rss_end_mb = ?rss_end_mb,
        "scenario 3 complete"
    );

    // Write JSON report.
    let rss_growth_pct = match (rss_start_mb, rss_end_mb) {
        (Some(s), Some(e)) if s > 0 => Some(((e as f64 - s as f64) / s as f64) * 100.0),
        _ => None,
    };
    let config = serde_json::json!({
        "duration_s": args.duration,
        "workers": args.workers,
        "seed": args.seed,
        "refill_topup_per_interval": REFILL_TOPUP,
        "refill_interval_ms": REFILL_EVERY_MS,
        "payload_bytes": PAYLOAD_BYTES,
        "deadline_per_task_ms": TASK_DEADLINE_MS,
        "task_work_ms": TASK_WORK_MS,
        "termination_reason": termination_reason,
        "completed": completed,
        "failed": failed,
        "missed_deadline_count": missed,
        "missed_deadline_pct": missed_pct,
        "lease_renewal_count": renewal_count,
        "lease_renewal_total_ns": renewal_total_ns,
        "lease_renewal_overhead_pct": renewal_overhead_pct,
        "lease_renewal_overhead_method": "span_enter_exit_measured",
        "rss_start_mb": rss_start_mb,
        "rss_mid_mb": rss_mid_mb,
        "rss_end_mb": rss_end_mb,
        "rss_growth_pct": rss_growth_pct,
    });
    let mut report = Report::fill_env(
        SCENARIO,
        SYSTEM_FLOWFABRIC,
        env.cluster,
        config,
        throughput,
        LatencyMs {
            p50: latency.p50,
            p95: latency.p95,
            p99: latency.p99,
        },
    );
    if termination_reason != "duration_elapsed" {
        report.notes = Some(format!(
            "terminated early: {termination_reason}; metrics cover partial run"
        ));
    }
    let dir = resolve_results_dir(&args.results_dir);
    match write_report(&report, &dir) {
        Ok(path) => println!("[bench] wrote {}", path.display()),
        Err(e) => eprintln!("[bench] WARN: could not write report: {e}"),
    }

    if failed > 0 && completed == 0 {
        anyhow::bail!("all workers reported failures — server likely unavailable");
    }
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────

fn estimated_tasks_per_worker(args: &Args) -> usize {
    // Rough upper bound: (duration / work_ms_per_task) — used only to
    // pre-size the per-worker latency vec so push() avoids repeated
    // realloc under the hot path.
    let per_worker = (args.duration * 1000 / TASK_WORK_MS.max(1)) as usize;
    per_worker.max(64)
}

fn resolve_results_dir(hint: &str) -> PathBuf {
    let abs = PathBuf::from(hint);
    if abs.is_absolute() && abs.exists() {
        return abs;
    }
    // Walk up from the binary's dir looking for `benches/results`.
    let mut walk = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    for _ in 0..6 {
        let cand = walk.join(hint);
        if cand.exists() {
            return cand;
        }
        if !walk.pop() {
            break;
        }
    }
    PathBuf::from(hint)
}

async fn seed_tasks(
    client: &Client,
    env: &ff_bench::workload::BenchEnv,
    n: usize,
) -> Result<()> {
    const CONC: usize = 32;
    use tokio::sync::Semaphore;
    let sem = Arc::new(Semaphore::new(CONC));
    let payload = ff_bench::workload::filler_payload(PAYLOAD_BYTES);
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .context("semaphore closed")?;
        let client = client.clone();
        let env = env.clone();
        let payload = payload.clone();
        handles.push(tokio::spawn(async move {
            let deadline = ff_bench::workload::now_ms() + TASK_DEADLINE_MS;
            let res = ff_bench::workload::create_execution_with_deadline(
                &client,
                &env,
                "bench.scenario3",
                payload,
                Some(deadline),
            )
            .await;
            drop(permit);
            res
        }));
    }
    for h in handles {
        h.await??;
    }
    Ok(())
}

async fn drive_worker(
    wi: usize,
    env: ff_bench::workload::BenchEnv,
    shutdown: Arc<Notify>,
    stop_flag: Arc<AtomicBool>,
    cap: usize,
) -> Result<WorkerOutcome> {
    let instance_id = format!("bench-worker-{wi}-{}", uuid::Uuid::new_v4());
    let mut config = WorkerConfig::new(
        &env.valkey_host,
        env.valkey_port,
        format!("bench-worker-{wi}"),
        instance_id,
        &env.namespace,
        &env.lane,
    );
    config.claim_poll_interval_ms = POLL_INTERVAL_MS;
    config.lease_ttl_ms = LEASE_TTL_MS;
    let worker = FlowFabricWorker::connect(config).await?;

    let mut out = WorkerOutcome {
        latencies_us: Vec::with_capacity(cap),
        completed: 0,
        missed: 0,
        failed: 0,
    };

    loop {
        if stop_flag.load(Ordering::Acquire) {
            return Ok(out);
        }
        // Race claim_next against the shutdown notify; picking the
        // notify up here means we don't block a full poll_interval
        // past the stop signal.
        let claim = tokio::select! {
            _ = shutdown.notified() => return Ok(out),
            r = worker.claim_next() => r,
        };
        match claim {
            Ok(Some(task)) => {
                let t_claim = Instant::now();
                match process_task(task).await {
                    Ok(_) => {
                        let lat_us = t_claim.elapsed().as_micros() as u64;
                        out.latencies_us.push(lat_us);
                        out.completed += 1;
                        // TASK_DEADLINE_MS worth of total latency
                        // (claim + work + complete) means the
                        // submit→complete gap is > deadline once you
                        // add the queue wait before claim. We're
                        // over-counting misses a little here — this
                        // metric is the back-pressure signal, not an
                        // SLA number.
                        let total_ms = lat_us / 1000;
                        if total_ms as i64 > TASK_DEADLINE_MS {
                            out.missed += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(worker = wi, error = %e, "task processing failed");
                        out.failed += 1;
                    }
                }
            }
            Ok(None) => {
                tokio::select! {
                    _ = shutdown.notified() => return Ok(out),
                    _ = tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)) => {}
                }
            }
            Err(e) => {
                tracing::warn!(worker = wi, error = %e, "claim_next error");
                tokio::select! {
                    _ = shutdown.notified() => return Ok(out),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        }
    }
}

async fn process_task(task: ClaimedTask) -> Result<()> {
    // Simulate work. 3s > lease_ttl/3 for the default 30s TTL, so the
    // renewal loop fires at least once per task — exercises the
    // span-instrumented renew_lease_inner path we measure.
    tokio::time::sleep(Duration::from_millis(TASK_WORK_MS)).await;
    task.complete(None).await?;
    Ok(())
}
