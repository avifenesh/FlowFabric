//! Round-2 cluster+TLS — ferriskey 80/20 GET/SET.
//!
//! Mirror of `ferriskey_80_20.rs`, but connects to the ElastiCache
//! cluster endpoint over TLS. Env-driven config (FF_HOST / FF_PORT /
//! FF_TLS / FF_CLUSTER) so the same binary can run against localhost
//! for a sanity check before the cluster run.
//!
//! Key-hygiene: every key is pinned to the `{wider}` hash tag → one
//! cluster slot → we measure cluster-client + TLS overhead, not
//! cross-slot routing. See `report-w2-round2.md §4` for why that's
//! intentional.

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

const SCENARIO: &str = "cluster_wider_80_20";
const SYSTEM: &str = "ferriskey-cluster";
const PAYLOAD_BYTES: usize = 4 * 1024;
const KEY_COUNT: usize = 10_000;

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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    cluster_shared::init_rustls_provider();
    let env = ClusterEnv::from_env();
    eprintln!("[cluster-ferriskey-80-20] {}", env.describe());

    let payload = shared::filler_payload(PAYLOAD_BYTES);
    let keys = Arc::new(shared::key_ring(KEY_COUNT));

    {
        let seeder = build_ferriskey(&env).await?;
        for k in keys.iter() {
            let _: Value = seeder
                .cmd("SET")
                .arg(k)
                .arg(&payload[..])
                .execute()
                .await
                .context("seed SET")?;
        }
    }

    let barrier = Arc::new(Barrier::new(args.workers + 1));
    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let client = build_ferriskey(&env).await?;
        let keys = keys.clone();
        let payload = payload.clone();
        let barrier = barrier.clone();
        let warmup = Duration::from_secs(args.warmup_secs);
        let run = Duration::from_secs(args.duration_secs);
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, keys, payload, barrier, warmup, run).await
        }));
    }
    eprintln!(
        "[cluster-ferriskey-80-20] warmup {}s + run {}s, {} workers",
        args.warmup_secs, args.duration_secs, args.workers
    );
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
        "workers": args.workers,
        "payload_bytes": PAYLOAD_BYTES,
        "key_count": KEY_COUNT,
        "duration_s": args.duration_secs,
        "warmup_s": args.warmup_secs,
        "mix": "80/20 GET/SET",
    });
    let path = cluster_shared::write_cluster_report(
        &args.results_dir, SCENARIO, SYSTEM, config, &env,
        wall, total_ops, total_errors, &lat,
        "ferriskey cluster+TLS = ClientBuilder per worker + 80/20 GET/SET on 4 KiB, \
         {wider} single-slot keys. Measures client-in-cluster-mode + TLS cost, \
         NOT cross-slot routing (see report §4).",
    )?;
    eprintln!(
        "[cluster-ferriskey-80-20] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
        path.display(),
        total_ops as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0, lat.p99_us / 1000.0, total_errors,
    );
    Ok(())
}

struct WorkerResult { ops: u64, errors: u64, hdr: hdrhistogram::Histogram<u64> }

async fn drive_worker(
    wi: usize, client: Client, keys: Arc<Vec<String>>, payload: Vec<u8>,
    barrier: Arc<Barrier>, warmup: Duration, run: Duration,
) -> Result<WorkerResult> {
    let mut rng = StdRng::seed_from_u64(0xB17E_1CEF ^ (wi as u64 * 0x9E3779B1));
    let mut hdr = shared::new_worker_hdr();
    let mut ops = 0u64;
    let mut errors = 0u64;

    let warmup_end = Instant::now() + warmup;
    while Instant::now() < warmup_end {
        let _ = do_one(&client, &keys, &payload, &mut rng).await;
    }
    barrier.wait().await;
    let run_end = Instant::now() + run;
    while Instant::now() < run_end {
        let t0 = Instant::now();
        match do_one(&client, &keys, &payload, &mut rng).await {
            Ok(_) => {
                hdr.record((t0.elapsed().as_micros() as u64).clamp(1, 60_000_000)).ok();
                ops += 1;
            }
            Err(_) => errors += 1,
        }
    }
    Ok(WorkerResult { ops, errors, hdr })
}

async fn do_one(client: &Client, keys: &[String], payload: &[u8], rng: &mut StdRng) -> Result<()> {
    let key = &keys[rng.random_range(0..keys.len())];
    if rng.random_ratio(80, 100) {
        let _: Option<Vec<u8>> = client.cmd("GET").arg(key).execute().await.context("GET")?;
    } else {
        let _: Value = client.cmd("SET").arg(key).arg(payload).execute().await.context("SET")?;
    }
    Ok(())
}
