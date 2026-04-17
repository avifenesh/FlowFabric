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
use ff_sdk::{ClaimedTask, FlowFabricWorker, WorkerConfig};
use media_pipeline::{EmbedResult, SummarizeResult};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "embed", about = "FlowFabric media-pipeline embed worker")]
struct Args {
    #[arg(long, env = "FF_HOST", default_value = "localhost")]
    host: String,

    #[arg(long, env = "FF_PORT", default_value_t = 6379)]
    port: u16,

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

    let mut model = TextEmbedding::try_new(InitOptions::new(EmbeddingModel::AllMiniLML6V2))
        .context("fastembed init")?;

    if args.warm {
        let _ = model
            .embed(vec!["warmup"], None)
            .context("fastembed warmup")?;
        tracing::info!("fastembed MiniLM-L6-v2 cache warmed");
        return Ok(());
    }

    let model = Arc::new(Mutex::new(model));

    let instance_id = format!("embed-{}", uuid::Uuid::new_v4());
    let mut config = WorkerConfig::new(
        &args.host,
        args.port,
        "embed",
        &instance_id,
        &args.namespace,
        &args.lane,
    );
    config.capabilities = vec!["embed".into(), "minilm-l6".into()];

    let worker = FlowFabricWorker::connect(config).await?;
    tracing::info!(instance = %instance_id, "embed worker connected");

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
                        if let Err(e) = process(task, model.clone()).await {
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

async fn process(task: ClaimedTask, model: Arc<Mutex<TextEmbedding>>) -> anyhow::Result<()> {
    let input: SummarizeResult = match serde_json::from_slice(task.input_payload()) {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("invalid SummarizeResult: {e}");
            task.fail(&reason, "bad_input").await?;
            anyhow::bail!(reason);
        }
    };

    // fastembed embed() is CPU-bound and takes &mut self. The Mutex serializes
    // concurrent claim paths (max_concurrent_tasks is 1 by default anyway);
    // held across the spawn_blocking join to keep the guard alive on the
    // blocking thread.
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

    let result = EmbedResult { vector, dim };
    task.complete(Some(serde_json::to_vec(&result)?)).await?;
    tracing::info!(dim, "embed complete");
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
