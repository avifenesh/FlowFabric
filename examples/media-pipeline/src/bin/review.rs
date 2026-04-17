//! Review CLI — tail the summarize execution, deliver an HMAC-signed
//! approval signal when it suspends.
//!
//! Demonstrates two Batch B primitives:
//!   * `GET /v1/executions/{id}/pending-waitpoints` to fetch the token.
//!   * `POST /v1/executions/{id}/signal` with the HMAC-bound token. The
//!     `--tamper-token` flag mutates the token before send so the user
//!     can watch the server reject it with `invalid_token`.

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

    // Tail summary tokens while polling state. Both run concurrently so
    // the reviewer sees the incremental summary before the prompt fires.
    let tail_eid = args.execution_id.clone();
    let tail_client = client.clone();
    let tail_server = args.server.clone();
    let tail = tokio::spawn(async move {
        let mut after = "0-0".to_owned();
        loop {
            let url = format!(
                "{tail_server}/v1/executions/{tail_eid}/attempts/0/stream/tail?after={after}&block_ms=3000&limit=100"
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

    // Poll for suspend.
    wait_for_state(&client, &args, "suspended").await?;
    println!("[review] execution suspended — fetching waitpoint token");

    let waitpoints = fetch_pending_waitpoints(&client, &args).await?;
    let wp = waitpoints
        .into_iter()
        .find(|w| w.state == "active" || w.state == "pending")
        .context("no active waitpoint after suspended state")?;

    let token = if args.tamper_token {
        tamper(&wp.waitpoint_token)
    } else {
        wp.waitpoint_token.clone()
    };

    let decision = prompt_decision(&args)?;
    let signal_payload = serde_json::to_vec(&decision)?;

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
        // Expected path — stop tailing since no resume will happen.
        tail.abort();
        return Ok(());
    }

    if !status.is_success() {
        anyhow::bail!("signal delivery failed ({status}): {text}");
    }
    println!("[review] signal delivered: {text}");

    match decision {
        ApprovalDecision::Approve => {
            println!("[review] waiting for summarize to complete");
        }
        ApprovalDecision::Reject { .. } => {
            println!("[review] rejected — summarize will not resume");
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

async fn wait_for_state(client: &Client, args: &Args, want: &str) -> Result<()> {
    let url = format!("{}/v1/executions/{}/state", args.server, args.execution_id);
    loop {
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("state poll failed: {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        let s = v.as_str().unwrap_or("unknown");
        if s == want {
            return Ok(());
        }
        if matches!(s, "completed" | "failed" | "cancelled" | "expired") {
            anyhow::bail!("execution ended in terminal state '{s}' before reaching '{want}'");
        }
        tokio::time::sleep(Duration::from_millis(args.poll_ms)).await;
    }
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

/// Flip the last hex char of the token so the HMAC fails validation.
fn tamper(token: &str) -> String {
    let mut chars: Vec<char> = token.chars().collect();
    if let Some(last) = chars.last_mut() {
        *last = match *last {
            'a' => 'b',
            'A' => 'B',
            '0' => '1',
            _ => '0',
        };
    }
    chars.into_iter().collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
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
