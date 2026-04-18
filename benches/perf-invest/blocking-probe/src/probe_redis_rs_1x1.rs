//! redis-rs — 1 client × 1 BLMPOP, no concurrency.
//!
//! Companion to probe-ferriskey-1x1. Same workload shape, same key,
//! same ITERATIONS count. Uses `get_multiplexed_async_connection()`
//! because that's the equivalent path a consumer of redis-rs picks
//! for concurrent async workloads — and it's what the baseline bench
//! uses.
//!
//! Overrides the 500 ms DEFAULT_RESPONSE_TIMEOUT (redis-1.2.0/src/
//! client.rs:180) so the blocking-command path gets a fair window.
//! Without the override, `query_async` on BLMPOP fails at 500 ms
//! client-side even though the server would return later.
//!
//! Usage:
//!   redis-cli FLUSHALL && redis-cli LPUSH bench:blocking-list A
//!   cargo run --release --bin probe-redis-rs-1x1

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use redis::AsyncCommands;

const KEY: &str = "bench:blocking-list";
const ITERATIONS: usize = 100;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let client = redis::Client::open("redis://127.0.0.1:6379/").context("redis Client::open")?;
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .context("get_multiplexed_async_connection")?;
    conn.set_response_timeout(Duration::from_secs(7));

    // Warmup
    let _: String = redis::cmd("PING").query_async(&mut conn).await.context("PING")?;

    // Seed
    for _ in 0..ITERATIONS {
        let _: i64 = conn.lpush(KEY, "ITEM").await.context("seed LPUSH")?;
    }

    let mut wall_us: Vec<u64> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let t0 = Instant::now();
        let _: redis::Value = redis::cmd("BLMPOP")
            .arg(5_u64)
            .arg(1_u64)
            .arg(KEY)
            .arg("LEFT")
            .query_async(&mut conn)
            .await
            .context("BLMPOP")?;
        wall_us.push(t0.elapsed().as_micros() as u64);
    }

    report("redis-rs-1x1", wall_us);
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
