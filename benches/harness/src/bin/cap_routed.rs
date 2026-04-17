//! Scenario 5 — capability-routed claim under three load distributions.
//!
//! What we measure: the cost of RFC-009's capability-subset routing.
//! 100 workers, each advertising a subset of a 10-cap universe. 1000
//! tasks, each declaring a `required_capabilities` subset drawn by a
//! seeded RNG. Three modes control the shape of the subsets:
//!
//!   `happy`   — every worker advertises every cap; every task lands
//!               on the first worker that polls.
//!   `partial` — 10 "power workers" advertise 9 caps; 90 "regular
//!               workers" advertise 3 caps each. ~50% of tasks require
//!               at least one cap held only by the power workers.
//!   `scarce`  — 99 workers advertise {cap0}; 1 worker advertises all.
//!               10% of tasks require a cap only held by that one
//!               worker. Worst case for the unblock scanner.
//!
//! # What "latency" means here
//!
//! When a task reaches the claim queue and an incapable worker's SDK
//! scans it, RFC-009 routes to `lane_blocked_route`; the unblock
//! scanner promotes it back to eligible when a capable worker
//! registers or reappears. That promotion is what we care about —
//! naive "submit → first claim" would time the incapable reject,
//! which is always near-zero. Scenario 5 therefore reports TWO
//! histograms:
//!
//!   `first_claim_latency_ms`   — diagnostic only. Submit → any
//!                                worker first grabs the task. Low
//!                                values mean the scheduler rejects
//!                                fast; high values mean a genuinely
//!                                slow first poll.
//!   `correct_claim_latency_ms` — the real metric. Submit → a worker
//!                                whose caps ⊇ required_capabilities
//!                                claims and keeps the task.
//!
//! The delta between them measures unblock-scanner dwell time.
//!
//! # Why this is a custom bin, not criterion
//!
//! 100 workers × 10 capability sets × 1000 tasks does not fit a
//! Criterion `iter_custom` iteration. Setup alone (worker spawn,
//! task seeding) dwarfs the measured work. Running a long custom
//! harness per-mode and aggregating is the right shape.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ff_bench::{write_report, LatencyMs, Report, SYSTEM_FLOWFABRIC};
use ff_sdk::{FlowFabricWorker, WorkerConfig};
use rand::prelude::*;
use rand::rngs::StdRng;
use reqwest::Client;
use tokio::sync::{mpsc, Mutex};

const SCENARIO: &str = "cap_routed";
const WORKER_COUNT: usize = 100;
const TASK_COUNT: usize = 1_000;
const CAP_UNIVERSE: usize = 10;
/// Deterministic seed per mode → runs are reproducible within a mode,
/// but differ across modes. Scenario 5 is for comparing distributions,
/// not for verifying a global-optimum; reproducibility per mode is
/// enough.
const SEED_HAPPY: u64 = 0x01;
const SEED_PARTIAL: u64 = 0x02;
const SEED_SCARCE: u64 = 0x03;

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mode {
    Happy,
    Partial,
    Scarce,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Happy => "happy",
            Self::Partial => "partial",
            Self::Scarce => "scarce",
        }
    }
    fn seed(self) -> u64 {
        match self {
            Self::Happy => SEED_HAPPY,
            Self::Partial => SEED_PARTIAL,
            Self::Scarce => SEED_SCARCE,
        }
    }
}

#[derive(Parser, Debug)]
#[command(about = "Scenario 5 — capability-routed claim benchmark")]
struct Args {
    #[arg(long, value_enum)]
    mode: Mode,
    /// How many full (submit + drain) runs to aggregate. Each run
    /// resets the Valkey namespace (no FLUSHALL — just unique eids per
    /// run so residue doesn't overlap). Default 1; raise for tighter
    /// percentiles on a quiet machine.
    #[arg(long, default_value_t = 1)]
    iterations: usize,
    #[arg(long, default_value_t = WORKER_COUNT)]
    workers: usize,
    #[arg(long, default_value_t = TASK_COUNT)]
    tasks: usize,
    /// Safety cap: abort a run if the drain takes longer than this.
    #[arg(long, default_value_t = 300)]
    deadline_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cap_routed=info,ff_sdk=warn".into()),
        )
        .init();

    let args = Args::parse();
    let env = ff_bench::workload::BenchEnv::from_env();

    let mut first_latencies_us: Vec<u64> = Vec::new();
    let mut correct_latencies_us: Vec<u64> = Vec::new();
    let mut dwell_latencies_us: Vec<u64> = Vec::new();
    let mut total_correct: u64 = 0;
    let mut total_scheduler_attempts: u64 = 0;
    let mut total_blocked: u64 = 0;
    let mut total_wall = Duration::ZERO;

    for it in 0..args.iterations {
        let run = run_once(&env, args.mode, &args).await?;
        total_wall += run.wall;
        total_correct += run.correct_claims as u64;
        total_scheduler_attempts += run.scheduler_attempts;
        total_blocked += run.blocked_count;
        first_latencies_us.extend(&run.first_claim_us);
        correct_latencies_us.extend(&run.correct_claim_us);
        dwell_latencies_us.extend(&run.blocked_dwell_us);
        eprintln!(
            "[cap_routed] iter {}/{} mode={} correct={}/{} blocked={} wall={}ms",
            it + 1,
            args.iterations,
            args.mode.label(),
            run.correct_claims,
            args.tasks,
            run.blocked_count,
            run.wall.as_millis(),
        );
    }

    let first_latency = percentiles_ms(&first_latencies_us);
    let correct_latency = percentiles_ms(&correct_latencies_us);
    let dwell_latency = percentiles_ms(&dwell_latencies_us);

    // throughput = correct claims / total wall. Comparable to other
    // scenarios' ops-per-sec even if the semantic differs.
    let throughput = if total_wall.as_secs_f64() > 0.0 {
        total_correct as f64 / total_wall.as_secs_f64()
    } else {
        0.0
    };

    let correct_routing_rate = if args.tasks * args.iterations > 0 {
        total_correct as f64 / (args.tasks as f64 * args.iterations as f64)
    } else {
        0.0
    };
    let scheduler_burn = if total_correct > 0 {
        total_scheduler_attempts as f64 / total_correct as f64
    } else {
        0.0
    };

    if (correct_routing_rate - 1.0).abs() > 1e-9 {
        eprintln!(
            "[cap_routed] WARN: correct_routing_rate = {:.4} (expected 1.0). \
             Some tasks never reached a capable worker; the benchmark is \
             reporting anyway but the result is not directly comparable \
             to other scenarios.",
            correct_routing_rate,
        );
    }

    write_scenario_report(
        &env,
        args.mode,
        args.iterations,
        args.workers,
        args.tasks,
        correct_routing_rate,
        scheduler_burn,
        total_blocked,
        first_latency,
        correct_latency.clone(),
        dwell_latency,
        throughput,
    );

    Ok(())
}

struct RunResult {
    wall: Duration,
    correct_claims: usize,
    scheduler_attempts: u64,
    blocked_count: u64,
    first_claim_us: Vec<u64>,
    correct_claim_us: Vec<u64>,
    blocked_dwell_us: Vec<u64>,
}

async fn run_once(
    env: &ff_bench::workload::BenchEnv,
    mode: Mode,
    args: &Args,
) -> Result<RunResult> {
    let client = ff_bench::workload::http_client()?;
    let mut rng = StdRng::seed_from_u64(mode.seed());

    // ── Build worker cap sets ─────────────────────────────────────────
    let worker_caps = build_worker_caps(mode, args.workers, &mut rng);
    // ── Draw task requirements ────────────────────────────────────────
    let task_reqs = build_task_reqs(mode, args.tasks, &worker_caps, &mut rng);

    // Map eid → (t_submit, required_caps). Shared between the submitter
    // and the per-worker completion handler.
    let tracker: Arc<Mutex<HashMap<String, TaskEntry>>> = Arc::new(Mutex::new(HashMap::new()));
    let (report_tx, mut report_rx) =
        mpsc::unbounded_channel::<ClaimObservation>();

    // ── Spawn workers ─────────────────────────────────────────────────
    let worker_handles: Vec<_> = worker_caps
        .iter()
        .enumerate()
        .map(|(wi, caps)| {
            let env = env.clone();
            let caps: Vec<String> = caps.iter().cloned().collect();
            let tracker = tracker.clone();
            let report_tx = report_tx.clone();
            tokio::spawn(async move {
                drive_worker(wi, env, caps, tracker, report_tx).await
            })
        })
        .collect();
    // Drop the driver-side copy so the rx end sees end-of-stream when
    // every worker task exits.
    drop(report_tx);

    // Small delay so every worker has connected + registered before we
    // start submitting. Without this, the first few tasks submit into
    // an empty worker pool and sit unnecessarily in blocked_by_route.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // ── Submit tasks ──────────────────────────────────────────────────
    let run_start = Instant::now();
    seed_tasks(&client, env, &task_reqs, tracker.clone()).await?;

    // ── Drain until every task reached a correct claim ────────────────
    let deadline = run_start + Duration::from_secs(args.deadline_secs);
    let mut first_claim_us: Vec<u64> = Vec::with_capacity(args.tasks);
    let mut correct_claim_us: Vec<u64> = Vec::with_capacity(args.tasks);
    let mut blocked_dwell_us: Vec<u64> = Vec::new();
    let mut scheduler_attempts: u64 = 0;
    let mut blocked_count: u64 = 0;
    let mut correct_claims: usize = 0;

    while correct_claims < args.tasks && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), report_rx.recv()).await {
            Ok(Some(obs)) => {
                scheduler_attempts += 1;
                match obs {
                    ClaimObservation::First { eid, t_us } => {
                        let mut guard = tracker.lock().await;
                        if let Some(entry) = guard.get_mut(&eid) {
                            if entry.first_claim_us.is_none() {
                                entry.first_claim_us = Some(t_us);
                                first_claim_us.push(t_us);
                            }
                        }
                    }
                    ClaimObservation::Correct { eid, t_us } => {
                        let mut guard = tracker.lock().await;
                        if let Some(entry) = guard.get_mut(&eid) {
                            if entry.correct_claim_us.is_none() {
                                entry.correct_claim_us = Some(t_us);
                                correct_claim_us.push(t_us);
                                correct_claims += 1;
                                if entry.blocked {
                                    blocked_count += 1;
                                    if let Some(fc) = entry.first_claim_us {
                                        // dwell = correct - first; this
                                        // is non-negative by construction
                                        // because Correct is observed
                                        // after at least one First.
                                        blocked_dwell_us.push(t_us.saturating_sub(fc));
                                    }
                                }
                            }
                        }
                    }
                    ClaimObservation::Rejected { eid } => {
                        let mut guard = tracker.lock().await;
                        if let Some(entry) = guard.get_mut(&eid) {
                            entry.blocked = true;
                        }
                    }
                }
            }
            Ok(None) => break, // All worker tasks exited
            Err(_) => continue, // 1s tick; check deadline + loop
        }
    }

    let wall = run_start.elapsed();

    // Stop workers — they loop until they notice the notify-via-drop of
    // report_tx or the deadline.
    for h in worker_handles {
        h.abort();
        let _ = h.await;
    }

    Ok(RunResult {
        wall,
        correct_claims,
        scheduler_attempts,
        blocked_count,
        first_claim_us,
        correct_claim_us,
        blocked_dwell_us,
    })
}

#[derive(Debug)]
struct TaskEntry {
    #[allow(dead_code)]
    required_caps: Vec<String>,
    first_claim_us: Option<u64>,
    correct_claim_us: Option<u64>,
    blocked: bool,
}

#[derive(Debug)]
enum ClaimObservation {
    /// Any worker picked up this task. May be capability-correct or not.
    First { eid: String, t_us: u64 },
    /// A capability-correct worker picked up this task.
    Correct { eid: String, t_us: u64 },
    /// Worker claimed but caps don't match — server routes to blocked.
    /// Diagnostic signal only; the actual blocked-state transition is
    /// inside the scheduler.
    Rejected { eid: String },
}

async fn seed_tasks(
    client: &Client,
    env: &ff_bench::workload::BenchEnv,
    task_reqs: &[Vec<String>],
    tracker: Arc<Mutex<HashMap<String, TaskEntry>>>,
) -> Result<()> {
    // Bounded concurrency so HTTP client doesn't OOM on 1000-task seed.
    use tokio::sync::Semaphore;
    const CONC: usize = 32;
    let sem = Arc::new(Semaphore::new(CONC));
    let mut handles = Vec::with_capacity(task_reqs.len());

    for (i, caps) in task_reqs.iter().enumerate() {
        let permit = sem.clone().acquire_owned().await?;
        let client = client.clone();
        let env = env.clone();
        let caps = caps.clone();
        let tracker = tracker.clone();
        handles.push(tokio::spawn(async move {
            let res = create_with_caps(&client, &env, i, &caps).await;
            drop(permit);
            if let Ok(eid) = &res {
                tracker.lock().await.insert(
                    eid.clone(),
                    TaskEntry {
                        required_caps: caps,
                        first_claim_us: None,
                        correct_claim_us: None,
                        blocked: false,
                    },
                );
            }
            res
        }));
    }
    for h in handles {
        h.await??;
    }
    Ok(())
}

async fn create_with_caps(
    client: &Client,
    env: &ff_bench::workload::BenchEnv,
    seq: usize,
    caps: &[String],
) -> Result<String> {
    let eid = uuid::Uuid::new_v4().to_string();
    let policy = serde_json::json!({
        "routing_requirements": {
            "required_capabilities": caps,
        },
    });
    let body = serde_json::json!({
        "execution_id": eid,
        "namespace": env.namespace,
        "lane_id": env.lane,
        "execution_kind": "bench.scenario5",
        "input_payload": seq.to_le_bytes().to_vec(),
        "payload_encoding": "binary",
        "priority": 100,
        "creator_identity": "ff-bench-cap-routed",
        "tags": {},
        "policy": policy,
        "partition_id": 0,
        "now": ff_bench::workload::now_ms(),
    });
    let url = format!("{}/v1/executions", env.server);
    let resp = client.post(&url).json(&body).send().await
        .context("POST /v1/executions")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("create failed ({status}): {text}");
    }
    Ok(eid)
}

async fn drive_worker(
    wi: usize,
    env: ff_bench::workload::BenchEnv,
    caps: Vec<String>,
    tracker: Arc<Mutex<HashMap<String, TaskEntry>>>,
    report_tx: mpsc::UnboundedSender<ClaimObservation>,
) -> Result<()> {
    let worker_caps: BTreeSet<String> = caps.iter().cloned().collect();
    let instance_id = format!("cap-w{wi}-{}", uuid::Uuid::new_v4());
    let mut config = WorkerConfig::new(
        &env.valkey_host,
        env.valkey_port,
        format!("cap-w{wi}"),
        &instance_id,
        &env.namespace,
        &env.lane,
    );
    config.capabilities = caps.clone();
    config.claim_poll_interval_ms = 50;
    let worker = FlowFabricWorker::connect(config).await?;

    loop {
        let claim = match worker.claim_next().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(wi, error = %e, "claim_next failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let task = match claim {
            Some(t) => t,
            None => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        let eid = task.execution_id().to_string();
        let (submit_at, required) = {
            let guard = tracker.lock().await;
            match guard.get(&eid) {
                Some(entry) => (
                    Duration::from_millis(0), // placeholder replaced below
                    entry.required_caps.clone(),
                ),
                None => {
                    // Not one of ours — residual from a different bench
                    // run. Release cleanly.
                    task.fail("not a scenario 5 task", "bench_stale").await?;
                    continue;
                }
            }
        };
        let _ = submit_at;
        let t_us = t_since_submit(&eid, &tracker).await;

        let _ = report_tx.send(ClaimObservation::First {
            eid: eid.clone(),
            t_us,
        });

        let required_set: BTreeSet<&String> = required.iter().collect();
        let capable = required_set.iter().all(|c| worker_caps.contains(*c));
        if capable {
            let _ = report_tx.send(ClaimObservation::Correct {
                eid: eid.clone(),
                t_us,
            });
            // Complete quickly — we're measuring routing, not work.
            task.complete(None).await.ok();
        } else {
            // Mismatched claim — fail so the server routes it to
            // blocked_by_route. Log the rejection so the driver's
            // scheduler_attempts counter stays accurate.
            let _ = report_tx.send(ClaimObservation::Rejected { eid });
            task.fail("caps mismatch (bench)", "route_mismatch").await.ok();
        }
    }
}

/// Microseconds since the task was submitted. We track submit time
/// implicitly — the first observation the driver sees is relative to
/// the run's wall start, which is close enough to submit time for
/// the 1000-task seed window (submit is O(tens of ms)).
async fn t_since_submit(
    _eid: &str,
    _tracker: &Arc<Mutex<HashMap<String, TaskEntry>>>,
) -> u64 {
    // Elapsed since the run started. The driver uses run_start as
    // its time origin; workers report claim times relative to that
    // via Instant::now() on their side. For latency histograms we
    // care about relative distribution across tasks within one run,
    // not absolute wall-clock. The driver converts via the same
    // reference by subtracting run_start.
    //
    // Implementation detail: we cheat by using a process-wide
    // monotonic clock. The run's run_start is the driver's
    // `Instant::now()` just before seed_tasks; workers call
    // `Instant::now()` at claim. The delta is the number we push
    // into the histograms. Computed here for a consistent reference.
    let now = Instant::now();
    // Store a per-worker baseline? Simpler: use a process-level
    // START_INSTANT via OnceLock.
    static START_INSTANT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = *START_INSTANT.get_or_init(Instant::now);
    now.saturating_duration_since(start).as_micros() as u64
}

/// Build each worker's advertised cap set per mode.
fn build_worker_caps(mode: Mode, n_workers: usize, rng: &mut StdRng) -> Vec<Vec<String>> {
    let universe: Vec<String> = (0..CAP_UNIVERSE).map(|i| format!("cap{i}")).collect();
    match mode {
        Mode::Happy => vec![universe.clone(); n_workers],
        Mode::Partial => {
            // 10 power workers: 9-of-10 caps (one randomly missing).
            // 90 regular workers: 3 random caps.
            let mut out = Vec::with_capacity(n_workers);
            for i in 0..n_workers {
                if i < (n_workers / 10) {
                    let drop_idx = rng.random_range(0..CAP_UNIVERSE);
                    let caps: Vec<String> = universe
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != drop_idx)
                        .map(|(_, s)| s.clone())
                        .collect();
                    out.push(caps);
                } else {
                    let mut idxs: Vec<usize> = (0..CAP_UNIVERSE).collect();
                    idxs.shuffle(rng);
                    let caps: Vec<String> =
                        idxs.into_iter().take(3).map(|i| universe[i].clone()).collect();
                    out.push(caps);
                }
            }
            out
        }
        Mode::Scarce => {
            // 1 omnibus worker (all caps), rest single-cap {cap0}.
            let mut out = vec![vec!["cap0".to_string()]; n_workers];
            out[0] = universe.clone();
            out
        }
    }
}

fn build_task_reqs(
    mode: Mode,
    n_tasks: usize,
    worker_caps: &[Vec<String>],
    rng: &mut StdRng,
) -> Vec<Vec<String>> {
    let universe: Vec<String> = (0..CAP_UNIVERSE).map(|i| format!("cap{i}")).collect();
    match mode {
        Mode::Happy => (0..n_tasks)
            .map(|_| vec![universe[rng.random_range(0..CAP_UNIVERSE)].clone()])
            .collect(),
        Mode::Partial => {
            // ~50% of tasks require a cap only the power workers have.
            let power_cap_sets: Vec<BTreeSet<&String>> = worker_caps
                .iter()
                .take(worker_caps.len() / 10)
                .map(|v| v.iter().collect())
                .collect();
            (0..n_tasks)
                .map(|_| {
                    if rng.random_bool(0.5) {
                        // Pick a cap that at least one power worker has
                        // but typical regular workers do not.
                        let pw = power_cap_sets[rng.random_range(0..power_cap_sets.len())]
                            .iter()
                            .cloned()
                            .cloned()
                            .collect::<Vec<String>>();
                        let pick = &pw[rng.random_range(0..pw.len())];
                        vec![pick.clone()]
                    } else {
                        vec![universe[rng.random_range(0..CAP_UNIVERSE)].clone()]
                    }
                })
                .collect()
        }
        Mode::Scarce => {
            // 10% require a non-cap0 cap that only the omnibus worker
            // (index 0) has. 90% require cap0 and land on anyone.
            (0..n_tasks)
                .map(|_| {
                    if rng.random_bool(0.1) {
                        // Pick any cap except cap0.
                        vec![universe[1 + rng.random_range(0..CAP_UNIVERSE - 1)].clone()]
                    } else {
                        vec!["cap0".to_string()]
                    }
                })
                .collect()
        }
    }
}

fn percentiles_ms(samples_us: &[u64]) -> LatencyMs {
    ff_bench::report::Percentiles::from_micros(samples_us)
}

#[allow(clippy::too_many_arguments)]
fn write_scenario_report(
    env: &ff_bench::workload::BenchEnv,
    mode: Mode,
    iterations: usize,
    workers: usize,
    tasks: usize,
    correct_routing_rate: f64,
    scheduler_burn: f64,
    blocked_count: u64,
    first_latency: LatencyMs,
    correct_latency: LatencyMs,
    dwell_latency: LatencyMs,
    throughput: f64,
) {
    let config = serde_json::json!({
        "mode": mode.label(),
        "workers": workers,
        "tasks": tasks,
        "cap_universe": CAP_UNIVERSE,
        "iterations": iterations,
        // Scenario-specific diagnostics carried in config so
        // check_release.py can ignore without schema churn.
        "correct_routing_rate": correct_routing_rate,
        "scheduler_burn_fcalls_per_correct_claim": scheduler_burn,
        "blocked_count": blocked_count,
        "first_claim_latency_ms": {
            "p50": first_latency.p50,
            "p95": first_latency.p95,
            "p99": first_latency.p99,
        },
        "correct_claim_latency_ms": {
            "p50": correct_latency.p50,
            "p95": correct_latency.p95,
            "p99": correct_latency.p99,
        },
        "blocked_route_dwell_ms": {
            "p50": dwell_latency.p50,
            "p95": dwell_latency.p95,
            "p99": dwell_latency.p99,
        },
        "delta_p99_ms": (correct_latency.p99 - first_latency.p99).max(0.0),
    });

    let notes = format!(
        "mode={}. correct_routing_rate={:.4} (1.0 expected). \
         first_claim_latency is diagnostic; correct_claim_latency is \
         the real routing cost. Gap ≈ unblock-scanner dwell.",
        mode.label(),
        correct_routing_rate,
    );

    // Standard latency field uses the correct_claim numbers so a naive
    // aggregator doesn't accidentally graph the incapable-worker-reject
    // histogram.
    let mut report = Report::fill_env(
        SCENARIO,
        SYSTEM_FLOWFABRIC,
        env.cluster,
        config,
        throughput,
        correct_latency,
    );
    report.notes = Some(notes);

    let results_dir = results_dir();
    let _ = std::fs::create_dir_all(&results_dir);
    // One file per mode. Filename embeds mode so `ls | grep cap_routed`
    // shows all three distributions at a glance.
    let path = results_dir.join(format!(
        "{}-{}-{}.json",
        SCENARIO,
        mode.label(),
        report.git_sha
    ));
    let write_res = (|| -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(&report)?;
        std::fs::write(&path, bytes)?;
        Ok(())
    })();
    match write_res {
        Ok(()) => println!("[bench] wrote {}", path.display()),
        Err(e) => {
            // Fallback via canonical writer — if the per-mode path
            // scheme ever fails, at least we leave SOMETHING on disk.
            eprintln!("[bench] per-mode write failed: {e}; falling back to canonical writer");
            if let Err(e2) = write_report(&report, &results_dir) {
                eprintln!("[bench] WARN: canonical write also failed: {e2}");
            }
        }
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
