//! verify worker — health-check the deployment. When `payload.fail` is
//! true, triggers `cancel_flow { cancel_all }` against its own flow_id
//! so every non-terminal sibling (e.g. a still-retrying execution, a
//! waitpoint) is cancelled by the cascade (RFC-016).

use std::time::Duration;

use clap::Parser;
use deploy_approval_example::{
    VerifyPayload, CAP_ROLE_VERIFY, DEFAULT_NAMESPACE, DEFAULT_SERVER_URL, LANE_VERIFY,
};
use ff_sdk::{FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};

#[derive(Parser)]
#[command(name = "verify", about = "deploy-approval verify worker")]
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
    #[arg(long, default_value = LANE_VERIFY)]
    lane: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "deploy_approval_example=info,verify=info".into()),
        )
        .init();

    let args = Args::parse();
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("deploy-verify"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "deploy-verify-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec![CAP_ROLE_VERIFY.to_owned()],
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    partition_config: None,
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
    let http = reqwest::Client::new();

    tracing::info!("verify worker started");

    loop {
        match worker.claim_via_server(&admin, &lane, 10_000).await {
            Ok(Some(task)) => {
                let eid = task.execution_id().to_string();
                let payload: VerifyPayload = match serde_json::from_slice(task.input_payload()) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = task.fail(&format!("bad payload: {e}"), "bad_payload").await;
                        continue;
                    }
                };
                println!(
                    "[timeline] verify_claimed execution_id={eid} artifact={} fail={}",
                    payload.artifact, payload.fail
                );
                tokio::time::sleep(Duration::from_millis(300)).await;

                if payload.fail {
                    println!(
                        "[timeline] verify_unhealthy execution_id={eid} triggering cancel_flow"
                    );
                    let cancel_body = serde_json::json!({
                        "flow_id": payload.flow_id,
                        "reason": "verify_failed",
                        "cancellation_policy": "cancel_all",
                        "now": ff_core::types::TimestampMs::now().0,
                    });
                    let url = format!(
                        "{}/v1/flows/{}/cancel?wait=true",
                        args.server_url, payload.flow_id
                    );
                    let mut req = http.post(&url).json(&cancel_body);
                    if let Some(t) = &args.api_token {
                        req = req.bearer_auth(t);
                    }
                    match req.send().await {
                        Ok(resp) => {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            println!(
                                "[timeline] cancel_flow_returned status={status} body={text}"
                            );
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "cancel_flow POST failed");
                        }
                    }
                    // Fail our own execution too; cancel_flow on a running
                    // member is a no-op for that member, so we finalize
                    // with fail().
                    let _ = task
                        .fail("verify reported unhealthy", "verify_failed")
                        .await;
                    println!("[timeline] verify_failed execution_id={eid}");
                    continue;
                }

                let result = serde_json::json!({
                    "artifact": payload.artifact,
                    "healthy": true,
                });
                if let Err(e) = task.complete(Some(serde_json::to_vec(&result)?)).await {
                    tracing::error!(%eid, error = %e, "complete failed");
                } else {
                    println!("[timeline] verify_completed execution_id={eid}");
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
