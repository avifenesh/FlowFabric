//! UC-38 `approve` CLI for the optional HITL 2-of-3 review gate.
//!
//! Each invocation delivers one `review_response` signal with a distinct
//! `source_identity = reviewer` so the aggregator's
//! `Count { n: 2, DistinctSources }` resume condition counts it as a
//! unique satisfier.
//!
//! The `--waitpoint-id` + `--waitpoint-token` pair is printed by the
//! aggregator worker on suspend (look for
//! `waitpoint_id=... waitpoint_token=...` in its log). ff-server dropped
//! the token from `GET /v1/executions/{id}/pending-waitpoints` in v0.8.0
//! (RFC-017 Stage E4); operators copy the values from the suspending
//! worker's log. v0.14 adds an admin-surface `read_waitpoint_token` so
//! this can go back to a fetch.

use clap::Parser;
use llm_race_example::{ReviewPayload, DEFAULT_SERVER_URL};

#[derive(Parser)]
#[command(name = "approve", about = "UC-38 HITL reviewer (2-of-3)")]
struct Args {
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server: String,
    /// Aggregator execution id (the suspended target).
    #[arg(long)]
    execution_id: String,
    /// Waitpoint id printed by the aggregator on suspend.
    #[arg(long)]
    waitpoint_id: String,
    /// Waitpoint HMAC token printed by the aggregator on suspend.
    #[arg(long)]
    waitpoint_token: String,
    /// Unique reviewer identity — the `Count { DistinctSources }`
    /// counter keys on this.
    #[arg(long)]
    reviewer: String,
    /// Approve or reject.
    #[arg(long, default_value_t = true)]
    approve: bool,
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let http = reqwest::Client::new();

    let payload = ReviewPayload {
        approved: args.approve,
        reviewer: args.reviewer.clone(),
    };
    let body = serde_json::json!({
        "execution_id": args.execution_id,
        "waitpoint_id": args.waitpoint_id,
        "signal_id": uuid::Uuid::new_v4().to_string(),
        "signal_name": "review_response",
        "signal_category": "human_review",
        "source_type": "human",
        "source_identity": args.reviewer,
        "payload": serde_json::to_vec(&payload)?,
        "payload_encoding": "json",
        "target_scope": "execution",
        "waitpoint_token": args.waitpoint_token,
        "now": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64,
    });

    let sig_url = format!("{}/v1/executions/{}/signal", args.server, args.execution_id);
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
    println!("{} signal delivered by reviewer={}: {text}",
        if args.approve { "approve" } else { "reject" },
        args.reviewer
    );
    Ok(())
}
