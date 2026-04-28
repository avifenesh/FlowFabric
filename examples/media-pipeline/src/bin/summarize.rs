//! Summarize worker — stage 2 of the media pipeline.
//!
//! Claims executions with capability `llm`, streams Qwen2.5-0.5B-Instruct
//! Q4_K_M tokens as `summary_token` frames, persists the final result as a
//! `summary_final` frame (so any worker can resume), then suspends for
//! human review. `suspend()` mints an HMAC-bound `waitpoint_token`
//! (RFC-004); it is printed on stdout so the review CLI can sign its
//! signal.
//!
//! Resume protocol:
//!   * Approval signal → engine re-queues → any worker re-claims (same or
//!     different process); it reads the last `summary_final` frame via
//!     `ff_sdk::read_stream` and calls `complete()`.
//!   * Rejection → review CLI calls `/v1/executions/{id}/cancel`; the
//!     execution never re-queues. The summarize worker has no re-claim
//!     path for rejection.
//!
//! v1 limitation: ff-sdk does not surface the resume signal's payload to
//! the re-claimed worker. We therefore encode reject-vs-approve in the
//! control plane (approve → signal + re-claim; reject → cancel, no
//! re-claim), not in the signal payload. See README §"Review decision
//! protocol".

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use ff_sdk::{
    ClaimedTask, FlowFabricAdminClient, FlowFabricWorker, ResumeCondition, ResumePolicy,
    SignalMatcher, StreamCursor, SuspensionReasonCode, TimeoutBehavior, WorkerConfig,
    STREAM_READ_HARD_CAP,
};
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
    sampling::LlamaSampler,
};
use media_pipeline::{
    find_last_summary_final, ApprovalDecision, FrameView, SummarizeResult, TranscribeResult,
    FRAME_FIELD_PAYLOAD, FRAME_FIELD_TYPE, FRAME_SUMMARY_FINAL, SIGNAL_NAME_APPROVAL,
    SUMMARY_CHAR_CAP,
};

#[derive(Parser)]
#[command(name = "summarize", about = "FlowFabric media-pipeline summarize worker")]
struct Args {
    #[arg(long, env = "FF_HOST", default_value = "localhost")]
    host: String,

    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,

    /// ff-server base URL for the scheduler-routed claim path.
    #[arg(long, env = "FF_SERVER_URL", default_value = "http://localhost:9090")]
    server_url: String,

    /// Optional bearer token for ff-server.
    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,

    #[arg(long, default_value = "default")]
    namespace: String,

    #[arg(long, default_value = "media")]
    lane: String,

    #[arg(
        long,
        env = "QWEN_MODEL",
        default_value = "examples/media-pipeline/models/qwen2.5-0.5b-instruct-q4_k_m.gguf"
    )]
    model: PathBuf,

    #[arg(long, env = "FF_MAX_NEW_TOKENS", default_value_t = 200)]
    max_new_tokens: usize,

    /// Skip the HMAC suspend loop (smoke mode). When true, `complete()` is
    /// called directly after generation.
    #[arg(long, env = "FF_SKIP_APPROVAL", default_value_t = false)]
    skip_approval: bool,
}

struct Engine {
    backend: LlamaBackend,
    model: LlamaModel,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "summarize=info,ff_sdk=info".into()),
        )
        .init();

    let args = Args::parse();

    if !args.model.exists() {
        anyhow::bail!(
            "Qwen GGUF not found at {} — run examples/media-pipeline/scripts/download-models.sh",
            args.model.display()
        );
    }

    let backend = LlamaBackend::init().context("llama backend init")?;
    let model = LlamaModel::load_from_file(&backend, &args.model, &LlamaModelParams::default())
        .with_context(|| format!("load gguf {}", args.model.display()))?;
    let engine = Arc::new(Engine { backend, model });

    let instance_id = format!("summarize-{}", uuid::Uuid::new_v4());
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("summarize"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(&instance_id),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec!["llm".into(), "qwen-500m-q4".into()],
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    partition_config: None,
    };

    let worker = FlowFabricWorker::connect(config).await?;
    let admin = match args.api_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(tok) => FlowFabricAdminClient::with_token(&args.server_url, tok)?,
        None => FlowFabricAdminClient::new(&args.server_url)?,
    };
    let lane = ff_core::types::LaneId::try_new(&args.lane)?;
    tracing::info!(
        instance = %instance_id,
        model = %args.model.display(),
        "summarize worker connected"
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received");
                break;
            }
            result = worker.claim_via_server(&admin, &lane, 10_000) => {
                match result {
                    Ok(Some(task)) => {
                        let eid = task.execution_id().to_string();
                        if let Err(e) = handle(
                            task,
                            engine.clone(),
                            args.max_new_tokens,
                            args.skip_approval,
                        ).await {
                            tracing::error!(execution_id = %eid, error = %e, "task failed");
                        }
                    }
                    Ok(None) => idle_sleep().await,
                    Err(e) => {
                        tracing::error!(error = %e, "claim_via_server failed");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Decide whether this claim is a fresh run or a resume-after-review,
/// then drive the appropriate path. The persisted `summary_final` frame
/// tells us generation has already happened; `resume_signals()` tells us
/// what the reviewer decided (approve vs reject with reason). Fresh
/// claims have no signals, so the reject branch only fires on a real
/// resume from a signed-rejection signal.
async fn handle(
    task: ClaimedTask,
    engine: Arc<Engine>,
    max_new_tokens: usize,
    skip_approval: bool,
) -> anyhow::Result<()> {
    let persisted = load_persisted_result(&task).await?;

    if persisted.is_some() {
        // Summary already generated in a prior attempt. Inspect the resume
        // signal to decide complete-vs-fail. Previously the reviewer had
        // to use POST /cancel to reject (no payload visibility); now the
        // review CLI delivers a signed ApprovalDecision signal for both
        // outcomes and we branch here.
        let signals = task
            .resume_signals()
            .await
            .context("resume_signals after review (approve or reject)")?;
        if let Some(sig) = signals
            .iter()
            .find(|s| s.signal_name == SIGNAL_NAME_APPROVAL)
        {
            let decision: Option<ApprovalDecision> = sig
                .payload
                .as_deref()
                .and_then(|b| serde_json::from_slice(b).ok());
            match decision {
                Some(ApprovalDecision::Approve) => {
                    let result = persisted.expect("checked above");
                    tracing::info!(
                        execution_id = %task.execution_id(),
                        tokens = result.token_count,
                        "review approved — completing from persisted summary_final frame"
                    );
                    task.complete(Some(serde_json::to_vec(&result)?)).await?;
                    return Ok(());
                }
                Some(ApprovalDecision::Reject { reason }) => {
                    tracing::info!(
                        execution_id = %task.execution_id(),
                        reason = %reason,
                        "review rejected — failing with reviewer reason"
                    );
                    task.fail(&reason, "human_rejected").await?;
                    return Ok(());
                }
                None => {
                    tracing::error!(
                        execution_id = %task.execution_id(),
                        "review signal had no parseable ApprovalDecision payload"
                    );
                    task.fail("review signal had unparseable payload", "bad_signal")
                        .await?;
                    return Ok(());
                }
            }
        }
        // No signal present — legacy path for approve-via-signal-only
        // deployments (pre-#36 review CLI). Complete as we did before so
        // existing operator tooling that still only delivers approve
        // keeps working during a rolling upgrade.
        let result = persisted.expect("checked above");
        tracing::info!(
            execution_id = %task.execution_id(),
            tokens = result.token_count,
            "resume without payload signal — completing (legacy reviewer)"
        );
        task.complete(Some(serde_json::to_vec(&result)?)).await?;
        return Ok(());
    }

    process(task, engine, max_new_tokens, skip_approval).await
}

/// Scan the attempt stream for the most recent `summary_final` frame.
/// Returns the decoded SummarizeResult if one is present. Absence means
/// this is a fresh claim (pre-generation).
///
/// We use the SDK's `read_stream` directly with a dedicated accessor;
/// worker's `client()` is shared with claims, but this read happens once
/// per re-claim and is bounded, so head-of-line risk is negligible.
///
/// # Read window sizing
///
/// `summary_final` is appended AFTER every `summary_token` frame, so any
/// read that truncates before the tail misses the frame and falsely
/// reports "no persisted result" — triggering a full LLM re-run on
/// resume and defeating the F1 invariant (caught by PR#8 Gemini review).
///
/// The ff-sdk's `append_frame` caps the Valkey stream at
/// `retention_maxlen = 10_000` (`STREAM_READ_HARD_CAP`), so requesting
/// that same cap reads the entire live stream by construction: no
/// pagination needed, no risk of missing the tail entry. We still walk
/// the frames in reverse so the first match wins, keeping the hot path
/// O(tokens_generated_since_last_summary_final) for typical cases.
async fn load_persisted_result(
    task: &ClaimedTask,
) -> anyhow::Result<Option<SummarizeResult>> {
    let frames = task
        .read_stream(
            StreamCursor::Start,
            StreamCursor::End,
            STREAM_READ_HARD_CAP,
        )
        .await
        .context("read_stream for resume check")?;

    let adapted: Vec<AdaptedFrame<'_>> = frames
        .frames
        .iter()
        .map(AdaptedFrame::from)
        .collect();
    find_last_summary_final(&adapted).context("decode summary_final payload")
}

/// Borrowed adapter so the shared `find_last_summary_final` helper can
/// walk `ff_core::contracts::StreamFrame` without pulling ff-core into
/// `media_pipeline`'s public API.
struct AdaptedFrame<'a> {
    frame_type: &'a str,
    payload: &'a str,
}

impl<'a> From<&'a ff_core::contracts::StreamFrame> for AdaptedFrame<'a> {
    fn from(frame: &'a ff_core::contracts::StreamFrame) -> Self {
        Self {
            frame_type: frame
                .fields
                .get(FRAME_FIELD_TYPE)
                .map(|s| s.as_str())
                .unwrap_or(""),
            payload: frame
                .fields
                .get(FRAME_FIELD_PAYLOAD)
                .map(|s| s.as_str())
                .unwrap_or(""),
        }
    }
}

impl<'a> FrameView for AdaptedFrame<'a> {
    fn frame_type(&self) -> &str {
        self.frame_type
    }
    fn payload(&self) -> &str {
        self.payload
    }
}

async fn process(
    task: ClaimedTask,
    engine: Arc<Engine>,
    max_new_tokens: usize,
    skip_approval: bool,
) -> anyhow::Result<()> {
    let eid = task.execution_id().to_string();

    let input: TranscribeResult = match serde_json::from_slice(task.input_payload()) {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("invalid TranscribeResult: {e}");
            task.fail(&reason, "bad_input").await?;
            anyhow::bail!(reason);
        }
    };

    tracing::info!(execution_id = %eid, transcript_chars = input.transcript.len(), "summarizing");

    let mut result = match generate(&engine, &input.transcript, max_new_tokens, &task).await {
        Ok(r) => r,
        Err(e) => {
            task.fail(&format!("llama inference: {e}"), "inference_error")
                .await?;
            return Err(e);
        }
    };

    // F5: cap summary length to keep downstream embed bounded.
    if result.summary.len() > SUMMARY_CHAR_CAP {
        result.summary = result.summary.chars().take(SUMMARY_CHAR_CAP).collect();
    }

    tracing::info!(execution_id = %eid, tokens = result.token_count, "generation complete");

    // F1: persist the final result BEFORE any control-plane action so any
    // worker that re-claims after resume can find it. Emitted as a frame
    // (stream is durable; append_frame is synchronously replicated).
    //
    // R2-F28: on persist failure, call task.fail explicitly so retry goes
    // through the engine's retry policy (with the caller's retry budget
    // honoured) rather than a silent lease-expiry / scanner re-queue which
    // would repeat the full LLM inference on the retry.
    let result_bytes = serde_json::to_vec(&result)?;
    if let Err(e) = task
        .append_frame(FRAME_SUMMARY_FINAL, &result_bytes, None)
        .await
    {
        let reason = format!("persist summary_final failed: {e}");
        // Best-effort: if the task.fail itself errors we still bubble the
        // original append error so the claim loop logs both.
        let _ = task.fail(&reason, "persist_failed").await;
        anyhow::bail!(reason);
    }

    if skip_approval {
        task.complete(Some(result_bytes)).await?;
        return Ok(());
    }

    let wp_key = format!("wpk:{}", uuid::Uuid::new_v4());
    let outcome = task
        .suspend(
            SuspensionReasonCode::WaitingForOperatorReview,
            ResumeCondition::Single {
                waitpoint_key: wp_key,
                matcher: SignalMatcher::ByName(SIGNAL_NAME_APPROVAL.into()),
            },
            Some((
                ff_core::types::TimestampMs::from_millis(
                    ff_core::types::TimestampMs::now().0 + 3_600_000,
                ),
                TimeoutBehavior::Fail,
            )),
            ResumePolicy::normal(),
        )
        .await;
    match outcome {
        Ok(handle) => {
            let waitpoint_id = &handle.details.waitpoint_id;
            let waitpoint_token = handle.details.waitpoint_token.token();
            println!("REVIEW_NEEDED eid={eid} wp={waitpoint_id}");
            println!("WAITPOINT_TOKEN={}", waitpoint_token.as_str());
        }
        Err(e) => {
            return Err(e.into());
        }
    }

    Ok(())
}

async fn generate(
    engine: &Engine,
    transcript: &str,
    max_new_tokens: usize,
    task: &ClaimedTask,
) -> anyhow::Result<SummarizeResult> {
    let prompt = format!(
        "Summarize this transcript in 2 sentences:\n\n{transcript}\n\nSummary:"
    );

    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(2048))
        .with_n_threads(num_cpus::get() as i32);
    let mut ctx = engine
        .model
        .new_context(&engine.backend, ctx_params)
        .context("new_context")?;

    let tokens = engine
        .model
        .str_to_token(&prompt, AddBos::Always)
        .context("tokenize prompt")?;
    let prompt_len = tokens.len();
    anyhow::ensure!(prompt_len < 2048, "prompt too long for n_ctx=2048");

    let mut batch = LlamaBatch::new(2048, 1);
    let last = prompt_len - 1;
    for (i, tok) in tokens.iter().enumerate() {
        batch.add(*tok, i as i32, &[0], i == last)?;
    }
    ctx.decode(&mut batch).context("decode prompt")?;

    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::temp(0.3),
        LlamaSampler::greedy(),
    ]);

    let mut text = String::new();
    let mut pending_bytes: Vec<u8> = Vec::new(); // F4: UTF-8 accumulation buffer.
    let mut cur_pos = prompt_len as i32;
    let mut generated: u32 = 0;

    for _ in 0..max_new_tokens {
        let tok = sampler.sample(&ctx, batch.n_tokens() - 1);
        sampler.accept(tok);
        if engine.model.is_eog_token(tok) {
            break;
        }
        let piece_bytes = engine
            .model
            .token_to_piece_bytes(tok, 32, false, None)
            .unwrap_or_default();
        pending_bytes.extend_from_slice(&piece_bytes);

        // F4: Qwen BPE tokens can split multibyte codepoints. Only emit the
        // bytes we can decode cleanly; carry the tail forward until the
        // rest of the codepoint arrives.
        let flushable = utf8_prefix_len(&pending_bytes);
        if flushable > 0 {
            let (good, rest) = pending_bytes.split_at(flushable);
            if let Ok(s) = std::str::from_utf8(good) {
                text.push_str(s);
            }
            if let Err(e) = task
                .append_frame("summary_token", good, None)
                .await
            {
                tracing::warn!(error = %e, "append_frame failed");
            }
            pending_bytes = rest.to_vec();
        }

        batch.clear();
        batch.add(tok, cur_pos, &[0], true)?;
        ctx.decode(&mut batch).context("decode token")?;
        cur_pos += 1;
        generated += 1;
    }

    // Flush any residual bytes that never formed a codepoint boundary —
    // emit lossy so the on-stream reconstruction at least sees something.
    if !pending_bytes.is_empty() {
        let s = String::from_utf8_lossy(&pending_bytes).into_owned();
        text.push_str(&s);
        let _ = task
            .append_frame("summary_token", s.as_bytes(), None)
            .await;
    }

    Ok(SummarizeResult {
        summary: text.trim().to_owned(),
        token_count: generated,
    })
}

/// Length of the longest prefix of `buf` that is valid UTF-8, in bytes.
/// Returns a multiple of valid codepoints; never returns a value that
/// lands mid-codepoint.
fn utf8_prefix_len(buf: &[u8]) -> usize {
    match std::str::from_utf8(buf) {
        Ok(_) => buf.len(),
        Err(e) => e.valid_up_to(),
    }
}

/// Jittered idle sleep for `claim_next` -> `Ok(None)`. Burst-avoidance for
/// worker fleets (F25): without jitter, N workers all poll at t=0, t=1s,
/// t=2s on the same partition ZSETs. 200–1200ms random keeps aggregate
/// claim QPS on the cluster similar to a 1Hz mean without the clock-
/// aligned thundering herd.
///
/// Seed source: `SystemTime::now().subsec_nanos()`. This is NOT a
/// cryptographic RNG — it's a cheap uniform-ish draw to de-phase
/// concurrent workers. Skipping a `rand` dep keeps the example crate
/// graph lean; jitter is the only randomness we need.
async fn idle_sleep() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let ms = 200 + (nanos % 1000) as u64;
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
