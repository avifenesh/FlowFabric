//! deploy worker — canary rollout with a 2-of-N human-approval gate.
//!
//! Exercises:
//!   - RFC-013 typed SuspendArgs + suspend() (lease released).
//!   - RFC-014 Composite(Count { n: 2, DistinctSources }) — two distinct
//!     reviewer identities required; duplicate signals from one reviewer
//!     do not satisfy the condition.
//!
//! On fresh claim: deploys canary, then suspends. On resumed claim
//! (after 2 approvals land): completes the rollout.

use std::time::Duration;

use clap::Parser;
use deploy_approval_example::{
    DeployPayload, DeployResult, CAP_ROLE_DEPLOY, DEFAULT_NAMESPACE, DEFAULT_SERVER_URL,
    LANE_DEPLOY, SIGNAL_NAME_APPROVAL,
};
use ff_core::contracts::{CompositeBody, CountKind, SignalMatcher};
use ff_sdk::{
    FlowFabricAdminClient, FlowFabricWorker, ResumeCondition, ResumePolicy, SuspensionReasonCode,
    TimeoutBehavior, WorkerConfig,
};

#[derive(Parser)]
#[command(name = "deploy", about = "deploy-approval canary deploy worker")]
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
    #[arg(long, default_value = LANE_DEPLOY)]
    lane: String,
    /// Simulate a canary failure (pre-suspend); the deploy fails instead
    /// of progressing to full rollout.
    #[arg(long)]
    fail: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "deploy_approval_example=info,deploy=info".into()),
        )
        .init();

    let args = Args::parse();
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("deploy-rollout"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "deploy-rollout-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec![CAP_ROLE_DEPLOY.to_owned()],
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

    tracing::info!("deploy worker started");

    loop {
        match worker.claim_via_server(&admin, &lane, 10_000).await {
            Ok(Some(task)) => {
                let eid = task.execution_id().to_string();
                let payload: DeployPayload = match serde_json::from_slice(task.input_payload()) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = task.fail(&format!("bad payload: {e}"), "bad_payload").await;
                        continue;
                    }
                };

                // Did we come back with resume signals from the waitpoint?
                let resumed = task.resume_signals().await.unwrap_or_default();
                if !resumed.is_empty() {
                    println!(
                        "[timeline] deploy_resumed execution_id={eid} signals={} artifact={}",
                        resumed.len(),
                        payload.artifact
                    );
                    // Full rollout after approvals.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let result = DeployResult {
                        artifact: payload.artifact.clone(),
                        replicas: 10,
                    };
                    if let Err(e) = task.complete(Some(serde_json::to_vec(&result)?)).await {
                        tracing::error!(%eid, error = %e, "complete failed");
                    } else {
                        println!(
                            "[timeline] deploy_full_rollout_completed execution_id={eid} replicas=10"
                        );
                    }
                    continue;
                }

                println!(
                    "[timeline] deploy_claimed execution_id={eid} artifact={} canary_pct={}",
                    payload.artifact, payload.canary_percent
                );

                if args.fail {
                    let _ = task
                        .fail("simulated canary regression", "canary_failed")
                        .await;
                    println!("[timeline] deploy_canary_failed execution_id={eid}");
                    continue;
                }

                // Simulated canary rollout.
                tokio::time::sleep(Duration::from_millis(400)).await;
                println!(
                    "[timeline] deploy_canary_healthy execution_id={eid} canary_pct={}",
                    payload.canary_percent
                );

                // Suspend for 2-of-N approvals.
                let deadline = ff_core::types::TimestampMs::from_millis(
                    ff_core::types::TimestampMs::now().0 + 3_600_000,
                );
                let cond = ResumeCondition::Composite(CompositeBody::Count {
                    n: 2,
                    count_kind: CountKind::DistinctSources,
                    matcher: Some(SignalMatcher::ByName(SIGNAL_NAME_APPROVAL.into())),
                    waitpoints: vec![payload.approval_waitpoint_key.clone()],
                });
                match task
                    .suspend(
                        SuspensionReasonCode::WaitingForOperatorReview,
                        cond,
                        Some((deadline, TimeoutBehavior::Fail)),
                        ResumePolicy::normal(),
                    )
                    .await
                {
                    Ok(handle) => {
                        println!(
                            "[timeline] deploy_suspended execution_id={eid} waitpoint_key={} waitpoint_id={} waitpoint_token={}",
                            payload.approval_waitpoint_key,
                            handle.details.waitpoint_id,
                            handle.details.waitpoint_token.token().as_str()
                        );
                    }
                    Err(e) => {
                        // task was moved into suspend(); just log.
                        tracing::error!(%eid, error = %e, "suspend failed");
                    }
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
