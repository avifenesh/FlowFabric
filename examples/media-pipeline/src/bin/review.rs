//! Review CLI — tail the summarize execution, deliver an HMAC-signed
//! approval signal when it suspends, OR cancel on rejection.
//!
//! Demonstrates `POST /v1/executions/{id}/signal` with the HMAC-bound
//! token. The `--tamper-token` flag mutates the token before send so the
//! user can watch the server reject it with `invalid_token`.
//!
//! The `--waitpoint-id` is printed by the summarize worker on suspend
//! (look for `REVIEW_NEEDED ... wp=...`). The raw HMAC
//! `waitpoint_token` is fetched through the admin client's
//! `read_waitpoint_token` helper (ff-sdk v0.14) — operators no longer
//! copy it out of the worker's log.
//!
//! # Decision protocol: both approve AND reject deliver a signed signal
//!
//! ff-sdk v0.3 added `ClaimedTask::resume_signals()`, which exposes the
//! resume signal's payload to the re-claimed worker. The summarize
//! worker now inspects the `ApprovalDecision` inside the signal to
//! branch between complete (Approve) and fail-with-reason (Reject).
//!
//! Pre-v0.3 this CLI used POST /cancel for rejects because there was
//! no way to ship the rejection reason through the resume path. With
//! the payload-carrying resume we now use one HMAC-authenticated
//! /signal call for both branches:
//!
//!   * `--approve` (or interactive `y`) → POST /signal with
//!     `ApprovalDecision::Approve`. Worker re-claims → `resume_signals()`
//!     returns the signal → `complete()` from the persisted
//!     `summary_final` frame.
//!   * `--reject <reason>` (or interactive `n`) → POST /signal with
//!     `ApprovalDecision::Reject { reason }`. Worker re-claims →
//!     `resume_signals()` surfaces the reason → `fail(reason, "human_rejected")`.
//!     The operator-supplied reason flows through exec_core.failure_reason
//!     and the terminal event, which POST /cancel used to drop on the floor.

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ff_core::types::{ExecutionId, WaitpointId};
use ff_sdk::admin::FlowFabricAdminClient;
use media_pipeline::{
    ApprovalDecision, FRAME_FIELD_PAYLOAD, FRAME_FIELD_TYPE, SIGNAL_NAME_APPROVAL,
};
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

    /// Waitpoint id printed by the summarize worker on suspend (look for
    /// `REVIEW_NEEDED eid=... wp=...`). The raw HMAC `waitpoint_token`
    /// is resolved through the admin client at runtime (v0.14).
    #[arg(long)]
    waitpoint_id: String,

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
        const FATAL_THRESHOLD: u32 = 60;
        let mut after = "0-0".to_owned();
        let mut consecutive_failures: u32 = 0;
        let mut warned_fatal = false;
        loop {
            let url = format!(
                "{tail_server}/v1/executions/{tail_eid}/attempts/{current_attempt}/stream/tail?after={after}&block_ms=3000&limit=100"
            );
            let step: Result<bool, String> = async {
                let resp = tail_client.get(&url).send().await.map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
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
                    println!("[summary {ft}] {payload}");
                    let _ = std::io::stdout().flush();
                }
                Ok(body.closed_at.is_some())
            }
            .await;

            match step {
                Ok(true) => return,
                Ok(false) => {
                    consecutive_failures = 0;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures == FATAL_THRESHOLD && !warned_fatal {
                        eprintln!(
                            "[review] WARNING: {consecutive_failures} consecutive tail failures (last: {e}); continuing to retry"
                        );
                        warned_fatal = true;
                    }
                    backoff_sleep(consecutive_failures).await;
                }
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

    // v0.14: fetch the HMAC waitpoint_token through the admin client.
    // Only reached on the Suspended branch, so a missing token here is
    // a legitimate error (race: peer reviewer consumed it between our
    // state poll and this fetch — the state recheck paths below catch
    // the concrete outcome).
    let admin = match args.api_token.as_deref() {
        Some(t) => FlowFabricAdminClient::with_token(&args.server, t)?,
        None => FlowFabricAdminClient::new(&args.server)?,
    };
    let execution_id_parsed = ExecutionId::parse(&args.execution_id)?;
    let waitpoint_id_parsed = WaitpointId::parse(&args.waitpoint_id)?;
    let fetched_waitpoint_token = match admin
        .read_waitpoint_token(&execution_id_parsed, &waitpoint_id_parsed)
        .await?
    {
        Some(t) => t,
        None => {
            // Race: peer already consumed the waitpoint. Re-check state
            // and surface the same "peer won" exit-0 path the /signal
            // error handlers use below.
            let recheck = recheck_state(&client, &args).await.unwrap_or_default();
            if recheck == "completed" || recheck == "failed" || recheck == "cancelled" {
                println!(
                    "[review] waitpoint token missing and state={recheck} — peer won; exiting 0"
                );
                drain_and_stop(tail).await;
                return Ok(());
            }
            anyhow::bail!(
                "waitpoint token fetch returned None (execution_id={}, \
                 waitpoint_id={}) but state={recheck}; this indicates \
                 waitpoint id drift or an expired waitpoint — re-check \
                 the `REVIEW_NEEDED eid=... wp=...` line from the summarize worker",
                args.execution_id,
                args.waitpoint_id,
            );
        }
    };

    let decision = prompt_decision(&args)?;

    match decision {
        ApprovalDecision::Approve => {
            let token = if args.tamper_token {
                tamper(&fetched_waitpoint_token)
            } else {
                fetched_waitpoint_token.clone()
            };
            let signal_payload = serde_json::to_vec(&ApprovalDecision::Approve)?;

            // Deterministic idempotency key. Two reviewers racing on the
            // same waitpoint produce the SAME key, so the second POST
            // dedupes server-side (ff_deliver_signal returns DUPLICATE with
            // the existing signal_id instead of a second signal). Without
            // this, dedup only fires when idempotency_key is non-empty
            // (see lua/signal.lua), and we'd accumulate parallel signals
            // in the audit trail. Domain-separated hash prevents collision
            // with other signal kinds we might add later.
            let idempotency_key = format!(
                "review-approve/{}/{}",
                args.execution_id, args.waitpoint_id
            );

            let body = serde_json::json!({
                "execution_id": args.execution_id,
                "waitpoint_id": args.waitpoint_id,
                "signal_id": uuid::Uuid::new_v4().to_string(),
                "signal_name": SIGNAL_NAME_APPROVAL,
                "signal_category": "human_review",
                "source_type": "human",
                "source_identity": "reviewer",
                "payload": signal_payload,
                "payload_encoding": "json",
                "target_scope": "execution",
                "idempotency_key": idempotency_key,
                "waitpoint_token": token,
                "now": now_ms(),
            });

            let url = format!("{}/v1/executions/{}/signal", args.server, args.execution_id);
            // /signal gets a longer timeout than the default 60s client
            // timeout — a slow Valkey + long dedup-window scan can push
            // latency past 60s even on a healthy cluster. Even with this
            // longer timeout, we still fall back to state recheck below
            // if the call errors, so a slow server never double-signals.
            let send_result = client
                .post(&url)
                .json(&body)
                .timeout(Duration::from_secs(120))
                .send()
                .await;

            let (status, text) = match send_result {
                Ok(resp) => {
                    let s = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    (Some(s), t)
                }
                Err(e) => {
                    // Timeout / connection error: server may or may not
                    // have processed the signal. Re-check state to decide.
                    // If terminal=completed, the peer (or an earlier retry)
                    // won the race — treat as success. If still suspended,
                    // the idempotency_key makes a safe retry but this
                    // reviewer reports the network hop issue and lets the
                    // user re-run.
                    println!("[review] /signal request error: {e} — rechecking state");
                    let recheck = recheck_state(&client, &args).await?;
                    if recheck == "completed" {
                        println!("[review] state=completed after error — peer (or retried signal) won; exiting 0");
                        drain_and_stop(tail).await;
                        return Ok(());
                    }
                    anyhow::bail!(
                        "signal delivery error: {e}. execution state={recheck}. \
                         Re-run review with the same args; idempotency_key \
                         {idempotency_key} guarantees at-most-once."
                    );
                }
            };
            let status = status.expect("matched Ok arm above");

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
                // Server-side error. Could be a dedup hit from a peer
                // reviewer (rare — same idempotency_key normally returns
                // 200 with DUPLICATE), a transient 5xx, or a real failure.
                // Re-check state: if the execution already moved on, treat
                // as peer-won and exit 0.
                let recheck = recheck_state(&client, &args).await.unwrap_or_default();
                if recheck == "completed" {
                    println!(
                        "[review] /signal returned {status} but state=completed — peer reviewer won; exiting 0"
                    );
                    drain_and_stop(tail).await;
                    return Ok(());
                }
                anyhow::bail!(
                    "signal delivery failed ({status}): {text}. recheck state={recheck}"
                );
            }
            println!("[review] signal delivered: {text}");
            println!("[review] waiting for summarize to complete");
        }
        ApprovalDecision::Reject { reason } => {
            // Reject now uses the SAME HMAC-signed /signal path as approve.
            // The ApprovalDecision payload tells the summarize worker which
            // branch to take on resume (complete-from-persist vs fail-with-
            // reason). This is symmetric with --approve, so --tamper-token
            // exercises HMAC rejection on both outcomes.
            let token = if args.tamper_token {
                tamper(&fetched_waitpoint_token)
            } else {
                fetched_waitpoint_token.clone()
            };
            let signal_payload = serde_json::to_vec(&ApprovalDecision::Reject {
                reason: reason.clone(),
            })?;

            let idempotency_key = format!(
                "review-reject/{}/{}",
                args.execution_id, args.waitpoint_id
            );

            let body = serde_json::json!({
                "execution_id": args.execution_id,
                "waitpoint_id": args.waitpoint_id,
                "signal_id": uuid::Uuid::new_v4().to_string(),
                "signal_name": SIGNAL_NAME_APPROVAL,
                "signal_category": "human_review",
                "source_type": "human",
                "source_identity": "reviewer",
                "payload": signal_payload,
                "payload_encoding": "json",
                "target_scope": "execution",
                "idempotency_key": idempotency_key,
                "waitpoint_token": token,
                "now": now_ms(),
            });

            let url = format!("{}/v1/executions/{}/signal", args.server, args.execution_id);
            let send_result = client
                .post(&url)
                .json(&body)
                .timeout(Duration::from_secs(120))
                .send()
                .await;

            let (status, text) = match send_result {
                Ok(resp) => {
                    let s = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    (Some(s), t)
                }
                Err(e) => {
                    println!("[review] /signal (reject) request error: {e} — rechecking state");
                    let recheck = recheck_state(&client, &args).await?;
                    if recheck == "failed" || recheck == "cancelled" {
                        println!("[review] state={recheck} after error — reject landed; exiting 0");
                        drain_and_stop(tail).await;
                        return Ok(());
                    }
                    anyhow::bail!(
                        "reject signal delivery error: {e}. execution state={recheck}. \
                         Re-run review with the same args; idempotency_key \
                         {idempotency_key} guarantees at-most-once."
                    );
                }
            };
            let status = status.expect("matched Ok arm above");

            if args.tamper_token {
                if status.is_success() {
                    eprintln!("[review] FAIL — tampered reject token was accepted: {text}");
                    std::process::exit(1);
                }
                println!("[review] tampered reject token rejected ({status}): {text}");
                println!("[review] ── tail aborted: tamper-reject path ──");
                drain_and_stop(tail).await;
                return Ok(());
            }

            if !status.is_success() {
                // Symmetric with the approve path: re-check state before
                // bailing. A peer reviewer or FF_SKIP_APPROVAL could have
                // moved the execution to a terminal state (completed,
                // failed, cancelled) between our suspend check and our
                // /signal call — any of those terminal outcomes means the
                // review window closed without us, which is the same
                // "someone else won" success case the approve path
                // already handles.
                let recheck = recheck_state(&client, &args).await.unwrap_or_default();
                if recheck == "failed" || recheck == "cancelled" || recheck == "completed" {
                    println!(
                        "[review] /signal returned {status} but state={recheck} — peer won; exiting 0"
                    );
                    drain_and_stop(tail).await;
                    return Ok(());
                }
                anyhow::bail!(
                    "reject signal delivery failed ({status}): {text}. recheck state={recheck}"
                );
            }
            println!("[review] reject signal delivered: {text}");
            // The summarize worker re-claims, calls resume_signals(), sees
            // the Reject payload, and calls fail(). Keep the tail up for
            // a short drain so any final frames flush, then return.
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
    /// Execution is already `completed` before this reviewer saw the
    /// suspend. Two distinct causes, same action ("exit 0, peer won"):
    ///
    /// 1. A concurrent reviewer delivered the approval signal and the
    ///    worker re-claimed + completed between our state polls.
    /// 2. The summarize worker was started with `FF_SKIP_APPROVAL=1`,
    ///    which skips the suspend entirely.
    ///
    /// Both are legitimate — no need to distinguish in logs until
    /// FF_SKIP_APPROVAL is promoted from a W3-internal switch to a
    /// supported config.
    AlreadyCompleted,
}

/// Poll state until we either see `suspended` (do the review) or
/// `completed` (peer reviewer beat us). Other terminal states are a real
/// error because they imply the execution ended without human review.
///
/// Transient 5xx + transport errors retry with exponential backoff so a
/// single Valkey blip doesn't kill the reviewer. 4xx bails loud.
async fn wait_for_review_window(client: &Client, args: &Args) -> Result<ReviewOutcome> {
    const MAX_TRANSIENT: u32 = 30;
    let url = format!("{}/v1/executions/{}/state", args.server, args.execution_id);
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

async fn fetch_current_attempt_index(client: &Client, args: &Args) -> Result<u32> {
    // NOTE: no #[serde(default)] — if the server ever stops emitting the
    // field (version skew, transition window, bug) we want to fail loud
    // instead of silently tailing attempt 0 and surfacing the mismatch
    // much later as a confusing empty stream.
    #[derive(Deserialize)]
    struct ExecInfo {
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

/// Single-shot state re-check used on the /signal error/timeout paths to
/// decide whether a peer reviewer already moved the execution to a
/// terminal state. Returns the state string ("completed", "failed",
/// "cancelled", "expired", "suspended", "running", …). Swallows network
/// errors by returning "unknown" so callers get an error message they
/// can surface to the operator.
async fn recheck_state(client: &Client, args: &Args) -> Result<String> {
    let url = format!("{}/v1/executions/{}/state", args.server, args.execution_id);
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Ok(format!("unreachable(http {})", resp.status()));
    }
    let v: serde_json::Value = resp.json().await?;
    Ok(v.as_str().unwrap_or("unknown").to_owned())
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

/// Exponential backoff sleep. For `attempt = 1, 2, 3, …` this sleeps
/// approximately `min(300 * 2^(attempt-1), 5000)` ms plus a small
/// jitter. Caller tracks the attempt count.
async fn backoff_sleep(attempt: u32) {
    const BASE_MS: u64 = 300;
    const CAP_MS: u64 = 5_000;
    let shift = (attempt.saturating_sub(1)).min(6);
    let ms = (BASE_MS.saturating_mul(1u64 << shift)).min(CAP_MS);
    jittered_sleep(ms).await;
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
