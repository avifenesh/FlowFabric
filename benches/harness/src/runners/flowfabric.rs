//! FlowFabric `ComparisonRunner` — the reference implementation for
//! every scenario in the bench suite.
//!
//! Each scenario method lives under its own `run_<scenario>` fn.
//! Scenarios that W1 / W2 haven't landed yet return
//! [`SystemSupport::Unsupported`] so the driver can still produce a
//! report row labelled "not implemented in this commit" — distinct
//! from "this system can't do it".
//!
//! ## Scenario 4 (flow DAG) clock boundaries
//!
//! Three metrics, measured three different ways:
//!
//!   * `flow_setup_ms` — pure client round-trips: first POST /v1/flows
//!     through the last /v1/flows/<id>/edges/apply returning 2xx.
//!     No execution has started at this point. Wall clock sampled on
//!     the client.
//!   * `end_to_end_exec_ms` — from the moment the last edge/apply
//!     returns up to the moment node N's attempt-hash shows
//!     `attempt_state=ended_success`. Reads the server-side
//!     `ended_at` field on the terminal node's attempt hash, NOT a
//!     client poll. A client polling loop would bake poll jitter
//!     into the number.
//!   * `stage_latency_ms[i]` — node i+1 `started_at` minus node i
//!     `ended_at`. Server-side timestamps only; measures scheduler
//!     propagation (dependency_reconciler → exec becomes eligible
//!     → worker claims).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ff_core::types::AttemptIndex;
use ff_sdk::{FlowFabricWorker, WorkerConfig};
use reqwest::Client;
use serde_json::Value as JsonValue;

use crate::comparison::{
    ComparisonRunner, RunConfig, RunResult, Scenario, SystemSupport,
};
use crate::workload::{http_client, now_ms, BenchEnv};

pub struct FlowFabricRunner {
    client: Option<Client>,
}

impl FlowFabricRunner {
    pub fn new() -> Self {
        Self { client: None }
    }

    fn http(&self) -> Result<&Client> {
        self.client
            .as_ref()
            .context("runner not initialised — call setup() first")
    }
}

impl Default for FlowFabricRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ComparisonRunner for FlowFabricRunner {
    fn system_name(&self) -> &'static str {
        crate::report::SYSTEM_FLOWFABRIC
    }

    fn supports(&self, scenario: Scenario) -> SystemSupport {
        match scenario {
            // Scenarios 1 + 4 live in this runner today. The others
            // land as W1/W2 finish their work; the runner returns
            // Unsupported in the interim so the driver can still
            // produce a skeleton report.
            Scenario::SubmitClaimComplete | Scenario::FlowDagLinear => SystemSupport::Native,
            Scenario::SuspendSignalResume => SystemSupport::Unsupported {
                notes: "W2 is implementing scenario 2 on this branch; runner wire-up pending",
            },
            Scenario::CapabilityRouted => SystemSupport::Unsupported {
                notes: "W1 is implementing scenario 5 on this branch; runner wire-up pending",
            },
        }
    }

    async fn setup(&mut self, _env: &BenchEnv) -> Result<()> {
        self.client = Some(http_client()?);
        Ok(())
    }

    async fn teardown(&mut self, _env: &BenchEnv) -> Result<()> {
        self.client = None;
        Ok(())
    }

    async fn run(
        &mut self,
        scenario: Scenario,
        config: &RunConfig,
        env: &BenchEnv,
    ) -> Result<RunResult> {
        match scenario {
            Scenario::FlowDagLinear => self.run_flow_dag(config, env).await,
            Scenario::SubmitClaimComplete => Err(anyhow::anyhow!(
                "scenario 1 is served by benches/harness/benches/submit_claim_complete.rs \
                 (criterion); not wired through the runner trait yet"
            )),
            other => Err(anyhow::anyhow!(
                "FlowFabric runner has no impl for {:?} yet",
                other
            )),
        }
    }
}

// ── Scenario 4: linear flow DAG ─────────────────────────────────────

struct FlowSample {
    flow_setup_ms: f64,
    end_to_end_exec_ms: f64,
    total_ms: f64,
    /// nodes-1 entries; stage_latencies_ms[i] = started_at(i+1) - ended_at(i)
    stage_latencies_ms: Vec<f64>,
}

impl FlowFabricRunner {
    async fn run_flow_dag(
        &mut self,
        config: &RunConfig,
        env: &BenchEnv,
    ) -> Result<RunResult> {
        anyhow::ensure!(config.nodes >= 2, "flow DAG needs nodes >= 2");
        anyhow::ensure!(config.flows >= 1, "flow DAG needs flows >= 1");

        let client = self.http()?.clone();

        // Spawn the echo worker pool BEFORE submitting so claims start
        // immediately. Scenario-4-specific: these workers claim any
        // exec in the bench lane regardless of capability (none set).
        let workers = spawn_echo_workers(env.clone(), config.workers).await?;

        // Submit all flows concurrently. Each flow task records its
        // own three timings + stage latencies and returns a
        // FlowSample. The outer `join_all` lets us wait on the whole
        // batch.
        let bench_start = Instant::now();
        let mut handles = Vec::with_capacity(config.flows);
        for _ in 0..config.flows {
            let client = client.clone();
            let env = env.clone();
            let nodes = config.nodes;
            handles.push(tokio::spawn(async move {
                drive_one_flow(&client, &env, nodes).await
            }));
        }

        let mut samples: Vec<FlowSample> = Vec::with_capacity(config.flows);
        for h in handles {
            match h.await {
                Ok(Ok(s)) => samples.push(s),
                Ok(Err(e)) => tracing::warn!(error = %e, "flow drive failed"),
                Err(e) => tracing::warn!(error = %e, "flow task join failed"),
            }
        }
        let total_wall = bench_start.elapsed();

        // Shutdown workers. Waiting for them to exit so they don't
        // bleed into the next scenario's drain window.
        workers.shutdown().await;

        let flows_count = samples.len();
        anyhow::ensure!(
            flows_count > 0,
            "scenario 4: zero successful flows — harness aborted"
        );

        let throughput = flows_count as f64 / total_wall.as_secs_f64();

        // Convert total_ms into microseconds for the shared latency
        // buffer the Report schema uses.
        let mut latencies_us: Vec<u64> = samples
            .iter()
            .map(|s| (s.total_ms * 1000.0) as u64)
            .collect();
        latencies_us.sort_unstable();

        let notes = format_notes(&samples, flows_count);

        Ok(RunResult {
            throughput_ops_per_sec: throughput,
            latencies_us,
            notes: Some(notes),
        })
    }
}

async fn drive_one_flow(
    client: &Client,
    env: &BenchEnv,
    nodes: usize,
) -> Result<FlowSample> {
    // Phase 1 — client-side setup. Pure round-trip cost, no
    // execution. We batch each rung (create_exec → add_member →
    // wire_edge) sequentially because each POST carries an
    // `expected_graph_revision` that the previous response dictates.
    let setup_start = Instant::now();

    let flow_id = uuid::Uuid::new_v4().to_string();
    create_flow(client, env, &flow_id).await?;

    let mut eids: Vec<String> = Vec::with_capacity(nodes);
    let mut graph_rev: u64 = 0;

    for i in 0..nodes {
        let eid = uuid::Uuid::new_v4().to_string();
        create_execution(client, env, &eid).await?;
        add_to_flow(client, env, &flow_id, &eid).await?;
        graph_rev += 1;
        if i > 0 {
            graph_rev = wire_edge(
                client,
                env,
                &flow_id,
                &eids[i - 1],
                &eid,
                graph_rev,
            )
            .await?;
        }
        eids.push(eid);
    }
    let flow_setup_ms = setup_start.elapsed().as_secs_f64() * 1000.0;

    // Phase 2 — wait for terminal node to finish. Blocking poll on the
    // final exec's state is simple, but we want server-side timestamps
    // for the reporting math. So: poll until state=completed, then
    // pull the per-attempt HGET payloads for all nodes.
    let exec_start = Instant::now();
    let terminal_eid = eids.last().expect("nodes >= 2").to_owned();
    wait_state(client, env, &terminal_eid, "completed", Duration::from_secs(300)).await?;
    let end_to_end_exec_ms = exec_start.elapsed().as_secs_f64() * 1000.0;

    // Phase 3 — server-side timestamps. One HGETALL per attempt hash
    // would be 10 round-trips; we only need started_at + ended_at per
    // node. The attempt_hash helper in the Valkey CLI is out of scope
    // for this bench (no direct Valkey client here by design); go via
    // a new REST surface if one exists, otherwise fall back to
    // executions/<eid> attempt-info.
    //
    // Today, the server exposes started_at and completed_at on the
    // execution core via GET /v1/executions/<eid>. That's one round
    // trip per node; 10 nodes × 100 flows = 1000 calls, trivial
    // compared to the drain wall.
    let mut started_at: Vec<i64> = Vec::with_capacity(nodes);
    let mut ended_at: Vec<i64> = Vec::with_capacity(nodes);
    for eid in &eids {
        let (s, e) = fetch_exec_timestamps(client, env, eid).await?;
        started_at.push(s);
        ended_at.push(e);
    }

    let mut stage_latencies_ms: Vec<f64> = Vec::with_capacity(nodes - 1);
    for i in 0..nodes - 1 {
        // node i+1's started_at minus node i's ended_at. Negative
        // values can happen if clocks skew or the reconciler fired
        // between the HSET calls; clamp to 0 rather than emit
        // nonsense.
        let gap = (started_at[i + 1] - ended_at[i]).max(0);
        stage_latencies_ms.push(gap as f64);
    }

    Ok(FlowSample {
        flow_setup_ms,
        end_to_end_exec_ms,
        total_ms: flow_setup_ms + end_to_end_exec_ms,
        stage_latencies_ms,
    })
}

async fn create_flow(client: &Client, env: &BenchEnv, flow_id: &str) -> Result<()> {
    let body = serde_json::json!({
        "flow_id": flow_id,
        "flow_kind": "bench_scenario_4",
        "namespace": env.namespace,
        "now": now_ms(),
    });
    post_ok(client, &format!("{}/v1/flows", env.server), &body, "create_flow").await
}

async fn create_execution(client: &Client, env: &BenchEnv, eid: &str) -> Result<()> {
    // input_payload serialises as an array of bytes; zero-length is
    // fine. The server-side path accepts empty payloads.
    let body = serde_json::json!({
        "execution_id": eid,
        "namespace": env.namespace,
        "lane_id": env.lane,
        "execution_kind": "bench.flow_dag",
        "input_payload": Vec::<u8>::new(),
        "payload_encoding": "binary",
        "priority": 100,
        "creator_identity": "ff-bench-flow-dag",
        "tags": {},
        "partition_id": 0,
        "now": now_ms(),
    });
    post_ok(client, &format!("{}/v1/executions", env.server), &body, "create_execution").await
}

async fn add_to_flow(client: &Client, env: &BenchEnv, flow_id: &str, eid: &str) -> Result<()> {
    let body = serde_json::json!({
        "flow_id": flow_id,
        "execution_id": eid,
        "now": now_ms(),
    });
    post_ok(
        client,
        &format!("{}/v1/flows/{flow_id}/members", env.server),
        &body,
        "add_to_flow",
    )
    .await
}

async fn wire_edge(
    client: &Client,
    env: &BenchEnv,
    flow_id: &str,
    upstream: &str,
    downstream: &str,
    expected_graph_rev: u64,
) -> Result<u64> {
    let edge_id = uuid::Uuid::new_v4().to_string();

    let stage_body = serde_json::json!({
        "flow_id": flow_id,
        "edge_id": edge_id,
        "upstream_execution_id": upstream,
        "downstream_execution_id": downstream,
        "dependency_kind": "success_only",
        "expected_graph_revision": expected_graph_rev,
        "now": now_ms(),
    });
    let stage_url = format!("{}/v1/flows/{flow_id}/edges", env.server);
    let resp = client.post(&stage_url).json(&stage_body).send().await?;
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("stage_edge failed ({status}): {body_text}");
    }
    let v: JsonValue = serde_json::from_str(&body_text)
        .with_context(|| format!("stage_edge response decode: {body_text}"))?;
    let new_rev = v
        .pointer("/Staged/new_graph_revision")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| anyhow::anyhow!("stage_edge: missing new_graph_revision"))?;

    let apply_body = serde_json::json!({
        "flow_id": flow_id,
        "edge_id": edge_id,
        "downstream_execution_id": downstream,
        "upstream_execution_id": upstream,
        "graph_revision": new_rev,
        "dependency_kind": "success_only",
        "now": now_ms(),
    });
    post_ok(
        client,
        &format!("{}/v1/flows/{flow_id}/edges/apply", env.server),
        &apply_body,
        "apply_edge",
    )
    .await?;
    Ok(new_rev)
}

async fn post_ok(
    client: &Client,
    url: &str,
    body: &serde_json::Value,
    op: &str,
) -> Result<()> {
    let resp = client.post(url).json(body).send().await?;
    let status = resp.status();
    if status.is_success() {
        // Drain the body so the connection returns to the pool; we
        // don't need the payload here.
        let _ = resp.text().await;
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default();
    anyhow::bail!("{op} failed ({status}): {text}")
}

async fn wait_state(
    client: &Client,
    env: &BenchEnv,
    eid: &str,
    target: &str,
    deadline: Duration,
) -> Result<()> {
    let url = format!("{}/v1/executions/{eid}/state", env.server);
    let start = Instant::now();
    loop {
        let resp = client.get(&url).send().await?;
        if resp.status().is_success() {
            let v: JsonValue = resp.json().await?;
            let state = v.as_str().unwrap_or_default();
            if state == target {
                return Ok(());
            }
            if matches!(state, "failed" | "cancelled" | "expired") {
                anyhow::bail!("exec {eid} ended in {state}, wanted {target}");
            }
        }
        if start.elapsed() > deadline {
            anyhow::bail!("wait_state({eid}, {target}): deadline exceeded");
        }
        // Brief sleep — we only need to observe terminal for the
        // reporting gate. The measurement-critical timestamp math
        // below reads server-side fields, so client poll jitter
        // doesn't pollute the numbers.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Fetch started_at + completed_at timestamps (ms since epoch) for an
/// execution we've already observed in terminal state. Bails loudly on
/// missing/unparseable fields: at call time the caller has confirmed
/// `state == "completed"` via `wait_state`, so both timestamps must be
/// populated. Silently returning 0 (the prior behavior) zeroed every
/// stage_latency sample in the scenario 4 report — R1 HIGH finding F1.
///
/// ExecutionInfo now serialises these as `Option<String>` (matching
/// the existing `created_at: String` pattern); both are elided when
/// empty on the wire, populated once Lua's HSET writes them to
/// exec_core.
async fn fetch_exec_timestamps(
    client: &Client,
    env: &BenchEnv,
    eid: &str,
) -> Result<(i64, i64)> {
    let url = format!("{}/v1/executions/{eid}", env.server);
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("get_execution({eid}) failed ({status}): {text}");
    }
    let v: JsonValue = resp.json().await?;

    let parse_ts = |field: &str| -> Result<i64> {
        let raw = v
            .pointer(&format!("/{field}"))
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow::anyhow!("exec {eid} missing field {field} post-completion"))?;
        raw.parse::<i64>()
            .map_err(|e| anyhow::anyhow!("exec {eid} field {field}={raw:?}: {e}"))
    };
    let started = parse_ts("started_at")?;
    let completed = parse_ts("completed_at")?;
    Ok((started, completed))
}

// ── Echo worker pool ────────────────────────────────────────────────

struct WorkerPool {
    handles: Vec<tokio::task::JoinHandle<()>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl WorkerPool {
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        for h in self.handles {
            let _ = h.await;
        }
    }
}

async fn spawn_echo_workers(env: BenchEnv, count: usize) -> Result<WorkerPool> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let shutdown_rx = Arc::new(shutdown_rx);
    let mut handles = Vec::with_capacity(count);
    for wi in 0..count {
        let env = env.clone();
        let shutdown_rx = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = echo_worker_loop(wi, env, shutdown_rx).await {
                tracing::warn!(worker = wi, error = %e, "echo worker exited with error");
            }
        }));
    }
    Ok(WorkerPool {
        handles,
        shutdown_tx,
    })
}

/// Minimal worker that claims and immediately completes.
///
/// Zero work in the hot path is deliberate: scenario 4 measures flow
/// overhead (scheduler propagation + dependency resolution), not
/// worker runtime. A realistic workload would add its own duration on
/// top of every stage_latency sample the bench reports; the reported
/// numbers are therefore a *floor* — real pipelines run slower, by
/// the cost of whatever the workers actually do.
async fn echo_worker_loop(
    wi: usize,
    env: BenchEnv,
    shutdown_rx: Arc<tokio::sync::watch::Receiver<bool>>,
) -> Result<()> {
    let mut config = WorkerConfig::new(
        &env.valkey_host,
        env.valkey_port,
        format!("bench-echo-{wi}"),
        format!("bench-echo-{wi}-{}", uuid::Uuid::new_v4()),
        &env.namespace,
        &env.lane,
    );
    config.claim_poll_interval_ms = 50;
    let worker = FlowFabricWorker::connect(config).await?;
    let _ = AttemptIndex::new(0); // compile-time guard that the SDK type alias still exists

    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }
        match worker.claim_next().await? {
            Some(task) => {
                task.complete(None).await?;
            }
            None => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

// ── Reporting helpers ───────────────────────────────────────────────

fn format_notes(samples: &[FlowSample], _count: usize) -> String {
    let (setup_p50, _setup_p95, _setup_p99) = percentiles(samples.iter().map(|s| s.flow_setup_ms));
    let (exec_p50, exec_p95, exec_p99) = percentiles(samples.iter().map(|s| s.end_to_end_exec_ms));

    let all_stages: Vec<f64> = samples
        .iter()
        .flat_map(|s| s.stage_latencies_ms.iter().copied())
        .collect();
    let (stage_p50, stage_p95, stage_p99) = percentiles(all_stages.iter().copied());

    // Ratio guards against 0 to avoid NaN when every exec completed in
    // the same millisecond — possible for tiny smoke runs.
    let ratio = if exec_p50 > 0.0 {
        setup_p50 / exec_p50
    } else {
        0.0
    };

    // Structured key=value pattern so operators / check_release.py can
    // grep without parsing JSON-in-a-string.
    format!(
        "flow_setup_p50_ms={setup_p50:.2},exec_p50_ms={exec_p50:.2},exec_p95_ms={exec_p95:.2},exec_p99_ms={exec_p99:.2},stage_latency_p50_ms={stage_p50:.2},stage_latency_p95_ms={stage_p95:.2},stage_latency_p99_ms={stage_p99:.2},setup_exec_ratio={ratio:.2}"
    )
}

fn percentiles<I: IntoIterator<Item = f64>>(values: I) -> (f64, f64, f64) {
    let mut v: Vec<f64> = values.into_iter().collect();
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |q: f64| {
        let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    (at(0.50), at(0.95), at(0.99))
}
