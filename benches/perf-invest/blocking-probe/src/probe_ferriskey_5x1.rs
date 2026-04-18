//! ferriskey — 5 cloned clients × 1 concurrent BLMPOP each.
//!
//! Exercises the mux under blocking-command concurrency. Pre-state:
//! list has 2 items; 5 BLMPOPs compete. 2 should return immediately
//! with values, 3 should block up to 5s server-side timeout then
//! return Nil.
//!
//! If the mux truly multiplexes: ALL 5 finish within ~5.5s wall,
//! 2 early-return, 3 return at ~5s.
//! If the mux serializes (head-of-line blocking): we see a staircase
//! — first two drain fast, later three progressively add the
//! timeout of the prior BLMPOP.
//!
//! Usage:
//!   redis-cli FLUSHALL && redis-cli LPUSH bench:blocking-list A B
//!   cargo run --release --bin probe-ferriskey-5x1

use std::time::Instant;

use anyhow::{Context, Result};
use ferriskey::ClientBuilder;

const KEY: &str = "bench:blocking-list";
const N_TASKS: usize = 5;
const TIMEOUT_S: u64 = 5;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    // ONE Client (the ferriskey public handle is Clone — each `clone`
    // shares the Arc<RwLock<ClientWrapper>> but runs its own
    // send_command future). That's the apples-to-apples to redis-rs's
    // MultiplexedConnection (Clone via pipeline.sender mpsc handle).
    let client = ClientBuilder::new()
        .host("localhost", 6379)
        .request_timeout(std::time::Duration::from_secs(TIMEOUT_S + 2))
        .build()
        .await
        .context("ferriskey connect")?;

    // Warmup
    let _: ferriskey::Value = client.cmd("PING").execute().await.context("PING warmup")?;

    // 5 tasks, each owning a clone of the Client. Spawn them close
    // together so they all race the same BLMPOP window.
    let started_at = Instant::now();
    let mut handles = Vec::with_capacity(N_TASKS);
    for i in 0..N_TASKS {
        let c = client.clone();
        handles.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let result: Result<ferriskey::Value, _> = c
                .cmd("BLMPOP")
                .arg(TIMEOUT_S.to_string().as_str())
                .arg("1")
                .arg(KEY)
                .arg("LEFT")
                .execute()
                .await;
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            let wall_since_start_ms = started_at.elapsed().as_millis() as u64;
            let shape = match result {
                Ok(ferriskey::Value::Nil) => "nil",
                Ok(ferriskey::Value::Array(_)) => "array",
                Ok(_) => "other-ok",
                Err(_) => "err",
            };
            (i, elapsed_ms, wall_since_start_ms, shape.to_string())
        }));
    }

    let mut results: Vec<(usize, u64, u64, String)> = Vec::with_capacity(N_TASKS);
    for h in handles {
        results.push(h.await.expect("task join"));
    }

    // Sort by task index for readable output.
    results.sort_by_key(|r| r.0);
    println!("ferriskey-5x1 per-task:");
    for (i, elapsed, wall, shape) in &results {
        println!(
            "  task={i} elapsed_ms={elapsed:>5} wall_from_spawn_ms={wall:>5} result={shape}"
        );
    }
    let max_wall = results.iter().map(|r| r.2).max().unwrap_or(0);
    let min_wall = results.iter().map(|r| r.2).min().unwrap_or(0);
    println!("ferriskey-5x1 spread: min_wall={min_wall}ms max_wall={max_wall}ms");
    Ok(())
}
