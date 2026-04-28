//! retry-and-cancel — end-to-end example that exercises two core
//! FlowFabric control-plane behaviours against a live `ff-server`:
//!
//! 1. **Retry exhaustion → terminal failed.** A flaky handler that
//!    always errors drives an execution through its retry schedule
//!    (`max_retries = 2`, fixed 50ms backoff). After the third attempt
//!    fails, `FailOutcome::TerminalFailed` is returned by the SDK and
//!    the server transitions the execution to the `failed` terminal
//!    state.
//!
//! 2. **`cancel_flow` cascade.** A flow with two member executions is
//!    cancelled via `POST /v1/flows/{id}/cancel?wait=true` with
//!    `cancellation_policy = "cancel_all"`. The handler returns
//!    synchronously and the example asserts that every member execution
//!    reaches the `cancelled` terminal state.
//!
//! The example is intentionally single-process: the worker loop runs in
//! a background tokio task so the orchestrator can drive the scenes and
//! terminate when the flow reaches its terminal state. No Ctrl-C needed.
//!
//! # Prereqs
//!
//! * `valkey-server` reachable at `FF_HOST:FF_PORT` (defaults
//!   `localhost:6379`).
//! * `ff-server` (the FlowFabric HTTP API / control-plane) reachable at
//!   `FF_SERVER_URL` (default `http://localhost:9090`).
//!
//! # Run
//!
//! ```text
//! cargo run -p retry-and-cancel-example
//! ```

mod flaky_api;

use std::sync::Arc;
use std::time::Duration;

use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::policy::{BackoffStrategy, ExecutionPolicy, RetryPolicy};
use ff_core::types::{ExecutionId, FlowId, LaneId};
use ff_sdk::{FailOutcome, FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};
use flaky_api::FlakyPayload;
use tokio::sync::Notify;
use tokio::time::timeout;
use tracing::info;

const NAMESPACE: &str = "demo";
const LANE: &str = "default";
const WORKER_POOL: &str = "retry-and-cancel-worker";

// Bounded self-termination guard so the example cannot hang forever
// (e.g. ff-server unreachable, worker deadlocked). 60s is comfortable
// for the two scenes combined: scene 1 finishes in <500ms of backoff,
// scene 2 in a single wait=true round-trip.
const DEMO_BUDGET: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "retry_and_cancel=info,ff_sdk=warn".into()),
        )
        .init();

    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);
    let server_url =
        std::env::var("FF_SERVER_URL").unwrap_or_else(|_| "http://localhost:9090".into());
    let num_flow_partitions: u16 = std::env::var("FF_FLOW_PARTITIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let partition_config = PartitionConfig {
        num_flow_partitions,
        ..PartitionConfig::default()
    };

    // Shared admin client (HTTP) for both the orchestrator and the
    // worker's server-routed claim path.
    let admin = Arc::new(FlowFabricAdminClient::new(&server_url)?);
    let http = reqwest::Client::new();

    // Spawn the worker loop. The orchestrator signals shutdown via
    // `done` once both scenes finish; the worker then exits its select
    // loop cleanly.
    let done = Arc::new(Notify::new());
    let worker_handle = {
        let admin = Arc::clone(&admin);
        let done = Arc::clone(&done);
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(e) = run_worker(host, port, admin, done).await {
                tracing::error!(error = %e, "worker loop failed");
            }
        })
    };

    let scene_result = timeout(DEMO_BUDGET, async {
        scene_retry_exhaustion(&http, &server_url, &partition_config).await?;
        scene_cancel_flow_cascade(&http, &server_url, &partition_config).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await;

    // Shut the worker down before surfacing any scene error so the
    // background task doesn't leak past `main`.
    done.notify_waiters();
    let _ = worker_handle.await;

    match scene_result {
        Ok(Ok(())) => {
            info!("demo complete — both scenes terminated");
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "demo exceeded {}s budget (is ff-server at {server_url} reachable?)",
            DEMO_BUDGET.as_secs()
        )
        .into()),
    }
}

// ─── Scene 1: retry exhaustion ───

async fn scene_retry_exhaustion(
    http: &reqwest::Client,
    server_url: &str,
    partition_config: &PartitionConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("── scene 1: retry exhaustion ──");

    // Two retries + the initial attempt = three FCALL rounds. Fixed
    // 50ms backoff keeps the demo fast while still exercising the
    // retry-schedule path (not a same-tick loop).
    let policy = ExecutionPolicy {
        retry_policy: Some(RetryPolicy {
            max_retries: 2,
            backoff: BackoffStrategy::Fixed { delay_ms: 50 },
            retryable_categories: Vec::new(),
        }),
        ..ExecutionPolicy::default()
    };

    let lane = LaneId::try_new(LANE)?;
    let eid = ExecutionId::solo(&lane, partition_config);
    submit_execution(
        http,
        server_url,
        &eid,
        &FlakyPayload {
            label: "scene-1".into(),
        },
        &policy,
    )
    .await?;
    info!(execution_id = %eid, "submitted flaky execution");

    let final_state = poll_until_terminal(http, server_url, &eid.to_string(), Duration::from_secs(15)).await?;
    info!(execution_id = %eid, state = %final_state, "scene 1 terminal");

    if final_state != "failed" {
        return Err(format!(
            "scene 1: expected terminal 'failed' after retry exhaustion, got '{final_state}'"
        )
        .into());
    }
    Ok(())
}

// ─── Scene 2: cancel_flow cascade ───

async fn scene_cancel_flow_cascade(
    http: &reqwest::Client,
    server_url: &str,
    partition_config: &PartitionConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("── scene 2: cancel_flow cascade ──");

    let flow_id = FlowId::new();
    let flow_partition_idx = flow_partition(&flow_id, partition_config).index;
    let start_ms = now_ms();

    // Create the flow (idempotent on re-POST of the same id).
    let create_flow_body = serde_json::json!({
        "flow_id": flow_id,
        "flow_kind": "pipeline",
        "namespace": NAMESPACE,
        "now": start_ms,
    });
    let resp = http
        .post(format!("{server_url}/v1/flows"))
        .json(&create_flow_body)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!("create_flow failed: {} {}", resp.status(), resp.text().await?).into());
    }
    info!(flow_id = %flow_id, "flow created");

    // Create two member executions. Use a long-enough delay_until that
    // the worker will NOT pick them up before cancel_flow fires. That
    // way scene 2 isolates the cancel_all cascade from the worker
    // runtime path.
    let far_future = start_ms + 600_000; // +10 minutes
    let mut member_eids = Vec::with_capacity(2);
    for i in 0..2 {
        let policy = ExecutionPolicy {
            delay_until: Some(ff_core::types::TimestampMs(far_future)),
            ..ExecutionPolicy::default()
        };
        // Members must co-locate on the flow's partition (hash-tag
        // co-location under RFC-011). `ExecutionId::for_flow` bakes
        // the flow's partition index into the hash-tag.
        let eid = ExecutionId::for_flow(&flow_id, partition_config);
        submit_execution(
            http,
            server_url,
            &eid,
            &FlakyPayload {
                label: format!("scene-2-member-{i}"),
            },
            &policy,
        )
        .await?;
        let add_body = serde_json::json!({
            "flow_id": flow_id,
            "execution_id": eid,
            "now": now_ms(),
        });
        let resp = http
            .post(format!("{server_url}/v1/flows/{flow_id}/members"))
            .json(&add_body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(format!(
                "add_execution_to_flow failed: {} {}",
                resp.status(),
                resp.text().await?
            )
            .into());
        }
        info!(execution_id = %eid, "member added to flow");
        member_eids.push(eid);
    }
    info!(flow_id = %flow_id, partition = flow_partition_idx, "flow members created (delayed, unclaimable)");

    // Cancel the flow with cancel_all + wait=true so the handler
    // returns only after every member has been cancelled.
    let cancel_body = serde_json::json!({
        "flow_id": flow_id,
        "reason": "demo_cancel_all",
        "cancellation_policy": "cancel_all",
        "now": now_ms(),
    });
    let resp = http
        .post(format!("{server_url}/v1/flows/{flow_id}/cancel?wait=true"))
        .json(&cancel_body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(format!("cancel_flow failed: {status} {text}").into());
    }
    info!(flow_id = %flow_id, response = %text, "cancel_flow returned");

    // Verify every member execution is in the cancelled terminal
    // state. wait=true should have flushed the cascade synchronously;
    // we still poll (short window) to tolerate observable-state
    // propagation jitter.
    for eid in &member_eids {
        let state = poll_until_terminal(http, server_url, &eid.to_string(), Duration::from_secs(10)).await?;
        if state != "cancelled" {
            return Err(format!(
                "scene 2: member {eid} expected 'cancelled', got '{state}'"
            )
            .into());
        }
        info!(execution_id = %eid, "member cancelled");
    }
    Ok(())
}

// ─── Worker ───

async fn run_worker(
    host: String,
    port: u16,
    admin: Arc<FlowFabricAdminClient>,
    done: Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(host, port),
        worker_id: ff_core::types::WorkerId::new(WORKER_POOL),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "{WORKER_POOL}-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: ff_core::types::Namespace::new(NAMESPACE),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    partition_config: None,
    };
    let worker = FlowFabricWorker::connect(config).await?;
    let lane = LaneId::try_new(LANE)?;

    info!("worker loop started");
    loop {
        tokio::select! {
            _ = done.notified() => {
                info!("worker shutdown requested");
                return Ok(());
            }
            claim = worker.claim_via_server(admin.as_ref(), &lane, 10_000) => {
                match claim {
                    Ok(Some(task)) => {
                        let eid = task.execution_id().to_string();
                        let attempt = task.attempt_index().0;
                        info!(execution_id = %eid, attempt, "claimed task");

                        // Deliberately fail. The server applies the
                        // retry policy: on the first two calls we get
                        // `FailOutcome::RetryScheduled`; on the third
                        // we get `TerminalFailed` and the execution
                        // moves to the `failed` terminal state.
                        let err = flaky_api::run(
                            &FlakyPayload { label: "flaky".into() },
                            attempt,
                        )
                        .expect_err("flaky_api always errors");
                        match task.fail(&err, "simulated_503").await? {
                            FailOutcome::RetryScheduled { delay_until } => {
                                info!(execution_id = %eid, delay_until_ms = delay_until.0, "retry scheduled");
                            }
                            FailOutcome::TerminalFailed => {
                                info!(execution_id = %eid, "retries exhausted — terminal failed");
                            }
                        }
                    }
                    Ok(None) => {
                        // Nothing to claim — short sleep keeps the
                        // demo responsive without hammering the server.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "claim failed");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }
}

// ─── HTTP helpers ───

async fn submit_execution(
    http: &reqwest::Client,
    server_url: &str,
    execution_id: &ExecutionId,
    payload: &FlakyPayload,
    policy: &ExecutionPolicy,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let payload_bytes = serde_json::to_vec(payload)?;
    // Use the typed accessor rather than reparsing the formatted
    // execution id string. The server rejects mismatched
    // `partition_id`, so this stays the single source of truth.
    let partition_id: u16 = execution_id.partition();

    // `ExecutionPolicy` now serializes with `skip_serializing_if =
    // "Option::is_none"` on every optional field, so unset fields are
    // omitted entirely (prior behaviour emitted `null`, which Lua's
    // `cjson.decode` maps to a `cjson.null` sentinel — that fails the
    // `type(field) == "table"` checks in `lua/policy.lua` and produced
    // `invalid_policy_json:<field>:not_object` errors on every consumer
    // that posted a default-constructed policy).
    let policy_value = serde_json::to_value(policy)?;

    let body = serde_json::json!({
        "execution_id": execution_id,
        "namespace": NAMESPACE,
        "lane_id": LANE,
        "execution_kind": "retry_cancel_demo",
        "input_payload": payload_bytes,
        "payload_encoding": "json",
        "priority": policy.priority,
        "creator_identity": "retry-and-cancel-example",
        "idempotency_key": null,
        "tags": {},
        "policy": policy_value,
        "delay_until": policy.delay_until.map(|t| t.0),
        "partition_id": partition_id,
        "now": now_ms(),
    });

    tracing::debug!(execution_id = %execution_id, "POST /v1/executions");
    let resp = http
        .post(format!("{server_url}/v1/executions"))
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    tracing::debug!(execution_id = %execution_id, status = %status, "create_execution response");
    if !status.is_success() {
        return Err(format!("create_execution failed: {status} {text}").into());
    }
    Ok(())
}

async fn poll_until_terminal(
    http: &reqwest::Client,
    server_url: &str,
    execution_id: &str,
    budget: Duration,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let deadline = tokio::time::Instant::now() + budget;
    let url = format!("{server_url}/v1/executions/{execution_id}/state");
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(
                format!("poll_until_terminal: execution {execution_id} did not terminate in {}s", budget.as_secs())
                    .into(),
            );
        }
        let resp = http.get(&url).send().await?;
        if resp.status().is_success() {
            let state: String = resp.json::<serde_json::Value>().await?
                .as_str()
                .unwrap_or("unknown")
                .to_owned();
            // All public terminal states (see `ff_core::state::PublicState`).
            // Missing any of these causes `poll_until_terminal` to spin
            // until the budget expires, producing a misleading timeout.
            if matches!(
                state.as_str(),
                "completed" | "failed" | "cancelled" | "expired" | "skipped"
            ) {
                return Ok(state);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_millis() as i64
}
