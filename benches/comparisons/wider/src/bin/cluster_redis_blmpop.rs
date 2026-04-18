//! Round-2 cluster+TLS — redis-rs scenario 1 (RPUSH / BLMPOP / INCR).
//! Mirror of cluster_ferriskey_blmpop.rs; queue + counter hashed to
//! `{benchq}` so BLMPOP doesn't CROSSSLOT.

#[path = "../shared.rs"]
mod shared;
#[path = "../cluster_shared.rs"]
mod cluster_shared;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use cluster_shared::{build_redis_cluster, ClusterEnv};
use redis::cluster_async::ClusterConnection;
use redis::AsyncCommands;

const SCENARIO: &str = "cluster_submit_claim_complete";
const SYSTEM: &str = "redis-rs-cluster";
const QUEUE_KEY: &str = "{benchq}:q";
const COMPLETED_KEY: &str = "{benchq}:completed";
const WORKER_COUNT: usize = 16;
const PAYLOAD_BYTES: usize = 4 * 1024;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 10_000)]
    tasks: usize,
    #[arg(long, default_value = "benches/perf-invest/cluster-tls")]
    results_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    cluster_shared::init_rustls_provider();
    let env = ClusterEnv::from_env();
    eprintln!("[cluster-redis-blmpop] {}", env.describe());

    let client = build_redis_cluster(&env)?;
    let mut admin = client.get_async_connection().await.context("admin conn")?;
    let _: () = redis::cmd("DEL").arg(QUEUE_KEY).arg(COMPLETED_KEY).query_async(&mut admin).await.context("DEL")?;

    let payload = shared::filler_payload(PAYLOAD_BYTES);
    let submit_start = Instant::now();
    seed_queue(&mut admin, args.tasks, &payload).await?;
    eprintln!("[cluster-redis-blmpop] seeded {} tasks in {} ms", args.tasks, submit_start.elapsed().as_millis());

    let drain_start = Instant::now();
    let stop_at = Arc::new(AtomicUsize::new(args.tasks));
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let conn = client.get_async_connection().await.context("per-worker conn")?;
        let stop_at = stop_at.clone();
        handles.push(tokio::spawn(async move {
            drive_worker(wi, conn, stop_at).await
        }));
    }
    let mut hdr = shared::new_worker_hdr();
    let mut errors = 0u64;
    let mut total_ops = 0u64;
    for h in handles {
        let res = h.await?.context("worker task")?;
        total_ops += res.ops;
        errors += res.errors;
        hdr.add(&res.hdr).ok();
    }
    let wall = drain_start.elapsed();
    let lat = shared::hdr_snapshot(&[hdr]);
    let config = serde_json::json!({
        "tasks": args.tasks, "workers": WORKER_COUNT,
        "payload_bytes": PAYLOAD_BYTES, "queue_key": QUEUE_KEY,
        "mix": "RPUSH/BLMPOP/INCR (scenario 1 shape)",
    });
    let path = cluster_shared::write_cluster_report(
        &args.results_dir, SCENARIO, SYSTEM, config, &env,
        wall, total_ops, errors, &lat,
        "redis-rs cluster+TLS = ClusterClient + RPUSH/BLMPOP/INCR, {benchq} single-slot.",
    )?;
    eprintln!("[cluster-redis-blmpop] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
        path.display(), total_ops as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0, lat.p99_us / 1000.0, errors);
    Ok(())
}

async fn seed_queue(conn: &mut ClusterConnection, n: usize, payload: &[u8]) -> Result<()> {
    const CHUNK: usize = 1_000;
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(CHUNK);
        let vals: Vec<&[u8]> = (0..take).map(|_| payload).collect();
        let _: () = conn.rpush(QUEUE_KEY, vals).await.context("RPUSH chunk")?;
        remaining -= take;
    }
    Ok(())
}

struct WorkerResult { ops: u64, errors: u64, hdr: hdrhistogram::Histogram<u64> }

async fn drive_worker(_wi: usize, mut conn: ClusterConnection, stop_at: Arc<AtomicUsize>) -> Result<WorkerResult> {
    let mut hdr = shared::new_worker_hdr();
    let mut ops = 0u64;
    let mut errors = 0u64;
    loop {
        if stop_at.load(Ordering::Relaxed) == 0 {
            return Ok(WorkerResult { ops, errors, hdr });
        }
        let t0 = Instant::now();
        let popped: Result<Option<redis::Value>, _> = redis::cmd("BLMPOP")
            .arg(1_u64).arg(1_u64).arg(QUEUE_KEY).arg("LEFT").arg("COUNT").arg(1_u64)
            .query_async(&mut conn).await;
        match popped {
            Ok(Some(_)) => {
                match redis::cmd("INCR").arg(COMPLETED_KEY).query_async::<i64>(&mut conn).await {
                    Ok(_) => {
                        hdr.record((t0.elapsed().as_micros() as u64).clamp(1, 60_000_000)).ok();
                        ops += 1;
                        stop_at.fetch_sub(1, Ordering::Relaxed);
                    }
                    Err(_) => errors += 1,
                }
            }
            Ok(None) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(_) => {
                errors += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}
