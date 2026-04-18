//! Wider perf investigation — redis-rs 50/50 GET/SET. Mirror of
//! ferriskey_50_50.rs.

#[path = "../shared.rs"]
mod shared;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use rand::prelude::*;
use rand::rngs::StdRng;
use redis::aio::MultiplexedConnection;
use redis::AsyncCommands;
use tokio::sync::Barrier;

const SCENARIO: &str = "wider_50_50";
const SYSTEM: &str = "redis-rs-wider";
const PAYLOAD_BYTES: usize = 4 * 1024;
const KEY_COUNT: usize = 10_000;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FF_BENCH_VALKEY_HOST", default_value = "localhost")]
    valkey_host: String,
    #[arg(long, env = "FF_BENCH_VALKEY_PORT", default_value_t = 6379)]
    valkey_port: u16,
    #[arg(long, default_value_t = 16)]
    workers: usize,
    #[arg(long, default_value_t = 30)]
    duration_secs: u64,
    #[arg(long, default_value_t = 5)]
    warmup_secs: u64,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

fn build_client(host: &str, port: u16) -> Result<redis::Client> {
    redis::Client::open(format!("redis://{host}:{port}/")).context("redis::Client::open")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let payload = shared::filler_payload(PAYLOAD_BYTES);
    let keys = Arc::new(shared::key_ring(KEY_COUNT));
    let client = build_client(&args.valkey_host, args.valkey_port)?;
    {
        let mut seeder = client.get_multiplexed_async_connection().await?;
        for k in keys.iter() {
            let _: () = seeder.set(k, &payload[..]).await?;
        }
    }
    let barrier = Arc::new(Barrier::new(args.workers + 1));
    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let conn = client.get_multiplexed_async_connection().await?;
        let keys = keys.clone();
        let payload = payload.clone();
        let barrier = barrier.clone();
        let warmup = Duration::from_secs(args.warmup_secs);
        let run = Duration::from_secs(args.duration_secs);
        handles.push(tokio::spawn(async move {
            drive_worker(wi, conn, keys, payload, barrier, warmup, run).await
        }));
    }
    eprintln!(
        "[wider-redis-50-50] warmup {}s + run {}s, {} workers",
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
        "workers": args.workers, "payload_bytes": PAYLOAD_BYTES,
        "key_count": KEY_COUNT, "duration_s": args.duration_secs,
        "warmup_s": args.warmup_secs, "mix": "50/50 GET/SET",
    });
    let path = shared::write_report(
        &args.results_dir, SCENARIO, SYSTEM, config,
        wall, total_ops, total_errors, &lat,
        "redis-rs wider = MultiplexedConnection per worker + 50% GET / 50% SET on 4 KiB, \
         {wider} hash-tagged ring. No blocking commands.",
    )?;
    eprintln!(
        "[wider-redis-50-50] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
        path.display(),
        total_ops as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0, lat.p99_us / 1000.0, total_errors,
    );
    Ok(())
}

struct WorkerResult { ops: u64, errors: u64, hdr: hdrhistogram::Histogram<u64> }

async fn drive_worker(
    wi: usize, mut conn: MultiplexedConnection, keys: Arc<Vec<String>>, payload: Vec<u8>,
    barrier: Arc<Barrier>, warmup: Duration, run: Duration,
) -> Result<WorkerResult> {
    let mut rng = StdRng::seed_from_u64(0xB17E_1CEF ^ (wi as u64 * 0x9E3779B1));
    let mut hdr = shared::new_worker_hdr();
    let mut ops = 0u64;
    let mut errors = 0u64;

    let warmup_end = Instant::now() + warmup;
    while Instant::now() < warmup_end {
        let _ = do_one(&mut conn, &keys, &payload, &mut rng).await;
    }
    barrier.wait().await;
    let run_end = Instant::now() + run;
    while Instant::now() < run_end {
        let t0 = Instant::now();
        match do_one(&mut conn, &keys, &payload, &mut rng).await {
            Ok(_) => {
                hdr.record((t0.elapsed().as_micros() as u64).clamp(1, 60_000_000)).ok();
                ops += 1;
            }
            Err(_) => errors += 1,
        }
    }
    Ok(WorkerResult { ops, errors, hdr })
}

async fn do_one(conn: &mut MultiplexedConnection, keys: &[String], payload: &[u8], rng: &mut StdRng) -> Result<()> {
    let key = &keys[rng.random_range(0..keys.len())];
    if rng.random_ratio(50, 100) {
        let _: Option<Vec<u8>> = conn.get(key).await.context("GET")?;
    } else {
        let _: () = conn.set(key, payload).await.context("SET")?;
    }
    Ok(())
}
