//! test worker — claims only test executions whose required_capabilities
//! match `--kind`. Three instances (unit/integration/e2e) run in parallel
//! and satisfy the AllOf edge group on `deploy` (RFC-007 + RFC-009).

use std::time::Duration;

use clap::Parser;
use deploy_approval_example::{
    TestPayload, CAP_TEST_E2E, CAP_TEST_INTEGRATION, CAP_TEST_UNIT, DEFAULT_NAMESPACE,
    DEFAULT_SERVER_URL, LANE_TEST,
};
use ff_sdk::{FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};

#[derive(Parser)]
#[command(name = "test", about = "deploy-approval test worker")]
struct Args {
    #[arg(long, env = "FF_HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,
    #[arg(long, env = "FF_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,
    #[arg(long, default_value = LANE_TEST)]
    lane: String,
    /// One of: unit | integration | e2e.
    #[arg(long)]
    kind: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "deploy_approval_example=info,test=info".into()),
        )
        .init();

    let args = Args::parse();
    let capability = match args.kind.as_str() {
        "unit" => CAP_TEST_UNIT,
        "integration" => CAP_TEST_INTEGRATION,
        "e2e" => CAP_TEST_E2E,
        other => return Err(format!("--kind must be unit|integration|e2e, got {other}").into()),
    };

    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new(format!("deploy-test-{}", args.kind)),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "deploy-test-{}-{}",
            args.kind,
            uuid::Uuid::new_v4()
        )),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec![capability.to_owned()],
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    };
    let worker = FlowFabricWorker::connect(config).await?;
    let admin = match args
        .api_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(t) => FlowFabricAdminClient::with_token(&args.server_url, t)?,
        None => FlowFabricAdminClient::new(&args.server_url)?,
    };
    let lane = ff_core::types::LaneId::try_new(&args.lane)?;

    tracing::info!("test worker started kind={}", args.kind);

    loop {
        match worker.claim_via_server(&admin, &lane, 10_000).await {
            Ok(Some(task)) => {
                let eid = task.execution_id().to_string();
                let payload: TestPayload = match serde_json::from_slice(task.input_payload()) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = task.fail(&format!("bad payload: {e}"), "bad_payload").await;
                        continue;
                    }
                };
                println!(
                    "[timeline] test_claimed kind={} execution_id={eid} artifact={}",
                    payload.kind, payload.artifact
                );
                // Simulate test duration; different kinds take different
                // wall time so the AllOf group's gating is visible.
                let duration_ms = match args.kind.as_str() {
                    "unit" => 300,
                    "integration" => 600,
                    "e2e" => 1_000,
                    _ => 500,
                };
                let _ = task
                    .append_frame(
                        "test_start",
                        serde_json::to_vec(&serde_json::json!({"kind": payload.kind}))?.as_slice(),
                        None,
                    )
                    .await;
                tokio::time::sleep(Duration::from_millis(duration_ms)).await;

                let result = serde_json::json!({
                    "kind": payload.kind,
                    "passed": true,
                    "duration_ms": duration_ms,
                });
                if let Err(e) = task.complete(Some(serde_json::to_vec(&result)?)).await {
                    tracing::error!(%eid, error = %e, "complete failed");
                } else {
                    println!(
                        "[timeline] test_completed kind={} execution_id={eid} passed=true",
                        payload.kind
                    );
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
            Err(e) => {
                tracing::error!(error = %e, "claim failed");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
