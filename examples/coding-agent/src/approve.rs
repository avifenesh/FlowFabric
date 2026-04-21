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

    /// HMAC-SHA1 waitpoint token in `kid:hex` form. Minted by the
    /// suspending worker and printed as `WAITPOINT_TOKEN=...`. Required
    /// when `--approve`; the engine rejects signals without it (RFC-004
    /// §Waitpoint Security).
    ///
    /// Alternative: a reviewer without access to the worker's stdout can
    /// fetch the token from
    /// `GET /v1/executions/{id}/pending-waitpoints` — the
    /// `waitpoint_token` field on the matching waitpoint is the same
    /// value.
    #[arg(long)]
    waitpoint_token: Option<String>,

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

    // Both approve and reject deliver the same `review_response` signal;
    // the `approved` field inside the ReviewPayload tells the worker which
    // branch to take. Previously, reject went through POST /cancel while
    // only approve delivered a signal — that split meant the worker could
    // not inspect rejection reason on re-claim (the cancel path never
    // re-claims). Post-#36 `resume_signals()` surfaces the payload to the
    // worker, which now does the fail-with-reason for rejections.
    let waitpoint_token = match args.waitpoint_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            eprintln!(
                "--waitpoint-token is required. The worker prints it as \
                 `WAITPOINT_TOKEN=...` on suspend, and \
                 `GET /v1/executions/{{id}}/pending-waitpoints` returns it on \
                 the matching waitpoint."
            );
            std::process::exit(1);
        }
    };

    let review = ReviewPayload {
        approved: args.approve,
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
        "waitpoint_token": waitpoint_token,
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

    if args.approve {
        println!("Approved. Signal delivered: {signal_id}");
    } else {
        println!("Rejected. Signal delivered: {signal_id}");
    }
    println!("Response: {text}");
}
