//! Postgres comparison — scenario 5 (capability-routed claim).
//!
//! 10 distinct capability sets, one worker pool per set, 1000 tasks
//! seeded per set. Each worker advertises its set's caps + pulls tasks
//! requiring that set. Measures claim-path overhead under
//! capability-filtering (happy-mode only; mismatched-mode was a
//! characterisation-only mode on the Valkey side and is skipped here).

mod common;

use std::sync::Arc;
use std::time::Instant;
use anyhow::Result;
use clap::Parser;
use common::*;
use uuid::Uuid;

const SCENARIO: &str = "cap_routed";
const SYSTEM: &str = "flowfabric-postgres";
const CAP_SETS: usize = 10;
const TASKS_PER_SET: usize = 1000;
const WORKERS_PER_SET: usize = 4;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let pool = connect_and_migrate((CAP_SETS * WORKERS_PER_SET + 4) as u32).await?;
    let backend = backend(pool.clone());
    let lane = format!("bench-s5-{}", Uuid::new_v4());
    let lane_id = ff_core::types::LaneId::new(lane.clone());

    // Seed per-cap-set tasks.
    for set_ix in 0..CAP_SETS {
        let cap = format!("cap-{set_ix}");
        bulk_seed(&pool, &lane, &[cap.as_str()], TASKS_PER_SET).await?;
    }
    let total = CAP_SETS * TASKS_PER_SET;
    let drained = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let start = Instant::now();
    let mut handles = Vec::new();
    for set_ix in 0..CAP_SETS {
        for w in 0..WORKERS_PER_SET {
            let backend = backend.clone();
            let lane_id = lane_id.clone();
            let drained = drained.clone();
            let cap = format!("cap-{set_ix}");
            let worker = format!("w-s5-{set_ix}-{w}");
            handles.push(tokio::spawn(async move {
                let caps = capset(&[cap.as_str()]);
                let mut local: Vec<u64> = Vec::new();
                loop {
                    if drained.load(std::sync::atomic::Ordering::Relaxed) >= total { break; }
                    let t0 = Instant::now();
                    match backend.claim(&lane_id, &caps, claim_policy(&worker, 30_000)).await {
                        Ok(Some(h)) => {
                            backend.complete(&h, None).await?;
                            local.push(t0.elapsed().as_micros() as u64);
                            drained.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                    }
                }
                Ok::<Vec<u64>, anyhow::Error>(local)
            }));
        }
    }
    let mut latencies: Vec<u64> = Vec::with_capacity(total);
    for h in handles { latencies.extend(h.await??); }
    let wall = start.elapsed();
    let throughput = total as f64 / wall.as_secs_f64();
    let (p50, p95, p99) = percentiles(&latencies);
    let report = serde_json::json!({
        "scenario": SCENARIO, "system": SYSTEM,
        "git_sha": ff_bench::git_sha(), "host": ff_bench::host_info(),
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": { "cap_sets": CAP_SETS, "tasks_per_set": TASKS_PER_SET, "workers_per_set": WORKERS_PER_SET, "backend": "postgres" },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "Capability subset-match runs in Rust after the FOR UPDATE SKIP LOCKED scan (see attempt.rs module comment). This bench exercises the happy-mode path where every claim succeeds.",
    });
    let path = write_report(&args.results_dir, SCENARIO, report)?;
    eprintln!("[pg-s5] wrote {} — {:.1} ops/sec p99={:.2}ms", path.display(), throughput, p99);
    Ok(())
}
