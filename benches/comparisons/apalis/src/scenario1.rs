//! apalis comparison — scenario 1.
//!
//! What a consumer picking apalis 1.0 (rc.7 at time of writing) gets
//! for the same submit → claim → complete workload ff-bench's scenario
//! 1 measures. apalis-redis is the closest equivalent of a Valkey-
//! backed job queue; we drive it via `TaskSink::push_bulk` and run
//! N separate `WorkerBuilder` instances — matching ff-bench's
//! per-worker shape rather than in-worker concurrency.
//!
//! # Config (post-2026-04 refresh, issue #51 tuning)
//!
//! Applied apalis maintainer @geofmureithi's recommendations from
//! issue #51:
//!   1. `RedisConfig { poll_interval: 5ms, buffer_size: 100 }` —
//!      theoretical cap ~20 k ops/s/worker (was ~100 ops/s on the
//!      100ms×10 default). Picked to sit well above FF's ~3 k ops/s
//!      scenario 1 number so poll cadence isn't the bottleneck.
//!   2. `N independent WorkerBuilder` instances (N=16) rather than
//!      one `.concurrency(16)` — per-maintainer, `.concurrency`
//!      governs in-worker task parallelism, not worker count. Matches
//!      ferriskey-baseline's shape (16 independent tokio tasks each
//!      with its own connection).
//!   3. `.parallelize(tokio::spawn)` — lets handlers run concurrently
//!      on the tokio runtime within each worker.
//!
//! `SharedRedisStorage`/pubsub declined: pubsub drops on failure, and
//! ff-bench scenario 1 measures durable at-least-once throughput.
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
use apalis_redis::{RedisConfig, RedisStorage};
use clap::Parser;
use serde::{Deserialize, Serialize};

const SCENARIO: &str = "submit_claim_complete";
const SYSTEM: &str = "apalis";
const WORKER_COUNT: usize = 16;
const PAYLOAD_BYTES: usize = 4 * 1024;
const POLL_INTERVAL_MS: u64 = 5;
const BUFFER_SIZE: usize = 100;

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
    /// Abort the drain if it takes longer than this — safety net so a
    /// worker failure (or a stuck claim) fails fast instead of hanging
    /// the CI loop.
    #[arg(long, default_value_t = 300)]
    deadline_secs: u64,
}

/// 4 KiB payload, serialized as JSON by apalis's default codec.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchJob {
    payload: Vec<u8>,
}

async fn noop_handler(_job: BenchJob) -> Result<(), BoxDynError> {
    Ok(())
}

fn tuned_config() -> RedisConfig {
    RedisConfig::default()
        .set_poll_interval(Duration::from_millis(POLL_INTERVAL_MS))
        .set_buffer_size(BUFFER_SIZE)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let url = format!("redis://{}:{}/", args.valkey_host, args.valkey_port);

    // Seed storage — a dedicated connection just for push_bulk.
    let seed_conn = apalis_redis::connect(url.as_str())
        .await
        .map_err(|e| anyhow::anyhow!("apalis_redis::connect (seed): {e}"))?;
    let mut seed_storage: RedisStorage<BenchJob> =
        RedisStorage::new_with_config(seed_conn, tuned_config());

    let payload = filler_payload(PAYLOAD_BYTES);
    let submit_start = Instant::now();
    for _ in 0..args.tasks {
        seed_storage
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

    let drained = Arc::new(AtomicUsize::new(0));
    let target = args.tasks;

    let latencies: Arc<tokio::sync::Mutex<Vec<u64>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::with_capacity(target)));

    let drain_start = Instant::now();

    // Spawn N independent WorkerBuilder instances, each with its own
    // backend storage (own connection). This is the maintainer's
    // recommended shape (issue #51 comment 2026-04-27): `.concurrency`
    // governs in-worker task parallelism, not worker count. Separate
    // workers give each its own fetch loop + buffer.
    use apalis::prelude::WorkerError;
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    let mut worker_futs = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let conn = apalis_redis::connect(url.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("apalis_redis::connect (worker {wi}): {e}"))?;
        let storage: RedisStorage<BenchJob> =
            RedisStorage::new_with_config(conn, tuned_config());

        let drained_clone = drained.clone();
        let latencies_clone = latencies.clone();
        let worker = WorkerBuilder::new(format!("ff-bench-s1-{wi}"))
            .backend(storage)
            .parallelize(tokio::spawn)
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

        let mut rx = shutdown_tx.subscribe();
        let signal = async move {
            let _ = rx.recv().await;
            Ok::<(), WorkerError>(())
        };
        worker_futs.push(worker.run_until(signal));
    }

    let drained_watch = drained.clone();
    let shutdown_tx_driver = shutdown_tx.clone();
    let deadline = Instant::now() + Duration::from_secs(args.deadline_secs);
    let driver = async move {
        loop {
            let got = drained_watch.load(Ordering::Relaxed);
            if got >= target {
                let _ = shutdown_tx_driver.send(());
                return Ok::<(), anyhow::Error>(());
            }
            if Instant::now() >= deadline {
                let _ = shutdown_tx_driver.send(());
                anyhow::bail!(
                    "deadline exceeded: {}/{} drained in {}s",
                    got,
                    target,
                    args.deadline_secs
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };

    let workers_joined = futures::future::join_all(worker_futs);
    let (driver_res, worker_res) = tokio::join!(driver, workers_joined);
    driver_res?;
    // Surface any worker failure — ignoring them would silently
    // mask stuck claims or connection drops.
    for (wi, res) in worker_res.into_iter().enumerate() {
        if let Err(e) = res {
            anyhow::bail!("worker {wi} run_until returned error: {e:?}");
        }
    }
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
            "workers": WORKER_COUNT,
            "payload_bytes": PAYLOAD_BYTES,
            "apalis_version": "1.0.0-rc.7",
            "poll_interval_ms": POLL_INTERVAL_MS,
            "buffer_size": BUFFER_SIZE,
            "parallelize": "tokio::spawn",
        },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "apalis 1.0.0-rc.7 + apalis-redis; default JSON codec; no lease-backed fencing (at-least-once via ack). Post-issue-#51 tuning: N=16 independent WorkerBuilders (not .concurrency(16)), RedisConfig poll_interval=5ms buffer_size=100, .parallelize(tokio::spawn). Directly comparable to ff-bench scenario 1 on throughput; feature delta documented in COMPARISON.md.",
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
