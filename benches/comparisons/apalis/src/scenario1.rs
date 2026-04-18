//! apalis comparison — scenario 1.
//!
//! What a consumer picking apalis 1.0 (rc.7 at time of writing) gets
//! for the same submit → claim → complete workload ff-bench's scenario
//! 1 measures. apalis-redis is the closest equivalent of a Valkey-
//! backed job queue; we drive it via `TaskSink::push_bulk` and run a
//! single `WorkerBuilder` with concurrency.
//!
//! # What's directly comparable
//!
//! * Backend: apalis-redis speaks `redis` 1.x (same client apalis
//!   ships with), talking to the SAME Valkey instance ff-bench uses.
//! * Payload: 4 KiB blob, matches scenario 1's `PAYLOAD_BYTES`.
//! * Concurrency: 16 workers, matches `WORKER_COUNT`.
//! * Measurement: submit wall excluded from throughput; drain wall
//!   (first-claim-seen → last-complete-done) / N = ops/sec.
//!
//! # What's NOT comparable (documented in COMPARISON.md)
//!
//! * apalis has no lease concept; tasks are at-least-once via ack.
//!   ff-bench's scenario 1 is at-least-once with lease-backed
//!   fencing. The numbers measure the same workload end-to-end but
//!   apalis's lower overhead reflects missing features, not raw
//!   speed.
//! * apalis uses JSON codec by default; scenario 1's payload is raw
//!   bytes. The added JSON framing is on apalis's side of the
//!   measurement — fair, because that's what a consumer sees.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use apalis::prelude::*;
use apalis_redis::RedisStorage;
use clap::Parser;
use serde::{Deserialize, Serialize};

const SCENARIO: &str = "submit_claim_complete";
const SYSTEM: &str = "apalis";
const WORKER_CONCURRENCY: usize = 16;
const PAYLOAD_BYTES: usize = 4 * 1024;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FF_BENCH_VALKEY_HOST", default_value = "localhost")]
    valkey_host: String,
    #[arg(long, env = "FF_BENCH_VALKEY_PORT", default_value_t = 6379)]
    valkey_port: u16,
    #[arg(long, default_value_t = 10_000)]
    tasks: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

/// 4 KiB payload, serialized as JSON by apalis's default codec.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchJob {
    payload: Vec<u8>,
}

async fn noop_handler(_job: BenchJob) -> Result<(), BoxDynError> {
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let url = format!("redis://{}:{}/", args.valkey_host, args.valkey_port);

    // Seed phase — connect, push N tasks, measure wall separately from
    // the drain wall so throughput reflects steady-state claim/complete
    // cost (matching ff-bench scenario 1's convention).
    let conn = apalis_redis::connect(url.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("apalis_redis::connect: {e}"))?;
    let mut storage: RedisStorage<BenchJob> = RedisStorage::new(conn);

    let payload = filler_payload(PAYLOAD_BYTES);
    let submit_start = Instant::now();
    for _ in 0..args.tasks {
        storage
            .push(BenchJob {
                payload: payload.clone(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("apalis push: {e}"))?;
    }
    let submit_wall = submit_start.elapsed();
    eprintln!(
        "[apalis-s1] seeded {} tasks in {} ms",
        args.tasks,
        submit_wall.as_millis()
    );

    // Drain phase — one WorkerBuilder with 16-way concurrency. apalis's
    // `Monitor` drives the worker to completion; we stop it once every
    // task has been acknowledged via a counter.
    let drained = Arc::new(AtomicUsize::new(0));
    let target = args.tasks;

    let drain_start = Instant::now();

    // Shared tokio::Mutex<Vec<u64>> for per-job latency. Under
    // WORKER_CONCURRENCY=16 the lock-contention bias is measurable but
    // matches apalis's natural concurrency model; a per-worker sink
    // requires a different WorkerBuilder shape that's not in the hot
    // documented path.
    let latencies: Arc<tokio::sync::Mutex<Vec<u64>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(target)));

    let drained_clone = drained.clone();
    let latencies_clone = latencies.clone();
    let worker = WorkerBuilder::new("ff-bench-s1")
        .backend(storage.clone())
        .concurrency(WORKER_CONCURRENCY)
        .build(move |job: BenchJob| {
            let drained = drained_clone.clone();
            let latencies = latencies_clone.clone();
            async move {
                let t0 = Instant::now();
                noop_handler(job).await?;
                let lat = t0.elapsed().as_micros() as u64;
                latencies.lock().await.push(lat);
                drained.fetch_add(1, Ordering::Relaxed);
                Ok::<(), BoxDynError>(())
            }
        });

    // Run worker + driver side-by-side via tokio::select!, avoiding
    // a tokio::spawn that would require the worker's Future to be
    // 'static + Send. The select! arms borrow the outer scope
    // locally, which sidesteps the HRTB issue hit by run_until
    // inside a spawn.
    use apalis::prelude::WorkerError;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let drained_watch = drained.clone();
    let driver = async {
        loop {
            if drained_watch.load(Ordering::Relaxed) >= target {
                let _ = shutdown_tx.send(());
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let signal = async {
        let _ = shutdown_rx.await;
        Ok::<(), WorkerError>(())
    };
    let worker_fut = worker.run_until(signal);
    let (_driver_res, _worker_res) = tokio::join!(driver, worker_fut);
    let wall = drain_start.elapsed();
    let lat = latencies.lock().await.clone();
    let throughput = args.tasks as f64 / wall.as_secs_f64();
    let (p50, p95, p99) = percentiles(&lat);

    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": ff_bench::git_sha(),
        "valkey_version": ff_bench::valkey_version(),
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": {
            "tasks": args.tasks,
            "workers": WORKER_CONCURRENCY,
            "payload_bytes": PAYLOAD_BYTES,
            "apalis_version": "1.0.0-rc.7",
        },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "apalis 1.0.0-rc.7 + apalis-redis; default JSON codec; no lease-backed fencing (at-least-once via ack, not at-most-once). Directly comparable to ff-bench scenario 1 on throughput; feature delta documented in COMPARISON.md.",
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
        "[apalis-s1] wrote {} — {:.1} ops/sec p99={:.2}ms",
        path.display(),
        throughput,
        p99
    );
    Ok(())
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

fn filler_payload(size: usize) -> Vec<u8> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut x = seed.wrapping_add(0x9E37_79B9);
    (0..size)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 16) & 0xFF) as u8
        })
        .collect()
}
