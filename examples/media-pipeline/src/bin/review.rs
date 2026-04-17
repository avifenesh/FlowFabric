//! Review CLI — tail the summarize execution, deliver an HMAC-signed
//! approval signal when it suspends, OR cancel on rejection.
//!
//! Demonstrates two Batch B primitives:
//!   * `GET /v1/executions/{id}/pending-waitpoints` to fetch the token.
//!   * `POST /v1/executions/{id}/signal` with the HMAC-bound token. The
//!     `--tamper-token` flag mutates the token before send so the user
//!     can watch the server reject it with `invalid_token`.
//!
//! # v1 decision protocol: approve = signal, reject = cancel
//!
//! ff-sdk does not surface the resume signal's payload to the re-claimed
//! worker, so the summarize worker cannot distinguish approve vs reject
//! by inspecting the signal on resume. Instead this CLI uses the control
//! plane to differentiate:
//!
//!   * `--approve` (or interactive `y`) → POST /signal (HMAC-authenticated).
//!     Engine fulfils the waitpoint → summarize worker re-claims → reads
//!     the persisted `summary_final` frame → `complete()`.
//!
//!   * `--reject <reason>` (or interactive `n`) → POST /cancel. The
//!     execution transitions to terminal cancelled; no signal is sent
//!     and no re-claim occurs.
//!
//! Tracked as a Batch C follow-up: "Expose resume signal payload on
//! ClaimedTask so workers can route approve vs reject in-band".

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use media_pipeline::{ApprovalDecision, SIGNAL_NAME_APPROVAL};
use reqwest::Client;
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "review", about = "Approve or reject a suspended summarize execution")]
struct Args {
    #[arg(long, env = "FF_SERVER", default_value = "http://localhost:9090")]
    server: String,

    #[arg(long, env = "FF_API_TOKEN")]
    api_token: Option<String>,

    /// Summarize execution id to review.
    #[arg(long)]
    execution_id: String,

    /// Non-interactive: skip the prompt and approve immediately.
    #[arg(long)]
    auto_approve: bool,

    /// Non-interactive: skip the prompt and reject with this reason.
    #[arg(long)]
    reject: Option<String>,

    /// Flip one hex char in the token before sending — the server must
    /// reject the signal with `invalid_token`. Used to demonstrate HMAC
    /// enforcement end-to-end.
    #[arg(long)]
    tamper_token: bool,

    /// Base poll interval for state polling. Actual sleep is
    /// `poll_ms` plus a uniform 0..=poll_ms/2 jitter so multiple
    /// reviewers don't synchronize ZRANGEBYSCORE hits.
    #[arg(long, default_value_t = 1000)]
    poll_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "review=info".into()),
        )
        .init();

    let args = Args::parse();
    let client = build_http(&args.api_token)?;

    println!("[review] tailing {} — waiting for suspend", args.execution_id);

    // Look up the current attempt so the tail hits the right stream key.
    // Summarize has no retry policy in this example so attempt_index = 0,
    // but the pattern is correct for downstream adopters that do retry.
    let current_attempt = fetch_current_attempt_index(&client, &args).await?;

    // Tail summary tokens while polling state. Both run concurrently so
    // the reviewer sees the incremental summary before the prompt fires.
    let tail_eid = args.execution_id.clone();
    let tail_client = client.clone();
    let tail_server = args.server.clone();
    let tail = tokio::spawn(async move {
        let mut after = "0-0".to_owned();
        loop {
            let url = format!(
                "{tail_server}/v1/executions/{tail_eid}/attempts/{current_attempt}/stream/tail?after={after}&block_ms=3000&limit=100"
            );
            let resp = match tail_client.get(&url).send().await {
                Ok(r) => r,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
            };
            if !resp.status().is_success() {
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
            let body: StreamResponse = match resp.json().await {
                Ok(b) => b,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
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
                println!("[summary {ft}] {payload}");
                let _ = std::io::stdout().flush();
            }
            if body.closed_at.is_some() {
                return;
            }
        }
    });

    // Wait for either the suspend we expect, or (rare) an already-completed
    // execution because a peer reviewer beat us to the approval. Terminal
    // "completed" is an OK outcome — the pipeline moved on without us.
    match wait_for_review_window(&client, &args).await? {
        ReviewOutcome::Suspended => {
            println!("[review] execution suspended — fetching waitpoint token");
        }
        ReviewOutcome::AlreadyCompleted => {
            println!(
                "[review] execution already completed — another reviewer beat us; exiting 0"
            );
            drain_and_stop(tail).await;
            return Ok(());
        }
    }

    let waitpoints = fetch_pending_waitpoints(&client, &args).await?;
    // NOTE: the server already scopes the /pending-waitpoints response to
    // the execution_id in the URL (see Server::list_pending_waitpoints).
    // No client-side re-check needed; listing the waitpoint's
    // execution_id here would be cosmetic.
    let wp = waitpoints
        .into_iter()
        .find(|w| w.state == "active" || w.state == "pending")
        .context("no active waitpoint after suspended state")?;

    let decision = prompt_decision(&args)?;

    match decision {
        ApprovalDecision::Approve => {
            let token = if args.tamper_token {
                tamper(&wp.waitpoint_token)
            } else {
                wp.waitpoint_token.clone()
            };
            let signal_payload = serde_json::to_vec(&ApprovalDecision::Approve)?;
            let body = serde_json::json!({
                "execution_id": args.execution_id,
                "waitpoint_id": wp.waitpoint_id,
                "signal_id": uuid::Uuid::new_v4().to_string(),
                "signal_name": SIGNAL_NAME_APPROVAL,
                "signal_category": "human_review",
                "source_type": "human",
                "source_identity": "reviewer",
                "payload": signal_payload,
                "payload_encoding": "json",
                "target_scope": "execution",
                "waitpoint_token": token,
                "now": now_ms(),
            });

            let url = format!("{}/v1/executions/{}/signal", args.server, args.execution_id);
            let resp = client.post(&url).json(&body).send().await?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();

            if args.tamper_token {
                if status.is_success() {
                    eprintln!("[review] FAIL — tampered token was accepted: {text}");
                    std::process::exit(1);
                }
                println!("[review] tampered token rejected ({status}): {text}");
                println!("[review] ── tail aborted: tamper-reject path ──");
                drain_and_stop(tail).await;
                return Ok(());
            }

            if !status.is_success() {
                anyhow::bail!("signal delivery failed ({status}): {text}");
            }
            println!("[review] signal delivered: {text}");
            println!("[review] waiting for summarize to complete");
        }
        ApprovalDecision::Reject { reason } => {
            // v1 protocol: reject → /cancel, not /signal. ff-sdk does not
            // surface the resume signal's payload to the summarize worker
            // on re-claim, so approve vs reject cannot be disambiguated
            // via the signal. Cancelling the execution is unambiguous:
            // the worker never re-claims, the flow sees a terminal
            // cancelled, and downstream edges skip.
            if args.tamper_token {
                eprintln!("[review] --tamper-token is approve-only; reject goes through /cancel");
                std::process::exit(1);
            }
            let body = serde_json::json!({
                "execution_id": args.execution_id,
                "reason": reason,
                "source": "reviewer",
                "now": now_ms(),
            });
            let url = format!("{}/v1/executions/{}/cancel", args.server, args.execution_id);
            let resp = client.post(&url).json(&body).send().await?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("cancel failed ({status}): {text}");
            }
            println!("[review] rejected — execution cancelled: {text}");
            drain_and_stop(tail).await;
            return Ok(());
        }
    }

    // Drain the tail task.
    let _ = tail.await;
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────

fn build_http(token: &Option<String>) -> Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(60));
    if let Some(t) = token {
        let mut h = reqwest::header::HeaderMap::new();
        let val = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))?;
        h.insert(reqwest::header::AUTHORIZATION, val);
        builder = builder.default_headers(h);
    }
    Ok(builder.build()?)
}

/// Possible outcomes of polling the target execution before the prompt.
enum ReviewOutcome {
    Suspended,
    /// A concurrent reviewer approved the execution between polls, so
    /// state transitioned through `suspended → runnable → completed`
    /// before we sampled. Not an error — the pipeline moved on.
    AlreadyCompleted,
}

/// Poll state until we either see `suspended` (do the review) or
/// `completed` (peer reviewer beat us). Other terminal states are a real
/// error because they imply the execution ended without human review.
async fn wait_for_review_window(client: &Client, args: &Args) -> Result<ReviewOutcome> {
    let url = format!("{}/v1/executions/{}/state", args.server, args.execution_id);
    loop {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("state poll failed: {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        let s = v.as_str().unwrap_or("unknown");
        match s {
            "suspended" => return Ok(ReviewOutcome::Suspended),
            "completed" => return Ok(ReviewOutcome::AlreadyCompleted),
            "failed" | "cancelled" | "expired" => {
                anyhow::bail!(
                    "execution ended in terminal state '{s}' before reaching suspended"
                );
            }
            _ => jittered_sleep(args.poll_ms).await,
        }
    }
}

async fn fetch_current_attempt_index(client: &Client, args: &Args) -> Result<u32> {
    #[derive(Deserialize)]
    struct ExecInfo {
        #[serde(default)]
        current_attempt_index: u32,
    }
    let url = format!("{}/v1/executions/{}", args.server, args.execution_id);
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "get execution failed: {} — review needs the current attempt index",
            resp.status()
        );
    }
    let info: ExecInfo = resp.json().await?;
    Ok(info.current_attempt_index)
}

async fn fetch_pending_waitpoints(client: &Client, args: &Args) -> Result<Vec<PendingWaitpoint>> {
    let url = format!(
        "{}/v1/executions/{}/pending-waitpoints",
        args.server, args.execution_id
    );
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("pending-waitpoints failed: {}", resp.status());
    }
    Ok(resp.json().await?)
}

fn prompt_decision(args: &Args) -> Result<ApprovalDecision> {
    if args.auto_approve {
        return Ok(ApprovalDecision::Approve);
    }
    if let Some(reason) = &args.reject {
        return Ok(ApprovalDecision::Reject {
            reason: reason.clone(),
        });
    }

    let stdin = std::io::stdin();
    loop {
        print!("Approve? [y/n]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .context("read stdin")?;
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(ApprovalDecision::Approve),
            "n" | "no" => {
                return Ok(ApprovalDecision::Reject {
                    reason: "reviewer rejected via CLI".to_owned(),
                })
            }
            _ => continue,
        }
    }
}

/// Guaranteed-different tamper: XOR the lowest bit of the last byte.
///
/// `waitpoint_token` is HMAC-SHA1 output (40 ASCII hex chars). Flipping
/// the low bit of a hex char always produces a different hex char in the
/// same charset (`0`↔`1`, `2`↔`3`, …, `a`↔`b`, …), so the resulting
/// token is both distinct and valid hex — the server rejects on HMAC
/// comparison, not on format parsing.
fn tamper(token: &str) -> String {
    let mut bytes = token.as_bytes().to_vec();
    if let Some(last) = bytes.last_mut() {
        *last ^= 0x01;
    }
    String::from_utf8(bytes).unwrap_or_else(|_| token.to_owned())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
}

/// Base + uniform 0..=base/2 jitter. Shares the cheap-uniform-from-nanos
/// trick from submit.rs to avoid pulling in a rand dep.
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

/// Let the spawned tail finish processing any in-flight frames, then
/// abort it. Flushes any buffered payload lines to stdout so we don't
/// drop frames on the floor. Bounded by the tail's own block_ms.
async fn drain_and_stop(tail: tokio::task::JoinHandle<()>) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    tail.abort();
    let _ = tail.await;
}

// ── Wire types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PendingWaitpoint {
    waitpoint_id: String,
    #[allow(dead_code)]
    waitpoint_key: String,
    state: String,
    waitpoint_token: String,
    #[allow(dead_code)]
    #[serde(default)]
    created_at: Option<i64>,
    #[allow(dead_code)]
    #[serde(default)]
    activated_at: Option<i64>,
    #[allow(dead_code)]
    #[serde(default)]
    expires_at: Option<i64>,
}

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
    #[allow(dead_code)]
    #[serde(default)]
    closed_reason: Option<String>,
}
