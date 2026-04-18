//! ferriskey — 1 client × 1 BLMPOP, no concurrency.
//!
//! Isolates per-command cost from mux-concurrency behavior. If this
//! probe shows a materially different return time from redis-rs's
//! equivalent (probe-redis-rs-1x1), the gap is per-command (timeout
//! wrapper, client-side alloc). If both are near-identical at 1
//! concurrent task, the 46% gap is concurrency-mux behavior.
//!
//! Pre-state: FLUSHALL + `LPUSH bench:blocking-list A` (1 item).
//! Then issue BLMPOP with 5s timeout, expect an immediate return
//! (item is there). We're NOT measuring the blocked path here — that's
//! the 5-task probe. This is the no-op fast path: how much client
//! overhead is there when the server returns instantly?
//!
//! Usage:
//!   redis-cli FLUSHALL && redis-cli LPUSH bench:blocking-list A
//!   cargo run --release --bin probe-ferriskey-1x1

use std::time::Instant;

use anyhow::{Context, Result};
use ferriskey::{ClientBuilder, Cmd};

const KEY: &str = "bench:blocking-list";
const ITERATIONS: usize = 100;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let client = ClientBuilder::new()
        .host("localhost", 6379)
        .build()
        .await
        .context("ferriskey connect")?;

    // Preflight: warm the client (lazy init path) so the first iter
    // isn't dominated by connection setup.
    let mut warmup = Cmd::new();
    warmup.arg("PING");
    let _: ferriskey::Value = client.cmd("PING").execute().await.context("PING warmup")?;

    // Seed: need one item per iter so the BLMPOP returns fast.
    for _ in 0..ITERATIONS {
        let _: ferriskey::Value = client
            .cmd("LPUSH")
            .arg(KEY)
            .arg("ITEM")
            .execute()
            .await
            .context("seed LPUSH")?;
    }

    let mut wall_us: Vec<u64> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let t0 = Instant::now();
        // BLMPOP takes timeout, numkeys, keys..., side, [COUNT n]
        let _: ferriskey::Value = client
            .cmd("BLMPOP")
            .arg("5")
            .arg("1")
            .arg(KEY)
            .arg("LEFT")
            .execute()
            .await
            .context("BLMPOP")?;
        wall_us.push(t0.elapsed().as_micros() as u64);
    }

    report("ferriskey-1x1", wall_us);
    Ok(())
}

fn report(label: &str, mut wall_us: Vec<u64>) {
    wall_us.sort_unstable();
    let n = wall_us.len();
    let p = |q: f64| wall_us[((n as f64 - 1.0) * q).round() as usize] as f64 / 1000.0;
    println!(
        "{label} n={n} p50={:.3}ms p95={:.3}ms p99={:.3}ms min={:.3}ms max={:.3}ms mean={:.3}ms",
        p(0.50),
        p(0.95),
        p(0.99),
        wall_us[0] as f64 / 1000.0,
        wall_us[n - 1] as f64 / 1000.0,
        wall_us.iter().sum::<u64>() as f64 / (n as f64 * 1000.0),
    );
}
