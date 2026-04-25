//! umbrella-quickstart — canonical v0.9 FlowFabric bring-up. Valkey-
//! only, single-process, one execution cycle. All imports route through
//! the `flowfabric` umbrella crate (#279). See `README.md` for the v0.9
//! issue grid, prereqs, and expected transcript.

use std::sync::Arc;
use std::time::Duration;

// v0.9 contract: every FlowFabric path comes through `flowfabric::`.
// No direct `ff_core` / `ff_sdk` / `ff_backend_valkey` imports.
use flowfabric::core::backend::{BackendConfig, PrepareOutcome};
use flowfabric::core::contracts::{
    ClaimForWorkerArgs, ClaimForWorkerOutcome, SeedOutcome, SeedWaitpointHmacSecretArgs,
};
use flowfabric::core::engine_backend::EngineBackend;
use flowfabric::core::partition::PartitionConfig;
use flowfabric::core::policy::ExecutionPolicy;
use flowfabric::core::types::{
    ExecutionId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId,
};
use flowfabric::sdk::{ClaimGrant, FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};
use flowfabric::valkey::ValkeyBackend;

use anyhow::{Context, Result, anyhow};
use tokio::time::timeout;
use tracing::info;

const NAMESPACE: &str = "demo";
const LANE: &str = "default";
const WORKER_POOL: &str = "umbrella-quickstart-worker";
const DEMO_BUDGET: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "umbrella_quickstart=info".into()),
        )
        .init();

    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379);
    let server_url =
        std::env::var("FF_SERVER_URL").unwrap_or_else(|_| "http://localhost:9090".into());

    timeout(DEMO_BUDGET, run(&host, port, &server_url))
        .await
        .map_err(|_| anyhow!("demo exceeded {}s budget", DEMO_BUDGET.as_secs()))?
}

async fn run(host: &str, port: u16, server_url: &str) -> Result<()> {
    // 1. Umbrella import: build the backend via flowfabric::valkey.
    let backend: Arc<dyn EngineBackend> =
        ValkeyBackend::connect(BackendConfig::valkey(host.to_owned(), port))
            .await
            .context("ValkeyBackend::connect")?;
    info!(backend_label = backend.backend_label(), "backend connected");

    // 2. prepare() — #281. FUNCTION LOAD on Valkey, NoOp on Postgres.
    match backend.prepare().await.context("backend.prepare")? {
        PrepareOutcome::Applied { description } => info!(%description, "prepare: Applied"),
        PrepareOutcome::NoOp => info!("prepare: NoOp"),
        other => info!(?other, "prepare: other outcome"),
    }

    // 3. seed_waitpoint_hmac_secret() — #280. Idempotent boot-time seed;
    //    supplying the same (kid, secret) the server seeded returns
    //    AlreadySeeded { same_secret: true } and is the supported path
    //    for consumer boot code.
    let kid =
        std::env::var("FF_WAITPOINT_HMAC_KID").unwrap_or_else(|_| "k1".into());
    let secret_hex = std::env::var("FF_WAITPOINT_HMAC_SECRET")
        .unwrap_or_else(|_| "0123456789abcdef".repeat(4));
    match backend
        .seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(&kid, &secret_hex))
        .await
        .context("seed_waitpoint_hmac_secret")?
    {
        SeedOutcome::Seeded { kid } => info!(%kid, "seed: Seeded"),
        SeedOutcome::AlreadySeeded { kid, same_secret } => {
            info!(%kid, same_secret, "seed: AlreadySeeded");
        }
        other => info!(?other, "seed: other outcome"),
    }

    // 4. Submit a single execution via HTTP.
    let http = reqwest::Client::new();
    let partition_config = partition_config_from_env();
    let lane = LaneId::try_new(LANE)?;
    let execution_id = ExecutionId::solo(&lane, &partition_config);
    submit_execution(&http, server_url, &execution_id).await?;
    info!(%execution_id, "submitted execution");

    // 5. Build a worker on the SAME backend handle via connect_with.
    let worker_id = WorkerId::new(WORKER_POOL);
    let worker_instance_id = WorkerInstanceId::new(format!(
        "{WORKER_POOL}-{}",
        uuid::Uuid::new_v4()
    ));
    let worker_config = WorkerConfig {
        backend: BackendConfig::valkey(host.to_owned(), port),
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
        namespace: Namespace::new(NAMESPACE),
        lanes: vec![lane.clone()],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 500,
        max_concurrent_tasks: 1,
    };
    let worker = FlowFabricWorker::connect_with(worker_config, Arc::clone(&backend), None)
        .await
        .context("FlowFabricWorker::connect_with")?;

    // 6. Trait-routed claim. #283 re-exports `ClaimGrant` at
    //    flowfabric::sdk so this code compiles without ff-scheduler.
    //
    //    v0.9 surface gap: `ValkeyBackend::connect()` does NOT wire a
    //    scheduler, so `backend.claim_for_worker(..)` on a consumer-
    //    built Arc<dyn EngineBackend> returns Unavailable. The
    //    production entry point is `claim_via_server`, which routes
    //    through the server-side scheduler and threads the resulting
    //    ClaimGrant into `claim_from_grant` under the covers. See the
    //    PR body / README for the follow-up issue.
    let probe = ClaimForWorkerArgs::new(
        lane.clone(),
        worker_id.clone(),
        worker_instance_id.clone(),
        Default::default(),
        5_000,
    );
    match backend.claim_for_worker(probe).await {
        Ok(ClaimForWorkerOutcome::Granted(_)) | Ok(ClaimForWorkerOutcome::NoWork) => {
            info!("claim_for_worker: trait-routed claim wired (scheduler present)");
        }
        Ok(other) => info!(?other, "claim_for_worker: other (non-exhaustive)"),
        Err(e) => info!(
            %e,
            "claim_for_worker: Unavailable on consumer-built ValkeyBackend \
             (v0.9 surface gap) — falling back to claim_via_server"
        ),
    }
    // Compile-time witness that `ClaimGrant` is nameable at flowfabric::sdk.
    let _ = ClaimGrant::partition;

    let admin = FlowFabricAdminClient::new(server_url)?;
    let task = loop {
        match worker
            .claim_via_server(&admin, &lane, 5_000)
            .await
            .context("claim_via_server")?
        {
            Some(t) => break t,
            None => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    };
    info!(
        execution_id = %task.execution_id(),
        attempt_index = task.attempt_index().0,
        "claim acquired",
    );

    // One renew so LeaseSummary.last_heartbeat_at is populated.
    task.renew_lease().await.context("renew_lease")?;

    // 7. LeaseSummary v0.9 fields (#278) — pre-complete describe.
    let snapshot = worker
        .describe_execution(task.execution_id())
        .await
        .context("describe_execution")?
        .ok_or_else(|| anyhow!("describe_execution returned None"))?;
    let lease = snapshot
        .current_lease
        .as_ref()
        .ok_or_else(|| anyhow!("current_lease None on an active claim"))?;
    info!(
        lease_id = %lease.lease_id,
        attempt_index = lease.attempt_index.0,
        last_heartbeat_at = ?lease.last_heartbeat_at,
        lease_epoch = lease.lease_epoch.0,
        "LeaseSummary (v0.9 fields: lease_id / attempt_index / last_heartbeat_at)",
    );

    // 8. Trivial handler: complete the task. Capture the claimed id
    //    before `complete` consumes `task` so the post-complete read
    //    targets the right execution (claim_via_server may surface an
    //    older eligible execution ahead of the one just submitted).
    let claimed_id = task.execution_id().clone();
    task.complete(Some(b"umbrella-quickstart ok".to_vec()))
        .await
        .context("complete")?;
    info!("task completed");

    let final_snapshot = worker
        .describe_execution(&claimed_id)
        .await
        .context("describe_execution (post-complete)")?
        .ok_or_else(|| anyhow!("execution missing post-complete"))?;
    info!(
        submitted = %execution_id,
        claimed = %claimed_id,
        public_state = ?final_snapshot.public_state,
        total_attempt_count = final_snapshot.total_attempt_count,
        "final execution state",
    );

    info!("umbrella-quickstart complete");
    Ok(())
}

// ─── helpers ───

fn partition_config_from_env() -> PartitionConfig {
    let num_flow_partitions: u16 = std::env::var("FF_FLOW_PARTITIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    PartitionConfig {
        num_flow_partitions,
        ..PartitionConfig::default()
    }
}

async fn submit_execution(
    http: &reqwest::Client,
    server_url: &str,
    execution_id: &ExecutionId,
) -> Result<()> {
    let policy = ExecutionPolicy::default();
    let body = serde_json::json!({
        "execution_id": execution_id,
        "namespace": NAMESPACE,
        "lane_id": LANE,
        "execution_kind": "umbrella_quickstart",
        "input_payload": b"{}".to_vec(),
        "payload_encoding": "json",
        "priority": policy.priority,
        "creator_identity": "umbrella-quickstart-example",
        "idempotency_key": null,
        "tags": {},
        "policy": serde_json::to_value(&policy)?,
        "delay_until": null,
        "partition_id": execution_id.partition(),
        "now": TimestampMs::now().0,
    });
    let resp = http
        .post(format!("{server_url}/v1/executions"))
        .json(&body)
        .send()
        .await
        .context("POST /v1/executions")?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("create_execution failed: {status} {text}"));
    }
    Ok(())
}
