//! redis-rs — 5 cloned MultiplexedConnection × 1 concurrent BLMPOP each.
//!
//! Companion to probe-ferriskey-5x1. Same pre-state, same workload.
//!
//! IMPORTANT: redis-rs's `get_multiplexed_async_connection()` applies
//! a client-side DEFAULT_RESPONSE_TIMEOUT of 500ms
//! (redis-1.2.0/src/client.rs:180). For blocking commands to survive
//! past 500ms, we MUST call `set_response_timeout(Duration::from_secs(N))`
//! with N > the BLMPOP timeout. Without that, every BLMPOP whose
//! server block lasts >500ms returns a client-side timeout error —
//! making the comparison invalid.
//!
//! Usage:
//!   redis-cli FLUSHALL && redis-cli LPUSH bench:blocking-list A B
//!   cargo run --release --bin probe-redis-rs-5x1

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const KEY: &str = "bench:blocking-list";
const N_TASKS: usize = 5;
const TIMEOUT_S: u64 = 5;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let client = redis::Client::open("redis://127.0.0.1:6379/").context("redis Client::open")?;
    // ONE MultiplexedConnection, cloned per task. MUST override the
    // 500ms default response_timeout so blocking commands aren't
    // killed client-side before the server returns.
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .context("get_multiplexed_async_connection")?;
    conn.set_response_timeout(Duration::from_secs(TIMEOUT_S + 2));

    // Warmup
    {
        let mut c = conn.clone();
        let _: String = redis::cmd("PING").query_async(&mut c).await.context("PING")?;
    }

    let started_at = Instant::now();
    let mut handles = Vec::with_capacity(N_TASKS);
    for i in 0..N_TASKS {
        let mut c = conn.clone();
        handles.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let result: redis::RedisResult<redis::Value> = redis::cmd("BLMPOP")
                .arg(TIMEOUT_S)
                .arg(1_u64)
                .arg(KEY)
                .arg("LEFT")
                .query_async(&mut c)
                .await;
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            let wall_since_start_ms = started_at.elapsed().as_millis() as u64;
            let shape = match result {
                Ok(redis::Value::Nil) => "nil".to_string(),
                Ok(redis::Value::Array(_)) => "array".to_string(),
                Ok(_) => "other-ok".to_string(),
                Err(e) => format!("err:{:?}", e.kind()),
            };
            (i, elapsed_ms, wall_since_start_ms, shape)
        }));
    }

    let mut results: Vec<(usize, u64, u64, String)> = Vec::with_capacity(N_TASKS);
    for h in handles {
        results.push(h.await.expect("task join"));
    }

    results.sort_by_key(|r| r.0);
    println!("redis-rs-5x1 per-task:");
    for (i, elapsed, wall, shape) in &results {
        println!(
            "  task={i} elapsed_ms={elapsed:>5} wall_from_spawn_ms={wall:>5} result={shape}"
        );
    }
    let max_wall = results.iter().map(|r| r.2).max().unwrap_or(0);
    let min_wall = results.iter().map(|r| r.2).min().unwrap_or(0);
    println!("redis-rs-5x1 spread: min_wall={min_wall}ms max_wall={max_wall}ms");
    Ok(())
}
