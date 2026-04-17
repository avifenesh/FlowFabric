//! Summarize worker — stage 2 of the media pipeline.
//!
//! Claims executions with capability `llm`, streams Qwen2.5-0.5B-Instruct
//! Q4_K_M tokens as `summary_token` frames, then suspends for human review.
//! The `suspend()` call mints an HMAC-bound `waitpoint_token` (RFC-004);
//! it is printed on stdout so the review CLI can sign its signal.
//!
//! Resume protocol:
//!   * Approval → review CLI delivers the signed signal → engine re-queues →
//!     this worker re-claims the same execution → we look up the stashed
//!     `SummarizeResult` and `complete()`.
//!   * Rejection → review CLI calls `/v1/executions/{id}/cancel` → the
//!     execution is terminal-cancelled and never re-queues.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use ff_sdk::{
    ClaimedTask, ConditionMatcher, FlowFabricWorker, SuspendOutcome, TimeoutBehavior,
    WorkerConfig,
};
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
    sampling::LlamaSampler,
};
use media_pipeline::{SummarizeResult, TranscribeResult, SIGNAL_NAME_APPROVAL};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "summarize", about = "FlowFabric media-pipeline summarize worker")]
struct Args {
    #[arg(long, env = "FF_HOST", default_value = "localhost")]
    host: String,

    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,

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
    let mut config = WorkerConfig::new(
        &args.host,
        args.port,
        "summarize",
        &instance_id,
        &args.namespace,
        &args.lane,
    );
    config.capabilities = vec!["llm".into(), "qwen-500m-q4".into()];

    let worker = FlowFabricWorker::connect(config).await?;
    tracing::info!(instance = %instance_id, model = %args.model.display(), "summarize worker connected");

    let pending: Arc<Mutex<HashMap<String, SummarizeResult>>> = Default::default();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received");
                break;
            }
            result = worker.claim_next() => {
                match result {
                    Ok(Some(task)) => {
                        let eid = task.execution_id().to_string();

                        // Re-claim after approval — complete with stashed result.
                        if let Some(stashed) = pending.lock().await.remove(&eid) {
                            tracing::info!(execution_id = %eid, "resumed after approval — completing");
                            if let Err(e) = task
                                .complete(Some(serde_json::to_vec(&stashed)?))
                                .await
                            {
                                tracing::error!(execution_id = %eid, error = %e, "complete failed");
                            }
                            continue;
                        }

                        if let Err(e) = process(
                            task,
                            engine.clone(),
                            args.max_new_tokens,
                            args.skip_approval,
                            pending.clone(),
                        ).await {
                            tracing::error!(execution_id = %eid, error = %e, "task failed");
                        }
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
                    Err(e) => {
                        tracing::error!(error = %e, "claim_next failed");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn process(
    task: ClaimedTask,
    engine: Arc<Engine>,
    max_new_tokens: usize,
    skip_approval: bool,
    pending: Arc<Mutex<HashMap<String, SummarizeResult>>>,
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

    let result = match generate(&engine, &input.transcript, max_new_tokens, &task).await {
        Ok(r) => r,
        Err(e) => {
            task.fail(&format!("llama inference: {e}"), "inference_error")
                .await?;
            return Err(e);
        }
    };

    tracing::info!(execution_id = %eid, tokens = result.token_count, "generation complete");

    if skip_approval {
        task.complete(Some(serde_json::to_vec(&result)?)).await?;
        return Ok(());
    }

    // Stash before suspending — the task is consumed by suspend().
    pending.lock().await.insert(eid.clone(), result);

    let outcome = task
        .suspend(
            "awaiting_summary_review",
            &[ConditionMatcher {
                signal_name: SIGNAL_NAME_APPROVAL.into(),
            }],
            Some(3_600_000),
            TimeoutBehavior::Fail,
        )
        .await;

    match outcome {
        Ok(SuspendOutcome::Suspended {
            waitpoint_id,
            waitpoint_token,
            ..
        }) => {
            println!("REVIEW_NEEDED eid={eid} wp={waitpoint_id}");
            println!("WAITPOINT_TOKEN={waitpoint_token}");
        }
        Ok(SuspendOutcome::AlreadySatisfied { .. }) => {
            // No pending waitpoint was created, so this branch is unreachable
            // in practice. Drop the stash to avoid leaking memory if it ever
            // fires.
            pending.lock().await.remove(&eid);
            tracing::warn!(execution_id = %eid, "suspend returned AlreadySatisfied unexpectedly");
        }
        Err(e) => {
            pending.lock().await.remove(&eid);
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
        if let Ok(piece) = std::str::from_utf8(&piece_bytes) {
            text.push_str(piece);
        }
        if let Err(e) = task
            .append_frame("summary_token", &piece_bytes, None)
            .await
        {
            tracing::warn!(error = %e, "append_frame failed");
        }
        batch.clear();
        batch.add(tok, cur_pos, &[0], true)?;
        ctx.decode(&mut batch).context("decode token")?;
        cur_pos += 1;
        generated += 1;
    }

    Ok(SummarizeResult {
        summary: text.trim().to_owned(),
        token_count: generated,
    })
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
