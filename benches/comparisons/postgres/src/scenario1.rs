//! Postgres comparison — scenario 1 (submit → claim → complete
//! throughput). Mirror of benches/comparisons/apalis/src/scenario1.rs
//! and benches/comparisons/ferriskey-baseline/src/scenario1.rs.
//!
//! Direct-backend shape: bulk-seed N exec rows via one multi-row INSERT
//! per chunk (analogous to the RPUSH-chunked seed in the Valkey
//! baselines), then drive WORKER_COUNT workers through
//! `EngineBackend::claim` / `complete`. Submit wall excluded from
//! throughput; drain wall (first-claim-seen → last-complete) / N =
//! ops/sec.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use common::*;
use ff_core::backend::CapabilitySet;
use uuid::Uuid;

const SCENARIO: &str = "submit_claim_complete";
const SYSTEM: &str = "flowfabric-postgres";
const WORKER_COUNT: usize = 16;
const PAYLOAD_BYTES: usize = 4 * 1024; // unused on PG path (create via raw SQL w/o payload), tracked for parity

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 10_000)]
    tasks: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let pool = connect_and_migrate(32).await?;
    let lane = format!("bench-s1-{}", Uuid::new_v4());

    // Seed phase — bulk INSERTs, measured separately.
    let submit_start = Instant::now();
    bulk_seed(&pool, &lane, &[], args.tasks).await?;
    let submit_wall = submit_start.elapsed();
    eprintln!("[pg-s1] seeded {} tasks in {} ms", args.tasks, submit_wall.as_millis());

    // Drain phase — 16 workers claim+complete in parallel.
    let backend = backend(pool.clone());
    let lane_id = ff_core::types::LaneId::new(lane);
    let caps = CapabilitySet::default();
    let drained = Arc::new(AtomicUsize::new(0));
    let target = args.tasks;

    let drain_start = Instant::now();
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let backend = backend.clone();
        let lane_id = lane_id.clone();
        let caps = caps.clone();
        let drained = drained.clone();
        let worker = format!("w-s1-{wi}");
        handles.push(tokio::spawn(async move {
            let mut local: Vec<u64> = Vec::new();
            loop {
                if drained.load(Ordering::Relaxed) >= target {
                    return Ok::<Vec<u64>, anyhow::Error>(local);
                }
                let policy = claim_policy(&worker, 30_000);
                let t0 = Instant::now();
                match backend.claim(&lane_id, &caps, policy).await? {
                    Some(handle) => {
                        backend.complete(&handle, None).await?;
                        local.push(t0.elapsed().as_micros() as u64);
                        drained.fetch_add(1, Ordering::Relaxed);
                    }
                    None => {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                }
            }
        }));
    }
    let mut latencies: Vec<u64> = Vec::with_capacity(target);
    for h in handles {
        latencies.extend(h.await??);
    }
    let wall = drain_start.elapsed();
    let throughput = args.tasks as f64 / wall.as_secs_f64();
    let (p50, p95, p99) = percentiles(&latencies);

    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": ff_bench::git_sha(),
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": {
            "tasks": args.tasks,
            "workers": WORKER_COUNT,
            "payload_bytes": PAYLOAD_BYTES,
            "backend": "postgres",
        },
        "submit_wall_ms": submit_wall.as_secs_f64() * 1000.0,
        "drain_wall_ms": wall.as_secs_f64() * 1000.0,
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "Direct EngineBackend::claim/complete path (no ff-server, no ff-sdk polling). Raw-SQL bulk seed matches the chunked-RPUSH shape of the Valkey baselines; exec rows carry zero-byte payload since the PG claim path does not exercise payload storage at this layer.",
    });
    let path = write_report(&args.results_dir, SCENARIO, report)?;
    eprintln!("[pg-s1] wrote {} — {:.1} ops/sec p99={:.2}ms", path.display(), throughput, p99);
    Ok(())
}
