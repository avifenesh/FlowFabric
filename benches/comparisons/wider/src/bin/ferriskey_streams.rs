//! Wider perf investigation — ferriskey streams (XADD + XRANGE).
//!
//! Modern-Valkey workload: one stream per worker (hash-tag-pinned to
//! the `{wider}` slot), 1:4 write:read ratio — each iteration, RNG
//! draws XADD (20%) or XRANGE-last-10 (80%). XADD field-value is the
//! 4 KiB payload; XRANGE reads up to 10 entries by ID range.
//!
//! Stream semantics exercise a code path the GET/SET workloads
//! don't: XADD returns an ID (RESP2 bulk string), XRANGE returns a
//! nested array of [id, [field, value, ...]]. The return-type decoder
//! on ferriskey + the nested-array parser on redis-rs are both
//! routinely exercised in production and are a separate bandwidth
//! envelope from flat string GET replies.

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

const SCENARIO: &str = "wider_streams";
const SYSTEM: &str = "ferriskey-wider";
const PAYLOAD_BYTES: usize = 4 * 1024;
const XRANGE_COUNT: usize = 10;

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

/// Per-worker stream key. `{wider}` keeps every stream on the same
/// slot in a cluster; `:s{wi}` makes the stream unique so workers
/// don't contend on XADD ordering.
fn stream_key(wi: usize) -> String {
    format!("{{wider}}:stream:w{wi}")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let payload = shared::filler_payload(PAYLOAD_BYTES);

    // Pre-seed each per-worker stream with a small backlog so XRANGE
    // always returns data in the hot loop. 50 entries × 4 KiB =
    // ~200 KiB per stream pre-seeded — small enough to fit easily,
    // large enough that XRANGE-last-10 returns a full batch.
    {
        let seeder = build_client(&args.valkey_host, args.valkey_port).await?;
        for wi in 0..args.workers {
            let key = stream_key(wi);
            for _ in 0..50 {
                let _: Value = seeder
                    .cmd("XADD")
                    .arg(&key)
                    .arg("*")
                    .arg("payload")
                    .arg(&payload[..])
                    .execute()
                    .await
                    .context("pre-seed XADD")?;
            }
        }
    }

    let barrier = Arc::new(Barrier::new(args.workers + 1));
    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let client = build_client(&args.valkey_host, args.valkey_port).await?;
        let payload = payload.clone();
        let barrier = barrier.clone();
        let warmup = Duration::from_secs(args.warmup_secs);
        let run = Duration::from_secs(args.duration_secs);
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, payload, barrier, warmup, run).await
        }));
    }
    eprintln!(
        "[wider-ferriskey-streams] warmup {}s + run {}s, {} workers, 1:4 XADD:XRANGE",
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
        "duration_s": args.duration_secs, "warmup_s": args.warmup_secs,
        "mix": "20% XADD / 80% XRANGE-10", "xrange_count": XRANGE_COUNT,
    });
    let path = shared::write_report(
        &args.results_dir, SCENARIO, SYSTEM, config,
        wall, total_ops, total_errors, &lat,
        "ferriskey wider = streams = 20% XADD + 80% XRANGE-10 per worker. Per-worker \
         stream key under the {wider} hash tag. Pre-seeded 50-entry backlog.",
    )?;
    eprintln!(
        "[wider-ferriskey-streams] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
        path.display(),
        total_ops as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0, lat.p99_us / 1000.0, total_errors,
    );
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
        // XADD <key> * payload <4 KiB> — single field:value pair.
        let _: Value = client
            .cmd("XADD")
            .arg(key).arg("*").arg("payload").arg(payload)
            .execute().await.context("XADD")?;
    } else {
        // XRANGE <key> - + COUNT 10 — tail-read last 10 entries.
        // Ferriskey's decoder returns the full nested array; we
        // don't inspect the data, just drain the reply.
        let _: Value = client
            .cmd("XRANGE")
            .arg(key).arg("-").arg("+").arg("COUNT").arg(XRANGE_COUNT as u64)
            .execute().await.context("XRANGE")?;
    }
    Ok(())
}
