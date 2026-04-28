//! Transcribe worker — audio -> text via whisper.cpp.
//!
//! Claims executions with capability `asr`, spawns the `whisper-cli` binary,
//! streams each output line as a `transcribe_line` frame, and completes with
//! a `TranscribeResult` JSON payload.
//!
//! # Streaming cadence
//!
//! `whisper-cli --no-prints --no-timestamps` emits one stdout line per
//! 30-second audio segment (one or two lines for the sample clips here),
//! not one per token. The `transcribe_line` frame stream is therefore
//! segment-scoped, not token-scoped — lower frequency than the
//! summarize stage's `summary_token` stream.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use ff_sdk::{ClaimedTask, FlowFabricAdminClient, FlowFabricWorker, WorkerConfig};
use media_pipeline::{PipelineInput, TranscribeResult};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

// ── CLI ──

#[derive(Parser)]
#[command(name = "transcribe", about = "FlowFabric media-pipeline ASR worker")]
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

    /// Path to whisper-cli binary. Default resolves under vendor/whisper.cpp.
    #[arg(long, env = "WHISPER_CLI", default_value = "examples/media-pipeline/vendor/whisper.cpp/build/bin/whisper-cli")]
    whisper_cli: PathBuf,

    /// Path to ggml model. Default resolves under examples/media-pipeline/models.
    #[arg(long, env = "WHISPER_MODEL", default_value = "examples/media-pipeline/models/ggml-tiny.en-q5_1.bin")]
    model: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "transcribe=info,ff_sdk=info".into()),
        )
        .init();

    let args = Args::parse();

    if !args.whisper_cli.exists() {
        anyhow::bail!(
            "whisper-cli not found at {} — run examples/media-pipeline/scripts/setup.sh",
            args.whisper_cli.display()
        );
    }
    if !args.model.exists() {
        anyhow::bail!(
            "model not found at {} — run examples/media-pipeline/scripts/setup.sh",
            args.model.display()
        );
    }

    let instance_id = format!("transcribe-{}", uuid::Uuid::new_v4());
    let config = WorkerConfig {
        backend: ff_core::backend::BackendConfig::valkey(&args.host, args.port),
        worker_id: ff_core::types::WorkerId::new("transcribe"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(&instance_id),
        namespace: ff_core::types::Namespace::new(&args.namespace),
        lanes: vec![ff_core::types::LaneId::new(&args.lane)],
        capabilities: vec!["asr".into(), "whisper-tiny-en".into()],
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
    tracing::info!(instance = %instance_id, "transcribe worker connected");

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
                        if let Err(e) = process(task, &args.whisper_cli, &args.model).await {
                            tracing::error!(execution_id = %eid, error = %e, "task failed");
                        }
                    }
                    // Fixed 1s sleep; summarize/embed use a jittered
                    // `idle_sleep` to avoid N workers waking on the
                    // same second. Pre-existing inconsistency — left
                    // for a follow-up that extracts one shared
                    // idle_sleep helper across all three binaries.
                    Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
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

async fn process(
    task: ClaimedTask,
    whisper_cli: &PathBuf,
    model: &PathBuf,
) -> anyhow::Result<()> {
    let input: PipelineInput = match serde_json::from_slice(task.input_payload()) {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("invalid payload: {e}");
            task.fail(&reason, "bad_input").await?;
            anyhow::bail!(reason);
        }
    };

    if !std::path::Path::new(&input.audio_path).exists() {
        let reason = format!("audio file not found: {}", input.audio_path);
        task.fail(&reason, "bad_input").await?;
        anyhow::bail!(reason);
    }

    tracing::info!(audio = %input.audio_path, "transcribing");
    let started = Instant::now();

    // kill_on_drop(true) so that if this task is dropped (worker Ctrl-C
    // mid-transcription, or claim_next future cancelled) the spawned
    // whisper-cli receives SIGKILL instead of being orphaned as a
    // grandchild of PID 1. Without this, a user interrupt leaves
    // whisper-cli chewing CPU until it finishes or the host is
    // rebooted.
    let mut child = Command::new(whisper_cli)
        .arg("-m").arg(model)
        .arg("-f").arg(&input.audio_path)
        .arg("--no-timestamps")
        .arg("--no-prints")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn whisper-cli")?;

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();

    let mut transcript = String::new();
    let mut stderr_tail = String::new();

    loop {
        tokio::select! {
            biased;
            line = stdout_lines.next_line() => {
                match line? {
                    Some(l) => {
                        let trimmed = l.trim();
                        if trimmed.is_empty() { continue; }
                        task.append_frame("transcribe_line", trimmed.as_bytes(), None).await?;
                        if !transcript.is_empty() { transcript.push(' '); }
                        transcript.push_str(trimmed);
                    }
                    None => break,
                }
            }
            line = stderr_lines.next_line() => {
                if let Some(l) = line? {
                    stderr_tail.push_str(&l);
                    stderr_tail.push('\n');
                    if stderr_tail.len() > 4096 {
                        let rev: String = stderr_tail.chars().rev().take(2048).collect();
                        stderr_tail = rev.chars().rev().collect();
                    }
                }
            }
        }
    }

    // Drain remaining stderr after stdout closes.
    while let Some(l) = stderr_lines.next_line().await? {
        stderr_tail.push_str(&l);
        stderr_tail.push('\n');
    }

    let status = child.wait().await.context("wait whisper-cli")?;
    let duration_ms = started.elapsed().as_millis() as u64;

    if !status.success() {
        let tail = stderr_tail.chars().rev().take(500).collect::<String>();
        let tail: String = tail.chars().rev().collect();
        let reason = format!("whisper-cli exit {status}: {tail}");
        task.fail(&reason, "transcode_failed").await?;
        anyhow::bail!(reason);
    }

    let result = TranscribeResult { transcript, duration_ms };
    task.complete(Some(serde_json::to_vec(&result)?)).await?;
    tracing::info!(duration_ms, "transcribe complete");
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
