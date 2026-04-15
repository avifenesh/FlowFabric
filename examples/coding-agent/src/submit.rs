mod common;

use std::time::Duration;

use clap::Parser;
use common::{TaskPayload, DEFAULT_LANE, DEFAULT_NAMESPACE, DEFAULT_SERVER_URL};
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "submit", about = "Submit a coding task to FlowFabric")]
struct Args {
    /// FlowFabric server URL.
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server: String,

    /// Coding task description.
    #[arg(long)]
    issue: String,

    /// Programming language.
    #[arg(long, default_value = "rust")]
    language: String,

    /// Repository context or file contents.
    #[arg(long, default_value = "")]
    context: String,

    /// Maximum agent reasoning turns.
    #[arg(long, default_value_t = 10)]
    max_turns: u32,

    /// FlowFabric namespace.
    #[arg(long, default_value = DEFAULT_NAMESPACE)]
    namespace: String,

    /// FlowFabric lane.
    #[arg(long, default_value = DEFAULT_LANE)]
    lane: String,

    /// Execution priority (0-9000).
    #[arg(long, default_value_t = 100)]
    priority: i32,

    /// Exit immediately after submit (don't poll for state changes).
    #[arg(long)]
    no_wait: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client = reqwest::Client::new();

    let execution_id = uuid::Uuid::new_v4().to_string();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;

    let payload = TaskPayload {
        issue: args.issue,
        repo_context: args.context,
        language: args.language,
        max_turns: args.max_turns,
    };
    let payload_bytes: Vec<u8> = serde_json::to_vec(&payload).expect("serialize payload");

    let body = serde_json::json!({
        "execution_id": execution_id,
        "namespace": args.namespace,
        "lane_id": args.lane,
        "execution_kind": "coding_agent",
        "input_payload": payload_bytes,
        "payload_encoding": "json",
        "priority": args.priority,
        "creator_identity": "submit-cli",
        "idempotency_key": null,
        "tags": {},
        "policy_json": "{}",
        "delay_until": null,
        "partition_id": 0,
        "now": now_ms
    });

    let url = format!("{}/v1/executions", args.server);
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .expect("request failed");

    let status = resp.status();
    let text = resp.text().await.expect("read body");

    if !status.is_success() {
        eprintln!("Error ({}): {}", status, text);
        std::process::exit(1);
    }

    println!("Created execution: {execution_id}");
    println!("Response: {text}");

    if args.no_wait {
        return;
    }

    // Poll loop
    let state_url = format!("{}/v1/executions/{}/state", args.server, execution_id);
    let mut last_state = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;

        let resp = client.get(&state_url).send().await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Poll error: {e}");
                continue;
            }
        };

        if !resp.status().is_success() {
            eprintln!("Poll returned {}", resp.status());
            continue;
        }

        let state: String = match resp.json::<serde_json::Value>().await {
            Ok(v) => v.as_str().unwrap_or("unknown").to_owned(),
            Err(e) => {
                eprintln!("Parse error: {e}");
                continue;
            }
        };

        if state != last_state {
            println!("[state] {state}");
            last_state = state.clone();
        }

        match state.as_str() {
            "suspended" => {
                // Try to read waitpoint_id from full execution info
                let info_url =
                    format!("{}/v1/executions/{}", args.server, execution_id);
                if let Ok(resp) = client.get(&info_url).send().await {
                    if let Ok(info) = resp.json::<ExecInfo>().await {
                        let wp_hint = if info.blocking_detail.is_empty() {
                            "<WAITPOINT_ID>".to_owned()
                        } else {
                            info.blocking_detail
                        };
                        println!(
                            "Awaiting review. Use:\n  \
                             cargo run --bin approve -- \
                             --execution-id {} --waitpoint-id {} --approve",
                            execution_id, wp_hint
                        );
                    }
                }
            }
            "completed" => {
                println!("Task completed!");
                break;
            }
            "failed" | "cancelled" => {
                println!("Task ended: {state}");
                break;
            }
            _ => {}
        }
    }
}

#[derive(Deserialize)]
struct ExecInfo {
    #[serde(default)]
    blocking_detail: String,
}
