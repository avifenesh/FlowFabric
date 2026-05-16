//! submit -> cancel -> inspect cycle against local Valkey.
use ff_client::{BackendConfig, Client, Namespace, PublicState, SubmitRequest, SubmitResult};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .namespace(Namespace::new("cancel-smoke"))
        .backend(BackendConfig::valkey("localhost", 6379))
        .build()
        .await?;
    let SubmitResult::Created { execution_id, .. } = client
        .submit(SubmitRequest::new("cancel-flow", b"{}".to_vec()))
        .await?
    else { panic!("expected Created") };
    println!("submitted: {execution_id:?}");
    let res = client.cancel(&execution_id, "smoke test cancellation").await?;
    println!("cancel result: {res:?}");
    let snap = client.inspect(&execution_id).await?.unwrap();
    assert_eq!(snap.public_state, PublicState::Cancelled);
    println!("post-cancel state: {:?}", snap.public_state);
    Ok(())
}
