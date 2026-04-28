//! UC-38 `provider` worker.
//!
//! Claims provider-capability executions off the `provider` lane, hits
//! OpenRouter with the execution's prompt+model, and completes with the
//! response bytes. Appends one `provider_start` and one
//! `provider_complete` frame (durable) so the aggregator can report the
//! timeline. When a sibling provider wins the race, the Stage-C
//! dispatcher (RFC-016) cancels this execution — the SDK surfaces that
//! as a lease-revocation error; we log it and loop.

use std::time::Duration;

use clap::Parser;
use ff_sdk::{FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};
use llm_race_example::{
    openrouter_chat, ProviderPayload, CAP_ROLE_PROVIDER, DEFAULT_NAMESPACE, DEFAULT_SERVER_URL,
    LANE_PROVIDER,
};

#[derive(Parser)]
#[command(name = "provider", about = "UC-38 provider worker (OpenRouter racer)")]
struct Args {
    #[arg(long, env = "FF_HOST", default_value = "localhost")]
    host: String,
    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,
    #[arg(long, env = "FF_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,
    #[arg(long, env = "OPENROUTER_API_KEY")]
    api_key: String,
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,
    #[arg(long, default_value = LANE_PROVIDER)]
    lane: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_race_example=info,provider=info,ff_sdk=info".into()),
        )
        .init();

    let args = Args::parse();
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("llm-race-provider"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "llm-race-provider-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec![CAP_ROLE_PROVIDER.to_owned()],
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    partition_config: None,
    };
    let worker = FlowFabricWorker::connect(config).await?;
    let admin = match args.api_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => FlowFabricAdminClient::with_token(&args.server_url, t)?,
        None => FlowFabricAdminClient::new(&args.server_url)?,
    };
    let lane = ff_core::types::LaneId::try_new(&args.lane)?;
    let http = reqwest::Client::new();

    tracing::info!("provider worker started, racing on lane {}", args.lane);

    loop {
        match worker.claim_via_server(&admin, &lane, 10_000).await {
            Ok(Some(task)) => {
                let eid = task.execution_id().to_string();
                let payload: ProviderPayload = match serde_json::from_slice(task.input_payload()) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(%eid, error = %e, "bad payload");
                        let _ = task.fail(&format!("bad payload: {e}"), "bad_payload").await;
                        continue;
                    }
                };
                println!(
                    "[timeline] provider_claimed execution_id={eid} model={}",
                    payload.model
                );
                let _ = task
                    .append_frame(
                        "provider_start",
                        serde_json::to_vec(&serde_json::json!({"model": payload.model}))?
                            .as_slice(),
                        None,
                    )
                    .await;

                match openrouter_chat(
                    &http,
                    &args.api_key,
                    &payload.model,
                    &payload.prompt,
                    payload.max_tokens,
                )
                .await
                {
                    Ok((content, usage)) => {
                        let record = serde_json::json!({
                            "model": payload.model,
                            "output": content,
                            "tokens_used": usage.total_tokens,
                        });
                        let bytes = serde_json::to_vec(&record)?;
                        let _ = task
                            .append_frame("provider_complete", &bytes, None)
                            .await;
                        println!(
                            "[timeline] provider_completed execution_id={eid} model={} tokens={}",
                            payload.model, usage.total_tokens
                        );
                        if let Err(e) = task.complete(Some(bytes)).await {
                            // If a sibling won first, the dispatcher
                            // will have flipped our execution to
                            // cancelled and our lease is stale — surface
                            // the cancel reason without noise.
                            println!(
                                "[timeline] provider_cancelled execution_id={eid} model={} reason={}",
                                payload.model, e
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%eid, error = %e, "openrouter call failed");
                        let _ = task.fail(&format!("openrouter: {e}"), "provider_error").await;
                    }
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
            Err(e) => {
                tracing::error!(error = %e, "claim_via_server failed");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
