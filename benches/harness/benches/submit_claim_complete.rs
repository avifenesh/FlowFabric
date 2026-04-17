//! Scenario 1 — submit → claim → complete throughput.
//!
//! Seeds `N` executions via the REST API, drives a worker pool through
//! `claim_next → complete` until the eligible queue drains, measures
//! total wall + per-task latency. Reports ops/sec and p50/p95/p99 to
//! `benches/results/submit_claim_complete-<sha>.json`.
//!
//! Two sizes run by default: 1_000 and 10_000. The 100_000 size from the
//! Phase A plan is available behind `FF_BENCH_LARGE=true` — skipped by
//! default so a bench invocation doesn't accidentally soak a developer
//! laptop for tens of minutes.
//!
//! Criterion is used for its throughput tracking + HTML reports; the
//! JSON we write is OUR schema (shared with the comparison packages),
//! not criterion's native output which is unstable across versions.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ff_bench::{
    report::Percentiles, write_report, LatencyMs, Report, SYSTEM_FLOWFABRIC,
};
use ff_sdk::{ClaimedTask, FlowFabricWorker, WorkerConfig};
use reqwest::Client;

const SCENARIO: &str = "submit_claim_complete";
const WORKER_COUNT: usize = 16;
const PAYLOAD_BYTES: usize = 4 * 1024;
const POLL_INTERVAL_MS: u64 = 50;

fn scenario_1(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let env = ff_bench::workload::BenchEnv::from_env();

    // Pick sizes. FF_BENCH_LARGE gates the 100k run.
    let mut sizes: Vec<usize> = vec![1_000, 10_000];
    if std::env::var("FF_BENCH_LARGE").ok().as_deref() == Some("true") {
        sizes.push(100_000);
    }

    let mut group = c.benchmark_group(SCENARIO);
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));

    // Criterion's throughput annotation drives the "Elements" display
    // in reports — makes reading the HTML easier.
    for &n in &sizes {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let env = env.clone();
            // iter_custom gives us the wall clock for the whole drain
            // cycle, which is what we actually care about; the default
            // iter timings include criterion's per-iter scaffolding
            // that inflates small-N numbers.
            b.iter_custom(|_iters| rt.block_on(drain_once(&env, n)));
        });
    }
    group.finish();

    // Final snapshot at the largest size → write our JSON schema so the
    // release gate can compare. We re-run one more drain just so the
    // latency histogram reflects the final state of the runtime after
    // warmup, not the first unstable iteration.
    let largest = *sizes.last().expect("at least one size");
    let result = rt
        .block_on(drain_once_with_stats(&env, largest))
        .expect("drain scenario 1");
    let throughput = result.tasks as f64 / result.wall.as_secs_f64();
    let latency = Percentiles::from_micros(&result.latencies_us);
    write_scenario_report(&env, largest, throughput, latency);
}

/// Result of one timed drain. `iter_custom` only returns a Duration;
/// this is the richer variant used for the final reporting pass.
struct DrainStats {
    tasks: usize,
    wall: Duration,
    latencies_us: Vec<u64>,
}

async fn drain_once(env: &ff_bench::workload::BenchEnv, n: usize) -> Duration {
    drain_once_with_stats(env, n)
        .await
        .map(|r| r.wall)
        .unwrap_or_else(|_| Duration::from_secs(0))
}

async fn drain_once_with_stats(
    env: &ff_bench::workload::BenchEnv,
    n: usize,
) -> anyhow::Result<DrainStats> {
    let client = ff_bench::workload::http_client()?;

    // Pre-seed BEFORE starting workers so the claim loop measures
    // steady-state and isn't racing the submitter. Submissions are
    // batched with bounded concurrency (32) to avoid OOMing the HTTP
    // client on 100k-size runs.
    let submit_start = Instant::now();
    seed_executions(&client, env, n).await?;
    let submit_wall = submit_start.elapsed();
    tracing::info!(
        n,
        submit_wall_ms = submit_wall.as_millis(),
        "seeded executions"
    );

    // Start worker pool. Each worker writes to a PER-TASK local buffer
    // and returns it via the JoinHandle — no shared Arc<Mutex>, so the
    // hot path has zero cross-worker synchronisation beyond the
    // `stop_at` atomic decrement. PR#10 review caught that the prior
    // shared-vec collection biased both throughput and latency.
    let drain_start = Instant::now();
    let mut worker_handles = Vec::with_capacity(WORKER_COUNT);
    let stop_at = Arc::new(std::sync::atomic::AtomicUsize::new(n));
    for wi in 0..WORKER_COUNT {
        let env = env.clone();
        let stop_at = stop_at.clone();
        worker_handles.push(tokio::spawn(async move {
            drive_worker(wi, env, stop_at).await
        }));
    }
    let mut latencies_us: Vec<u64> = Vec::with_capacity(n);
    for h in worker_handles {
        match h.await {
            Ok(Ok(mut v)) => latencies_us.append(&mut v),
            Ok(Err(e)) => tracing::warn!(error = %e, "worker returned error"),
            Err(e) => tracing::warn!(error = %e, "worker join failed"),
        }
    }
    let wall = drain_start.elapsed();

    Ok(DrainStats {
        tasks: n,
        wall,
        latencies_us,
    })
}

async fn seed_executions(
    client: &Client,
    env: &ff_bench::workload::BenchEnv,
    n: usize,
) -> anyhow::Result<()> {
    const CONC: usize = 32;
    use tokio::sync::Semaphore;
    let sem = Arc::new(Semaphore::new(CONC));
    let payload = ff_bench::workload::filler_payload(PAYLOAD_BYTES);
    let mut handles = Vec::with_capacity(n);
    for _ in 0..n {
        let permit = sem.clone().acquire_owned().await?;
        let client = client.clone();
        let env = env.clone();
        let payload = payload.clone();
        handles.push(tokio::spawn(async move {
            let res = ff_bench::workload::create_execution(
                &client,
                &env,
                "bench.scenario1",
                payload,
            )
            .await;
            drop(permit);
            res
        }));
    }
    for h in handles {
        h.await??;
    }
    Ok(())
}

async fn drive_worker(
    wi: usize,
    env: ff_bench::workload::BenchEnv,
    stop_at: Arc<std::sync::atomic::AtomicUsize>,
) -> anyhow::Result<Vec<u64>> {
    use std::sync::atomic::Ordering;

    let mut config = WorkerConfig::new(
        &env.valkey_host,
        env.valkey_port,
        format!("bench-worker-{wi}"),
        format!("bench-worker-{wi}-{}", uuid::Uuid::new_v4()),
        &env.namespace,
        &env.lane,
    );
    // Keep poll interval tight so drains don't get dragged by backoff.
    config.claim_poll_interval_ms = POLL_INTERVAL_MS;
    let worker = FlowFabricWorker::connect(config).await?;

    // Per-worker buffer: no lock, no contention. Merged by the driver
    // after every worker finishes.
    let mut local: Vec<u64> = Vec::new();

    loop {
        // stop_at starts at N and counts down as tasks complete. Exit
        // once every task is accounted for; avoids workers spinning on
        // an empty queue after the drain.
        if stop_at.load(Ordering::Relaxed) == 0 {
            return Ok(local);
        }
        match worker.claim_next().await? {
            Some(task) => {
                let lat = complete_one(task).await?;
                local.push(lat);
                stop_at.fetch_sub(1, Ordering::Relaxed);
            }
            None => {
                tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            }
        }
    }
}

async fn complete_one(task: ClaimedTask) -> anyhow::Result<u64> {
    let t0 = Instant::now();
    task.complete(None).await?;
    Ok(t0.elapsed().as_micros() as u64)
}

fn write_scenario_report(
    env: &ff_bench::workload::BenchEnv,
    n: usize,
    throughput_ops_per_sec: f64,
    latency_ms: LatencyMs,
) {
    let config = serde_json::json!({
        "tasks": n,
        "workers": WORKER_COUNT,
        "payload_bytes": PAYLOAD_BYTES,
    });
    let report = Report::fill_env(
        SCENARIO,
        SYSTEM_FLOWFABRIC,
        env.cluster,
        config,
        throughput_ops_per_sec,
        latency_ms,
    );
    // Write alongside the workspace results/ dir so check_release.py
    // can find it by known path.
    let results_dir = results_dir();
    match write_report(&report, &results_dir) {
        Ok(path) => println!("[bench] wrote {}", path.display()),
        Err(e) => eprintln!("[bench] WARN: could not write report: {e}"),
    }
}

fn results_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    // Criterion runs from target/bench; walk up to find `benches/results`.
    let mut walk = exe_dir.unwrap_or_else(|| PathBuf::from("."));
    for _ in 0..6 {
        let cand = walk.join("benches").join("results");
        if cand.exists() {
            return cand;
        }
        if !walk.pop() {
            break;
        }
    }
    PathBuf::from("benches/results")
}

criterion_group!(benches, scenario_1);
criterion_main!(benches);
