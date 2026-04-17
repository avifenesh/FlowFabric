//! Wider perf investigation — ferriskey on 80/20 GET/SET.
//!
//! Hot loop draws GET-or-SET from a seeded PRNG at 80:20, hits a
//! 10 000-key ring sharing `{wider}` hash tag, records per-op
//! latency in a thread-local HdrHistogram. 16 concurrent workers,
//! one Client per worker, 30 s per run × 3 runs (median picked by
//! the driver — here we emit one run at a time; outer shell loops).
//!
//! Client-per-worker mirrors the ferriskey-baseline fix at 4d89a89:
//! Arc-sharing one Client across N workers serialises their commands
//! on the client's multiplex queue and collapses throughput. The
//! native ferriskey bench at benches/connections_benchmark.rs uses
//! the same "one Client per worker" shape, so this port is
//! apples-to-apples against both the native bench and
//! benches/comparisons/baseline/ (redis-rs equivalent).
//!
//! NOTE: no blocking commands (BLMPOP etc) — 80/20 GET/SET is pure
//! non-blocking. This is the workload where ferriskey's reply-queue
//! shape should diverge from redis-rs *without* the BLMPOP
//! confound; if ferriskey still falls behind here we know the gap is
//! not BLMPOP-specific.

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

const SCENARIO: &str = "wider_80_20";
const SYSTEM: &str = "ferriskey-wider";
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

async fn build_client(host: &str, port: u16) -> Result<Client> {
    // Shape: same ClientBuilder config the ferriskey-baseline uses.
    // If we add TLS / auth / cluster here it MUST also flip on the
    // redis-rs side of this pair, otherwise the comparison is not
    // measuring the same thing.
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

    // Pre-seed every key with the 4 KiB value so GETs always return
    // (not Nil). Matters for p99: a Nil reply is faster than a 4 KiB
    // payload reply, and mixing them pollutes the histogram. One
    // seeding client, discarded before the hot loop.
    {
        let seeder = build_client(&args.valkey_host, args.valkey_port).await?;
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

    // Barrier so every worker begins the measurement window together.
    // `workers + 1` so the driver can wait alongside them before
    // stamping t0.
    let barrier = Arc::new(Barrier::new(args.workers + 1));

    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let client = build_client(&args.valkey_host, args.valkey_port).await?;
        let keys = keys.clone();
        let payload = payload.clone();
        let barrier = barrier.clone();
        let warmup = Duration::from_secs(args.warmup_secs);
        let run = Duration::from_secs(args.duration_secs);
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, keys, payload, barrier, warmup, run).await
        }));
    }

    // Warmup is driven inside the worker — it runs the loop for
    // `warmup` before the barrier drops, discards those samples.
    eprintln!(
        "[wider-ferriskey-80-20] warmup {}s + run {}s, {} workers",
        args.warmup_secs, args.duration_secs, args.workers
    );

    // Driver-side barrier rendezvous. After this the window is open.
    barrier.wait().await;
    let t0 = Instant::now();

    let mut total_ops: u64 = 0;
    let mut total_errors: u64 = 0;
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
    let path = shared::write_report(
        &args.results_dir,
        SCENARIO,
        SYSTEM,
        config,
        wall,
        total_ops,
        total_errors,
        &lat,
        "ferriskey wider = ClientBuilder per worker + 80% GET / 20% SET on 4 KiB values, \
         {wider} hash-tagged ring. No blocking commands. Paired with \
         redis_80_20 for apples-to-apples; any delta is client overhead under \
         non-blocking RESP2 traffic.",
    )?;
    eprintln!(
        "[wider-ferriskey-80-20] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
        path.display(),
        total_ops as f64 / wall.as_secs_f64(),
        lat.p50_us / 1000.0,
        lat.p99_us / 1000.0,
        total_errors,
    );
    Ok(())
}

struct WorkerResult {
    ops: u64,
    errors: u64,
    hdr: hdrhistogram::Histogram<u64>,
}

async fn drive_worker(
    wi: usize,
    client: Client,
    keys: Arc<Vec<String>>,
    payload: Vec<u8>,
    barrier: Arc<Barrier>,
    warmup: Duration,
    run: Duration,
) -> Result<WorkerResult> {
    // Per-worker PRNG seed so the op mix is deterministic at a worker
    // slot but different across workers (10 000-key ring stays
    // well-spread). `wi` in the seed avoids every worker hammering
    // the same sequence of keys.
    let mut rng = StdRng::seed_from_u64(0xB17E_1CEF ^ (wi as u64 * 0x9E3779B1));
    let mut hdr = shared::new_worker_hdr();
    let mut ops: u64 = 0;
    let mut errors: u64 = 0;

    // Warmup: run the same loop for `warmup` but don't record. This
    // primes the connection, populates any per-connection caches,
    // and eats the first-poll jitter that would otherwise push
    // p99 into the air.
    let warmup_end = Instant::now() + warmup;
    while Instant::now() < warmup_end {
        let _ = do_one(&client, &keys, &payload, &mut rng).await;
    }

    // Rendezvous with driver. Every worker hits the barrier; the
    // instant the last one arrives, the window opens for all of
    // them + the driver stamps t0.
    barrier.wait().await;

    let run_end = Instant::now() + run;
    while Instant::now() < run_end {
        let t0 = Instant::now();
        match do_one(&client, &keys, &payload, &mut rng).await {
            Ok(_) => {
                let us = t0.elapsed().as_micros() as u64;
                // Clamp against HdrHistogram's upper bound so an
                // anomalous 60s+ RTT doesn't panic the recorder.
                hdr.record(us.clamp(1, 60_000_000)).ok();
                ops += 1;
            }
            Err(_) => errors += 1,
        }
    }
    Ok(WorkerResult { ops, errors, hdr })
}

async fn do_one(
    client: &Client,
    keys: &[String],
    payload: &[u8],
    rng: &mut StdRng,
) -> Result<()> {
    let key = &keys[rng.random_range(0..keys.len())];
    if rng.random_ratio(80, 100) {
        // GET — 80% of ops. Return type: String bytes or Nil.
        let _: Option<Vec<u8>> = client
            .cmd("GET")
            .arg(key)
            .execute()
            .await
            .context("GET")?;
    } else {
        // SET — 20% of ops. 4 KiB value.
        let _: Value = client
            .cmd("SET")
            .arg(key)
            .arg(payload)
            .execute()
            .await
            .context("SET")?;
    }
    Ok(())
}
