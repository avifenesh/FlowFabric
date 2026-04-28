#![recursion_limit = "512"]
#![type_length_limit = "16777216"]
//! apalis comparison — scenario 4 (linear 10-stage workflow).
//!
//! Uses `apalis-workflow::Workflow` — apalis's first-class sequential
//! workflow primitive (`and_then` combinator). The prior harness
//! hand-rolled the chain by wiring 10 `WorkerBuilder` instances with
//! per-stage inter-queue pushes, which issue #51's maintainer
//! (geofmureithi) flagged as under-representing apalis. See Worker VV's
//! investigation at `rfcs/drafts/apalis-comparison.md`.
//!
//! # Shape
//!
//! A single `Workflow::new("…").and_then(stage0)…and_then(stage9)`
//! composed over ONE `RedisStorage` backend and ONE `WorkerBuilder`
//! (concurrency 16). Each stage is a trivial async fn forwarding the
//! `u64` flow_id; `stage_terminal` INCRs the
//! `bench:apalis:flow_completed` counter. The driver seeds `--flows`
//! via `WorkflowSink::push_start` and drains by polling the counter
//! — same drain protocol as the prior harness so the only moving part
//! is the engine-side primitive.
//!
//! Note: `u64` is used as the inter-stage payload rather than a custom
//! struct because `apalis-workflow`'s `DagCodec`/workflow plumbing
//! requires primitive types (or user types with a `DagCodec` impl).
//! The prior harness also only threaded a `flow_id` token (no
//! data_passing), so this preserves the measurement contract.
//!
//! # What this measures
//!
//! Throughput = flows / drain_wall for a 10-stage linear chain using
//! apalis's idiomatic workflow primitive. State persists per-step in
//! the same Redis storage rather than round-tripping across 10 typed
//! queues.
//!
//! # NOT comparable
//!
//! Same caveats as before (no data_passing payload, no per-flow
//! latency, poll-jitter floor of 50 ms). Documented in COMPARISON.md.

use std::time::{Duration, Instant};

use anyhow::Result;
use apalis::prelude::*;
use apalis_redis::{RedisConfig, RedisStorage};
use apalis_workflow::{Workflow, WorkflowSink};
use clap::Parser;
use redis::AsyncCommands;
use redis::aio::MultiplexedConnection;

const SCENARIO: &str = "flow_dag";
const SYSTEM: &str = "apalis";
const STAGES: usize = 10;
const WORKER_COUNT: usize = 16;
const COMPLETED_KEY: &str = "bench:apalis:flow_completed";
const POLL_INTERVAL_MS: u64 = 5;
const BUFFER_SIZE: usize = 100;

fn tuned_config() -> RedisConfig {
    RedisConfig::default()
        .set_poll_interval(Duration::from_millis(POLL_INTERVAL_MS))
        .set_buffer_size(BUFFER_SIZE)
}

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FF_BENCH_VALKEY_HOST", default_value = "localhost")]
    valkey_host: String,
    #[arg(long, env = "FF_BENCH_VALKEY_PORT", default_value_t = 6379)]
    valkey_port: u16,
    /// Number of parallel flows to seed. Each flow is one linear
    /// 10-stage workflow instance.
    #[arg(long, default_value_t = 100)]
    flows: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
    /// Abort if drain takes longer than this.
    #[arg(long, default_value_t = 120)]
    deadline_secs: u64,
    /// N=5 methodology per PR #140. Number of independent samples.
    #[arg(long, default_value_t = 5)]
    samples: usize,
}

async fn stage_pass(
    s: u64,
    _conn: Data<MultiplexedConnection>,
) -> Result<u64, BoxDynError> {
    Ok(s)
}

/// Terminal stage. INCRs the completion counter via the injected
/// `Data<MultiplexedConnection>`.
async fn stage_terminal(
    _flow_id: u64,
    conn: Data<MultiplexedConnection>,
) -> Result<(), BoxDynError> {
    let mut c: MultiplexedConnection = (*conn).clone();
    let _: () = c.incr(COMPLETED_KEY, 1).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let url = format!("redis://{}:{}/", args.valkey_host, args.valkey_port);

    let raw_client = redis::Client::open(url.clone())?;

    let mut sample_walls: Vec<Duration> = Vec::with_capacity(args.samples);
    for sample_idx in 0..args.samples {
        let wall = run_sample(&args, &url, &raw_client).await?;
        eprintln!(
            "[apalis-s4] sample {}/{}: {:.2}s",
            sample_idx + 1,
            args.samples,
            wall.as_secs_f64()
        );
        sample_walls.push(wall);
    }

    let mean_wall = sample_walls.iter().sum::<Duration>() / (args.samples as u32);
    let throughput = args.flows as f64 / mean_wall.as_secs_f64();
    let walls_secs: Vec<f64> = sample_walls.iter().map(|d| d.as_secs_f64()).collect();

    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": ff_bench::git_sha(),
        "valkey_version": ff_bench::valkey_version(),
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": {
            "flows": args.flows,
            "nodes_per_flow": STAGES,
            "workers": WORKER_COUNT,
            "samples": args.samples,
            "apalis_version": "1.0.0-rc.7",
            "apalis_workflow_version": "0.1.0-rc.7",
            "primitive": "apalis_workflow::Workflow (and_then combinator)",
            "poll_interval_ms": POLL_INTERVAL_MS,
            "buffer_size": BUFFER_SIZE,
            "parallelize": "tokio::spawn",
        },
        "throughput_ops_per_sec": throughput,
        "per_sample_wall_secs": walls_secs,
        "mean_wall_secs": mean_wall.as_secs_f64(),
        "latency_ms": { "p50": 0.0, "p95": 0.0, "p99": 0.0 },
        "notes": "Scenario 4 harness uses apalis-workflow::Workflow (sequential and_then) per issue #51. Prior harness hand-rolled the 10-stage chain across 10 typed queues; maintainer flagged as under-representing apalis. N=5 per PR #140 methodology. Post-issue-#51 tuning (2026-04): N=16 independent WorkerBuilders (not .concurrency(16)), RedisConfig poll_interval=5ms buffer_size=100, .parallelize(tokio::spawn). latency_ms not reported — apalis doesn't surface per-flow completion without middleware we'd have to write ourselves.",
    });

    let results_dir = std::path::Path::new(&args.results_dir);
    std::fs::create_dir_all(results_dir)?;
    let path = results_dir.join(format!(
        "{}-{}-{}.json",
        SCENARIO,
        SYSTEM,
        report["git_sha"].as_str().unwrap_or("unknown")
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    eprintln!(
        "[apalis-s4] wrote {} — {:.1} flows/sec mean_wall={:.2}s N={}",
        path.display(),
        throughput,
        mean_wall.as_secs_f64(),
        args.samples
    );
    Ok(())
}

/// One independent sample: clean counter, build a fresh storage +
/// worker, seed `--flows`, drain via counter poll.
async fn run_sample(
    args: &Args,
    url: &str,
    raw_client: &redis::Client,
) -> Result<Duration> {
    // FLUSHALL between samples — apalis-workflow persists step-state
    // keys in the backing Redis and reuses the default namespace
    // across runs, so a second sample would see sample 1's leftovers
    // unless the DB is cleared. Matches the N=5 methodology note in
    // `benches/results/baseline.md` for scenario-4 harnesses.
    let mut raw = raw_client.get_multiplexed_async_connection().await?;
    let _: () = redis::cmd("FLUSHALL").query_async(&mut raw).await?;
    let _: () = raw.del(COMPLETED_KEY).await?;

    // Seed-side storage (separate connection): the driver pushes flow
    // starts into this one. Workers get their own storage instances
    // below so each has an independent fetch loop + config.
    let seed_conn = apalis_redis::connect(url)
        .await
        .map_err(|e| anyhow::anyhow!("apalis_redis::connect (seed): {e}"))?;
    // Workflow requires `Backend::Args == BackendExt::Compact`; for
    // RedisStorage `Compact = Vec<u8>`, so the storage must be typed
    // over `Vec<u8>` (raw codec-encoded bytes). Stage handlers still
    // take the typed `u64` — the codec roundtrips between compact and
    // typed at step boundaries.
    let mut storage = RedisStorage::<Vec<u8>>::new_with_config(seed_conn, tuned_config());

    use apalis::prelude::WorkerError;
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // Spawn N independent WorkerBuilder instances per maintainer's
    // recommendation (issue #51, 2026-04-27). Each worker rebuilds
    // the 10-stage Workflow (cheap — closures only) and binds it to
    // its own RedisStorage + counter connection.
    let mut worker_futs = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let worker_conn = apalis_redis::connect(url)
            .await
            .map_err(|e| anyhow::anyhow!("apalis_redis::connect (worker {wi}): {e}"))?;
        let worker_storage =
            RedisStorage::<Vec<u8>>::new_with_config(worker_conn, tuned_config());

        let counter_conn = raw_client.get_multiplexed_async_connection().await?;

        // 10-stage sequential workflow via apalis-workflow::Workflow.
        // Each `and_then` persists a step transition in the backing
        // RedisStorage rather than enqueueing across typed queues.
        let workflow = Workflow::new("ff-bench-s4-linear")
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_pass)
            .and_then(stage_terminal);

        let worker = WorkerBuilder::new(format!("s4-workflow-{wi}"))
            .backend(worker_storage)
            .data(counter_conn)
            .parallelize(tokio::spawn)
            .build(workflow);

        let mut rx = shutdown_tx.subscribe();
        let shutdown_fut = async move {
            let _ = rx.recv().await;
            Ok::<(), WorkerError>(())
        };
        worker_futs.push(worker.run_until(shutdown_fut));
    }
    let worker_fut = futures::future::join_all(worker_futs);

    let driver_raw_client = raw_client.clone();
    let flows = args.flows;
    let deadline_secs = args.deadline_secs;
    let driver = async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let submit_start = Instant::now();
        for flow_id in 0..flows {
            storage
                .push_start(flow_id as u64)
                .await
                .map_err(|e| anyhow::anyhow!("push_start {flow_id}: {e}"))?;
        }
        let submit_wall = submit_start.elapsed();
        eprintln!(
            "[apalis-s4] seeded {} flows in {} ms",
            flows,
            submit_wall.as_millis()
        );

        let drain_start = Instant::now();
        let deadline = drain_start + Duration::from_secs(deadline_secs);
        let mut c = driver_raw_client
            .get_multiplexed_async_connection()
            .await?;
        loop {
            let done: i64 = c.get(COMPLETED_KEY).await.unwrap_or(0);
            if done as usize >= flows {
                break;
            }
            if Instant::now() >= deadline {
                let _ = shutdown_tx.send(());
                anyhow::bail!(
                    "deadline exceeded: {}/{} flows completed in {}s",
                    done,
                    flows,
                    deadline_secs
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let wall = drain_start.elapsed();
        let _ = shutdown_tx.send(());
        Ok::<Duration, anyhow::Error>(wall)
    };

    let (driver_res, worker_res) = tokio::join!(driver, worker_fut);
    // Surface any worker failure — if a worker bails early the
    // counter-poll driver might already have succeeded, and silently
    // dropping the error would let a degraded sample slip into the
    // mean_wall. Re-report whichever side failed first.
    for (wi, res) in worker_res.into_iter().enumerate() {
        if let Err(e) = res {
            anyhow::bail!("worker {wi} run_until returned error: {e:?}");
        }
    }
    driver_res
}
