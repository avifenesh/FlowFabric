//! Postgres comparison — quorum cancel fanout (RFC-016 §4.2 SLO gate).
//!
//! Valkey-side uses the full AnyOf/Quorum edge-group machinery. On the
//! Postgres side, create_flow + stage_edge_dependency +
//! apply_dependency_to_child are Valkey-side FCALLs NOT exposed on the
//! EngineBackend trait, so we can't seed a real Quorum(1,n) group.
//! cancel_flow IS implemented as a SERIALIZABLE cascade that flips
//! every non-terminal sibling to cancelled in one tx (3-attempt retry).
//! We seed a flow + N siblings into the FlowId's natural partition
//! (partition_key = flow_partition(FlowId, config).index) so the
//! cancel tx actually matches the rows it scans, then measure the
//! cascade wall. This is a conservative upper bound on the full
//! AnyOf/Quorum+CancelRemaining path once the flow-ingress FCALLs land
//! on the Postgres side.
//!
//! Sizes: {10, 100, 1000}. Ship SLO (§4.2): p99 <= 500 ms at n=100.

mod common;

use std::time::Instant;
use anyhow::{Context, Result};
use clap::Parser;
use common::*;
use ff_core::backend::{CancelFlowPolicy, CancelFlowWait};
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::types::FlowId;
use sqlx::PgPool;
use uuid::Uuid;

const SCENARIO: &str = "quorum_cancel_fanout";
const SYSTEM: &str = "flowfabric-postgres";

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 10)]
    samples: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

async fn seed_flow_with_siblings(pool: &PgPool, n: usize) -> Result<FlowId> {
    let flow_uuid = Uuid::new_v4();
    let flow_id = FlowId::from_uuid(flow_uuid);
    let part: i16 = flow_partition(&flow_id, &PartitionConfig::default()).index as i16;
    let lane = format!("qcf-{flow_uuid}");
    sqlx::query(
        "INSERT INTO ff_flow_core (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms) \
         VALUES ($1, $2, 0, 'runnable', $3)"
    ).bind(part).bind(flow_uuid).bind(now_ms()).execute(pool).await.context("seed flow_core")?;
    let created = now_ms();
    let mut q = String::from(
        "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, required_capabilities, attempt_index, lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, priority, created_at_ms) VALUES ",
    );
    let ids: Vec<Uuid> = (0..n).map(|_| Uuid::new_v4()).collect();
    for i in 0..n {
        if i > 0 { q.push(','); }
        let b = i * 2;
        q.push_str(&format!(
            "({}, ${}, '{}', ${}, '{{}}', 0, 'runnable', 'unowned', 'eligible_now', 'waiting', 'pending_first_attempt', 0, {})",
            part, b + 1, flow_uuid, b + 2, created,
        ));
    }
    let mut stmt = sqlx::query(&q);
    for id in &ids {
        stmt = stmt.bind(id).bind(&lane);
    }
    stmt.execute(pool).await.context("seed siblings")?;
    Ok(flow_id)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let pool = connect_and_migrate(8).await?;
    let backend = backend(pool.clone());
    let sizes = [10usize, 100, 1000];
    let mut per_size: Vec<(usize, Vec<u64>, Vec<usize>)> = Vec::new();
    for &n in &sizes {
        eprintln!("qcf: n={n} samples={}", args.samples);
        let mut samples_us: Vec<u64> = Vec::with_capacity(args.samples);
        let mut member_counts: Vec<usize> = Vec::with_capacity(args.samples);
        for s in 0..args.samples {
            let flow_id = seed_flow_with_siblings(&pool, n).await?;
            let t0 = Instant::now();
            let res = backend.cancel_flow(&flow_id, CancelFlowPolicy::CancelAll, CancelFlowWait::NoWait).await.context("cancel_flow")?;
            let elapsed = t0.elapsed();
            samples_us.push(elapsed.as_micros() as u64);
            let members = match &res {
                ff_core::contracts::CancelFlowResult::Cancelled { member_execution_ids, .. } => member_execution_ids.len(),
                _ => 0,
            };
            member_counts.push(members);
            eprintln!("  n={n} sample {}/{}: {:.2}ms members_cancelled={}", s+1, args.samples, elapsed.as_secs_f64()*1000.0, members);
        }
        per_size.push((n, samples_us, member_counts));
    }
    for (n, samples, members) in &per_size {
        let (p50, p95, p99) = percentiles(samples);
        let avg_members = if members.is_empty() { 0.0 } else { members.iter().sum::<usize>() as f64 / members.len() as f64 };
        let report = serde_json::json!({
            "scenario": SCENARIO, "system": SYSTEM,
            "git_sha": ff_bench::git_sha(), "host": ff_bench::host_info(),
            "timestamp_utc": ff_bench::iso8601_utc(),
            "config": { "n": n, "samples": samples.len(), "backend": "postgres" },
            "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
            "avg_members_cancelled": avg_members,
            "slo_target_ms_p99_at_n100": 500,
            "slo_verdict": if *n == 100 { if p99 <= 500.0 { "PASS" } else { "FAIL" } } else { "characterization-only" },
            "notes": "Approximation via cancel_flow cascade. Upper bound on the full AnyOf/Quorum+CancelRemaining path once flow-ingress FCALLs (create_flow + stage_edge_dependency + apply_dependency_to_child) land on the PG backend. avg_members_cancelled verifies the cascade actually fired.",
        });
        let path = write_report(&args.results_dir, &format!("{SCENARIO}-n{n}"), report)?;
        eprintln!("[pg-qcf] n={n} wrote {} — p99={:.2}ms avg_members={}", path.display(), p99, avg_members);
    }
    Ok(())
}
