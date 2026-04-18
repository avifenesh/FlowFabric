//! Round-2 cluster+TLS — ferriskey streams (XADD + XRANGE).
//! Per-worker stream keys all share `{wider}` tag → single slot.

#[path = "../shared.rs"]
mod shared;
#[path = "../cluster_shared.rs"]
mod cluster_shared;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use cluster_shared::{build_ferriskey, ClusterEnv};
use ferriskey::{Client, Value};
use rand::prelude::*;
use rand::rngs::StdRng;
use tokio::sync::Barrier;

const SCENARIO: &str = "cluster_wider_streams";
const SYSTEM: &str = "ferriskey-cluster";
const PAYLOAD_BYTES: usize = 4 * 1024;
const XRANGE_COUNT: usize = 10;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 16)]
    workers: usize,
    #[arg(long, default_value_t = 30)]
    duration_secs: u64,
    #[arg(long, default_value_t = 5)]
    warmup_secs: u64,
    #[arg(long, default_value = "benches/perf-invest/cluster-tls")]
    results_dir: String,
}

fn stream_key(wi: usize) -> String {
    format!("{{wider}}:stream:w{wi}")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    cluster_shared::init_rustls_provider();
    let env = ClusterEnv::from_env();
    eprintln!("[cluster-ferriskey-streams] {}", env.describe());

    let payload = shared::filler_payload(PAYLOAD_BYTES);
    {
        let seeder = build_ferriskey(&env).await?;
        for wi in 0..args.workers {
            let key = stream_key(wi);
            for _ in 0..50 {
                let _: Value = seeder.cmd("XADD").arg(&key).arg("*").arg("payload").arg(&payload[..]).execute().await?;
            }
        }
    }
    let barrier = Arc::new(Barrier::new(args.workers + 1));
    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let client = build_ferriskey(&env).await?;
        let payload = payload.clone();
        let barrier = barrier.clone();
        let warmup = Duration::from_secs(args.warmup_secs);
        let run = Duration::from_secs(args.duration_secs);
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, payload, barrier, warmup, run).await
        }));
    }
    eprintln!("[cluster-ferriskey-streams] warmup {}s + run {}s, {} workers, 1:4 XADD:XRANGE",
        args.warmup_secs, args.duration_secs, args.workers);
    barrier.wait().await;
    let t0 = Instant::now();

    let mut total_ops = 0u64;
    let mut total_errors = 0u64;
    let mut histos = Vec::with_capacity(args.workers);
    for h in handles {
        let res = h.await?.context("worker task")?;
        total_ops += res.ops;
        total_errors += res.errors;
        histos.push(res.hdr);
    }
    let wall = t0.elapsed();
    let lat = shared::hdr_snapshot(&histos);
    let config = serde_json::json!({
        "workers": args.workers, "payload_bytes": PAYLOAD_BYTES,
        "duration_s": args.duration_secs, "warmup_s": args.warmup_secs,
        "mix": "20% XADD / 80% XRANGE-10", "xrange_count": XRANGE_COUNT,
    });
    let path = cluster_shared::write_cluster_report(
        &args.results_dir, SCENARIO, SYSTEM, config, &env,
        wall, total_ops, total_errors, &lat,
        "ferriskey cluster+TLS = 20% XADD + 80% XRANGE-10, per-worker {wider} stream keys.",
    )?;
    eprintln!("[cluster-ferriskey-streams] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
        path.display(), total_ops as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0, lat.p99_us / 1000.0, total_errors);
    Ok(())
}

struct WorkerResult { ops: u64, errors: u64, hdr: hdrhistogram::Histogram<u64> }

async fn drive_worker(
    wi: usize, client: Client, payload: Vec<u8>,
    barrier: Arc<Barrier>, warmup: Duration, run: Duration,
) -> Result<WorkerResult> {
    let key = stream_key(wi);
    let mut rng = StdRng::seed_from_u64(0xB17E_1CEF ^ (wi as u64 * 0x9E3779B1));
    let mut hdr = shared::new_worker_hdr();
    let mut ops = 0u64;
    let mut errors = 0u64;

    let warmup_end = Instant::now() + warmup;
    while Instant::now() < warmup_end {
        let _ = do_one(&client, &key, &payload, &mut rng).await;
    }
    barrier.wait().await;
    let run_end = Instant::now() + run;
    while Instant::now() < run_end {
        let t0 = Instant::now();
        match do_one(&client, &key, &payload, &mut rng).await {
            Ok(_) => {
                hdr.record((t0.elapsed().as_micros() as u64).clamp(1, 60_000_000)).ok();
                ops += 1;
            }
            Err(_) => errors += 1,
        }
    }
    Ok(WorkerResult { ops, errors, hdr })
}

async fn do_one(client: &Client, key: &str, payload: &[u8], rng: &mut StdRng) -> Result<()> {
    if rng.random_ratio(20, 100) {
        let _: Value = client.cmd("XADD").arg(key).arg("*").arg("payload").arg(payload).execute().await.context("XADD")?;
    } else {
        let _: Value = client.cmd("XRANGE").arg(key).arg("-").arg("+").arg("COUNT").arg(XRANGE_COUNT as u64).execute().await.context("XRANGE")?;
    }
    Ok(())
}
