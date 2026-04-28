//! build worker — streams a DurableSummary with JsonMergePatch (RFC-015).
//!
//! Each simulated build step appends a summary_delta frame carrying a
//! JSON merge patch. The server folds the patches into a rolling summary
//! hash (readable via GET /v1/executions/{id}/summary); tailers see
//! every XADD. Exercises RFC-015 PatchKind::JsonMergePatch.

use std::time::Duration;

use clap::Parser;
use deploy_approval_example::{
    BuildPayload, CAP_ROLE_BUILD, DEFAULT_NAMESPACE, DEFAULT_SERVER_URL, LANE_BUILD,
};
use ff_core::backend::{PatchKind, StreamMode};
use ff_sdk::{FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};

#[derive(Parser)]
#[command(name = "build", about = "deploy-approval build worker")]
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
    #[arg(long, default_value = LANE_BUILD)]
    lane: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "deploy_approval_example=info,build=info".into()),
        )
        .init();

    let args = Args::parse();
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("deploy-build"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "deploy-build-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec![CAP_ROLE_BUILD.to_owned()],
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

    tracing::info!("build worker started");

    loop {
        match worker.claim_via_server(&admin, &lane, 10_000).await {
            Ok(Some(task)) => {
                let eid = task.execution_id().to_string();
                let payload: BuildPayload = match serde_json::from_slice(task.input_payload()) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = task.fail(&format!("bad payload: {e}"), "bad_payload").await;
                        continue;
                    }
                };
                println!(
                    "[timeline] build_claimed execution_id={eid} artifact={} commit={}",
                    payload.artifact, payload.commit_sha
                );

                let mode = StreamMode::DurableSummary {
                    patch_kind: PatchKind::JsonMergePatch,
                };
                let steps = [
                    ("compiling", 25),
                    ("linking", 50),
                    ("packaging", 75),
                    ("pushing", 100),
                ];
                for (label, pct) in steps {
                    let patch = serde_json::json!({
                        "step": label,
                        "percent": pct,
                        "artifact": payload.artifact,
                    });
                    if let Err(e) = task
                        .append_frame_with_mode(
                            "summary_delta",
                            &serde_json::to_vec(&patch)?,
                            None,
                            mode,
                        )
                        .await
                    {
                        tracing::warn!(%eid, error = %e, "append_frame failed");
                        break;
                    }
                    println!("[timeline] build_step execution_id={eid} step={label} pct={pct}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }

                let final_patch = serde_json::json!({
                    "step": "done",
                    "percent": 100,
                    "done": true,
                });
                let _ = task
                    .append_frame_with_mode(
                        "summary_final",
                        &serde_json::to_vec(&final_patch)?,
                        None,
                        mode,
                    )
                    .await;

                let result = serde_json::json!({
                    "artifact": payload.artifact,
                    "commit_sha": payload.commit_sha,
                });
                if let Err(e) = task.complete(Some(serde_json::to_vec(&result)?)).await {
                    tracing::error!(%eid, error = %e, "complete failed");
                } else {
                    println!("[timeline] build_completed execution_id={eid}");
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
