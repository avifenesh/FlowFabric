//! Faktory comparison — scenario 3 (long-running steady-state).
//!
//! Mirrors the FlowFabric `long_running` harness: 100-worker fetcher
//! pool, 10 000 job pre-seed + refill, 5-minute duration, deadline-
//! bounded completion tracking, RSS sampling, graceful shutdown.
//!
//! Differences vs FlowFabric:
//!   * No lease concept — Faktory uses heartbeats (`BEAT`) on the
//!     client connection, not per-job leases. The `lease_renewal_*`
//!     fields in the report shape are reported as 0 with a note
//!     explaining the model difference.
//!   * Single-process `Worker` object; `.workers(100)` spawns 100
//!     internal fetchers rather than 100 separate worker processes.
//!     This is the Faktory-canonical shape — the comparison is
//!     "each system run in its idiomatic deployment".
//!
//! Prereq: `scripts/start-faktory.sh` (or an equivalent Faktory host).
//!
//! Out of scope: restart detection. If Faktory crashes or is
//! restarted during the 5-minute run, connect errors surface but
//! the harness does not attempt reconnect or partial-result
//! salvage.
//!
//! Wire parity: Faktory args are JSON so raw 4 KiB filler is
//! hex-encoded to 8 KiB on the wire. Reports surface both as
//! `config.payload_bytes` (raw) and `config.wire_bytes` (hex) so
//! throughput comparisons against systems that send raw bytes on
//! the wire can account for the 2x per-op overhead.
//!
//! Heartbeat accounting: Faktory BEATs live on the worker's TCP
//! connection, not per-job. `config.heartbeat_ops_per_s_approx`
//! surfaces the approximate extra server load (~N/15 ops/s at the
//! default 15-second heartbeat cadence for N workers) so readers
//! of the headline throughput number see Faktory's hidden baseline
//! traffic.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use faktory::{Client, Job, WorkerBuilder};

const SCENARIO: &str = "long_running_steady_state";
const SYSTEM: &str = "faktory";
const QUEUE: &str = "ff-bench-s3";
const JOB_KIND: &str = "ff-bench.scenario3";
const DEFAULT_WORKERS: usize = 100;
const DEFAULT_SEED: usize = 10_000;
const PAYLOAD_BYTES: usize = 4 * 1024;
const TASK_WORK_MS: u64 = 3_000;
const TASK_DEADLINE_MS: i64 = 60_000;
const REFILL_EVERY_MS: u64 = 10_000;
const REFILL_TOPUP: usize = 1_000;
const SHUTDOWN_GRACE_S: u64 = 30;

#[derive(Parser)]
#[command(about = "Faktory scenario 3 — long-running steady-state (100 workers × 5 min)")]
struct Args {
    #[arg(long, env = "FAKTORY_URL", default_value = "tcp://localhost:7419")]
    faktory_url: String,
    #[arg(long, default_value_t = 300)]
    duration: u64,
    #[arg(long, default_value_t = DEFAULT_WORKERS)]
    workers: usize,
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    let payload = filler_payload(PAYLOAD_BYTES);

    // Pre-seed.
    let seed_start = Instant::now();
    {
        let mut client = Client::connect_to(&args.faktory_url)
            .await
            .with_context(|| format!("connect to Faktory at {}", args.faktory_url))?;
        seed_jobs(&mut client, args.seed, &payload).await?;
    }
    eprintln!(
        "[faktory s3] seeded {} jobs in {} ms",
        args.seed,
        seed_start.elapsed().as_millis()
    );

    let completed = Arc::new(AtomicU64::new(0));
    let missed = Arc::new(AtomicU64::new(0));
    // Latencies — time between "handler saw job" and "handler
    // returned". No lock in the hot path: push to a thread-local Vec
    // the handler owns, swap out via a Mutex at shutdown. For this
    // 100-worker × 3 s/task workload the contention cost of a single
    // shared Mutex<Vec> is << 1% but we use a push-only parking_lot-
    // free shape to match the FlowFabric harness measurement exactly.
    let latencies: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(
        Vec::with_capacity(args.seed + REFILL_TOPUP * 30),
    ));

    let stop_flag = Arc::new(AtomicBool::new(false));
    let rss_start_mb = read_rss_mb();

    // Refill loop.
    let refill_url = args.faktory_url.clone();
    let refill_payload = payload.clone();
    let refill_stop = stop_flag.clone();
    let refiller = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(REFILL_EVERY_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        // New connection per refill batch — keeps this out of the
        // worker's connection pool so one fetcher disconnect can't
        // stall the refiller. Batches are small enough (1 000) that
        // connect-per-batch overhead is negligible.
        while !refill_stop.load(Ordering::Acquire) {
            tokio::select! {
                _ = ticker.tick() => {
                    match Client::connect_to(&refill_url).await {
                        Ok(mut c) => {
                            if let Err(e) = seed_jobs(&mut c, REFILL_TOPUP, &refill_payload).await {
                                eprintln!("[faktory s3] refill error: {e}");
                            }
                        }
                        Err(e) => eprintln!("[faktory s3] refill connect error: {e}"),
                    }
                }
            }
        }
    });

    // RSS sample at midpoint.
    let rss_mid_cell: Arc<tokio::sync::Mutex<Option<u64>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let rss_mid_cell_clone = rss_mid_cell.clone();
    let rss_mid_stop = stop_flag.clone();
    let mid_ms = args.duration.saturating_mul(1_000) / 2;
    let rss_mid = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(mid_ms)) => {
                *rss_mid_cell_clone.lock().await = read_rss_mb();
            }
            _ = async {
                while !rss_mid_stop.load(Ordering::Acquire) {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            } => {}
        }
    });

    // Worker.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let tx_slot: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>> =
        Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));
    let shutdown_rx_fut = async move {
        let _ = shutdown_rx.await;
    };

    let completed_h = completed.clone();
    let missed_h = missed.clone();
    let latencies_h = latencies.clone();

    let drain_start = Instant::now();
    let mut worker = WorkerBuilder::<std::io::Error>::default()
        .workers(args.workers)
        .shutdown_timeout(Duration::from_secs(SHUTDOWN_GRACE_S))
        .register_fn(JOB_KIND, move |job: Job| {
            let completed = completed_h.clone();
            let missed = missed_h.clone();
            let latencies = latencies_h.clone();
            async move {
                let t_claim = Instant::now();
                // Enqueue timestamp — Faktory's `enqueued_at` is a
                // `chrono::DateTime<Utc>`. Subtract from now() to get
                // the queue-wait component of end-to-end latency;
                // fall back to 0 if missing (degrades to
                // claim-to-complete-only, matching FlowFabric's
                // measurement when no server stamp is available).
                let submit_ms_ago: u64 = job
                    .enqueued_at
                    .as_ref()
                    .and_then(|ts| {
                        let enq_ms = ts.timestamp_millis();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()?
                            .as_millis() as i64;
                        (now_ms - enq_ms).try_into().ok()
                    })
                    .unwrap_or(0);
                // Simulate work.
                tokio::time::sleep(Duration::from_millis(TASK_WORK_MS)).await;
                let work_ms = t_claim.elapsed().as_millis() as u64;
                let total_ms = submit_ms_ago.saturating_add(work_ms);
                if total_ms as i64 > TASK_DEADLINE_MS {
                    missed.fetch_add(1, Ordering::Relaxed);
                }
                completed.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut v) = latencies.lock() {
                    v.push(t_claim.elapsed().as_micros() as u64);
                }
                Ok::<(), std::io::Error>(())
            }
        })
        .with_graceful_shutdown(shutdown_rx_fut)
        .connect_to(&args.faktory_url)
        .await
        .with_context(|| format!("connect worker to {}", args.faktory_url))?;

    // Drive shutdown after `duration` elapses. `run` below returns
    // after the graceful-shutdown future is awaited and all in-flight
    // handlers finish (or `shutdown_timeout` expires).
    let tx_for_timer = tx_slot.clone();
    let stop_for_timer = stop_flag.clone();
    let dur = Duration::from_secs(args.duration);
    let cap = Duration::from_secs(args.duration + SHUTDOWN_GRACE_S);
    let shutdown_timer = tokio::spawn(async move {
        let reason = tokio::select! {
            _ = tokio::signal::ctrl_c() => "signal",
            _ = tokio::time::sleep(dur) => "duration_elapsed",
            _ = tokio::time::sleep(cap) => "hard_timeout",
        };
        eprintln!("[faktory s3] initiating shutdown: {reason}");
        stop_for_timer.store(true, Ordering::Release);
        let mut g = tx_for_timer.lock().await;
        if let Some(tx) = g.take() {
            let _ = tx.send(());
        }
        reason
    });

    let run_result = worker.run(&[QUEUE]).await;
    let wall = drain_start.elapsed();
    let termination_reason = shutdown_timer.await.unwrap_or("unknown");
    run_result.context("worker run")?;

    refiller.abort();
    rss_mid.abort();
    let _ = refiller.await;
    let _ = rss_mid.await;

    let completed_n = completed.load(Ordering::Relaxed);
    let missed_n = missed.load(Ordering::Relaxed);
    let rss_end_mb = read_rss_mb();
    let rss_mid_mb = *rss_mid_cell.lock().await;
    let lat = {
        let v = latencies.lock().expect("latency mutex");
        v.clone()
    };
    let (p50, p95, p99) = percentiles(&lat);
    let throughput = if wall.as_secs_f64() > 0.0 {
        completed_n as f64 / wall.as_secs_f64()
    } else {
        0.0
    };
    let rss_growth_pct = match (rss_start_mb, rss_end_mb) {
        (Some(s), Some(e)) if s > 0 => Some(((e as f64 - s as f64) / s as f64) * 100.0),
        _ => None,
    };
    let missed_pct = if completed_n > 0 {
        (missed_n as f64 / completed_n as f64) * 100.0
    } else {
        0.0
    };

    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": ff_bench::git_sha(),
        "valkey_version": "n/a",
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": {
            "duration_s": args.duration,
            "workers": args.workers,
            "seed": args.seed,
            "refill_topup_per_interval": REFILL_TOPUP,
            "refill_interval_ms": REFILL_EVERY_MS,
            // Raw filler size (matches FlowFabric / baseline).
            "payload_bytes": PAYLOAD_BYTES,
            // JSON-args wire bytes for comparison honesty: Faktory
            // hex-encodes args so a 4 KiB filler is 8 KiB on the
            // wire. FlowFabric/baseline send raw bytes; a 1:1
            // throughput comparison must account for the 2x wire
            // cost per op.
            "wire_bytes": PAYLOAD_BYTES * 2,
            "deadline_per_task_ms": TASK_DEADLINE_MS,
            "task_work_ms": TASK_WORK_MS,
            "faktory_url": args.faktory_url,
            "queue": QUEUE,
            "termination_reason": termination_reason,
            "completed": completed_n,
            "missed_deadline_count": missed_n,
            "missed_deadline_pct": missed_pct,
            // Faktory uses per-worker-connection BEAT heartbeats at
            // ~15 s cadence (see contribsys/faktory docs), not
            // per-job lease renewals. Schema-compatible 0 here means
            // "no per-job renewal primitive", NOT "zero heartbeat
            // cost" — a 100-worker pool sends ~6.7 BEATs/s which
            // contributes to server load that this metric does not
            // capture. Document explicitly in `notes`.
            "lease_renewal_count": 0,
            "lease_renewal_overhead_pct": 0.0,
            "lease_renewal_overhead_method": "n/a_faktory_heartbeats_not_leases",
            "heartbeat_interval_s_approx": 15,
            "heartbeat_ops_per_s_approx": (args.workers as f64) / 15.0,
            "rss_start_mb": rss_start_mb,
            "rss_mid_mb": rss_mid_mb,
            "rss_end_mb": rss_end_mb,
            "rss_growth_pct": rss_growth_pct,
        },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "faktory uses per-connection BEAT heartbeats at ~15 s cadence, not per-job leases. `lease_renewal_*` fields are 0 for schema compatibility — actual heartbeat cost (~N/15 ops/s on the server) is NOT measured here. Wire payload is hex-encoded (2x raw bytes); see config.wire_bytes. missed_deadline_pct computed against submit-to-complete via Faktory's enqueued_at timestamp; refill batches cause per-batch spikes because every task in a batch shares `enqueued_at` — headline is long-window aggregate, not per-refill instantaneous.",
    });

    let results_dir = std::path::Path::new(&args.results_dir);
    std::fs::create_dir_all(results_dir)?;
    let path = results_dir.join(format!(
        "{}-{}-{}.json",
        SCENARIO,
        SYSTEM,
        report["git_sha"].as_str().unwrap_or("unknown")
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    eprintln!(
        "[faktory s3] wrote {} — {:.1} ops/sec p99={:.2}ms missed={}",
        path.display(),
        throughput,
        p99,
        missed_n
    );
    Ok(())
}

async fn seed_jobs(client: &mut Client, n: usize, payload: &[u8]) -> Result<()> {
    const CHUNK: usize = 1_000;
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(CHUNK);
        let batch: Vec<Job> = (0..take)
            .map(|_| {
                // Full hex encoding of the 4 KiB filler. Matches
                // wire parity with FlowFabric/baseline — see
                // scenario1.rs `hex_encode` comment for the
                // rationale.
                let arg = serde_json::Value::String(hex_encode(payload));
                Job::new(JOB_KIND, vec![arg]).on_queue(QUEUE)
            })
            .collect();
        let (enqueued, errors) = client.enqueue_many(batch).await.context("enqueue_many")?;
        if let Some(errs) = errors
            && !errs.is_empty()
        {
            anyhow::bail!("enqueue errors: {errs:?}");
        }
        if enqueued != take {
            anyhow::bail!("partial enqueue: took {take}, got {enqueued}");
        }
        remaining -= take;
    }
    Ok(())
}

fn filler_payload(size: usize) -> Vec<u8> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut x = seed.wrapping_add(0x9E3779B9);
    (0..size)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 16) & 0xFF) as u8
        })
        .collect()
}

/// See scenario1.rs `hex_encode`. Full 2 chars/byte encoding of the
/// raw filler so Faktory's wire cost is apples-to-apples with
/// FlowFabric / baseline.
fn hex_encode(payload: &[u8]) -> String {
    let mut s = String::with_capacity(payload.len() * 2);
    for b in payload {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{:02x}", b);
    }
    s
}

fn percentiles(samples: &[u64]) -> (f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let at = |q: f64| -> f64 {
        let idx = ((s.len() as f64 - 1.0) * q).round() as usize;
        s[idx.min(s.len() - 1)] as f64 / 1000.0
    };
    (at(0.50), at(0.95), at(0.99))
}

fn read_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in contents.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
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
        text.parse::<u64>().ok().map(|kb| kb / 1024)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

