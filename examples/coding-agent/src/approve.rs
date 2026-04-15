mod common;

use clap::Parser;
use common::{ReviewPayload, DEFAULT_SERVER_URL};

#[derive(Parser)]
#[command(name = "approve", about = "Approve or reject a suspended coding task")]
struct Args {
    /// FlowFabric server URL.
    #[arg(long, default_value = DEFAULT_SERVER_URL)]
    server: String,

    /// Execution ID of the suspended task.
    #[arg(long)]
    execution_id: String,

    /// Waitpoint ID to deliver the signal to.
    #[arg(long)]
    waitpoint_id: String,

    /// Approve the task output.
    #[arg(long)]
    approve: bool,

    /// Reject the task output.
    #[arg(long)]
    reject: bool,

    /// Feedback message (used as rejection reason).
    #[arg(long)]
    feedback: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.approve == args.reject {
        eprintln!("Specify exactly one of --approve or --reject");
        std::process::exit(1);
    }

    let client = reqwest::Client::new();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;

    if args.approve {
        // Deliver approval signal — resumes the suspended execution
        let review = ReviewPayload {
            approved: true,
            feedback: args.feedback,
        };
        let payload_bytes: Vec<u8> = serde_json::to_vec(&review).expect("serialize review");

        let signal_id = uuid::Uuid::new_v4().to_string();
        let body = serde_json::json!({
            "execution_id": args.execution_id,
            "waitpoint_id": args.waitpoint_id,
            "signal_id": signal_id,
            "signal_name": "review_response",
            "signal_category": "human_review",
            "source_type": "human",
            "source_identity": "operator",
            "payload": payload_bytes,
            "payload_encoding": "json",
            "target_scope": "execution",
            "now": now_ms
        });

        let url = format!(
            "{}/v1/executions/{}/signal",
            args.server, args.execution_id
        );
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .expect("signal request failed");

        let status = resp.status();
        let text = resp.text().await.expect("read body");

        if !status.is_success() {
            eprintln!("Signal error ({}): {}", status, text);
            std::process::exit(1);
        }

        println!("Approved. Signal delivered: {signal_id}");
        println!("Response: {text}");
    } else {
        // Reject — cancel directly. ff_cancel_execution handles suspended
        // executions (closes waitpoint, transitions to terminal cancelled).
        // No signal needed — avoids race where signal resumes before cancel arrives.
        let reason = args
            .feedback
            .unwrap_or_else(|| "rejected by reviewer".to_owned());

        let cancel_body = serde_json::json!({
            "execution_id": args.execution_id,
            "reason": reason,
            "source": "operator_override",
            "now": now_ms
        });

        let cancel_url = format!(
            "{}/v1/executions/{}/cancel",
            args.server, args.execution_id
        );
        let resp = client
            .post(&cancel_url)
            .json(&cancel_body)
            .send()
            .await
            .expect("cancel request failed");

        let status = resp.status();
        let text = resp.text().await.expect("read body");

        if status.is_success() {
            println!("Rejected. Execution cancelled.");
        } else {
            eprintln!("Cancel error ({}): {}", status, text);
            std::process::exit(1);
        }
    }
}
