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
    PipelineInput, SummarizeResult, TranscribeResult, FRAME_FIELD_PAYLOAD,
    FRAME_FIELD_TYPE, KIND_EMBED, KIND_SUMMARIZE, KIND_TRANSCRIBE, LABEL_EMBED,
    LABEL_SUMMARIZE, LABEL_TRANSCRIBE,
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

    // Print the flow ID + transcribe EID up-front so diagnostics can
    // correlate early failures. The summarize and embed EIDs are
    // emitted AFTER their executions are actually created — otherwise
    // `demo.sh` races, grepping the EID out of the submit log and
    // launching `review --execution-id <summarize>` against an
    // execution the server has not heard of yet (404, no retry).
    log(stdout, &format!("flow_id={flow_id}")).await;
    log(stdout, &format!("transcribe={eid_transcribe}")).await;

    // Shared flag the SIGINT branch reads to decide whether to POST
    // /cancel. Once the pipeline has reached terminal completion we
    // only need to finish draining the tails; cancelling an
    // already-terminal flow is cosmetic (server returns
    // flow_already_terminal) but adds a spurious "cancelling"
    // log line that confuses operators doing post-hoc log reads.
    let pipeline_terminal = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
        pipeline_terminal.clone(),
    );
    tokio::pin!(pipeline);

    tokio::select! {
        res = &mut pipeline => res,
        _ = tokio::signal::ctrl_c() => {
            if pipeline_terminal.load(std::sync::atomic::Ordering::Acquire) {
                log(stdout, "SIGINT after embed completed — skipping cancel (flow already terminal)").await;
                anyhow::bail!("interrupted by user (SIGINT) after completion");
            }
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

#[allow(clippy::too_many_arguments)]
async fn pipeline_inner(
    client: &Client,
    args: &Args,
    stdout: &Arc<Mutex<Stdout>>,
    flow_id: &str,
    eid_transcribe: &str,
    eid_summarize: &str,
    eid_embed: &str,
    pipeline_terminal: Arc<std::sync::atomic::AtomicBool>,
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
        drain_and_stop(tail_transcribe).await;
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
    // Emit summarize EID now that the server knows about the execution.
    // `demo.sh` greps this line to launch `review --execution-id ...`;
    // emitting it earlier races the review bin against the create.
    log(stdout, &format!("summarize={eid_summarize}")).await;

    let tail_summarize = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_summarize.to_owned(),
        LABEL_SUMMARIZE,
    );

    let s_state = wait_terminal(client, args, eid_summarize).await?;
    if s_state != "completed" {
        drain_and_stop(tail_transcribe).await;
        drain_and_stop(tail_summarize).await;
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
    // Emit embed EID after the execution exists on the server — same
    // reasoning as summarize above. Keeps `demo.sh`-style consumers
    // grep-driven and race-free.
    log(stdout, &format!("embed={eid_embed}")).await;

    let tail_embed = spawn_tail(
        client.clone(),
        args.server.clone(),
        stdout.clone(),
        eid_embed.to_owned(),
        LABEL_EMBED,
    );

    let e_state = wait_terminal(client, args, eid_embed).await?;
    if e_state != "completed" {
        drain_and_stop(tail_transcribe).await;
        drain_and_stop(tail_summarize).await;
        drain_and_stop(tail_embed).await;
        anyhow::bail!("embed ended in {e_state}");
    }
    // Flag the SIGINT handler that the flow is terminal so a Ctrl-C
    // during the remaining tail drain doesn't POST a redundant /cancel.
    pipeline_terminal.store(true, std::sync::atomic::Ordering::Release);
    log(stdout, "embed done — pipeline complete").await;

    // Happy path: give each tail a 300ms grace to flush buffered
    // frames, then abort. `tokio::join!`-ing them (as an earlier
    // version did) hangs indefinitely on executions that complete
    // without ever writing a stream frame — embed in this pipeline
    // produces a result payload only (no streamed output), so its
    // attempt stream is never created and therefore never carries a
    // `closed_at` marker for the tail loop to observe. 300ms per tail
    // is the same grace we give on the error path via
    // `drain_and_stop` above.
    drain_and_stop(tail_transcribe).await;
    drain_and_stop(tail_summarize).await;
    drain_and_stop(tail_embed).await;
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
///
/// Transient server errors (5xx) and transport errors retry with
/// exponential backoff (300ms base, 5s cap). 4xx bails loud — those are
/// real client errors. After `MAX_TRANSIENT` consecutive transient
/// failures we give up. Sized for ~2 minutes of outage tolerance at the
/// 5s cap, long enough to ride through a typical Valkey failover without
/// killing the pipeline.
async fn wait_terminal(client: &Client, args: &Args, eid: &str) -> Result<String> {
    const MAX_TRANSIENT: u32 = 30;
    let url = format!("{}/v1/executions/{eid}/state", args.server);
    let mut transient_failures: u32 = 0;
    loop {
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_client_error() {
                    anyhow::bail!("state poll client error: {status}");
                }
                if !status.is_success() {
                    transient_failures += 1;
                    if transient_failures >= MAX_TRANSIENT {
                        anyhow::bail!(
                            "state poll: {transient_failures} consecutive transient failures (last: {status})"
                        );
                    }
                    backoff_sleep(transient_failures).await;
                    continue;
                }
                transient_failures = 0;
                let v: serde_json::Value = resp.json().await?;
                let state = v.as_str().unwrap_or("unknown").to_owned();
                match state.as_str() {
                    "completed" | "failed" | "cancelled" | "expired" => return Ok(state),
                    _ => jittered_sleep(args.poll_ms).await,
                }
            }
            Err(e) => {
                transient_failures += 1;
                if transient_failures >= MAX_TRANSIENT {
                    anyhow::bail!(
                        "state poll: {transient_failures} consecutive transport failures (last: {e})"
                    );
                }
                backoff_sleep(transient_failures).await;
            }
        }
    }
}

/// Fetch the JSON-decoded completion result through
/// `GET /v1/executions/{id}/result`.
///
/// WARNING: the endpoint returns bytes written by the most recent
/// `ff_complete_execution` call. If retries are configured and an earlier
/// attempt completed before being invalidated, those bytes would persist
/// until a newer attempt overwrites them.
///
/// Defensive guard: this helper refuses to decode the result when the
/// execution's `current_attempt_index > 0`, because in that case the
/// stored bytes may be from a prior attempt that the completion-write
/// of the current attempt has NOT yet overwritten. The media-pipeline
/// example configures no retries so this is always 0 on the happy path;
/// downstream adopters that enable retries must either use a different
/// result-exchange channel or wait for Batch C's per-attempt result
/// isolation.
async fn fetch_result<T: for<'de> Deserialize<'de>>(
    client: &Client,
    args: &Args,
    eid: &str,
) -> Result<T> {
    // Sanity-check the attempt index before reading the result key.
    let info_url = format!("{}/v1/executions/{eid}", args.server);
    let info_resp = client.get(&info_url).send().await?;
    if !info_resp.status().is_success() {
        anyhow::bail!(
            "pre-fetch execution info failed: {} — cannot safely read /result",
            info_resp.status()
        );
    }
    #[derive(Deserialize)]
    struct ExecInfoSubset {
        current_attempt_index: u32,
    }
    let info: ExecInfoSubset = info_resp.json().await?;
    if info.current_attempt_index > 0 {
        anyhow::bail!(
            "execution {eid} has current_attempt_index={} (retried). \
             /result is not attempt-scoped in v1 — refusing to decode to \
             avoid returning stale bytes. See Batch C for retry-safe \
             result exchange.",
            info.current_attempt_index
        );
    }

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
    /// Tail failures are noisier on startup (stream not yet created) and
    /// during concurrency caps (429). We warn loudly at this threshold so
    /// operators see it but keep retrying — a tail giving up silently is
    /// worse than a tail complaining.
    const FATAL_THRESHOLD: u32 = 60;

    tokio::spawn(async move {
        let mut after = "0-0".to_owned();
        let mut consecutive_failures: u32 = 0;
        let mut warned_fatal = false;
        loop {
            let url = format!(
                "{server}/v1/executions/{eid}/attempts/0/stream/tail?after={after}&block_ms=5000&limit=100"
            );
            let step_result: Result<bool, String> = async {
                let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    // 429 (concurrency cap) or 404 (stream not yet created)
                    // or 5xx — all retryable. Burn one failure slot.
                    return Err(format!("status {}", resp.status()));
                }
                let body: StreamResponse = resp.json().await.map_err(|e| e.to_string())?;
                for f in &body.frames {
                    after = f.id.clone();
                    let ft = f.fields.get(FRAME_FIELD_TYPE).cloned().unwrap_or_default();
                    if ft == "result_payload" {
                        continue;
                    }
                    let payload = f.fields.get(FRAME_FIELD_PAYLOAD).cloned().unwrap_or_default();
                    log_raw(&stdout, &format!("[{label}] {ft}: {payload}")).await;
                }
                Ok(body.closed_at.is_some().then(|| {
                    body.closed_reason.clone().unwrap_or_default()
                }).is_some())
                // Returning bool doesn't carry closed_reason; pass it through
                // via a closure side-channel would overcomplicate — just
                // re-read outside if needed. In practice closed_at => return.
            }
            .await;

            match step_result {
                Ok(true) => {
                    log_raw(&stdout, &format!("[{label}] stream closed")).await;
                    return;
                }
                Ok(false) => {
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures == FATAL_THRESHOLD && !warned_fatal {
                        log_raw(
                            &stdout,
                            &format!(
                                "[{label}] WARNING: {consecutive_failures} consecutive tail failures (last: {e}); continuing to retry"
                            ),
                        )
                        .await;
                        warned_fatal = true;
                    } else if consecutive_failures <= 3 {
                        // Avoid spamming: only log the first few transient
                        // errors. Post-threshold we already fired the
                        // WARNING once.
                        log_raw(&stdout, &format!("[{label}] tail error: {e}")).await;
                    }
                    backoff_sleep(consecutive_failures).await;
                }
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

/// Exponential backoff sleep. For `attempt = 1, 2, 3, …` this sleeps
/// approximately `min(300 * 2^(attempt-1), 5000)` ms plus a small
/// jitter. Caller tracks the attempt count.
async fn backoff_sleep(attempt: u32) {
    const BASE_MS: u64 = 300;
    const CAP_MS: u64 = 5_000;
    let shift = (attempt.saturating_sub(1)).min(6); // 2^6 = 64x base = 19_200 → capped
    let ms = (BASE_MS.saturating_mul(1u64 << shift)).min(CAP_MS);
    jittered_sleep(ms).await;
}

/// Let the tail drain any in-flight frames for ~300ms before aborting.
/// Without the grace window, aborting immediately after a non-completed
/// wait_terminal return discards the last block_ms of buffered frames —
/// exactly the debug context an operator wants to see when something
/// failed. Bounded by the 300ms sleep.
async fn drain_and_stop(tail: JoinHandle<()>) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    tail.abort();
    let _ = tail.await;
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
