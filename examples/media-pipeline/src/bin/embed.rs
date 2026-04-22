//! Embed worker — stage 3 of the media pipeline.
//!
//! Claims executions with capability `embed`, runs the MiniLM-L6-v2 model
//! via fastembed-rs on the approved summary, and completes with an
//! `EmbedResult` carrying a 384-dim vector. No streaming — embedding a
//! single 2-sentence summary is sub-second.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use ff_sdk::{ClaimedTask, FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};
use media_pipeline::{EmbedResult, SummarizeResult, SUMMARY_CHAR_CAP};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "embed", about = "FlowFabric media-pipeline embed worker")]
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

    /// Load the MiniLM model and exit — used by the model download script to
    /// pre-warm the fastembed cache without entering the claim loop.
    #[arg(long)]
    warm: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "embed=info,ff_sdk=info".into()),
        )
        .init();

    let args = Args::parse();

    // Synchronous: TextEmbedding::try_new blocks the thread while it
    // downloads MiniLM (~90MB on first run) and opens ONNX Runtime. Fine
    // here — we run this BEFORE the worker is connected to Valkey, so no
    // claim loop is starved waiting. On a warm cache (see
    // scripts/download-models.sh) init is sub-second.
    let mut model = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::AllMiniLML6V2))
        .context("fastembed init")?;

    if args.warm {
        let _ = model
            .embed(vec!["warmup"], None)
            .context("fastembed warmup")?;
        tracing::info!("fastembed MiniLM-L6-v2 cache warmed");
        return Ok(());
    }

    // INVARIANT: embed worker assumes `WorkerConfig::max_concurrent_tasks = 1`
    // (the SDK default). `TextEmbedding::embed` takes `&mut self`, so the
    // Mutex below serialises embeds — safe at concurrency=1, a bottleneck
    // otherwise. Do NOT raise max_concurrent_tasks without either
    // (a) replacing this with one TextEmbedding per concurrent task, or
    // (b) accepting the serialisation.
    let model = Arc::new(Mutex::new(model));

    let instance_id = format!("embed-{}", uuid::Uuid::new_v4());
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("embed"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(&instance_id),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec!["embed".into(), "minilm-l6".into()],
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 1_000,
        max_concurrent_tasks: 1,
    };

    let worker = FlowFabricWorker::connect(config).await?;
    let admin = match args.api_token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(tok) => FlowFabricAdminClient::with_token(&args.server_url, tok)?,
        None => FlowFabricAdminClient::new(&args.server_url)?,
    };
    let lane = ff_core::types::LaneId::try_new(&args.lane)?;
    tracing::info!(instance = %instance_id, "embed worker connected");

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
                        if let Err(e) = process(task, model.clone()).await {
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

async fn process(task: ClaimedTask, model: Arc<Mutex<TextEmbedding>>) -> anyhow::Result<()> {
    let mut input: SummarizeResult = match serde_json::from_slice(task.input_payload()) {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("invalid SummarizeResult: {e}");
            task.fail(&reason, "bad_input").await?;
            anyhow::bail!(reason);
        }
    };

    // F5: guard against pathological summaries pushing embed past the lease
    // TTL. MiniLM truncates at 256 tokens anyway; keep input bounded.
    if input.summary.len() > SUMMARY_CHAR_CAP {
        input.summary = input.summary.chars().take(SUMMARY_CHAR_CAP).collect();
    }

    // F5: bail early if the lease is already unhealthy — no point starting
    // an inference we'll likely lose.
    if !task.is_lease_healthy() {
        task.fail("lease unhealthy before embed", "lease_stale")
            .await?;
        anyhow::bail!("lease unhealthy at embed start");
    }

    // fastembed's embed() takes &mut self and is CPU-bound. block_in_place
    // avoids a spawn_blocking ownership dance while still letting the
    // runtime reschedule other tasks on the freed core.
    let summary = input.summary.clone();
    let vectors = {
        let mut guard = model.lock().await;
        let texts = vec![summary];
        tokio::task::block_in_place(|| guard.embed(texts, None))
            .context("fastembed embed")?
    };
    let vector = vectors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("fastembed returned empty vector set"))?;
    let dim = vector.len() as u32;

    // R2-F31: fastembed is not cancellable mid-call; a pathological summary
    // can push embed past lease TTL. Re-check lease health before complete;
    // if stale, fail cleanly rather than submitting a doomed complete (which
    // the engine would reject as StaleLease, dropping the task without a
    // terminal operation — the scanner would then re-queue and a second
    // embed worker would repeat the work).
    if !task.is_lease_healthy() {
        task.fail("lease expired during embed", "lease_stale")
            .await?;
        anyhow::bail!("lease unhealthy post-embed");
    }

    let result = EmbedResult { vector, dim };
    task.complete(Some(serde_json::to_vec(&result)?)).await?;
    tracing::info!(dim, "embed complete");
    Ok(())
}

/// Jittered idle sleep for `claim_next` → `Ok(None)`. Mirrors summarize.rs
/// (F25): random 200–1200ms avoids N workers waking on the same second and
/// dog-piling partition ZSET reads.
///
/// Seed from `SystemTime::now().subsec_nanos()` — non-cryptographic, the
/// only randomness the example needs; keeps the crate graph `rand`-free.
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
