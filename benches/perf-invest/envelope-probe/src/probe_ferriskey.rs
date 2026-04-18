//! ferriskey — 1 client × 1 worker × 100_000 × INCR.
//!
//! Track-A envelope probe. Isolates per-command envelope cost on the
//! post-round-3 ferriskey HEAD. INCR is:
//!
//!   * non-blocking (no BLMPOP head-of-line effects)
//!   * scalar integer reply (no value_conversion nested-array walk)
//!   * fast server-side (tens of microseconds)
//!
//! Result: everything on this bin's hot path that ISN'T TCP round-trip
//! is envelope. Compare frames against `probe-redis-rs` flame for the
//! delta.
//!
//! Usage:
//!   redis-cli FLUSHALL
//!   cargo flamegraph --profile perf --bin probe-ferriskey -o <dir>/probe-ferriskey.svg

use std::time::Instant;

use anyhow::{Context, Result};
use ferriskey::ClientBuilder;

const KEY: &str = "bench:envelope-counter";
const WARMUP: usize = 1_000;
const ITERATIONS: usize = 100_000;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let client = ClientBuilder::new()
        .host("localhost", 6379)
        .build()
        .await
        .context("ferriskey connect")?;

    // Warmup: prime the connection, allocator pools, and any lazy
    // initialisation paths. Discarded.
    for _ in 0..WARMUP {
        let _v: i64 = client
            .cmd("INCR")
            .arg(KEY)
            .execute()
            .await
            .context("warmup INCR")?;
    }

    // Pre-allocate so there's no Vec resize inside the timed loop.
    let mut lat_us: Vec<u64> = Vec::with_capacity(ITERATIONS);

    // ── captured window ────────────────────────────────────────────
    for _ in 0..ITERATIONS {
        let t0 = Instant::now();
        let _v: i64 = client
            .cmd("INCR")
            .arg(KEY)
            .execute()
            .await
            .context("INCR")?;
        lat_us.push(t0.elapsed().as_micros() as u64);
    }

    let mut ok = 0u64;
    for _ in &lat_us {
        ok += 1;
    }
    report("ferriskey", lat_us, ok, 0, 0);
    Ok(())
}

fn report(label: &str, mut wall_us: Vec<u64>, ok: u64, nil: u64, err: u64) {
    wall_us.sort_unstable();
    let n = wall_us.len();
    if n == 0 {
        println!("{label} n=0 (no iterations captured)");
        return;
    }
    let p = |q: f64| wall_us[((n as f64 - 1.0) * q).round() as usize] as f64 / 1000.0;
    let mean_ms = wall_us.iter().sum::<u64>() as f64 / (n as f64 * 1000.0);
    println!(
        "{label} n={n} ok={ok} nil={nil} err={err} \
         p50={:.3}ms p95={:.3}ms p99={:.3}ms p999={:.3}ms \
         min={:.3}ms max={:.3}ms mean={:.3}ms",
        p(0.50),
        p(0.95),
        p(0.99),
        p(0.999),
        wall_us[0] as f64 / 1000.0,
        wall_us[n - 1] as f64 / 1000.0,
        mean_ms,
    );
}
