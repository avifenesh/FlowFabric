//! approve CLI — delivers a deploy-approval signal with distinct
//! source_identity (RFC-014 Count{n=2, DistinctSources}).
//!
//! Two invocations with distinct --reviewer values satisfy the deploy
//! worker's resume condition. A second invocation with the same
//! reviewer does NOT — DistinctSources dedupes by source_identity.

use clap::Parser;
use deploy_approval_example::{ApprovalPayload, DEFAULT_SERVER_URL, SIGNAL_NAME_APPROVAL};
use ff_core::types::{ExecutionId, WaitpointId};
use ff_sdk::admin::FlowFabricAdminClient;

#[derive(Parser)]
#[command(name = "approve", about = "deploy-approval reviewer CLI")]
struct Args {
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server: String,
    /// Flow id from `submit` output (informational; the deploy
    /// execution id is also required because ff-server 0.6.1 does not
    /// expose a `list flow members` route — copy it from submit stdout).
    #[arg(long)]
    flow_id: Option<String>,
    /// Deploy execution id (the suspended target).
    #[arg(long)]
    execution_id: String,
    /// Waitpoint id printed by `deploy` on suspend (look for
    /// `[timeline] deploy_suspended ... waitpoint_id=...`). v0.14 the
    /// raw `waitpoint_token` is fetched through the admin client's
    /// `read_waitpoint_token` helper — callers no longer copy it from
    /// the worker log.
    #[arg(long)]
    waitpoint_id: String,
    /// Unique reviewer identity — `Count{DistinctSources}` keys on this.
    #[arg(long)]
    reviewer: String,
    #[arg(long, default_value_t = true)]
    approve: bool,
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let http = reqwest::Client::new();

    // v0.14: fetch the raw waitpoint HMAC token through the admin
    // client so operator tooling doesn't depend on the worker's log
    // output. The bearer token (if any) gates access server-side.
    let admin = match args.api_token.as_deref() {
        Some(t) => FlowFabricAdminClient::with_token(&args.server, t)?,
        None => FlowFabricAdminClient::new(&args.server)?,
    };
    let execution_id = ExecutionId::parse(&args.execution_id)?;
    let waitpoint_id = WaitpointId::parse(&args.waitpoint_id)?;
    let waitpoint_token = admin
        .read_waitpoint_token(&execution_id, &waitpoint_id)
        .await?
        .ok_or_else(|| {
            format!(
                "waitpoint {} token not found on server (consumed, expired, or unknown)",
                args.waitpoint_id
            )
        })?;

    let payload = ApprovalPayload {
        approved: args.approve,
        reviewer: args.reviewer.clone(),
    };
    let body = serde_json::json!({
        "execution_id": args.execution_id,
        "waitpoint_id": args.waitpoint_id,
        "signal_id": uuid::Uuid::new_v4().to_string(),
        "signal_name": SIGNAL_NAME_APPROVAL,
        "signal_category": "human_review",
        "source_type": "human",
        "source_identity": args.reviewer,
        "payload": serde_json::to_vec(&payload)?,
        "payload_encoding": "json",
        "target_scope": "execution",
        "waitpoint_token": waitpoint_token,
        "now": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64,
    });

    let sig_url = format!(
        "{}/v1/executions/{}/signal",
        args.server, args.execution_id
    );
    let mut req = http.post(&sig_url).json(&body);
    if let Some(t) = &args.api_token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("signal POST {status}: {text}").into());
    }
    println!(
        "[approve] reviewer={} flow_id={} execution_id={} approved={} response={}",
        args.reviewer,
        args.flow_id.as_deref().unwrap_or("-"),
        args.execution_id,
        args.approve,
        text
    );
    Ok(())
}
