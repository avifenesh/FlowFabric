//! Round-2 cluster+TLS — ferriskey scenario 1 (RPUSH / BLMPOP / INCR)
//! against ElastiCache.
//!
//! This is the workload where round-1 localhost showed -46% ferriskey
//! vs redis-rs. Re-running over cluster+TLS tells us whether BLMPOP
//! divergence grows, shrinks, or holds under production-shape
//! transport.
//!
//! Keys pinned to `{benchq}` so the queue + completed counter land on
//! the same slot — cross-slot BLMPOP would error with CROSSSLOT.

#[path = "../shared.rs"]
mod shared;
#[path = "../cluster_shared.rs"]
mod cluster_shared;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use cluster_shared::{build_ferriskey, ClusterEnv};
use ferriskey::{Client, Value};

const SCENARIO: &str = "cluster_submit_claim_complete";
const SYSTEM: &str = "ferriskey-cluster";
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
    eprintln!("[cluster-ferriskey-blmpop] {}", env.describe());

    let admin = build_ferriskey(&env).await?;
    let _: Value = admin.cmd("DEL").arg(QUEUE_KEY).arg(COMPLETED_KEY).execute().await.context("DEL")?;

    let payload = shared::filler_payload(PAYLOAD_BYTES);
    let submit_start = Instant::now();
    seed_queue(&admin, args.tasks, &payload).await?;
    eprintln!("[cluster-ferriskey-blmpop] seeded {} tasks in {} ms", args.tasks, submit_start.elapsed().as_millis());

    let drain_start = Instant::now();
    let stop_at = Arc::new(AtomicUsize::new(args.tasks));
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let client = build_ferriskey(&env).await?;
        let stop_at = stop_at.clone();
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, stop_at).await
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
        "tasks": args.tasks,
        "workers": WORKER_COUNT,
        "payload_bytes": PAYLOAD_BYTES,
        "queue_key": QUEUE_KEY,
        "mix": "RPUSH/BLMPOP/INCR (scenario 1 shape)",
    });
    let path = cluster_shared::write_cluster_report(
        &args.results_dir, SCENARIO, SYSTEM, config, &env,
        wall, total_ops, errors, &lat,
        "ferriskey cluster+TLS = ClientBuilder per worker + RPUSH/BLMPOP/INCR, \
         {benchq} single-slot. Scenario 1 shape re-run over cluster+TLS — round 1 \
         localhost showed -46% ferriskey vs redis-rs.",
    )?;
    eprintln!("[cluster-ferriskey-blmpop] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
        path.display(), total_ops as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0, lat.p99_us / 1000.0, errors);
    Ok(())
}

async fn seed_queue(client: &Client, n: usize, payload: &[u8]) -> Result<()> {
    const CHUNK: usize = 1_000;
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(CHUNK);
        let elements: Vec<&[u8]> = (0..take).map(|_| payload).collect();
        let _: i64 = client.rpush(QUEUE_KEY, &elements).await.context("RPUSH chunk")?;
        remaining -= take;
    }
    Ok(())
}

struct WorkerResult { ops: u64, errors: u64, hdr: hdrhistogram::Histogram<u64> }

async fn drive_worker(_wi: usize, client: Client, stop_at: Arc<AtomicUsize>) -> Result<WorkerResult> {
    let mut hdr = shared::new_worker_hdr();
    let mut ops = 0u64;
    let mut errors = 0u64;
    loop {
        if stop_at.load(Ordering::Relaxed) == 0 {
            return Ok(WorkerResult { ops, errors, hdr });
        }
        let t0 = Instant::now();
        let popped: Option<Value> = client
            .cmd("BLMPOP")
            .arg(1_u64).arg(1_u64).arg(QUEUE_KEY).arg("LEFT").arg("COUNT").arg(1_u64)
            .execute().await.unwrap_or(None);
        if popped.is_some() {
            match client.incr(COMPLETED_KEY).await {
                Ok(_) => {
                    hdr.record((t0.elapsed().as_micros() as u64).clamp(1, 60_000_000)).ok();
                    ops += 1;
                    stop_at.fetch_sub(1, Ordering::Relaxed);
                }
                Err(_) => errors += 1,
            }
        } else {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
