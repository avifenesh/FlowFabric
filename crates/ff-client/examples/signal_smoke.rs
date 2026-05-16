//! Verify `Client::deliver_signal` reaches the backend even though
//! we don't have a real suspended execution to deliver to. Expects
//! a typed engine-error rejection (missing waitpoint / invalid
//! token), which proves the args translation + wire call work.

use ff_client::{
    BackendConfig, Client, ClientError, ExecutionId, Namespace, SignalRequest, WaitpointId,
    WaitpointToken,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .namespace(Namespace::new("signal-smoke"))
        .backend(BackendConfig::valkey("localhost", 6379))
        .build()
        .await?;

    // Bogus eid / waitpoint — backend will reject.
    let req = SignalRequest::new(
        ExecutionId::parse("{fp:0}:00000000-0000-4000-8000-000000000000")?,
        WaitpointId::new(),
        WaitpointToken("definitely-not-a-real-token".to_owned()),
        "human-approval",
    )
    .source("test", "signal_smoke")
    .target_scope("execution");

    match client.deliver_signal(req).await {
        Ok(r) => {
            // Should NOT happen — we passed a token that can't validate.
            println!("UNEXPECTED Accepted: {r:?}");
            Err("expected backend rejection, got Ok".into())
        }
        Err(ClientError::Engine(e)) => {
            println!("Engine rejection (expected): {e}");
            Ok(())
        }
        Err(e) => Err(format!("unexpected error class: {e}").into()),
    }
}
