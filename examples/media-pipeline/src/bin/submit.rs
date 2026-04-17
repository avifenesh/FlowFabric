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
//!   * Handle Ctrl-C by cancelling the flow (inline in the main select)
//!   * Return exit 0 only if all three completed successfully

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
use tokio::task::JoinHandle;

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

    /// Base poll interval for state polling. Actual sleep is
    /// `poll_ms` plus a uniform 0..=poll_ms/2 jitter so multiple CLIs
    /// don't synchronize ZRANGEBYSCORE hits on an idle lane.
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

    match run_pipeline(&client, &args, &stdout).await {
        Ok(()) => Ok(()),
        Err(e) => {
            log(&stdout, &format!("pipeline failed: {e:#}")).await;
            Err(e)
        }
    }
}

/// Full-pipeline driver. Wrapped so Ctrl-C in the main select can cancel
/// the whole future and also POST /cancel before exit.
async fn run_pipeline(client: &Client, args: &Args, stdout: &Arc<Mutex<Stdout>>) -> Result<()> {
    // Flow + execution IDs up-front — we need them for dependency edges
    // before each execution is created.
    let flow_id = uuid::Uuid::new_v4().to_string();
    let eid_transcribe = uuid::Uuid::new_v4().to_string();
    let eid_summarize = uuid::Uuid::new_v4().to_string();
    let eid_embed = uuid::Uuid::new_v4().to_string();

    log(stdout, &format!("flow_id={flow_id}")).await;
    log(stdout, &format!("transcribe={eid_transcribe}")).await;
    log(stdout, &format!("summarize={eid_summarize}")).await;
    log(stdout, &format!("embed={eid_embed}")).await;

    // Install Ctrl-C handler for this run. Folding ctrl_c into the main
    // driver via tokio::select! ensures the handler is cancelled when the
    // pipeline finishes normally, avoiding the spawned-task race where a
    // Ctrl-C during tail-drain would trigger a spurious cancel on an
    // already-completed flow.
    let pipeline = pipeline_inner(
        client,
        args,
        stdout,
        &flow_id,
        &eid_transcribe,
        &eid_summarize,
        &eid_embed,
    );
    tokio::pin!(pipeline);

    tokio::select! {
        res = &mut pipeline => res,
        _ = tokio::signal::ctrl_c() => {
            log(stdout, "SIGINT — cancelling flow").await;
            let body = serde_json::json!({
                "flow_id": flow_id,
                "reason": "user_cancelled",
                "cancellation_policy": "cancel_all",
                "now": now_ms(),
            });
            let url = format!("{}/v1/flows/{flow_id}/cancel", args.server);
            let _ = client.post(&url).json(&body).send().await;
            anyhow::bail!("interrupted by user (SIGINT)");
        }
    }
}

async fn pipeline_inner(
    client: &Client,
    args: &Args,
    stdout: &Arc<Mutex<Stdout>>,
    flow_id: &str,
    eid_transcribe: &str,
    eid_summarize: &str,
    eid_embed: &str,
) -> Result<()> {
    create_flow(client, args, flow_id).await?;
    log(stdout, "flow created").await;

    // graph_revision is incremented atomically by the server on every
    // add_member and every stage_edge. Track locally so the next mutation
    // can pass the right `expected_graph_revision`.
    let mut graph_rev: u64 = 0;

    // Create transcribe execution (input = PipelineInput).
    let pipeline_input = PipelineInput {
        audio_path: args.audio.clone(),
        title: args.title.clone(),
    };
    create_execution(
        client,
        args,
        eid_transcribe,
        KIND_TRANSCRIBE,
        &["asr", "whisper-tiny-en"],
        serde_json::to_vec(&pipeline_input)?,
    )
    .await?;
    add_to_flow(client, args, flow_id, eid_transcribe).await?;
    graph_rev += 1;
    log(stdout, "transcribe execution created").await;

    // Tail transcribe immediately — if we waited until we see the worker
    // claim it, we'd miss the opening frames on fast transcriptions.
    let tail_transcribe = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_transcribe.to_owned(),
        LABEL_TRANSCRIBE,
    );

    let t_state = wait_terminal(client, args, eid_transcribe).await?;
    if t_state != "completed" {
        tail_transcribe.abort();
        anyhow::bail!("transcribe ended in {t_state}");
    }
    let t_result: TranscribeResult = fetch_result(client, args, eid_transcribe).await?;
    log(
        stdout,
        &format!(
            "transcribe done ({} chars, {} ms)",
            t_result.transcript.len(),
            t_result.duration_ms
        ),
    )
    .await;

    create_execution(
        client,
        args,
        eid_summarize,
        KIND_SUMMARIZE,
        &["llm", "qwen-500m-q4"],
        serde_json::to_vec(&t_result)?,
    )
    .await?;
    add_to_flow(client, args, flow_id, eid_summarize).await?;
    graph_rev += 1;
    graph_rev = wire_edge(
        client,
        args,
        flow_id,
        eid_transcribe,
        eid_summarize,
        graph_rev,
    )
    .await?;

    let tail_summarize = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_summarize.to_owned(),
        LABEL_SUMMARIZE,
    );

    let s_state = wait_terminal(client, args, eid_summarize).await?;
    if s_state != "completed" {
        tail_transcribe.abort();
        tail_summarize.abort();
        anyhow::bail!("summarize ended in {s_state}");
    }
    let s_result: SummarizeResult = fetch_result(client, args, eid_summarize).await?;
    log(
        stdout,
        &format!(
            "summarize done ({} tokens, {} chars)",
            s_result.token_count,
            s_result.summary.len()
        ),
    )
    .await;

    create_execution(
        client,
        args,
        eid_embed,
        KIND_EMBED,
        &["embed", "minilm-l6"],
        serde_json::to_vec(&s_result)?,
    )
    .await?;
    add_to_flow(client, args, flow_id, eid_embed).await?;
    graph_rev += 1;
    graph_rev = wire_edge(
        client,
        args,
        flow_id,
        eid_summarize,
        eid_embed,
        graph_rev,
    )
    .await?;
    let _ = graph_rev;

    let tail_embed = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_embed.to_owned(),
        LABEL_EMBED,
    );

    let e_state = wait_terminal(client, args, eid_embed).await?;
    if e_state != "completed" {
        tail_transcribe.abort();
        tail_summarize.abort();
        tail_embed.abort();
        anyhow::bail!("embed ended in {e_state}");
    }
    log(stdout, "embed done — pipeline complete").await;

    // Happy path: each tail exits on closed_at. join them so pending
    // frames flush before we return.
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
    capabilities: &[&str],
    input_payload: Vec<u8>,
) -> Result<()> {
    // Caller supplies a small, already-deduped capability list. Pass it
    // through directly — no BTreeSet allocation needed; the routing
    // layer sorts and dedups on its own side.
    let policy = serde_json::json!({
        "routing_requirements": {
            "required_capabilities": capabilities,
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

/// Add an execution to a flow. Each successful call bumps the flow's
/// graph_revision by 1 on the server side; callers track it locally by
/// incrementing after each successful response (both node_count and
/// graph_revision move together on add_member — see lua/flow.lua).
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

/// Stage + apply a `success_only` dependency edge.
///
/// `expected_graph_rev` is the revision the caller believes the flow is
/// at right now. Stage atomically bumps the revision to
/// `expected_graph_rev + 1`; apply runs on the downstream execution's
/// partition and does NOT touch the flow record. Returns the new
/// graph_revision parsed out of the stage response so the caller can
/// chain further mutations.
async fn wire_edge(
    client: &Client,
    args: &Args,
    flow_id: &str,
    upstream: &str,
    downstream: &str,
    expected_graph_rev: u64,
) -> Result<u64> {
    let edge_id = uuid::Uuid::new_v4().to_string();

    let stage_body = serde_json::json!({
        "flow_id": flow_id,
        "edge_id": edge_id,
        "upstream_execution_id": upstream,
        "downstream_execution_id": downstream,
        "dependency_kind": "success_only",
        "expected_graph_revision": expected_graph_rev,
        "now": now_ms(),
    });
    let stage_url = format!("{}/v1/flows/{flow_id}/edges", args.server);
    let resp = client.post(&stage_url).json(&stage_body).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("stage_edge failed ({status}): {body}");
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("stage_edge: decode body {body}"))?;
    let new_rev = v
        .pointer("/Staged/new_graph_revision")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| anyhow::anyhow!("stage_edge: missing new_graph_revision in {v}"))?;

    let apply_body = serde_json::json!({
        "flow_id": flow_id,
        "edge_id": edge_id,
        "downstream_execution_id": downstream,
        "upstream_execution_id": upstream,
        "graph_revision": new_rev,
        "dependency_kind": "success_only",
        "now": now_ms(),
    });
    let apply_url = format!("{}/v1/flows/{flow_id}/edges/apply", args.server);
    let resp = client.post(&apply_url).json(&apply_body).send().await?;
    check_ok(resp, "apply_edge").await?;
    Ok(new_rev)
}

/// Poll execution state until it reaches a terminal value. Returns the
/// terminal state name (`completed`, `failed`, `cancelled`, `expired`).
/// Uses jittered backoff so concurrent submits don't synchronize their
/// ZRANGEBYSCORE polls against one lane.
async fn wait_terminal(client: &Client, args: &Args, eid: &str) -> Result<String> {
    let url = format!("{}/v1/executions/{eid}/state", args.server);
    loop {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("state poll failed: {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        let state = v.as_str().unwrap_or("unknown").to_owned();
        match state.as_str() {
            "completed" | "failed" | "cancelled" | "expired" => return Ok(state),
            _ => jittered_sleep(args.poll_ms).await,
        }
    }
}

/// Fetch the JSON-decoded completion result through
/// `GET /v1/executions/{id}/result`.
///
/// WARNING: the endpoint returns bytes written by the most recent
/// `ff_complete_execution` call. If retries are configured and an earlier
/// attempt completed before being invalidated, those bytes would persist
/// until a newer attempt overwrites them. The media-pipeline example
/// configures no retries, so reading right after a success poll is
/// unambiguous; adapt this pattern with care when retries are on.
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
) -> JoinHandle<()> {
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

/// Sleep `base` ms plus a uniform 0..=base/2 jitter. Cheap uniform draw
/// from nanos-of-system-time avoids pulling in a rand dep for this.
async fn jittered_sleep(base: u64) {
    let jitter = if base > 1 {
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        now_nanos % (base / 2 + 1)
    } else {
        0
    };
    tokio::time::sleep(Duration::from_millis(base + jitter)).await;
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
