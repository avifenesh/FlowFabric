//! Submit CLI — drives the 3-stage media pipeline end-to-end.
//!
//! Pipeline: transcribe (asr) → summarize (llm) → embed (embed).
//!
//! This client orchestrates dependency chaining server-side via POST
//! /v1/flows + /members + /edges, and ALSO fetches each upstream's result
//! to embed as the downstream's input_payload. The engine stores the
//! completion payload in `result_key`, and today's wire doesn't
//! auto-propagate it into the next execution's `input_payload` — that's a
//! Batch C task. Document the shortcut in the README.
//!
//! Responsibilities:
//!   * Create flow + three executions with the right capability sets
//!   * Wire the dependency edges so the flow graph is accurate even though
//!     we gate sequencing ourselves
//!   * Tail each execution's stream concurrently, prefix-labeled, through
//!     a shared stdout mutex
//!   * Handle Ctrl-C by cancelling the flow
//!   * Return exit 0 only if all three completed successfully

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use media_pipeline::{
    PipelineInput, SummarizeResult, TranscribeResult, KIND_EMBED, KIND_SUMMARIZE,
    KIND_TRANSCRIBE, LABEL_EMBED, LABEL_SUMMARIZE, LABEL_TRANSCRIBE,
};
use reqwest::Client;
use serde::Deserialize;
use tokio::io::{AsyncWriteExt, Stdout};
use tokio::sync::Mutex;

// ── CLI ──

#[derive(Parser)]
#[command(name = "submit", about = "Submit a media-pipeline flow and tail all three stages")]
struct Args {
    /// FlowFabric server base URL.
    #[arg(long, env = "FF_SERVER", default_value = "http://localhost:9090")]
    server: String,

    /// Bearer token. If unset, no Authorization header is sent (matches the
    /// server's behavior when FF_API_TOKEN is unconfigured).
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,

    /// Path to an audio file.
    #[arg(long)]
    audio: String,

    /// Optional title for the pipeline input.
    #[arg(long)]
    title: Option<String>,

    /// Lane to submit on.
    #[arg(long, default_value = "media")]
    lane: String,

    /// Namespace.
    #[arg(long, default_value = "default")]
    namespace: String,

    /// Poll interval for state checks.
    #[arg(long, default_value_t = 2000)]
    poll_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "submit=info".into()),
        )
        .init();

    let args = Args::parse();
    let client = build_http(&args.api_token)?;
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    // Flow + execution IDs up-front — we need them for dependency edges
    // before each execution is created.
    let flow_id = uuid::Uuid::new_v4().to_string();
    let eid_transcribe = uuid::Uuid::new_v4().to_string();
    let eid_summarize = uuid::Uuid::new_v4().to_string();
    let eid_embed = uuid::Uuid::new_v4().to_string();

    log(&stdout, &format!("flow_id={flow_id}")).await;
    log(&stdout, &format!("transcribe={eid_transcribe}")).await;
    log(&stdout, &format!("summarize={eid_summarize}")).await;
    log(&stdout, &format!("embed={eid_embed}")).await;

    create_flow(&client, &args, &flow_id).await?;
    log(&stdout, "flow created").await;

    // Create transcribe execution (input = PipelineInput).
    let pipeline_input = PipelineInput {
        audio_path: args.audio.clone(),
        title: args.title.clone(),
    };
    create_execution(
        &client,
        &args,
        &eid_transcribe,
        KIND_TRANSCRIBE,
        ["asr", "whisper-tiny-en"].iter().copied().collect(),
        serde_json::to_vec(&pipeline_input)?,
    )
    .await?;
    add_to_flow(&client, &args, &flow_id, &eid_transcribe).await?;
    log(&stdout, "transcribe execution created").await;

    // Ctrl-C handler: cancel the flow.
    let cancel_flow_id = flow_id.clone();
    let cancel_client = client.clone();
    let cancel_server = args.server.clone();
    let cancel_stdout = stdout.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = log(&cancel_stdout, "SIGINT — cancelling flow").await;
            let body = serde_json::json!({
                "flow_id": cancel_flow_id,
                "reason": "user_cancelled",
                "cancellation_policy": "cancel_all",
                "now": now_ms(),
            });
            let url = format!("{cancel_server}/v1/flows/{cancel_flow_id}/cancel");
            let _ = cancel_client.post(&url).json(&body).send().await;
            std::process::exit(130);
        }
    });

    // Start tailing transcribe immediately (worker may already be running).
    let tail_transcribe = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_transcribe.clone(),
        LABEL_TRANSCRIBE,
    );

    // Wait for transcribe to reach terminal state; read its result and wire
    // the next stage.
    let t_state = wait_terminal(&client, &args, &eid_transcribe).await?;
    if t_state != "completed" {
        anyhow::bail!("transcribe ended in {t_state}");
    }
    let t_result: TranscribeResult = fetch_result(&client, &args, &eid_transcribe).await?;
    log(
        &stdout,
        &format!(
            "transcribe done ({} chars, {} ms)",
            t_result.transcript.len(),
            t_result.duration_ms
        ),
    )
    .await;

    create_execution(
        &client,
        &args,
        &eid_summarize,
        KIND_SUMMARIZE,
        ["llm", "qwen-500m-q4"].iter().copied().collect(),
        serde_json::to_vec(&t_result)?,
    )
    .await?;
    add_to_flow(&client, &args, &flow_id, &eid_summarize).await?;
    wire_edge(&client, &args, &flow_id, &eid_transcribe, &eid_summarize, 1).await?;

    let tail_summarize = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_summarize.clone(),
        LABEL_SUMMARIZE,
    );

    let s_state = wait_terminal(&client, &args, &eid_summarize).await?;
    if s_state != "completed" {
        anyhow::bail!("summarize ended in {s_state}");
    }
    let s_result: SummarizeResult = fetch_result(&client, &args, &eid_summarize).await?;
    log(
        &stdout,
        &format!(
            "summarize done ({} tokens, {} chars)",
            s_result.token_count,
            s_result.summary.len()
        ),
    )
    .await;

    create_execution(
        &client,
        &args,
        &eid_embed,
        KIND_EMBED,
        ["embed", "minilm-l6"].iter().copied().collect(),
        serde_json::to_vec(&s_result)?,
    )
    .await?;
    add_to_flow(&client, &args, &flow_id, &eid_embed).await?;
    wire_edge(&client, &args, &flow_id, &eid_summarize, &eid_embed, 2).await?;

    let tail_embed = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_embed.clone(),
        LABEL_EMBED,
    );

    let e_state = wait_terminal(&client, &args, &eid_embed).await?;
    if e_state != "completed" {
        anyhow::bail!("embed ended in {e_state}");
    }
    log(&stdout, "embed done — pipeline complete").await;

    // Drain tails (each exits on closed_at).
    let _ = tokio::join!(tail_transcribe, tail_summarize, tail_embed);
    Ok(())
}

// ── HTTP helpers ────────────────────────────────────────────────────────

fn build_http(token: &Option<String>) -> Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(60));
    if let Some(t) = token {
        let mut h = reqwest::header::HeaderMap::new();
        let val = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
            .context("bad FF_API_TOKEN header value")?;
        h.insert(reqwest::header::AUTHORIZATION, val);
        builder = builder.default_headers(h);
    }
    Ok(builder.build()?)
}

async fn create_flow(client: &Client, args: &Args, flow_id: &str) -> Result<()> {
    let body = serde_json::json!({
        "flow_id": flow_id,
        "flow_kind": "media-pipeline",
        "namespace": args.namespace,
        "now": now_ms(),
    });
    let url = format!("{}/v1/flows", args.server);
    let resp = client.post(&url).json(&body).send().await?;
    check_ok(resp, "create_flow").await
}

async fn create_execution(
    client: &Client,
    args: &Args,
    eid: &str,
    kind: &str,
    capabilities: BTreeSet<&str>,
    input_payload: Vec<u8>,
) -> Result<()> {
    let caps: Vec<String> = capabilities.iter().map(|s| (*s).to_string()).collect();
    let policy = serde_json::json!({
        "routing_requirements": {
            "required_capabilities": caps,
        },
    });
    let body = serde_json::json!({
        "execution_id": eid,
        "namespace": args.namespace,
        "lane_id": args.lane,
        "execution_kind": kind,
        "input_payload": input_payload,
        "payload_encoding": "json",
        "priority": 100,
        "creator_identity": "media-pipeline-submit",
        "tags": {},
        "policy": policy,
        "partition_id": 0,
        "now": now_ms(),
    });
    let url = format!("{}/v1/executions", args.server);
    let resp = client.post(&url).json(&body).send().await?;
    check_ok(resp, "create_execution").await
}

async fn add_to_flow(client: &Client, args: &Args, flow_id: &str, eid: &str) -> Result<()> {
    let body = serde_json::json!({
        "flow_id": flow_id,
        "execution_id": eid,
        "now": now_ms(),
    });
    let url = format!("{}/v1/flows/{flow_id}/members", args.server);
    let resp = client.post(&url).json(&body).send().await?;
    check_ok(resp, "add_to_flow").await
}

/// Stage + apply a `success_only` dependency edge. `graph_rev` is the
/// expected revision the caller thinks it's writing against. Since we
/// always start from a fresh flow and know the number of edges added so
/// far, we can pass the count as `expected_graph_revision`.
async fn wire_edge(
    client: &Client,
    args: &Args,
    flow_id: &str,
    upstream: &str,
    downstream: &str,
    graph_rev: u64,
) -> Result<()> {
    let edge_id = uuid::Uuid::new_v4().to_string();

    let stage_body = serde_json::json!({
        "flow_id": flow_id,
        "edge_id": edge_id,
        "upstream_execution_id": upstream,
        "downstream_execution_id": downstream,
        "dependency_kind": "success_only",
        "expected_graph_revision": graph_rev,
        "now": now_ms(),
    });
    let stage_url = format!("{}/v1/flows/{flow_id}/edges", args.server);
    let resp = client.post(&stage_url).json(&stage_body).send().await?;
    check_ok(resp, "stage_edge").await?;

    let apply_body = serde_json::json!({
        "flow_id": flow_id,
        "edge_id": edge_id,
        "downstream_execution_id": downstream,
        "upstream_execution_id": upstream,
        "graph_revision": graph_rev + 1,
        "dependency_kind": "success_only",
        "now": now_ms(),
    });
    let apply_url = format!("{}/v1/flows/{flow_id}/edges/apply", args.server);
    let resp = client.post(&apply_url).json(&apply_body).send().await?;
    check_ok(resp, "apply_edge").await
}

/// Poll execution state until it reaches a terminal value. Returns the
/// terminal state name (`completed`, `failed`, `cancelled`, `expired`).
async fn wait_terminal(client: &Client, args: &Args, eid: &str) -> Result<String> {
    let url = format!("{}/v1/executions/{eid}/state", args.server);
    let mut last = String::new();
    loop {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("state poll failed: {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        let state = v.as_str().unwrap_or("unknown").to_owned();
        if state != last {
            last = state.clone();
        }
        match state.as_str() {
            "completed" | "failed" | "cancelled" | "expired" => return Ok(state),
            _ => tokio::time::sleep(Duration::from_millis(args.poll_ms)).await,
        }
    }
}

/// Fetch the JSON-decoded completion result through
/// `GET /v1/executions/{id}/result`. The endpoint returns the raw bytes
/// written by the worker's `complete(Some(...))` call.
async fn fetch_result<T: for<'de> Deserialize<'de>>(
    client: &Client,
    args: &Args,
    eid: &str,
) -> Result<T> {
    let url = format!("{}/v1/executions/{eid}/result", args.server);
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("result fetch failed: {}", resp.status());
    }
    let bytes = resp.bytes().await?;
    serde_json::from_slice(&bytes).with_context(|| format!("decode result for {eid}"))
}

// ── Tail multiplexer ────────────────────────────────────────────────────

fn spawn_tail(
    client: Client,
    server: String,
    stdout: Arc<Mutex<Stdout>>,
    eid: String,
    label: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut after = "0-0".to_owned();
        loop {
            let url = format!(
                "{server}/v1/executions/{eid}/attempts/0/stream/tail?after={after}&block_ms=5000&limit=100"
            );
            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    log_raw(&stdout, &format!("[{label}] tail error: {e}")).await;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            if !resp.status().is_success() {
                // 429 (concurrency cap) or 404 (stream not yet created) —
                // back off and try again.
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            let body: StreamResponse = match resp.json().await {
                Ok(b) => b,
                Err(e) => {
                    log_raw(&stdout, &format!("[{label}] tail decode: {e}")).await;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            for f in &body.frames {
                after = f.id.clone();
                let ft = f.fields.get("frame_type").cloned().unwrap_or_default();
                if ft == "result_payload" {
                    // Don't dump large result blobs into stdout; the CLI
                    // reports completion elsewhere.
                    continue;
                }
                let payload = f.fields.get("payload").cloned().unwrap_or_default();
                log_raw(&stdout, &format!("[{label}] {ft}: {payload}")).await;
            }
            if body.closed_at.is_some() {
                let reason = body.closed_reason.unwrap_or_default();
                log_raw(&stdout, &format!("[{label}] stream closed ({reason})")).await;
                return;
            }
        }
    })
}

// ── Wire shapes ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamFrame {
    id: String,
    fields: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct StreamResponse {
    frames: Vec<StreamFrame>,
    #[serde(default)]
    closed_at: Option<i64>,
    #[serde(default)]
    closed_reason: Option<String>,
}

// ── Small utilities ──────────────────────────────────────────────────────

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
}

async fn check_ok(resp: reqwest::Response, op: &str) -> Result<()> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(())
    } else {
        anyhow::bail!("{op} failed ({status}): {body}")
    }
}

async fn log(stdout: &Arc<Mutex<Stdout>>, line: &str) {
    log_raw(stdout, &format!("[submit] {line}")).await;
}

async fn log_raw(stdout: &Arc<Mutex<Stdout>>, line: &str) {
    let mut guard = stdout.lock().await;
    let _ = guard.write_all(line.as_bytes()).await;
    let _ = guard.write_all(b"\n").await;
    let _ = guard.flush().await;
}
