//! UC-38 `aggregator` worker.
//!
//! Claims aggregator-capability executions off the `aggregator` lane,
//! discovers the winning upstream provider via `list_incoming_edges`
//! (RFC-007) + REST `/result`, streams the winner's output back as
//! `DurableSummary { JsonMergePatch }` frames (RFC-015), and — when a
//! `review_waitpoint_key` is present on the payload — suspends on a
//! `Composite(Count { n: 2, DistinctSources })` resume condition
//! (RFC-014) before completing.

use std::time::Duration;

use clap::Parser;
use ff_core::backend::{PatchKind, StreamMode};
use ff_core::contracts::{CompositeBody, CountKind, SignalMatcher};
use ff_sdk::{
    FlowFabricAdminClient, FlowFabricWorker, ResumeCondition, ResumePolicy, SuspensionReasonCode,
    TimeoutBehavior, WorkerConfig,
};
use llm_race_example::{
    AggregatorPayload, WinnerRecord, CAP_ROLE_AGGREGATOR, DEFAULT_NAMESPACE, DEFAULT_SERVER_URL,
    LANE_AGGREGATOR,
};

#[derive(Parser)]
#[command(name = "aggregator", about = "UC-38 aggregator worker (race winner streamer)")]
struct Args {
    #[arg(long, env = "FF_HOST", default_value = "localhost")]
    host: String,
    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,
    #[arg(long, env = "FF_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server_url: String,
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,
    #[arg(long, default_value = LANE_AGGREGATOR)]
    lane: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_race_example=info,aggregator=info,ff_sdk=info".into()),
        )
        .init();

    let args = Args::parse();
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("llm-race-aggregator"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(format!(
            "llm-race-aggregator-{}",
            uuid::Uuid::new_v4()
        )),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec![CAP_ROLE_AGGREGATOR.to_owned()],
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    };
    let worker = FlowFabricWorker::connect(config).await?;
    let admin = match args.api_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => FlowFabricAdminClient::with_token(&args.server_url, t)?,
        None => FlowFabricAdminClient::new(&args.server_url)?,
    };
    let lane = ff_core::types::LaneId::try_new(&args.lane)?;
    let http = reqwest::Client::new();

    tracing::info!("aggregator worker started");

    loop {
        match worker.claim_via_server(&admin, &lane, 10_000).await {
            Ok(Some(task)) => {
                let eid = task.execution_id().to_string();
                println!("[timeline] aggregator_claimed execution_id={eid}");

                // Fresh-claim path only. If the lease carries a
                // resumed signal (HITL approved), the task moves
                // straight to publish.
                let resumed = task.resume_signals().await.unwrap_or_default();
                if !resumed.is_empty() {
                    // HITL review satisfied — just complete with the
                    // execution's input_payload echoed back. In a real
                    // pipeline the aggregator would re-project the
                    // provider output it previously stashed. Keeping
                    // it simple for the scaffold.
                    let echo = task.input_payload().to_vec();
                    let _ = task.complete(Some(echo)).await;
                    println!("[timeline] aggregator_published_after_hitl execution_id={eid}");
                    continue;
                }

                let payload: AggregatorPayload = match serde_json::from_slice(task.input_payload()) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(%eid, error = %e, "bad payload");
                        let _ = task.fail(&format!("bad payload: {e}"), "bad_payload").await;
                        continue;
                    }
                };

                // ── Find the winning upstream ──
                let incoming = match worker
                    .list_incoming_edges(task.execution_id())
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(%eid, error = %e, "list_incoming_edges failed");
                        let _ = task.fail(&format!("list edges: {e}"), "aggregator_error").await;
                        continue;
                    }
                };
                let winner = match find_winner(&http, &args, &incoming).await {
                    Ok(Some(w)) => w,
                    Ok(None) => {
                        tracing::warn!(%eid, "no completed upstream yet");
                        let _ = task
                            .fail("no completed upstream provider", "no_winner")
                            .await;
                        continue;
                    }
                    Err(e) => {
                        tracing::error!(%eid, error = %e, "find_winner failed");
                        let _ = task.fail(&format!("find_winner: {e}"), "aggregator_error").await;
                        continue;
                    }
                };
                println!(
                    "[timeline] aggregator_received_winner execution_id={eid} model={}",
                    winner.model
                );

                // ── Stream DurableSummary deltas ──
                // Roll the winner's output into the summary field by
                // field; send tokens_used as a rolling counter so the
                // consumer sees the merge-patch write path exercised.
                let mode = StreamMode::DurableSummary {
                    patch_kind: PatchKind::JsonMergePatch,
                };
                let mut frames_appended = 0u64;
                for chunk in chunk_string(&winner.output, 64) {
                    let patch = serde_json::json!({
                        "winner_model": winner.model,
                        "output": chunk,
                    });
                    if let Err(e) = task
                        .append_frame_with_mode(
                            "summary_delta",
                            &serde_json::to_vec(&patch)?,
                            None,
                            mode.clone(),
                        )
                        .await
                    {
                        tracing::warn!(%eid, error = %e, "append_frame_with_mode failed");
                        break;
                    }
                    frames_appended += 1;
                }
                // Final counter frame.
                let counter = serde_json::json!({
                    "tokens_used": winner.tokens_used,
                    "done": true,
                });
                let _ = task
                    .append_frame_with_mode(
                        "summary_final",
                        &serde_json::to_vec(&counter)?,
                        None,
                        mode.clone(),
                    )
                    .await;
                println!(
                    "[timeline] aggregator_streamed frames={} execution_id={eid}",
                    frames_appended + 1
                );

                // ── Optional HITL suspend (2-of-3) ──
                if let Some(wp_key) = payload.review_waitpoint_key {
                    let deadline = ff_core::types::TimestampMs::from_millis(
                        ff_core::types::TimestampMs::now().0 + 3_600_000,
                    );
                    let cond = ResumeCondition::Composite(CompositeBody::Count {
                        n: 2,
                        count_kind: CountKind::DistinctSources,
                        matcher: Some(SignalMatcher::ByName("review_response".into())),
                        waitpoints: vec![wp_key.clone()],
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
                                "REVIEW NEEDED (2-of-3) execution_id={eid} waitpoint_key={wp_key} \
                                 waitpoint_id={} waitpoint_token={}",
                                handle.details.waitpoint_id,
                                handle.details.waitpoint_token.token().as_str()
                            );
                        }
                        Err(e) => {
                            tracing::error!(%eid, error = %e, "suspend failed");
                        }
                    }
                } else {
                    // Non-review path: complete inline.
                    let out = WinnerRecord {
                        model: winner.model.clone(),
                        output: winner.output.clone(),
                        tokens_used: winner.tokens_used,
                    };
                    if let Err(e) = task.complete(Some(serde_json::to_vec(&out)?)).await {
                        tracing::error!(%eid, error = %e, "complete failed");
                    } else {
                        println!("[timeline] aggregator_published execution_id={eid}");
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

async fn find_winner(
    http: &reqwest::Client,
    args: &Args,
    incoming: &[ff_core::contracts::EdgeSnapshot],
) -> Result<Option<WinnerRecord>, Box<dyn std::error::Error>> {
    for edge in incoming {
        let ups = &edge.upstream_execution_id;
        let state_url = format!("{}/v1/executions/{}/state", args.server_url, ups);
        let mut req = http.get(&state_url);
        if let Some(t) = &args.api_token {
            req = req.bearer_auth(t);
        }
        let state = req
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?
            .as_str()
            .unwrap_or("")
            .to_owned();
        if state != "completed" {
            continue;
        }
        let result_url = format!("{}/v1/executions/{}/result", args.server_url, ups);
        let mut req = http.get(&result_url);
        if let Some(t) = &args.api_token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            continue;
        }
        let bytes = resp.bytes().await?;
        if let Ok(record) = serde_json::from_slice::<WinnerRecord>(&bytes) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn chunk_string(s: &str, chunk: usize) -> Vec<String> {
    // Simple char-based chunking (keeps UTF-8 safe for the demo).
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    for c in chars.chunks(chunk.max(1)) {
        out.push(c.iter().collect::<String>());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
