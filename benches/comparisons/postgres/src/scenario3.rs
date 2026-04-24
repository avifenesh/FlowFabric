//! Postgres comparison — scenario 3 (steady-state, time-boxed).
//!
//! Mirrors the Valkey-side long_running bench: N workers drain a
//! continuously-refilled queue for a bounded wall. Each time we drop
//! below REFILL_LOW, top up by REFILL_BATCH. Reports ops/sec averaged
//! over the window + p50/p95/p99 of per-task latency.
//!
//! Default duration = 120s (2 min), half the Valkey 5-min bench. Longer
//! runs can be configured via --duration-secs. The watchdog is 6h so a
//! full 5-min run is feasible; the default is kept short so a
//! reviewer's CI walk-through doesn't burn an hour.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use common::*;
use ff_core::backend::CapabilitySet;
use uuid::Uuid;

const SCENARIO: &str = "long_running";
const SYSTEM: &str = "flowfabric-postgres";
const REFILL_LOW: usize = 500;
const REFILL_BATCH: usize = 1_000;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 120)]
    duration_secs: u64,
    #[arg(long, default_value_t = 100)]
    workers: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let pool = connect_and_migrate((args.workers as u32 + 8).min(64)).await?;
    let backend = backend(pool.clone());
    let lane = format!("bench-s3-{}", Uuid::new_v4());
    let lane_id = ff_core::types::LaneId::new(lane.clone());
    bulk_seed(&pool, &lane, &[], REFILL_BATCH).await?;
    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(args.workers);
    for wi in 0..args.workers {
        let backend = backend.clone();
        let lane_id = lane_id.clone();
        let stop = stop.clone();
        let completed = completed.clone();
        let worker = format!("w-s3-{wi}");
        handles.push(tokio::spawn(async move {
            let mut local: Vec<u64> = Vec::new();
            let caps = CapabilitySet::default();
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                match backend.claim(&lane_id, &caps, claim_policy(&worker, 30_000)).await {
                    Ok(Some(h)) => {
                        if backend.complete(&h, None).await.is_ok() {
                            local.push(t0.elapsed().as_micros() as u64);
                            completed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            Ok::<Vec<u64>, anyhow::Error>(local)
        }));
    }
    // Refiller task.
    let pool_r = pool.clone();
    let lane_r = lane.clone();
    let stop_r = stop.clone();
    let refill = tokio::spawn(async move {
        while !stop_r.load(Ordering::Relaxed) {
            let depth: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM ff_exec_core WHERE lane_id=$1 AND lifecycle_phase='runnable' AND eligibility_state='eligible_now'")
                .bind(&lane_r).fetch_one(&pool_r).await.unwrap_or(0);
            if depth < REFILL_LOW as i64 {
                let _ = bulk_seed(&pool_r, &lane_r, &[], REFILL_BATCH).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    tokio::time::sleep_until(deadline.into()).await;
    stop.store(true, Ordering::Relaxed);
    let _ = refill.await;
    let mut latencies: Vec<u64> = Vec::new();
    for h in handles { if let Ok(Ok(v)) = h.await { latencies.extend(v); } }
    let total = completed.load(Ordering::Relaxed);
    let throughput = total as f64 / args.duration_secs as f64;
    let (p50, p95, p99) = percentiles(&latencies);
    let report = serde_json::json!({
        "scenario": SCENARIO, "system": SYSTEM,
        "git_sha": ff_bench::git_sha(), "host": ff_bench::host_info(),
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": { "duration_secs": args.duration_secs, "workers": args.workers, "backend": "postgres", "refill_batch": REFILL_BATCH, "refill_low": REFILL_LOW },
        "total_completed": total, "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "Steady-state drain with refiller topping up when depth falls below REFILL_LOW. Time-boxed (default 120s, half the Valkey 5-min bench; raise via --duration-secs if the host budget allows).",
    });
    let path = write_report(&args.results_dir, SCENARIO, report)?;
    eprintln!("[pg-s3] wrote {} — {:.1} ops/sec over {}s (total {})", path.display(), throughput, args.duration_secs, total);
    Ok(())
}
