//! Wider perf investigation — ferriskey 100-command GET pipeline.
//!
//! CRITICAL workload for the investigation. If pipeline ties between
//! ferriskey and redis-rs, it confirms the glide-core-inherited
//! per-command dispatch envelope (InflightRequestTracker, telemetry,
//! per-command timeout wrap) amortises across a batch — one pipeline
//! = one envelope cost. If pipeline DOESN'T tie, the per-command
//! overhead is not amortising and is a real problem for any workload
//! with tight RTT pressure.
//!
//! Shape: each iteration, one worker issues a pipeline of 100 GET
//! commands against random keys sharing the same `{wider}` hash tag
//! (single slot so pipelining is actually valid — Valkey Cluster
//! pipeline semantics require same-slot keys). Pipeline execution
//! via ferriskey's TypedPipeline (client.pipeline().get().get()...
//! .execute()). One latency sample per pipeline = per-100-ops-
//! amortised cost.
//!
//! Reported ops = pipelines × 100; the headline throughput number
//! is per-GET, not per-pipeline, so it's comparable against the
//! 80/20 + 100%GET numbers.

#[path = "../shared.rs"]
mod shared;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use ferriskey::{Client, ClientBuilder, Value};
use rand::prelude::*;
use rand::rngs::StdRng;
use tokio::sync::Barrier;

const SCENARIO: &str = "wider_pipeline_100";
const SYSTEM: &str = "ferriskey-wider";
const PAYLOAD_BYTES: usize = 4 * 1024;
const KEY_COUNT: usize = 10_000;
const PIPELINE_SIZE: usize = 100;

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

async fn build_client(host: &str, port: u16) -> Result<Client> {
    ClientBuilder::new()
        .host(host, port)
        .connect_timeout(Duration::from_secs(10))
        .request_timeout(Duration::from_millis(5_000))
        .build()
        .await
        .context("ferriskey ClientBuilder.build()")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let payload = shared::filler_payload(PAYLOAD_BYTES);
    let keys = Arc::new(shared::key_ring(KEY_COUNT));
    {
        let seeder = build_client(&args.valkey_host, args.valkey_port).await?;
        for k in keys.iter() {
            let _: Value = seeder.cmd("SET").arg(k).arg(&payload[..]).execute().await?;
        }
    }
    let barrier = Arc::new(Barrier::new(args.workers + 1));
    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let client = build_client(&args.valkey_host, args.valkey_port).await?;
        let keys = keys.clone();
        let barrier = barrier.clone();
        let warmup = Duration::from_secs(args.warmup_secs);
        let run = Duration::from_secs(args.duration_secs);
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, keys, barrier, warmup, run).await
        }));
    }
    eprintln!(
        "[wider-ferriskey-pipeline] warmup {}s + run {}s, {} workers, {}-cmd pipe",
        args.warmup_secs, args.duration_secs, args.workers, PIPELINE_SIZE
    );
    barrier.wait().await;
    let t0 = Instant::now();

    let mut total_pipelines = 0u64;
    let mut total_errors = 0u64;
    let mut histos = Vec::with_capacity(args.workers);
    for h in handles {
        let res = h.await?.context("worker task")?;
        total_pipelines += res.ops;
        total_errors += res.errors;
        histos.push(res.hdr);
    }
    let wall = t0.elapsed();
    let total_ops = total_pipelines * PIPELINE_SIZE as u64;
    let lat = shared::hdr_snapshot(&histos);
    let config = serde_json::json!({
        "workers": args.workers, "payload_bytes": PAYLOAD_BYTES,
        "key_count": KEY_COUNT, "duration_s": args.duration_secs,
        "warmup_s": args.warmup_secs, "mix": "100-cmd pipeline (GET)",
        "pipeline_size": PIPELINE_SIZE,
        "pipelines_executed": total_pipelines,
    });
    let path = shared::write_report(
        &args.results_dir, SCENARIO, SYSTEM, config,
        wall, total_ops, total_errors, &lat,
        "ferriskey wider = TypedPipeline of 100 GETs per iteration, same {wider} hash tag. \
         Throughput counts per-GET (pipelines × 100). Latency = per-pipeline = per-100-ops-amortised.",
    )?;
    eprintln!(
        "[wider-ferriskey-pipeline] wrote {} — {:.1} ops/sec ({:.1} pipe/s) p50={:.3}ms p99={:.3}ms errs={}",
        path.display(),
        total_ops as f64 / wall.as_secs_f64(),
        total_pipelines as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0, lat.p99_us / 1000.0, total_errors,
    );
    Ok(())
}

struct WorkerResult { ops: u64, errors: u64, hdr: hdrhistogram::Histogram<u64> }

async fn drive_worker(
    wi: usize, client: Client, keys: Arc<Vec<String>>,
    barrier: Arc<Barrier>, warmup: Duration, run: Duration,
) -> Result<WorkerResult> {
    let mut rng = StdRng::seed_from_u64(0xB17E_1CEF ^ (wi as u64 * 0x9E3779B1));
    let mut hdr = shared::new_worker_hdr();
    let mut pipes = 0u64;
    let mut errors = 0u64;

    let warmup_end = Instant::now() + warmup;
    while Instant::now() < warmup_end {
        let _ = do_one(&client, &keys, &mut rng).await;
    }
    barrier.wait().await;
    let run_end = Instant::now() + run;
    while Instant::now() < run_end {
        let t0 = Instant::now();
        match do_one(&client, &keys, &mut rng).await {
            Ok(_) => {
                hdr.record((t0.elapsed().as_micros() as u64).clamp(1, 60_000_000)).ok();
                pipes += 1;
            }
            Err(_) => errors += 1,
        }
    }
    Ok(WorkerResult { ops: pipes, errors, hdr })
}

async fn do_one(client: &Client, keys: &[String], rng: &mut StdRng) -> Result<()> {
    // Build the pipeline, queue PIPELINE_SIZE GETs, execute. Drop
    // the slots — we don't need to inspect the returned values for a
    // throughput measurement. The `execute()` call fires the pipeline
    // and awaits every reply; that's the per-pipeline RTT sample.
    let mut pipe = client.pipeline();
    for _ in 0..PIPELINE_SIZE {
        let k = &keys[rng.random_range(0..keys.len())];
        let _slot = pipe.get::<Vec<u8>>(k);
    }
    pipe.execute().await.context("pipeline execute")?;
    Ok(())
}
