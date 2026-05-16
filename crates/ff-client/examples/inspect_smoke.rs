//! Submit -> inspect cycle against local Valkey at localhost:6379.
//! Run with `cargo run -p ff-client --example inspect_smoke`.

use ff_client::{BackendConfig, Client, Namespace, SubmitRequest, SubmitResult};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .namespace(Namespace::new("inspect-smoke"))
        .backend(BackendConfig::valkey("localhost", 6379))
        .build()
        .await?;

    let req = SubmitRequest::new("inspect-flow", b"{}".to_vec())
        .payload_encoding("json")
        .creator_identity("inspect_smoke");
    let eid = match client.submit(req).await? {
        SubmitResult::Created { execution_id, .. } => execution_id,
        SubmitResult::Duplicate { execution_id } => execution_id,
    };
    println!("submitted: {eid:?}");

    let snap = client.inspect(&eid).await?.expect("just-submitted execution should exist");
    println!(
        "snapshot:\n  state           = {:?}\n  blocking_reason = {:?}\n  attempts        = {}\n  created_at      = {:?}",
        snap.public_state, snap.blocking_reason, snap.total_attempt_count, snap.created_at
    );

    let result = client.result(&eid).await?;
    println!("result (None expected since not complete): {result:?}");

    Ok(())
}
