//! redis-rs — 1 MultiplexedConnection × 1 worker × 100_000 × INCR.
//!
//! Companion to probe-ferriskey. Same workload shape (KEY, WARMUP,
//! ITERATIONS, command, argv), same runtime flavour. Uses
//! `get_multiplexed_async_connection()` — the redis-rs path a
//! consumer picks for async concurrent workloads (same as the
//! redis-rs baseline in benches/comparisons/baseline/).
//!
//! `set_response_timeout(30s)` overrides the 500 ms
//! DEFAULT_RESPONSE_TIMEOUT so the probe can't be blamed on
//! redis-rs's short-window default. INCR on localhost is sub-
//! millisecond; this override only matters if the box stalls.
//!
//! Usage:
//!   redis-cli FLUSHALL
//!   cargo flamegraph --profile perf --bin probe-redis-rs -o <dir>/probe-redis-rs.svg

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const KEY: &str = "bench:envelope-counter";
const WARMUP: usize = 1_000;
const ITERATIONS: usize = 100_000;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let client = redis::Client::open("redis://127.0.0.1:6379/").context("redis Client::open")?;
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .context("get_multiplexed_async_connection")?;
    conn.set_response_timeout(Duration::from_secs(30));

    // Warmup
    for _ in 0..WARMUP {
        let _v: i64 = redis::cmd("INCR")
            .arg(KEY)
            .query_async(&mut conn)
            .await
            .context("warmup INCR")?;
    }

    let mut lat_us: Vec<u64> = Vec::with_capacity(ITERATIONS);

    // ── captured window ────────────────────────────────────────────
    for _ in 0..ITERATIONS {
        let t0 = Instant::now();
        let _v: i64 = redis::cmd("INCR")
            .arg(KEY)
            .query_async(&mut conn)
            .await
            .context("INCR")?;
        lat_us.push(t0.elapsed().as_micros() as u64);
    }

    let ok = lat_us.len() as u64;
    report("redis-rs", lat_us, ok, 0, 0);
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
