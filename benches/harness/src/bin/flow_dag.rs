//! Scenario 4 — linear flow DAG (custom harness).
//!
//! Drives N concurrent chains of M nodes each against a running
//! FlowFabric cluster. Requires:
//!   * `ff-server` listening on `FF_BENCH_SERVER` (default
//!     `http://localhost:9090`).
//!   * Server started with `FF_LANES=default,bench` so the unblock
//!     scanner covers the lane this bench uses.
//!   * `ff-bench-echo` workers spawned inside the binary (no external
//!     worker process required).
//!
//! Emits one JSON report to `benches/results/flow_dag_linear-<sha>.json`
//! using the shared `ff_bench::comparison::run_scenario` envelope.
//! Report `notes` contains the scenario-4-specific metrics as a
//! `key=value,…` string (see `runners::flowfabric::format_notes`).

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use ff_bench::comparison::{run_scenario, ComparisonRunner, RunConfig, Scenario};
use ff_bench::runners::flowfabric::FlowFabricRunner;
use ff_bench::workload::BenchEnv;
use ff_bench::write_report;

#[derive(Parser, Debug)]
#[command(name = "flow_dag", about = "Scenario 4: linear flow DAG benchmark")]
struct Args {
    /// Number of parallel flows.
    #[arg(long, default_value_t = 100)]
    flows: usize,

    /// Chain length — number of nodes per flow.
    #[arg(long, default_value_t = 10)]
    nodes: usize,

    /// Worker pool size (echo workers claiming from the bench lane).
    #[arg(long, default_value_t = 16)]
    workers: usize,

    /// Where to write the JSON report.
    #[arg(long, default_value = "benches/results")]
    results_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flow_dag=info,ff_bench=info".into()),
        )
        .init();

    let args = Args::parse();
    let env = BenchEnv::from_env();
    let config = RunConfig {
        tasks: args.flows * args.nodes,
        workers: args.workers,
        payload_bytes: 0,
        nodes: args.nodes,
        flows: args.flows,
    };

    let mut runner = FlowFabricRunner::new();
    runner.setup(&env).await?;
    let report = run_scenario(&mut runner, Scenario::FlowDagLinear, &config, &env).await?;
    runner.teardown(&env).await?;

    let path = write_report(&report, &args.results_dir)?;
    println!("[flow_dag] wrote {}", path.display());
    println!(
        "[flow_dag] flows={} nodes={} throughput={:.2} flows/s p50_total_ms={:.2} p99_total_ms={:.2}",
        args.flows, args.nodes, report.throughput_ops_per_sec, report.latency_ms.p50, report.latency_ms.p99,
    );
    if let Some(notes) = &report.notes {
        println!("[flow_dag] notes: {notes}");
    }
    Ok(())
}
