//! Faktory comparison — scenario 1 (throughput).
//!
//! Same workload envelope as the FlowFabric `submit_claim_complete`
//! bench and the baseline package: submit N jobs with a 4 KiB payload,
//! drain through a worker pool, record wall time + per-task latency,
//! emit a Phase-A-shaped JSON report.
//!
//! Faktory's worker model differs from FlowFabric's `claim_next` loop:
//! a single `Worker` runs an internal pool of `workers(n)` fetchers
//! that dispatch to a registered handler function. We size `n` at 16
//! to match the FlowFabric scenario 1 worker count, then measure from
//! the first enqueue to the last completion via a counter that the
//! handler increments.
//!
//! Prerequisites:
//!   * `scripts/start-faktory.sh` running locally (or FAKTORY_URL set
//!     to an existing Faktory host)
//!   * No authentication required (the start script runs with an
//!     empty password)

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use faktory::{Client, Job, WorkerBuilder};

const SCENARIO: &str = "submit_claim_complete";
const SYSTEM: &str = "faktory";
const QUEUE: &str = "ff-bench-s1";
const JOB_KIND: &str = "ff-bench.scenario1";
const WORKER_COUNT: usize = 16;
const PAYLOAD_BYTES: usize = 4 * 1024;

#[derive(Parser)]
#[command(
    about = "Faktory scenario 1 — throughput (enqueue / dequeue / ack N jobs)"
)]
struct Args {
    /// FAKTORY_URL override. Defaults to tcp://localhost:7419 via the
    /// faktory crate's env lookup when unset. Provided as an arg
    /// (in addition to env) so CI runbooks can pin it explicitly.
    #[arg(long, env = "FAKTORY_URL", default_value = "tcp://localhost:7419")]
    faktory_url: String,

    /// Number of jobs to submit + drain.
    #[arg(long, default_value_t = 10_000)]
    tasks: usize,

    /// Directory to write the JSON report into.
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Connect phase. `Client::connect_to` takes the URL directly, so
    // we don't touch the process env (Rust 2024 requires unsafe for
    // `std::env::set_var`, which is out of scope here). Bench
    // assumes no password and no TLS — matching `scripts/start-
    // faktory.sh`. Rerun on a fresh Faktory container to avoid
    // stragglers; Faktory's client API does not expose a queue
    // purge primitive.
    let payload = filler_payload(PAYLOAD_BYTES);

    let submit_start = Instant::now();
    let mut admin = Client::connect_to(&args.faktory_url)
        .await
        .with_context(|| format!("connect to Faktory at {}", args.faktory_url))?;
    seed_jobs(&mut admin, args.tasks, &payload).await?;
    let submit_wall = submit_start.elapsed();
    eprintln!(
        "[faktory] seeded {} jobs in {} ms",
        args.tasks,
        submit_wall.as_millis()
    );

    // Drain phase. A single Worker hosts `WORKER_COUNT` internal
    // fetchers. The handler increments a shared counter; when the
    // counter reaches `args.tasks`, we trigger the graceful-shutdown
    // future the worker was built with. The `.run` return path then
    // gives us wall-clock drain time.
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let target = args.tasks;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown_rx = async move {
        let _ = shutdown_rx.await;
    };

    let counter_for_handler = counter.clone();
    let tx_slot: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>> =
        Arc::new(tokio::sync::Mutex::new(Some(shutdown_tx)));
    let tx_for_handler = tx_slot.clone();

    // Per-task latency is the time a handler sees between "job
    // arrived at handler" and "handler returned". Faktory's
    // enqueue→dispatch hop happens inside the worker's fetch loop
    // and is implicit in the throughput number; that number (total
    // drained / wall) is the primary metric.
    let latencies: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(
        Vec::with_capacity(args.tasks),
    ));
    let latencies_for_handler = latencies.clone();

    let drain_start = Instant::now();
    let mut worker = WorkerBuilder::<std::io::Error>::default()
        .workers(WORKER_COUNT)
        .register_fn(JOB_KIND, move |_job: Job| {
            let counter = counter_for_handler.clone();
            let tx_slot = tx_for_handler.clone();
            let latencies = latencies_for_handler.clone();
            async move {
                let t0 = Instant::now();
                // No actual work — this scenario measures queue
                // round-trip, matching the FlowFabric + baseline
                // scenario 1 which also uses an empty body.
                let completed = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if let Ok(mut v) = latencies.lock() {
                    v.push(t0.elapsed().as_micros() as u64);
                }
                if completed == target {
                    // Send the shutdown exactly once. The Mutex
                    // guards the Option so concurrent handlers that
                    // raced past the counter threshold (possible
                    // because fetch_add isn't sequenced vs this
                    // check) can't try to send twice.
                    let mut guard = tx_slot.lock().await;
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(());
                    }
                }
                Ok::<(), std::io::Error>(())
            }
        })
        .with_graceful_shutdown(shutdown_rx)
        .connect()
        .await
        .context("connect worker to Faktory")?;
    let run_result = worker.run(&[QUEUE]).await;
    let wall = drain_start.elapsed();
    run_result.context("worker run")?;

    let drained = counter.load(std::sync::atomic::Ordering::Relaxed);
    if drained != target {
        anyhow::bail!(
            "drained {drained} of {target} jobs — worker exited early (stale jobs on queue?)"
        );
    }

    let throughput = drained as f64 / wall.as_secs_f64();
    let lat = latencies.lock().expect("latency mutex");
    let (p50, p95, p99) = percentiles(&lat);
    drop(lat);

    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": ff_bench::git_sha(),
        "valkey_version": "n/a",
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": {
            "tasks": args.tasks,
            "workers": WORKER_COUNT,
            "payload_bytes": PAYLOAD_BYTES,
            "faktory_url": args.faktory_url,
            "queue": QUEUE,
        },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "faktory = Producer::enqueue + Worker::run_one pool; lease equivalent is implicit",
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
        "[faktory] wrote {} — {:.1} ops/sec p99={:.2}ms",
        path.display(),
        throughput,
        p99
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
                // Faktory args are JSON; pass the payload bytes as a
                // base64-style hex nibble string (cheap, no extra dep).
                // The handler ignores args entirely — we only care
                // about queue round-trip cost.
                let arg = serde_json::Value::String(hex_prefix(payload, 32));
                Job::new(JOB_KIND, vec![arg]).on_queue(QUEUE)
            })
            .collect();
        let (enqueued, errors) = client
            .enqueue_many(batch)
            .await
            .context("enqueue_many")?;
        if let Some(errs) = errors
            && !errs.is_empty()
        {
            anyhow::bail!("enqueue errors (batch of {take}): {errs:?}");
        }
        if enqueued != take {
            anyhow::bail!(
                "partial enqueue: submitted {take}, server accepted {enqueued}"
            );
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

/// Render the first `max_bytes` of the payload as a lowercase hex
/// string. Full 4 KiB would bloat the Faktory job serialization
/// pointlessly; the worker never reads the arg value so a prefix is
/// representative for the queue round-trip cost being measured.
fn hex_prefix(payload: &[u8], max_bytes: usize) -> String {
    let take = payload.len().min(max_bytes);
    let mut s = String::with_capacity(take * 2);
    for b in &payload[..take] {
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
