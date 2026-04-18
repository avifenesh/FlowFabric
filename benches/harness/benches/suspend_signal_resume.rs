//! Scenario 2 — suspend → signal → resume latency (HMAC roundtrip).
//!
//! What we measure: the primitive cost of one full suspend-signal-resume
//! cycle. End-to-end microseconds from the worker's `task.suspend()` call
//! through an authenticated POST /signal and back out of the re-claimed
//! `task.complete()`.
//!
//! Shape — per-Criterion-iteration:
//!   PRE-WORK (excluded from measurement, done inside iter_custom):
//!     1. POST /v1/executions (new execution)
//!     2. worker.claim_next() → ClaimedTask
//!     3. (optional) task.append_frame() to warm the partition
//!   MEASURED WINDOW (Instant::now → Instant::now):
//!     4. task.suspend(...)                          (mints waitpoint_token)
//!     5. GET /v1/executions/{id}/pending-waitpoints (reviewer fetches token)
//!     6. POST /v1/executions/{id}/signal            (authenticates token)
//!     7. worker.claim_next() — tight-loop, no sleep
//!     8. task.complete(None)
//!
//! The claim loop in step 7 is intentionally tight (`claim_poll_interval_ms
//! = 0`) so the measurement reflects the HMAC roundtrip, NOT the worker's
//! idle poll cadence. Production workers with a nonzero poll interval will
//! add ~(poll_interval / 2) on top. See `notes` in the emitted JSON for
//! the production-projected range.
//!
//! Criterion is used for sample_size + iter_custom timing; the JSON we
//! emit is the shared `ff_bench::report` schema.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use ff_bench::{
    report::Percentiles, write_report, LatencyMs, Report, SYSTEM_FLOWFABRIC,
};
use ff_sdk::{ConditionMatcher, FlowFabricWorker, SuspendOutcome, TimeoutBehavior, WorkerConfig};
use serde::Deserialize;

const SCENARIO: &str = "suspend_signal_resume";
const SIGNAL_NAME: &str = "bench_resume";
/// Worker poll interval during the measured re-claim leg. 1 ms (not 0)
/// so the tokio multi-thread runtime has a chance to schedule other
/// tasks (e.g. the HTTP handler completing the /signal POST) between
/// poll attempts — a strict 0 ms busy-spin starves peer tasks and
/// adds microseconds of poll-wake overhead that isn't HMAC cost.
const TIGHT_LOOP_POLL_MS: u64 = 1;
/// The default ff-sdk claim_poll_interval. Surfaced only to compute the
/// "production estimate" string in notes; the bench worker itself uses
/// `TIGHT_LOOP_POLL_MS`.
const DEFAULT_PROD_POLL_MS: u64 = 1_000;

fn scenario_2(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let env = ff_bench::workload::BenchEnv::from_env();

    // Shared latency sink — each iter_custom call pushes one sample. The
    // final report pass reads this to compute p50/p95/p99. Mutex is fine:
    // iter_custom runs iterations sequentially inside one async-block, so
    // there's no cross-thread contention on the hot path.
    let latencies_us: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    let mut group = c.benchmark_group(SCENARIO);
    group.sample_size(100);
    group.measurement_time(Duration::from_secs(10));

    {
        let env = env.clone();
        let latencies_us = latencies_us.clone();
        group.bench_function("roundtrip", move |b| {
            b.iter_custom(|iters| {
                let env = env.clone();
                let sink = latencies_us.clone();
                rt.block_on(async move {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        match measure_one(&env).await {
                            Ok(d) => {
                                sink.lock().expect("poisoned").push(d.as_micros() as u64);
                                total += d;
                            }
                            Err(e) => {
                                eprintln!("[bench] iter failed: {e:#}");
                                // Count a failed iteration as zero-time so
                                // Criterion doesn't hang waiting for a
                                // sample. The failure is logged; the JSON
                                // report's sample count will reflect it.
                            }
                        }
                    }
                    total
                })
            });
        });
    }
    group.finish();

    let snapshot = latencies_us.lock().expect("poisoned").clone();
    let latency_ms = Percentiles::from_micros(&snapshot);
    let sample_count = snapshot.len();
    // throughput = resumes / sec, computed from the median per-sample
    // duration. Using mean would let a tail outlier bias throughput
    // down; check_release.py compares on p50/p99 anyway.
    let median_us = if snapshot.is_empty() {
        0.0
    } else {
        latency_ms.p50 * 1_000.0
    };
    let throughput = if median_us > 0.0 {
        1_000_000.0 / median_us
    } else {
        0.0
    };

    write_scenario_report(&env, sample_count, throughput, latency_ms);
}

/// One measured cycle. The function owns the full envelope — setup
/// outside the Instant::now window, measured roundtrip inside.
///
/// If the measured body returns an error partway through, the created
/// execution would otherwise sit in Valkey until lease-TTL expiry,
/// polluting state for the next iteration (and the next `cargo bench`
/// run). `measure_one` wraps the body in an inner async so the outer
/// shell can POST `/v1/executions/{eid}/cancel` on any failure —
/// bounded cleanup, no TTL wait.
async fn measure_one(env: &ff_bench::workload::BenchEnv) -> anyhow::Result<Duration> {
    let reviewer = ff_bench::workload::http_client()?;
    let eid_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let eid_slot_inner = eid_slot.clone();
    let reviewer_inner = reviewer.clone();
    let res = measure_one_inner(env, &reviewer_inner, &eid_slot_inner).await;
    if res.is_err() {
        if let Some(eid) = eid_slot.lock().expect("poisoned").take() {
            // Best-effort cleanup; ignore errors (execution may have
            // already landed in a terminal state via the failed path).
            let cancel_url = format!("{}/v1/executions/{}/cancel", env.server, eid);
            let _ = reviewer
                .post(&cancel_url)
                .json(&serde_json::json!({
                    "execution_id": eid,
                    "reason": "bench iter failed",
                    "now": ff_bench::workload::now_ms(),
                }))
                .send()
                .await;
        }
    }
    res
}

/// Body of one measured cycle. The `eid_slot` arg is a handoff so the
/// outer [`measure_one`] wrapper can drive the cleanup path if this
/// function returns an error.
async fn measure_one_inner(
    env: &ff_bench::workload::BenchEnv,
    reviewer: &reqwest::Client,
    eid_slot: &Arc<Mutex<Option<String>>>,
) -> anyhow::Result<Duration> {

    // ── PRE-WORK — excluded from measurement ──────────────────────────

    // One fresh worker per iteration. Reusing a worker would measure
    // state from the prior iteration's lease-renewal timer; fresh is
    // cleaner for latency sampling.
    let instance_id = format!("bench-susp-{}", uuid::Uuid::new_v4());
    let mut config = WorkerConfig::new(
        &env.valkey_host,
        env.valkey_port,
        "bench-susp-worker",
        &instance_id,
        &env.namespace,
        &env.lane,
    );
    config.claim_poll_interval_ms = TIGHT_LOOP_POLL_MS;
    let worker = FlowFabricWorker::connect(config).await?;

    let eid = ff_bench::workload::create_execution(
        reviewer,
        env,
        "bench.scenario2",
        Vec::new(),
    )
    .await?;
    // Record the eid so the outer wrapper can cancel on iter failure.
    *eid_slot.lock().expect("poisoned") = Some(eid.clone());

    // Spin until this worker picks up the execution we just created.
    // Tight loop; no sleep.
    let task = loop {
        if let Some(t) = worker.claim_next().await? {
            if t.execution_id().to_string() == eid {
                break t;
            }
            // Not ours — this bench creates one execution per iter, so a
            // mismatch means we claimed someone else's bench task (stale
            // from a prior aborted run). Release via fail() and retry.
            t.fail("bench iter: not our execution", "bench_stale").await?;
        }
    };

    // Yield once so any prior-iter cleanup (worker Drop, lease TTL
    // bookkeeping) has been polled before we start the stopwatch.
    // Belt-and-suspenders against iter-N-setup overlapping iter-(N-1)-
    // quiesce on a contended runtime.
    tokio::task::yield_now().await;

    // ── MEASURED WINDOW ───────────────────────────────────────────────
    let t0 = Instant::now();

    let outcome = task
        .suspend(
            "bench_suspend",
            &[ConditionMatcher {
                signal_name: SIGNAL_NAME.into(),
            }],
            Some(60_000),
            TimeoutBehavior::Fail,
        )
        .await?;

    let (waitpoint_id, waitpoint_token) = match outcome {
        SuspendOutcome::Suspended {
            waitpoint_id,
            waitpoint_token,
            ..
        } => (waitpoint_id, waitpoint_token),
        SuspendOutcome::AlreadySatisfied { .. } => {
            anyhow::bail!("scenario 2: AlreadySatisfied on a fresh waitpoint");
        }
    };

    // Reviewer fetches the token via the server's REST endpoint. This is
    // the realistic path (matches media-pipeline review CLI); fetching
    // straight from the suspend outcome would skip an HTTP roundtrip
    // that real reviewers pay.
    let pending_url = format!(
        "{}/v1/executions/{}/pending-waitpoints",
        env.server, eid
    );
    let pending: Vec<PendingWaitpoint> =
        reviewer.get(&pending_url).send().await?.json().await?;
    let chosen = pending
        .iter()
        .find(|w| w.waitpoint_id == waitpoint_id.to_string())
        .ok_or_else(|| anyhow::anyhow!("waitpoint {waitpoint_id} not in pending list"))?;
    debug_assert_eq!(chosen.waitpoint_token, waitpoint_token.as_str());

    // Deliver the HMAC-authenticated signal.
    let signal_url = format!("{}/v1/executions/{}/signal", env.server, eid);
    let signal_body = serde_json::json!({
        "execution_id": eid,
        "waitpoint_id": waitpoint_id.to_string(),
        "signal_id": uuid::Uuid::new_v4().to_string(),
        "signal_name": SIGNAL_NAME,
        "signal_category": "bench",
        "source_type": "bench",
        "source_identity": "suspend_signal_resume",
        "payload": Vec::<u8>::new(),
        "payload_encoding": "binary",
        "target_scope": "execution",
        "waitpoint_token": waitpoint_token.as_str(),
        "now": ff_bench::workload::now_ms(),
    });
    let resp = reviewer.post(&signal_url).json(&signal_body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("signal failed ({status}): {body}");
    }

    // Re-claim — tight loop, no sleep. Measures HMAC roundtrip cost, not
    // worker wake-up latency.
    let resumed = loop {
        if let Some(t) = worker.claim_next().await? {
            if t.execution_id().to_string() == eid {
                break t;
            }
            // Stale claim from a different execution (shouldn't happen
            // with our fresh-per-iter model, but fail loud).
            t.fail("bench iter: unexpected claim during resume", "bench_stale")
                .await?;
        }
    };

    resumed.complete(None).await?;

    // Success path — clear the eid so the outer cleanup fallback
    // doesn't try to cancel an already-completed execution.
    *eid_slot.lock().expect("poisoned") = None;

    Ok(t0.elapsed())
}

fn write_scenario_report(
    env: &ff_bench::workload::BenchEnv,
    samples: usize,
    throughput_ops_per_sec: f64,
    latency_ms: LatencyMs,
) {
    let config = serde_json::json!({
        "samples": samples,
        "measurement_secs": 10,
        "claim_poll_interval_ms": TIGHT_LOOP_POLL_MS,
        "signal_name": SIGNAL_NAME,
    });
    let production_projected_p50_ms =
        latency_ms.p50 + (DEFAULT_PROD_POLL_MS as f64 / 2.0);
    let notes = format!(
        "tight-loop re-claim (claim_poll_interval_ms={}); production with \
         default claim_poll_interval_ms={}ms adds ~{:.1}ms on the re-claim \
         leg, so production-projected p50 ≈ {:.2}ms.",
        TIGHT_LOOP_POLL_MS,
        DEFAULT_PROD_POLL_MS,
        DEFAULT_PROD_POLL_MS as f64 / 2.0,
        production_projected_p50_ms,
    );

    let mut report = Report::fill_env(
        SCENARIO,
        SYSTEM_FLOWFABRIC,
        env.cluster,
        config,
        throughput_ops_per_sec,
        latency_ms,
    );
    report.notes = Some(notes);

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

#[derive(Debug, Deserialize)]
struct PendingWaitpoint {
    waitpoint_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    waitpoint_key: String,
    #[allow(dead_code)]
    state: String,
    waitpoint_token: String,
}

criterion_group!(benches, scenario_2);
criterion_main!(benches);
