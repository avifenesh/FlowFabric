//! Issue #90: `CompletionBackend::subscribe_completions` integration
//! test. Covers the happy path (subscribe + receive payload) and
//! reconnect-friendly fanout (two independent streams off the same
//! backend, both receive a published completion).
//!
//! Run with: `cargo test -p ff-test --test completion_backend -- --test-threads=1`

use std::time::Duration;

use ferriskey::ClientBuilder;
use ff_backend_valkey::{ValkeyBackend, COMPLETION_CHANNEL};
use ff_core::backend::ValkeyConnection;
use ff_core::completion_backend::CompletionBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::FlowId;
use futures::StreamExt;

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).ok().as_deref(), Some("1" | "true"))
}

fn host() -> String {
    std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn port() -> u16 {
    std::env::var("FF_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379)
}

async fn build_publisher() -> ferriskey::Client {
    let mut builder = ClientBuilder::new().host(&host(), port());
    if env_flag("FF_TLS") {
        builder = builder.tls();
    }
    if env_flag("FF_CLUSTER") {
        builder = builder.cluster();
    }
    builder.build().await.expect("publisher client build")
}

async fn backend() -> std::sync::Arc<ValkeyBackend> {
    // Dummy client — subscribe_completions() opens its own dedicated
    // RESP3 subscriber. The stored `client` field is only used by
    // EngineBackend methods we don't exercise here.
    let client = build_publisher().await;
    let mut conn = ValkeyConnection::new(host(), port());
    conn.tls = env_flag("FF_TLS");
    conn.cluster = env_flag("FF_CLUSTER");
    ValkeyBackend::from_client_partitions_and_connection(client, PartitionConfig::default(), conn)
}

async fn publish(client: &ferriskey::Client, eid: &str, flow_id: &str) {
    let payload = format!(
        r#"{{"execution_id":"{eid}","flow_id":"{flow_id}","outcome":"success"}}"#
    );
    let _: i64 = client
        .cmd("PUBLISH")
        .arg(COMPLETION_CHANNEL)
        .arg(&payload)
        .execute()
        .await
        .expect("PUBLISH");
}

/// Happy path: `subscribe_completions` yields a `CompletionPayload`
/// with `execution_id` + `flow_id` populated when the Lua emitters'
/// wire format is published on the channel.
#[tokio::test]
async fn subscribe_yields_published_completion() {
    let backend = backend().await;
    let mut stream = backend
        .subscribe_completions()
        .await
        .expect("subscribe_completions");

    // Give the subscriber task time to SUBSCRIBE + receive the
    // server's confirmation push before we PUBLISH.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let flow_id = FlowId::new();
    let eid = "{fp:7}:11111111-1111-1111-1111-111111111111".to_owned();
    let pub_client = build_publisher().await;
    publish(&pub_client, &eid, &flow_id.to_string()).await;

    let received = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("timed out waiting for payload")
        .expect("stream ended unexpectedly");

    assert_eq!(received.execution_id.to_string(), eid);
    assert_eq!(received.flow_id.as_ref().unwrap(), &flow_id);
    assert_eq!(received.outcome, "success");
}

/// Fanout: two independent `subscribe_completions()` calls each open
/// their own subscriber. A single PUBLISH should reach both streams.
/// Proves the per-call-mpsc policy: neither stream steals from the
/// other.
#[tokio::test]
async fn two_streams_both_receive() {
    let backend = backend().await;
    let mut a = backend
        .subscribe_completions()
        .await
        .expect("subscribe_completions a");
    let mut b = backend
        .subscribe_completions()
        .await
        .expect("subscribe_completions b");

    tokio::time::sleep(Duration::from_millis(300)).await;

    let flow_id = FlowId::new();
    let eid = "{fp:3}:22222222-2222-2222-2222-222222222222".to_owned();
    let pub_client = build_publisher().await;
    publish(&pub_client, &eid, &flow_id.to_string()).await;

    let got_a = tokio::time::timeout(Duration::from_secs(3), a.next())
        .await
        .expect("a timed out")
        .expect("a ended");
    let got_b = tokio::time::timeout(Duration::from_secs(3), b.next())
        .await
        .expect("b timed out")
        .expect("b ended");

    assert_eq!(got_a.execution_id.to_string(), eid);
    assert_eq!(got_b.execution_id.to_string(), eid);
}

/// Dropping the stream cleanly shuts down the subscriber task.
/// Verified indirectly: drop the stream, wait, and confirm a NEW
/// subscribe still works (the backend isn't wedged).
#[tokio::test]
async fn drop_then_resubscribe_works() {
    let backend = backend().await;

    {
        let _stream = backend
            .subscribe_completions()
            .await
            .expect("first subscribe");
        tokio::time::sleep(Duration::from_millis(200)).await;
        // _stream dropped here → subscriber task exits cleanly.
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut stream = backend
        .subscribe_completions()
        .await
        .expect("second subscribe");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let flow_id = FlowId::new();
    let eid = "{fp:1}:33333333-3333-3333-3333-333333333333".to_owned();
    let pub_client = build_publisher().await;
    publish(&pub_client, &eid, &flow_id.to_string()).await;

    let received = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("timed out")
        .expect("stream ended");
    assert_eq!(received.execution_id.to_string(), eid);
}
