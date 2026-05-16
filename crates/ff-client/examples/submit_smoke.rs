//! Quick smoke test for `Client::submit` against a local Valkey on
//! `localhost:6379`. Not a CI test — exists so the
//! foundation can be validated end-to-end without spinning up
//! ff-server. Run with `cargo run -p ff-client --example submit_smoke`.

use ff_client::{BackendConfig, Client, Namespace, SubmitRequest, SubmitResult};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .namespace(Namespace::new("smoke-tenant"))
        .backend(BackendConfig::valkey("localhost", 6379))
        .build()
        .await?;

    let req = SubmitRequest::new("smoke-workflow", b"{\"hello\":\"world\"}".to_vec())
        .payload_encoding("json")
        .creator_identity("submit_smoke")
        .idempotency_key("smoke-key-1");

    match client.submit(req.clone()).await? {
        SubmitResult::Created { execution_id, .. } => {
            println!("created: {execution_id:?}");
        }
        SubmitResult::Duplicate { execution_id } => {
            println!("duplicate (first time): {execution_id:?}");
        }
    }

    // Re-submit with same idempotency key — must come back Duplicate.
    match client.submit(req).await? {
        SubmitResult::Created { execution_id, .. } => {
            println!("UNEXPECTED Created on retry: {execution_id:?}");
        }
        SubmitResult::Duplicate { execution_id } => {
            println!("duplicate on retry (as expected): {execution_id:?}");
        }
    }

    Ok(())
}
