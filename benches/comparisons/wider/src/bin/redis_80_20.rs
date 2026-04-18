//! Wider perf investigation — redis-rs on 80/20 GET/SET.
//!
//! Mirror of ferriskey_80_20.rs — same PRNG seed strategy, same key
//! ring, same payload, same barrier-sync, same histogram bounds, same
//! report schema. Only differences are the client crate + its
//! idiomatic API:
//!
//!   * `redis::Client::open(...)` replaces ferriskey's ClientBuilder.
//!   * `client.get_multiplexed_async_connection()` per worker
//!     produces an independent logical connection; the underlying
//!     crate multiplexes commands on top. Matches how the
//!     redis-rs `baseline` package already does it — serves as the
//!     apples-to-apples counterpart to ferriskey's "one Client per
//!     worker" shape.
//!   * Commands issued via `redis::cmd("GET").arg(...).query_async(...)`.
//!
//! If this port shows ferriskey winning on non-blocking workloads,
//! the 45% scenario-1 gap is BLMPOP-specific. If it shows ferriskey
//! still losing, the gap is universal and the investigation widens.

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

const SCENARIO: &str = "wider_80_20";
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
    // Same shape as baseline/scenario1.rs — no TLS, no auth, no db.
    // The URL form is what redis-rs accepts; `Client::open` parses
    // it lazily, so the actual TCP connect happens on first
    // `get_multiplexed_async_connection()`.
    let url = format!("redis://{host}:{port}/");
    redis::Client::open(url).context("redis::Client::open")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let payload = shared::filler_payload(PAYLOAD_BYTES);
    let keys = Arc::new(shared::key_ring(KEY_COUNT));
    let client = build_client(&args.valkey_host, args.valkey_port)?;

    // Seed every key with the 4 KiB payload so GETs return data, not
    // Nil — same rationale as ferriskey_80_20.rs.
    {
        let mut seeder = client
            .get_multiplexed_async_connection()
            .await
            .context("seeder multiplexed connection")?;
        for k in keys.iter() {
            let _: () = seeder
                .set(k, &payload[..])
                .await
                .context("seed SET")?;
        }
    }

    let barrier = Arc::new(Barrier::new(args.workers + 1));

    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        // One logical connection per worker. `MultiplexedConnection`
        // is cheap to clone but sharing one across 16 workers still
        // serialises commands on the underlying multiplex queue in
        // the same way a single ferriskey Client does — and the
        // ferriskey-baseline fix at 4d89a89 proved that kills
        // throughput. Per-worker connection matches the ferriskey
        // side's "build_client per worker".
        let conn = client
            .get_multiplexed_async_connection()
            .await
            .context("per-worker multiplexed connection")?;
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
        "[wider-redis-80-20] warmup {}s + run {}s, {} workers",
        args.warmup_secs, args.duration_secs, args.workers
    );

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
        "redis-rs wider = MultiplexedConnection per worker + 80% GET / 20% SET on 4 KiB \
         values, {wider} hash-tagged ring. No blocking commands. Paired with \
         ferriskey_80_20 for apples-to-apples; any delta is client overhead under \
         non-blocking RESP2 traffic.",
    )?;
    eprintln!(
        "[wider-redis-80-20] wrote {} — {:.1} ops/sec p50={:.3}ms p99={:.3}ms errs={}",
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
    mut conn: MultiplexedConnection,
    keys: Arc<Vec<String>>,
    payload: Vec<u8>,
    barrier: Arc<Barrier>,
    warmup: Duration,
    run: Duration,
) -> Result<WorkerResult> {
    // Same per-worker seed as ferriskey_80_20.rs so both ports draw
    // the same sequence of GET-or-SET decisions and the same keys —
    // any throughput delta is client overhead, not workload
    // asymmetry.
    let mut rng = StdRng::seed_from_u64(0xB17E_1CEF ^ (wi as u64 * 0x9E3779B1));
    let mut hdr = shared::new_worker_hdr();
    let mut ops: u64 = 0;
    let mut errors: u64 = 0;

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
                let us = t0.elapsed().as_micros() as u64;
                hdr.record(us.clamp(1, 60_000_000)).ok();
                ops += 1;
            }
            Err(_) => errors += 1,
        }
    }
    Ok(WorkerResult { ops, errors, hdr })
}

async fn do_one(
    conn: &mut MultiplexedConnection,
    keys: &[String],
    payload: &[u8],
    rng: &mut StdRng,
) -> Result<()> {
    let key = &keys[rng.random_range(0..keys.len())];
    if rng.random_ratio(80, 100) {
        let _: Option<Vec<u8>> = conn.get(key).await.context("GET")?;
    } else {
        let _: () = conn.set(key, payload).await.context("SET")?;
    }
    Ok(())
}
