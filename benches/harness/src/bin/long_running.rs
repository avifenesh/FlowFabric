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
// Refill cadence tuned so drain roughly keeps pace: 100 workers at
// 3 s per task = ~33 ops/s; refilling ~20 tasks every 10 s adds
// ~2 ops/s to the backlog so some tasks miss the 60 s deadline
// (back-pressure signal) but most complete within it. Earlier
// refill (1000/10 s = 100 ops/s) over-saturated — queue grew
// unboundedly and every task missed, hiding the signal.
const REFILL_EVERY_MS: u64 = 10_000;
const REFILL_TOPUP: usize = 20;
const SHUTDOWN_GRACE_S: u64 = 30;

// RSS sampling interval. 10 s over a 300 s default run = 30 samples.
// 3-point start/mid/end can't distinguish sawtooth from monotonic
// leak; the series + slope detect both.
const RSS_SAMPLE_EVERY_MS: u64 = 10_000;

// Steady-state window size, seconds. Reported as
// `steady_state_throughput_ops_per_sec = completions_in_last_WINDOW /
// WINDOW`. The top-level `throughput_ops_per_sec` still reports
// whole-run for reference but steady-state is the headline number
// the consumer wants (cairn-rs doesn't care how fast you drain a
// pre-warmed queue on startup).
const STEADY_STATE_WINDOW_S: u64 = 60;

// Payload layout: first 16 bytes are an ASCII-hex-encoded
// little-endian i64 Unix-ms timestamp stamped at seed time.
// ASCII-hex rather than raw bytes because ff-server's
// create_execution path passes input_payload through
// `String::from_utf8_lossy` (server.rs:505) which replaces any
// non-UTF-8 byte with U+FFFD — a binary `submit_ms.to_le_bytes()`
// would be silently corrupted. Workers decode 16 hex chars →
// Unix-ms at claim time. Remaining bytes are random filler to hit
// the documented PAYLOAD_BYTES wire size.
const PAYLOAD_HEADER_BYTES: usize = 16;

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
    /// Number of independent samples to run. Each sample is a full
    /// `duration`-second run preceded by a Valkey FLUSHALL so residual
    /// queue state from the previous sample does not pollute the
    /// miss-rate. With `samples >= 2`, the reported
    /// `missed_deadline_pct` is the mean across samples; the JSON
    /// report additionally carries the per-sample array and stddev.
    ///
    /// Default is 1 to preserve existing single-sample behavior for
    /// smoke tests; release-gate runs should use N >= 5 (see
    /// `benches/results/baseline.md`) because the scenario is bimodal
    /// across the 10s refill / 60s deadline boundary and single-run
    /// variance covers 1.98%-7.04% on the reference host class (see
    /// `rfcs/drafts/scenario-4-regression-investigation.md`).
    #[arg(long, default_value_t = 1)]
    samples: usize,
    /// Where to write the JSON report (relative to cwd / repo root).
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

/// Per-sample aggregate, collected across samples when `--samples > 1`.
struct SampleResult {
    completed: u64,
    failed: u64,
    missed: u64,
    missed_pct: f64,
    throughput: f64,
    steady_state_throughput: f64,
    steady_state_window_completions: u64,
    p50: f64,
    p95: f64,
    p99: f64,
    renewal_count: u64,
    renewal_total_ns: u64,
    renewal_overhead_pct: f64,
    rss_start_mb: Option<u64>,
    rss_end_mb: Option<u64>,
    rss_min_mb: Option<u64>,
    rss_max_mb: Option<u64>,
    rss_slope_mb_per_min: Option<f64>,
    rss_growth_pct: Option<f64>,
    rss_series: Vec<[u64; 2]>,
    termination_reason: &'static str,
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
        if let Some(span) = ctx.span(id)
            && Self::is_renewal(span.metadata())
        {
            let mut guard = self.active.lock().expect("renewal mutex");
            if let Some(state) = guard.get_mut(id) {
                state.last_enter = Instant::now();
            }
        }
    }

    fn on_exit(
        &self,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span) = ctx.span(id)
            && Self::is_renewal(span.metadata())
        {
            let mut guard = self.active.lock().expect("renewal mutex");
            if let Some(state) = guard.get_mut(id) {
                state.busy_ns += state.last_enter.elapsed().as_nanos() as u64;
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
    /// claim-to-complete latency samples, microseconds. One entry per
    /// completed task.
    latencies_us: Vec<u64>,
    /// Completion timestamps (Unix-ms). Parallel to `latencies_us`;
    /// used at aggregation time to bucket completions into the
    /// steady-state window for honest throughput reporting.
    completion_ms: Vec<i64>,
    completed: u64,
    /// Tasks whose (now_complete - submit_ms) exceeded
    /// TASK_DEADLINE_MS. Computed from the payload-header submit
    /// timestamp — the real submit-to-complete primitive, not the
    /// claim-to-complete undercount.
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

    let samples = args.samples.max(1);
    tracing::info!(
        duration_s = args.duration,
        workers = args.workers,
        seed = args.seed,
        samples,
        "scenario 3 starting"
    );

    let http = ff_bench::workload::http_client()?;

    let mut results: Vec<SampleResult> = Vec::with_capacity(samples);
    for sample_idx in 0..samples {
        // Between-sample reset: FLUSHALL residual queue + reset
        // per-process renewal counters. The first sample inherits
        // whatever state the operator left behind (matches existing
        // single-sample protocol); subsequent samples get a clean
        // slate so miss-rate is per-sample, not cumulative.
        if sample_idx > 0 {
            if let Err(e) = flush_valkey(&env).await {
                tracing::warn!(
                    sample_idx,
                    error = %e,
                    "FLUSHALL between samples failed; sample may inherit stale queue state",
                );
            }
            renewal_counters.count.store(0, Ordering::Relaxed);
            renewal_counters.total_ns.store(0, Ordering::Relaxed);
        }
        tracing::info!(sample_idx, samples, "starting sample");
        let sample = run_sample(&args, &env, &http, &renewal_counters).await?;
        tracing::info!(
            sample_idx,
            completed = sample.completed,
            missed = sample.missed,
            missed_pct = sample.missed_pct,
            "sample complete",
        );
        results.push(sample);
    }

    // Aggregate across samples (mean + stddev on the key metrics).
    let agg = aggregate_samples(&results);

    // Write a single report. For multi-sample runs, the top-level
    // `throughput_ops_per_sec` + latency percentiles are the mean
    // across samples, and the config block carries the per-sample
    // arrays + stddev. Single-sample runs emit the legacy shape.
    write_aggregate_report(&args, &env, &results, &agg)?;

    let first = &results[0];
    if first.failed > 0 && first.completed == 0 {
        anyhow::bail!("all workers reported failures — server likely unavailable");
    }
    Ok(())
}

/// Mean + stddev + min/max for the multi-sample headline metrics.
#[derive(Debug)]
struct Aggregate {
    missed_pct_mean: f64,
    missed_pct_stddev: f64,
    missed_pct_min: f64,
    missed_pct_max: f64,
    throughput_mean: f64,
    steady_state_throughput_mean: f64,
    p50_mean: f64,
    p95_mean: f64,
    p99_mean: f64,
}

fn aggregate_samples(results: &[SampleResult]) -> Aggregate {
    fn mean(xs: &[f64]) -> f64 {
        if xs.is_empty() {
            0.0
        } else {
            xs.iter().sum::<f64>() / xs.len() as f64
        }
    }
    // Sample stddev (n-1 denominator); with N=1 returns 0.0.
    fn stddev(xs: &[f64]) -> f64 {
        if xs.len() < 2 {
            return 0.0;
        }
        let m = mean(xs);
        let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
        var.sqrt()
    }
    let missed: Vec<f64> = results.iter().map(|r| r.missed_pct).collect();
    let tput: Vec<f64> = results.iter().map(|r| r.throughput).collect();
    let ss_tput: Vec<f64> =
        results.iter().map(|r| r.steady_state_throughput).collect();
    let p50s: Vec<f64> = results.iter().map(|r| r.p50).collect();
    let p95s: Vec<f64> = results.iter().map(|r| r.p95).collect();
    let p99s: Vec<f64> = results.iter().map(|r| r.p99).collect();
    Aggregate {
        missed_pct_mean: mean(&missed),
        missed_pct_stddev: stddev(&missed),
        missed_pct_min: missed.iter().cloned().fold(f64::INFINITY, f64::min),
        missed_pct_max: missed.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        throughput_mean: mean(&tput),
        steady_state_throughput_mean: mean(&ss_tput),
        p50_mean: mean(&p50s),
        p95_mean: mean(&p95s),
        p99_mean: mean(&p99s),
    }
}

fn write_aggregate_report(
    args: &Args,
    env: &ff_bench::workload::BenchEnv,
    results: &[SampleResult],
    agg: &Aggregate,
) -> Result<()> {
    let samples = results.len();
    // For single-sample runs, preserve the legacy report shape
    // (scalar fields, no `_samples` arrays) so downstream consumers
    // that post-date this change but predate multi-sample see the
    // same JSON. For multi-sample runs, headline scalar fields
    // carry the mean, and `_samples` arrays + stddev let consumers
    // re-aggregate.
    let first = &results[0];
    let mut config = serde_json::json!({
        "duration_s": args.duration,
        "samples": samples,
        "workers": args.workers,
        "seed": args.seed,
        "refill_topup_per_interval": REFILL_TOPUP,
        "refill_interval_ms": REFILL_EVERY_MS,
        "payload_bytes": PAYLOAD_BYTES,
        "deadline_per_task_ms": TASK_DEADLINE_MS,
        "task_work_ms": TASK_WORK_MS,
        "termination_reason": first.termination_reason,
        "completed": first.completed,
        "failed": first.failed,
        "missed_deadline_count": first.missed,
        "missed_deadline_pct": if samples > 1 { agg.missed_pct_mean } else { first.missed_pct },
        "missed_deadline_primitive": "submit_to_complete",
        "steady_state_throughput_ops_per_sec": if samples > 1 {
            agg.steady_state_throughput_mean
        } else {
            first.steady_state_throughput
        },
        "steady_state_window_s": STEADY_STATE_WINDOW_S,
        "steady_state_window_completions": first.steady_state_window_completions,
        "lease_renewal_count": first.renewal_count,
        "lease_renewal_total_ns": first.renewal_total_ns,
        "lease_renewal_overhead_pct": first.renewal_overhead_pct,
        "lease_renewal_overhead_method": "span_enter_exit_measured",
        "rss_start_mb": first.rss_start_mb,
        "rss_end_mb": first.rss_end_mb,
        "rss_growth_pct": first.rss_growth_pct,
        "rss_sample_every_ms": RSS_SAMPLE_EVERY_MS,
        "rss_series": first.rss_series,
        "rss_min_mb": first.rss_min_mb,
        "rss_max_mb": first.rss_max_mb,
        "rss_slope_mb_per_min": first.rss_slope_mb_per_min,
    });
    if samples > 1 {
        let obj = config.as_object_mut().expect("config is object");
        obj.insert(
            "missed_deadline_pct_mean".into(),
            serde_json::json!(agg.missed_pct_mean),
        );
        obj.insert(
            "missed_deadline_pct_stddev".into(),
            serde_json::json!(agg.missed_pct_stddev),
        );
        obj.insert(
            "missed_deadline_pct_min".into(),
            serde_json::json!(agg.missed_pct_min),
        );
        obj.insert(
            "missed_deadline_pct_max".into(),
            serde_json::json!(agg.missed_pct_max),
        );
        obj.insert(
            "missed_deadline_pct_samples".into(),
            serde_json::json!(results.iter().map(|r| r.missed_pct).collect::<Vec<_>>()),
        );
        obj.insert(
            "completed_samples".into(),
            serde_json::json!(results.iter().map(|r| r.completed).collect::<Vec<_>>()),
        );
        obj.insert(
            "missed_count_samples".into(),
            serde_json::json!(results.iter().map(|r| r.missed).collect::<Vec<_>>()),
        );
        obj.insert(
            "steady_state_throughput_ops_per_sec_mean".into(),
            serde_json::json!(agg.steady_state_throughput_mean),
        );
        obj.insert(
            "steady_state_throughput_ops_per_sec_samples".into(),
            serde_json::json!(results
                .iter()
                .map(|r| r.steady_state_throughput)
                .collect::<Vec<_>>()),
        );
    }
    let headline_throughput = if samples > 1 { agg.throughput_mean } else { first.throughput };
    let headline_latency = if samples > 1 {
        LatencyMs { p50: agg.p50_mean, p95: agg.p95_mean, p99: agg.p99_mean }
    } else {
        LatencyMs { p50: first.p50, p95: first.p95, p99: first.p99 }
    };
    let mut report = Report::fill_env(
        SCENARIO,
        SYSTEM_FLOWFABRIC,
        env.cluster,
        config,
        headline_throughput,
        headline_latency,
    );
    let base_notes = "steady_state_throughput_ops_per_sec is the headline; \
         whole-run throughput_ops_per_sec is burst-dominated by the \
         pre-seeded queue drain. missed_deadline_pct is \
         submit-to-complete; refill batches cause per-batch spikes \
         because every task in a batch shares `submit_ms`, so the \
         rate smooths only over windows >> refill_interval_ms. \
         lease renewal overhead measured via span enter/exit — real \
         under steady-state load, not a quiescent calibration.";
    let mut notes = if samples > 1 {
        format!(
            "{base_notes} multi-sample run: N={samples}, \
             missed_deadline_pct reported as mean across samples \
             (stddev in config); Valkey FLUSHALL between samples. \
             Headline scalar fields are per-sample means; \
             per-sample arrays are in config.",
        )
    } else {
        String::from(base_notes)
    };
    if first.termination_reason != "duration_elapsed" {
        notes.push_str(&format!(
            " terminated early on first sample: {}; metrics cover partial run.",
            first.termination_reason,
        ));
    }
    report.notes = Some(notes);
    let dir = resolve_results_dir(&args.results_dir);
    match write_report(&report, &dir) {
        Ok(path) => println!("[bench] wrote {}", path.display()),
        Err(e) => eprintln!("[bench] WARN: could not write report: {e}"),
    }
    Ok(())
}

/// Send `FLUSHALL` to Valkey via a raw RESP TCP round-trip. Used
/// between samples to clear residual queue state; matches the
/// operator-level "`valkey-cli FLUSHALL` before each run" protocol
/// documented in `rfcs/drafts/scenario-4-regression-investigation.md`.
async fn flush_valkey(env: &ff_bench::workload::BenchEnv) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    let addr = format!("{}:{}", env.valkey_host, env.valkey_port);
    let mut stream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&addr),
    )
    .await
    .context("FLUSHALL connect timeout")?
    .context("FLUSHALL connect")?;
    stream
        .write_all(b"*1\r\n$8\r\nFLUSHALL\r\n")
        .await
        .context("FLUSHALL write")?;
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .context("FLUSHALL read timeout")?
        .context("FLUSHALL read")?;
    let reply = std::str::from_utf8(&buf[..n]).unwrap_or("<non-utf8>");
    if !reply.starts_with("+OK") {
        anyhow::bail!("FLUSHALL unexpected reply: {reply:?}");
    }
    Ok(())
}

/// Run a single sample of the scenario: seed, spawn workers, drive
/// for `args.duration` seconds, drain, aggregate.
async fn run_sample(
    args: &Args,
    env: &ff_bench::workload::BenchEnv,
    http: &Client,
    renewal_counters: &Arc<RenewalCounters>,
) -> Result<SampleResult> {
    // Pre-seed the queue before spinning up workers so the first claim
    // attempts all succeed. Failures at this stage are usually "server
    // down" — fail loud rather than silently measuring an empty run.
    let seed_start = Instant::now();
    seed_tasks(http, env, args.seed).await?;
    tracing::info!(
        seeded = args.seed,
        seed_wall_ms = seed_start.elapsed().as_millis() as u64,
        "seeded initial backlog"
    );

    // Shared state.
    let shutdown = Arc::new(Notify::new());
    let stop_flag = Arc::new(AtomicBool::new(false));
    let outcomes: Arc<Mutex<Vec<WorkerOutcome>>> = Arc::new(Mutex::new(Vec::new()));
    let outcomes_cap_per_worker = estimated_tasks_per_worker(args);

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

    // RSS time-series sampler. 3-point start/mid/end can't
    // distinguish sawtooth from monotonic leak — a linear-slope fit
    // over a dense series is the honest leak signal. One sample
    // every RSS_SAMPLE_EVERY_MS into a Vec<(t_s_from_start, rss_mb)>
    // owned by the sampler; swapped out to the aggregator at
    // shutdown.
    let rss_series_cell: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let rss_series_cell_clone = rss_series_cell.clone();
    let rss_series_shutdown = shutdown.clone();
    let rss_series_start = drain_start;
    let rss_series = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(RSS_SAMPLE_EVERY_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = rss_series_shutdown.notified() => return,
                _ = ticker.tick() => {
                    if let Some(mb) = read_rss_mb() {
                        let t_s = rss_series_start.elapsed().as_secs();
                        rss_series_cell_clone.lock().await.push((t_s, mb));
                    }
                }
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
    rss_series.abort();
    let _ = rss_series.await;
    let _ = refiller.await;

    let wall = drain_start.elapsed();
    let rss_end_mb = read_rss_mb();
    // Snapshot the sampler's series + append a final end-of-run
    // sample so the last data point matches the end-of-wall.
    let mut rss_series_vec = rss_series_cell.lock().await.clone();
    if let Some(end_mb) = rss_end_mb {
        rss_series_vec.push((wall.as_secs(), end_mb));
    }
    let (rss_min_mb, rss_max_mb, rss_slope_mb_per_min) =
        rss_summary(rss_start_mb, &rss_series_vec);

    // Aggregate worker outcomes.
    let outcomes = outcomes.lock().await;
    let mut all_latencies: Vec<u64> = Vec::new();
    let mut all_completion_ms: Vec<i64> = Vec::new();
    let mut completed: u64 = 0;
    let mut missed: u64 = 0;
    let mut failed: u64 = 0;
    for o in outcomes.iter() {
        all_latencies.extend(&o.latencies_us);
        all_completion_ms.extend(&o.completion_ms);
        completed += o.completed;
        missed += o.missed;
        failed += o.failed;
    }
    drop(outcomes);

    // Whole-run throughput — kept for reference but NOT the headline
    // under a pre-seeded queue shape. Default 300 s run drains a
    // 10 000-task pre-seeded queue in the first minutes and the
    // refiller then keeps it topped up; whole-run throughput is
    // therefore burst-dominated. Use `steady_state_throughput` below
    // as the headline, and see the notes field for the caveat.
    let throughput = if wall.as_secs_f64() > 0.0 {
        completed as f64 / wall.as_secs_f64()
    } else {
        0.0
    };

    // Steady-state throughput: count completions in the final
    // STEADY_STATE_WINDOW_S seconds of the run and divide by the
    // window. Honest primitive for "how fast does the engine churn
    // under sustained load". Requires wall > window; shorter runs
    // (smoke tests) fall back to whole-run throughput and the notes
    // field explains.
    let window_ms = (STEADY_STATE_WINDOW_S * 1_000) as i64;
    let run_end_ms = ff_bench::workload::now_ms();
    let window_start_ms = run_end_ms - window_ms;
    let in_window = all_completion_ms
        .iter()
        .filter(|&&ts| ts >= window_start_ms)
        .count() as u64;
    let steady_state_throughput = if wall.as_secs() >= STEADY_STATE_WINDOW_S {
        in_window as f64 / STEADY_STATE_WINDOW_S as f64
    } else {
        throughput
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
        steady_state_throughput_ops_s = steady_state_throughput,
        p50_ms = latency.p50,
        p95_ms = latency.p95,
        p99_ms = latency.p99,
        renewal_count,
        renewal_overhead_pct,
        rss_start_mb = ?rss_start_mb,
        rss_end_mb = ?rss_end_mb,
        rss_min_mb,
        rss_max_mb,
        rss_slope_mb_per_min,
        "scenario 3 complete"
    );

    let rss_growth_pct = match (rss_start_mb, rss_end_mb) {
        (Some(s), Some(e)) if s > 0 => Some(((e as f64 - s as f64) / s as f64) * 100.0),
        _ => None,
    };
    // Serialize the RSS series as an array of [t_s, mb] 2-tuples so
    // JSON consumers (check_release.py) can plot a line without
    // reshaping.
    let rss_series_json: Vec<[u64; 2]> =
        rss_series_vec.iter().map(|&(t, mb)| [t, mb]).collect();

    Ok(SampleResult {
        completed,
        failed,
        missed,
        missed_pct,
        throughput,
        steady_state_throughput,
        steady_state_window_completions: in_window,
        p50: latency.p50,
        p95: latency.p95,
        p99: latency.p99,
        renewal_count,
        renewal_total_ns,
        renewal_overhead_pct,
        rss_start_mb,
        rss_end_mb,
        rss_min_mb,
        rss_max_mb,
        rss_slope_mb_per_min,
        rss_growth_pct,
        rss_series: rss_series_json,
        termination_reason,
    })
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Compact summary of the RSS time series: (min, max, slope).
///
/// `slope_mb_per_min` is a least-squares linear fit across
/// (t_seconds, rss_mb) — positive means monotonic growth (leak
/// signal), near-zero means steady. `start_mb` seeds the series at
/// t=0 so a short run with one-sample series still produces a
/// meaningful slope.
fn rss_summary(
    start_mb: Option<u64>,
    series: &[(u64, u64)],
) -> (Option<u64>, Option<u64>, Option<f64>) {
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(series.len() + 1);
    if let Some(s) = start_mb {
        pts.push((0.0, s as f64));
    }
    for &(t, mb) in series {
        pts.push((t as f64, mb as f64));
    }
    if pts.is_empty() {
        return (None, None, None);
    }
    let min = pts.iter().map(|&(_, y)| y as u64).min();
    let max = pts.iter().map(|&(_, y)| y as u64).max();
    let slope = if pts.len() < 2 {
        Some(0.0)
    } else {
        let n = pts.len() as f64;
        let sum_x: f64 = pts.iter().map(|&(x, _)| x).sum();
        let sum_y: f64 = pts.iter().map(|&(_, y)| y).sum();
        let sum_xy: f64 = pts.iter().map(|&(x, y)| x * y).sum();
        let sum_xx: f64 = pts.iter().map(|&(x, _)| x * x).sum();
        let denom = n * sum_xx - sum_x * sum_x;
        if denom.abs() < 1e-9 {
            None
        } else {
            // per-second slope → per-minute
            let per_s = (n * sum_xy - sum_x * sum_y) / denom;
            Some(per_s * 60.0)
        }
    };
    (min, max, slope)
}

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
    // Filler body (static shared across tasks). The header is
    // per-task and stamped inside the spawned submitter so each
    // job carries its own submit timestamp. Pre-compute the
    // filler once so N parallel submitters aren't racing on
    // filler generation.
    let filler = ff_bench::workload::filler_payload(PAYLOAD_BYTES - PAYLOAD_HEADER_BYTES);
    let filler = Arc::new(filler);
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .context("semaphore closed")?;
        let client = client.clone();
        let env = env.clone();
        let filler = filler.clone();
        handles.push(tokio::spawn(async move {
            // Stamp submit_ms in the first 16 bytes as ASCII-hex so
            // workers can read it without a separate server
            // round-trip — see PAYLOAD_HEADER_BYTES comment for why
            // hex and not raw bytes.
            let submit_ms = ff_bench::workload::now_ms();
            let header = format!("{:016x}", submit_ms as u64);
            debug_assert_eq!(header.len(), PAYLOAD_HEADER_BYTES);
            let mut payload = Vec::with_capacity(PAYLOAD_BYTES);
            payload.extend_from_slice(header.as_bytes());
            payload.extend_from_slice(&filler);
            debug_assert_eq!(payload.len(), PAYLOAD_BYTES);
            let deadline = submit_ms + TASK_DEADLINE_MS;
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
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(
            env.valkey_host.clone(),
            env.valkey_port,
        ),
        worker_id: ff_core::types::WorkerId::new(format!("bench-worker-{wi}")),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(instance_id),
        namespace: ff_core::types::Namespace::new(&env.namespace),
        lanes: vec![ff_core::types::LaneId::new(&env.lane)],
        capabilities: Vec::new(),
        lease_ttl_ms: LEASE_TTL_MS,
        claim_poll_interval_ms: POLL_INTERVAL_MS,
        max_concurrent_tasks: 1,
    partition_config: None,
    };
    let worker = FlowFabricWorker::connect(config).await?;

    let mut out = WorkerOutcome {
        latencies_us: Vec::with_capacity(cap),
        completion_ms: Vec::with_capacity(cap),
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
                // Read submit timestamp from the payload header
                // BEFORE the task is moved into process_task. The
                // header is an 8-byte little-endian i64 Unix-ms
                // stamped by seed_tasks — no server round-trip
                // needed to resolve deadline state.
                let submit_ms = parse_submit_ms(task.input_payload());
                let t_claim = Instant::now();
                match process_task(task).await {
                    Ok(_) => {
                        let lat_us = t_claim.elapsed().as_micros() as u64;
                        let complete_ms = ff_bench::workload::now_ms();
                        out.latencies_us.push(lat_us);
                        out.completion_ms.push(complete_ms);
                        out.completed += 1;
                        // Submit-to-complete is the honest deadline
                        // primitive: queue wait + claim + work +
                        // complete. Under steady-state the queue
                        // wait dominates; measuring only
                        // claim-to-complete (as the earlier draft
                        // did) artificially suppressed the miss
                        // rate because no task missed the 60 s
                        // deadline on claim-to-complete alone
                        // (claim-to-complete ~= work_ms = 3 s).
                        if let Some(submit_ms) = submit_ms
                            && complete_ms - submit_ms > TASK_DEADLINE_MS
                        {
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

/// Parse the 16-ASCII-hex-char submit-timestamp header. See
/// PAYLOAD_HEADER_BYTES comment for the encoding rationale
/// (server collapses non-UTF-8 bytes to U+FFFD). Returns None on
/// any shape / encoding mismatch — None widens the miss-rate
/// undercount rather than crashing a bench mid-run.
fn parse_submit_ms(payload: &[u8]) -> Option<i64> {
    if payload.len() < PAYLOAD_HEADER_BYTES {
        return None;
    }
    let hex = std::str::from_utf8(&payload[..PAYLOAD_HEADER_BYTES]).ok()?;
    u64::from_str_radix(hex, 16).ok().map(|v| v as i64)
}

async fn process_task(task: ClaimedTask) -> Result<()> {
    // Simulate work. 3s > lease_ttl/3 for the default 30s TTL, so the
    // renewal loop fires at least once per task — exercises the
    // span-instrumented renew_lease_inner path we measure.
    tokio::time::sleep(Duration::from_millis(TASK_WORK_MS)).await;
    task.complete(None).await?;
    Ok(())
}
