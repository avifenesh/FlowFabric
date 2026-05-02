//! token-budget — UC-37 (per-attempt token / cost accounting) + UC-39
//! (flow-level budget enforcement) exercised end-to-end against a live
//! FlowFabric deployment.
//!
//! **Scenario.** A `batch-inference` runner dispatches a batch of LLM
//! prompts as child executions of one flow. The flow shares a single
//! token budget (`scope_type = "flow"`). Each worker simulates an
//! inference, reports per-attempt usage via [`report_usage`], and
//! completes. The submitter polls budget status; when usage crosses
//! the soft threshold it logs a warning, and when it breaches the
//! hard limit it cancels the flow with `cancel_pending` so in-flight
//! executions drain while new work is rejected.
//!
//! This exercises:
//!
//! | UC     | Surface                                                 | Callsite                         |
//! |--------|---------------------------------------------------------|----------------------------------|
//! | UC-29  | Fan-out: one flow, N children                           | [`create_flow_and_members`]      |
//! | UC-34  | Flow-scoped failure policy: `cancel_pending` on breach  | [`poll_and_enforce_budget`]      |
//! | UC-37  | Real-time per-attempt token accounting                  | [`run_worker_loop`]              |
//! | UC-39  | Whole-flow shared max-token + max-cost budget           | [`create_flow_budget`]           |
//!
//! v0.9 surface naturally exercised (not narrated): `backend.prepare()`
//! (#281), `backend.seed_waitpoint_hmac_secret()` (#280),
//! `LeaseSummary { lease_id, attempt_index, last_heartbeat_at }`
//! (#278), `flowfabric` umbrella crate (#279),
//! `flowfabric::sdk::ClaimGrant` (#283).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use flowfabric::core::backend::{CancelFlowPolicy, CancelFlowWait};
use flowfabric::core::contracts::{
    CreateBudgetArgs, CreateBudgetResult, ReportUsageResult, SeedWaitpointHmacSecretArgs,
};
use flowfabric::core::partition::{flow_partition, PartitionConfig};
use flowfabric::core::policy::ExecutionPolicy;
use flowfabric::core::types::{
    BudgetId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId,
};
use flowfabric::sdk::{
    FailOutcome, FlowFabricAdminClient, FlowFabricWorker, WorkerConfig,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, timeout, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const NAMESPACE: &str = "demo";
const LANE: &str = "default";
const WORKER_POOL: &str = "token-budget-worker";
const FLOW_KIND: &str = "batch-inference";
const DEMO_BUDGET: Duration = Duration::from_secs(60);

// ── Budget parameters ──
//
// Tuned so the batch (5 prompts × ~100-500 tokens each) deterministically
// trips the soft limit, usually breaches the hard limit, and the flow
// is cancelled mid-batch. The seeded RNG below keeps the run reproducible.
const PROMPT_COUNT: usize = 5;
const HARD_LIMIT_TOKENS: u64 = 1_200;
const SOFT_LIMIT_TOKENS: u64 = 800;
const HARD_LIMIT_COST_MICROS: u64 = 15_000; // $0.015, simulated
const SOFT_LIMIT_COST_MICROS: u64 = 10_000;
const COST_PER_1K_TOKENS_MICROS: u64 = 25; // simulated $0.000025/token

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PromptPayload {
    prompt: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "token_budget=info,flowfabric=warn".into()),
        )
        .init();

    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);
    let server_url =
        std::env::var("FF_SERVER_URL").unwrap_or_else(|_| "http://localhost:9090".into());
    let partition_config = PartitionConfig::default();

    let admin = Arc::new(FlowFabricAdminClient::new(&server_url)?);
    let http = reqwest::Client::new();

    // v0.9: one `WorkerConfig` carries the backend pin + the worker
    // identity. The worker's connect() resolves the backend handle we
    // use below for `create_budget` / `get_budget_status` / `cancel_flow`
    // — no second Valkey connection.
    let worker = FlowFabricWorker::connect(WorkerConfig {
        backend: Some(flowfabric::core::backend::BackendConfig::valkey(host.clone(), port)),
        worker_id: WorkerId::new(WORKER_POOL),
        worker_instance_id: WorkerInstanceId::new(format!(
            "{WORKER_POOL}-{}",
            uuid_suffix()
        )),
        namespace: Namespace::new(NAMESPACE),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 500,
        max_concurrent_tasks: 1,
    partition_config: None,
    })
    .await?;

    let backend = worker
        .backend()
        .ok_or_else(|| anyhow!("worker has no engine backend handle"))?
        .clone();

    // v0.9 #281 + #280 — boot-time idempotent provisioning. A fresh
    // consumer does this once at startup so the deployment is ready to
    // serve waitpoint-backed features. Cheap and no-op on repeat runs.
    backend
        .prepare()
        .await
        .context("backend.prepare() (v0.9 #281)")?;
    // Match ff-server's default kid ("k1") so re-running against an
    // already-provisioned server returns `SeedOutcome::AlreadySeeded`
    // — the natural idempotent path. A fresh deployment gets the same
    // kid + caller-supplied secret installed on first boot.
    let seed_kid = std::env::var("FF_WAITPOINT_HMAC_KID").unwrap_or_else(|_| "k1".into());
    let seed_secret_hex = std::env::var("FF_WAITPOINT_HMAC_SECRET")
        .unwrap_or_else(|_| "0".repeat(64));
    let seed_outcome = backend
        .seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(&seed_kid, &seed_secret_hex))
        .await
        .context("seed_waitpoint_hmac_secret (v0.9 #280)")?;
    info!(outcome = ?seed_outcome, "waitpoint HMAC secret provisioned");

    // ── Submit phase ──

    let flow_id = FlowId::new();
    let budget_id = BudgetId::new();

    create_flow_budget(&backend, &budget_id, &flow_id).await?;
    let execution_ids = create_flow_and_members(
        &http,
        &server_url,
        &flow_id,
        PROMPT_COUNT,
        &partition_config,
    )
    .await?;

    // Worker loop drains claims in the background; submit polls the
    // budget and owns the cancel decision.
    //
    // Uses tokio-util CancellationToken (level-triggered) rather
    // than tokio::sync::Notify (edge-triggered, loses cancels fired
    // while the worker is between polls — #483).
    let shutdown = CancellationToken::new();
    let worker_handle = {
        let shutdown = shutdown.clone();
        let admin = Arc::clone(&admin);
        let budget_id = budget_id.clone();
        let worker = Arc::new(worker);
        let worker_for_task = Arc::clone(&worker);
        tokio::spawn(async move {
            if let Err(e) = run_worker_loop(worker_for_task, admin, budget_id, shutdown).await {
                tracing::error!(error = %e, "worker loop failed");
            }
        })
    };

    let outcome = timeout(DEMO_BUDGET, async {
        poll_and_enforce_budget(&backend, &budget_id, &flow_id).await
    })
    .await
    .map_err(|_| anyhow!("demo exceeded {}s budget", DEMO_BUDGET.as_secs()))??;

    // Give in-flight reports time to drain, then stop the worker.
    sleep(Duration::from_millis(500)).await;
    shutdown.cancel();
    // Bound the worker-shutdown wait so the example can't hang past
    // the demo budget. `claim_via_server` blocks up to its block_ms;
    // since CancellationToken is level-triggered the in-flight claim
    // will exit promptly once its block finishes, but we still cap
    // the join at a generous ceiling to guard against a wedged call.
    let _ = timeout(Duration::from_secs(15), worker_handle).await;

    // ── Post-mortem: per-child LeaseSummary (v0.9 #278). Even a
    // terminal exec carries its last-seen lease fields in the snapshot,
    // which is the natural inspection surface for operator tooling.
    print_post_mortem(&backend, &execution_ids).await?;

    let status = backend.get_budget_status(&budget_id).await?;
    info!(
        final_tokens = status.usage.get("tokens").copied().unwrap_or(0),
        final_cost_micros = status.usage.get("cost_micros").copied().unwrap_or(0),
        hard_limit_tokens = HARD_LIMIT_TOKENS,
        breach_count = status.breach_count,
        soft_breach_count = status.soft_breach_count,
        "final budget status"
    );
    info!(outcome = ?outcome, "demo complete");

    Ok(())
}

// ── Budget + flow wiring ──

async fn create_flow_budget(
    backend: &Arc<dyn flowfabric::core::engine_backend::EngineBackend>,
    budget_id: &BudgetId,
    flow_id: &FlowId,
) -> Result<()> {
    // UC-39: the whole flow's executions share one budget keyed by
    // `scope_type=flow`, `scope_id=<flow_id>`. Two dimensions (tokens
    // + simulated cost) so the SoftBreach / HardBreach signals fire on
    // whichever pressure hits first.
    let args = CreateBudgetArgs {
        budget_id: budget_id.clone(),
        scope_type: "flow".into(),
        scope_id: flow_id.to_string(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0, // no auto-reset; this flow is single-shot
        dimensions: vec!["tokens".into(), "cost_micros".into()],
        hard_limits: vec![HARD_LIMIT_TOKENS, HARD_LIMIT_COST_MICROS],
        soft_limits: vec![SOFT_LIMIT_TOKENS, SOFT_LIMIT_COST_MICROS],
        now: TimestampMs::now(),
    };
    match backend.create_budget(args).await? {
        CreateBudgetResult::Created { .. } => {
            info!(budget_id = %budget_id, flow_id = %flow_id, hard_tokens = HARD_LIMIT_TOKENS, "flow budget created");
        }
        CreateBudgetResult::AlreadySatisfied { .. } => {
            info!(budget_id = %budget_id, "flow budget already existed (idempotent)");
        }
    }
    Ok(())
}

async fn create_flow_and_members(
    http: &reqwest::Client,
    server_url: &str,
    flow_id: &FlowId,
    prompt_count: usize,
    partition_config: &PartitionConfig,
) -> Result<Vec<ExecutionId>> {
    let now = now_ms();
    let create_flow_body = serde_json::json!({
        "flow_id": flow_id,
        "flow_kind": FLOW_KIND,
        "namespace": NAMESPACE,
        "now": now,
    });
    let resp = http
        .post(format!("{server_url}/v1/flows"))
        .json(&create_flow_body)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "create_flow failed: {} {}",
            resp.status(),
            resp.text().await?
        ));
    }
    let partition = flow_partition(flow_id, partition_config);
    info!(flow_id = %flow_id, partition = partition.index, children = prompt_count, "flow created");

    let mut eids = Vec::with_capacity(prompt_count);
    for i in 0..prompt_count {
        // Members must co-locate on the flow partition (RFC-011).
        let eid = ExecutionId::for_flow(flow_id, partition_config);
        let payload = PromptPayload {
            prompt: format!("prompt-{i}: summarize the FlowFabric v0.9 release notes"),
        };
        let payload_bytes = serde_json::to_vec(&payload)?;
        let policy = ExecutionPolicy::default();
        let body = serde_json::json!({
            "execution_id": eid,
            "namespace": NAMESPACE,
            "lane_id": LANE,
            "execution_kind": "llm_inference",
            "input_payload": payload_bytes,
            "payload_encoding": "json",
            "priority": policy.priority,
            "creator_identity": "token-budget-example",
            "idempotency_key": null,
            "tags": {},
            "policy": serde_json::to_value(&policy)?,
            "delay_until": null,
            "partition_id": eid.partition(),
            "now": now_ms(),
        });
        let resp = http
            .post(format!("{server_url}/v1/executions"))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "create_execution failed: {} {}",
                resp.status(),
                resp.text().await?
            ));
        }

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
            return Err(anyhow!(
                "add_execution_to_flow failed: {} {}",
                resp.status(),
                resp.text().await?
            ));
        }
        info!(execution_id = %eid, index = i, "child execution enqueued");
        eids.push(eid);
    }
    Ok(eids)
}

// ── Budget-enforcement loop (UC-34) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    DrainedUnderBudget,
    HardBreachCancelled,
}

async fn poll_and_enforce_budget(
    backend: &Arc<dyn flowfabric::core::engine_backend::EngineBackend>,
    budget_id: &BudgetId,
    flow_id: &FlowId,
) -> Result<PollOutcome> {
    let deadline = Instant::now() + DEMO_BUDGET;
    let mut soft_warned = false;
    loop {
        if Instant::now() > deadline {
            return Err(anyhow!("budget-enforcement poll exceeded demo budget"));
        }
        let status = backend.get_budget_status(budget_id).await?;
        let tokens = status.usage.get("tokens").copied().unwrap_or(0);
        let cost = status.usage.get("cost_micros").copied().unwrap_or(0);

        // Soft-limit warning: UC-39's "approaching exhaustion" signal.
        // We log once and keep serving in-flight work — exactly what
        // the UC calls for.
        if !soft_warned && (tokens >= SOFT_LIMIT_TOKENS || cost >= SOFT_LIMIT_COST_MICROS) {
            warn!(
                tokens,
                cost_micros = cost,
                soft_tokens = SOFT_LIMIT_TOKENS,
                soft_cost_micros = SOFT_LIMIT_COST_MICROS,
                "soft limit approaching — flow will reject new claims on hard breach"
            );
            soft_warned = true;
        }

        // Hard-limit cancellation: UC-34 flow-scoped failure policy.
        // `cancel_pending` lets currently-running members drain; newly
        // eligible children get cancelled synchronously.
        //
        // Two trip signals, either is sufficient:
        //   1. Usage observably exceeds a hard limit (strict accounting
        //      model — only trips if Lua applied the increment before
        //      the breach threshold).
        //   2. `breach_count > 0` — the Lua `ff_report_usage_and_check`
        //      counter for rejected-because-would-breach attempts. The
        //      more common trip in practice since the strict-mode
        //      implementation rejects the increment rather than applying
        //      it past the hard limit.
        if tokens >= HARD_LIMIT_TOKENS
            || cost >= HARD_LIMIT_COST_MICROS
            || status.breach_count > 0
        {
            warn!(
                tokens,
                cost_micros = cost,
                hard_tokens = HARD_LIMIT_TOKENS,
                hard_cost_micros = HARD_LIMIT_COST_MICROS,
                "hard budget breached — cancelling flow"
            );
            let result = backend
                .cancel_flow(flow_id, CancelFlowPolicy::CancelPending, CancelFlowWait::NoWait)
                .await?;
            info!(result = ?result, "cancel_flow returned");
            return Ok(PollOutcome::HardBreachCancelled);
        }

        // Flow drained under budget? Check by describing the flow.
        if let Some(flow) = backend.describe_flow(flow_id).await? {
            // Happy path: every child terminated before we tripped.
            // (`describe_flow.public_flow_state` is authoritative on
            // `flow_core`; see `FlowSnapshot` docs.)
            if matches!(flow.public_flow_state.as_str(), "completed" | "failed") {
                return Ok(PollOutcome::DrainedUnderBudget);
            }
        }

        sleep(Duration::from_millis(150)).await;
    }
}

// ── Worker loop (UC-37) ──

async fn run_worker_loop(
    worker: Arc<FlowFabricWorker>,
    admin: Arc<FlowFabricAdminClient>,
    budget_id: BudgetId,
    shutdown: CancellationToken,
) -> Result<()> {
    let lane = LaneId::new(LANE);
    info!("worker loop started");
    loop {
        // Level-triggered short-circuit — catches cancels fired
        // between iterations when no `.cancelled()` waiter was armed.
        if shutdown.is_cancelled() {
            info!("worker shutdown requested");
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("worker shutdown requested");
                return Ok(());
            }
            claim = worker.claim_via_server(admin.as_ref(), &lane, 1_000) => {
                match claim {
                    Ok(Some(task)) => {
                        let eid = task.execution_id().clone();
                        let attempt = task.attempt_index().0;
                        let (input_tokens, output_tokens) = simulate_inference(&eid);
                        let total_tokens = input_tokens + output_tokens;
                        let cost_micros = total_tokens * COST_PER_1K_TOKENS_MICROS / 1_000;

                        info!(
                            execution_id = %eid,
                            attempt,
                            input_tokens,
                            output_tokens,
                            cost_micros,
                            "simulated inference complete; reporting usage"
                        );

                        // UC-37: per-attempt token + cost accounting.
                        // `dedup_key` is derived from the execution id +
                        // attempt so a retried attempt doesn't
                        // double-count (RFC-012 §R7.2.3).
                        let dedup = format!("{eid}:{attempt}");
                        let usage = task.report_usage(
                            &budget_id,
                            &[
                                ("tokens", total_tokens),
                                ("cost_micros", cost_micros),
                            ],
                            Some(&dedup),
                        ).await;

                        match usage {
                            Ok(ReportUsageResult::Ok) => {
                                info!(execution_id = %eid, "usage reported; within budget");
                                task.complete(Some(serde_json::to_vec(&serde_json::json!({
                                    "tokens": total_tokens,
                                    "cost_micros": cost_micros,
                                }))?)).await?;
                            }
                            Ok(ReportUsageResult::SoftBreach { dimension, current_usage, soft_limit }) => {
                                warn!(
                                    execution_id = %eid,
                                    %dimension, current_usage, soft_limit,
                                    "usage reported; soft-limit breach (increments applied, execution finishes)"
                                );
                                task.complete(Some(serde_json::to_vec(&serde_json::json!({
                                    "tokens": total_tokens,
                                    "cost_micros": cost_micros,
                                    "soft_breach": dimension,
                                }))?)).await?;
                            }
                            Ok(ReportUsageResult::HardBreach { dimension, current_usage, hard_limit }) => {
                                warn!(
                                    execution_id = %eid,
                                    %dimension, current_usage, hard_limit,
                                    "usage reported; HARD breach — increments not applied, failing execution"
                                );
                                // Fail — the submitter will observe the
                                // hard breach on its next poll and cancel
                                // the flow.
                                let reason = format!(
                                    "hard budget breach on {dimension}: {current_usage} >= {hard_limit}"
                                );
                                match task.fail(&reason, "budget_hard_breach").await? {
                                    FailOutcome::RetryScheduled { .. } => {}
                                    FailOutcome::TerminalFailed => {}
                                }
                            }
                            Ok(ReportUsageResult::AlreadyApplied) => {
                                info!(execution_id = %eid, "usage already applied (idempotent retry)");
                                task.complete(None).await?;
                            }
                            Ok(other) => {
                                // `#[non_exhaustive]` — handle future variants as
                                // "unknown, log and complete" so a new variant
                                // added upstream doesn't panic the example.
                                warn!(execution_id = %eid, outcome = ?other, "unknown report_usage result");
                                task.complete(None).await?;
                            }
                            Err(e) => {
                                tracing::error!(execution_id = %eid, error = %e, "report_usage failed");
                                let _ = task.fail(&format!("report_usage: {e}"), "report_usage_error").await;
                            }
                        }
                    }
                    Ok(None) => {
                        // No claim ready. Short sleep — the poll loop is
                        // driven by the submitter's deadline, not by us.
                        sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "claim_via_server returned error (likely flow cancelled)");
                        sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
}

/// Deterministic token simulator — seeded by the execution id hash so
/// re-runs against the same flow produce reproducible token counts.
/// Essential for the "live-validation surface" framing: a flaky
/// example can't gate a release. (CLAUDE.md §5 rule.)
fn simulate_inference(eid: &ExecutionId) -> (u64, u64) {
    let mut h = DefaultHasher::new();
    eid.to_string().hash(&mut h);
    let seed = h.finish();
    let mut rng = StdRng::seed_from_u64(seed);
    // Input prompts vary 50-150 tokens; output varies 100-400. Mean
    // total ≈ 350 tokens; 5 prompts ≈ 1750 tokens → trips the 1200
    // hard limit partway through the batch.
    let input_tokens = rng.gen_range(50..=150);
    let output_tokens = rng.gen_range(100..=400);
    (input_tokens, output_tokens)
}

// ── Post-mortem: LeaseSummary (v0.9 #278) ──

async fn print_post_mortem(
    backend: &Arc<dyn flowfabric::core::engine_backend::EngineBackend>,
    eids: &[ExecutionId],
) -> Result<()> {
    info!("── post-mortem: per-child LeaseSummary ──");
    for eid in eids {
        match backend.describe_execution(eid).await? {
            Some(snap) => {
                let state = &snap.public_state;
                if let Some(lease) = &snap.current_lease {
                    info!(
                        execution_id = %eid,
                        state = ?state,
                        lease_id = %lease.lease_id,
                        attempt_index = lease.attempt_index.0,
                        last_heartbeat_at = ?lease.last_heartbeat_at,
                        "child snapshot + lease summary"
                    );
                } else {
                    info!(
                        execution_id = %eid,
                        state = ?state,
                        "child snapshot (no active lease — terminal or never leased)"
                    );
                }
            }
            None => {
                warn!(execution_id = %eid, "describe_execution returned None");
            }
        }
    }
    Ok(())
}

// ── helpers ──

fn now_ms() -> i64 {
    TimestampMs::now().0
}

fn uuid_suffix() -> String {
    // Avoid a direct uuid dep — we just need a unique-enough suffix
    // for the worker instance id. `TimestampMs` + thread id is
    // sufficient for a single-process example.
    let t = TimestampMs::now().0;
    format!("{t:x}")
}
