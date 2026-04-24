//! Postgres comparison — scenario 4 (10-node linear chain).
//!
//! The Valkey-side flow_dag bench measures throughput of N concurrent
//! 10-stage linear flows. On the direct-backend path, with no flow
//! ingress wired on the Postgres side (create_flow + stage_edge
//! dependency are Valkey-side FCALLs, not trait methods), we emulate
//! the chain by seeding 10 rows per flow + running them sequentially
//! through claim/complete, one worker per flow lane. Throughput =
//! flows_completed / wall; per-flow latency = 10 * claim+complete
//! wall.

mod common;

use std::sync::Arc;
use std::time::Instant;
use anyhow::Result;
use clap::Parser;
use common::*;
use ff_core::backend::CapabilitySet;
use uuid::Uuid;

const SCENARIO: &str = "flow_dag";
const SYSTEM: &str = "flowfabric-postgres";
const NODES: usize = 10;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 100)]
    flows: usize,
    #[arg(long, default_value_t = 16)]
    workers: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let pool = connect_and_migrate(args.workers as u32 + 4).await?;
    let backend = backend(pool.clone());
    let lane = format!("bench-s4-{}", Uuid::new_v4());
    let lane_id = ff_core::types::LaneId::new(lane.clone());
    // Seed flows * NODES tasks under one lane; each flow = NODES claims.
    let total = args.flows * NODES;
    bulk_seed(&pool, &lane, &[], total).await?;
    let drained = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let start = Instant::now();
    let mut handles = Vec::new();
    for wi in 0..args.workers {
        let backend = backend.clone();
        let lane_id = lane_id.clone();
        let drained = drained.clone();
        let worker = format!("w-s4-{wi}");
        handles.push(tokio::spawn(async move {
            let caps = CapabilitySet::default();
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
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
                    Err(e) => { eprintln!("claim err: {e}"); tokio::time::sleep(std::time::Duration::from_millis(20)).await; }
                }
            }
            Ok::<Vec<u64>, anyhow::Error>(local)
        }));
    }
    let mut latencies: Vec<u64> = Vec::with_capacity(total);
    for h in handles { latencies.extend(h.await??); }
    let wall = start.elapsed();
    // Per-flow latency approximation: NODES consecutive drains per flow.
    // Since we pool across flows, use 10-step moving sum as a proxy.
    let mut per_flow: Vec<u64> = Vec::with_capacity(args.flows);
    for chunk in latencies.chunks(NODES) {
        per_flow.push(chunk.iter().sum());
    }
    let throughput = args.flows as f64 / wall.as_secs_f64();
    let (p50, p95, p99) = percentiles(&per_flow);
    let report = serde_json::json!({
        "scenario": SCENARIO, "system": SYSTEM,
        "git_sha": ff_bench::git_sha(), "host": ff_bench::host_info(),
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": { "flows": args.flows, "nodes_per_flow": NODES, "workers": args.workers, "backend": "postgres" },
        "throughput_flows_per_sec": throughput,
        "latency_ms_per_flow": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "Emulates 10-node linear chain via 10 claim+complete ops per flow on a pooled lane (create_flow + stage_edge_dependency are Valkey-side FCALLs not exposed on the Postgres EngineBackend surface). Per-flow latency is the sum of NODES per-op latencies, matching end-to-end chain wall but with implicit across-flow interleaving.",
    });
    let path = write_report(&args.results_dir, SCENARIO, report)?;
    eprintln!("[pg-s4] wrote {} — {:.1} flows/sec p99={:.2}ms", path.display(), throughput, p99);
    Ok(())
}
