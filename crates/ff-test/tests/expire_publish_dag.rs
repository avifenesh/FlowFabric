//! Issue #44: `ff_expire_execution` must `PUBLISH` to `ff:dag:completions`
//! on every flow-bound terminal transition, matching the Batch C pattern
//! used by `ff_complete_execution` / `ff_fail_execution` /
//! `ff_cancel_execution`. Without this, a never-claimed flow-bound
//! execution that hits its `execution_deadline` only unblocks children
//! via the 15s `dependency_reconciler` safety net.
//!
//! Run with: cargo test -p ff-test --test expire_publish_dag -- --test-threads=1

use std::time::Duration;

use ferriskey::value::ProtocolVersion;
use ferriskey::{Client, ClientBuilder, PushKind, Value};
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn assert_ok(raw: &Value, label: &str) {
    match raw {
        Value::Array(arr) => {
            let first = arr.first().and_then(|r| r.as_ref().ok());
            assert!(
                matches!(first, Some(Value::Int(1))),
                "{label} expected ok tuple, got {raw:?}"
            );
        }
        other => panic!("{label} expected Array, got {other:?}"),
    }
}
use tokio::sync::mpsc;

const COMPLETION_CHANNEL: &str = "ff:dag:completions";
const LANE: &str = "#44-lane";
const NS: &str = "#44-ns";

fn config() -> ff_core::partition::PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

/// Build a dedicated RESP3 client subscribed to COMPLETION_CHANNEL.
/// Returns the client (so the test owns its lifetime — dropping closes
/// the subscription) and the push receiver. Mirrors the engine's
/// CompletionListener construction, including the FF_TLS / FF_CLUSTER
/// env flags so this test behaves identically in CI's cluster mode.
async fn subscribe_to_completions()
-> (Client, mpsc::UnboundedReceiver<ferriskey::PushInfo>) {
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".to_owned());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);
    let tls = ff_test::fixtures::env_flag("FF_TLS");
    let cluster = ff_test::fixtures::env_flag("FF_CLUSTER");

    let (push_tx, mut push_rx) = mpsc::unbounded_channel();
    let mut builder = ClientBuilder::new()
        .protocol(ProtocolVersion::RESP3)
        .push_sender(push_tx)
        .host(&host, port);
    if tls {
        builder = builder.tls();
    }
    if cluster {
        builder = builder.cluster();
    }
    let client = builder.build().await.expect("build RESP3 listener client");

    let sub_result: Result<(), _> = client
        .cmd("SUBSCRIBE")
        .arg(COMPLETION_CHANNEL)
        .execute()
        .await;
    sub_result.expect("SUBSCRIBE ff:dag:completions");

    // Wait for the server's Subscribe confirmation push frame so the
    // subsequent PUBLISH can't race the subscription. Without this,
    // the "no publish" assertion in expire_standalone_does_not_publish
    // can pass vacuously if the subscription never actually registered.
    let subscribed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let push = push_rx
                .recv()
                .await
                .expect("push channel closed before subscribe confirmation");
            if push.kind == PushKind::Subscribe {
                return;
            }
        }
    })
    .await;
    subscribed.expect("SUBSCRIBE confirmation push did not arrive within 2s");

    (client, push_rx)
}

/// Await one `Message` push frame from COMPLETION_CHANNEL. Panics on
/// timeout so a silent PUBLISH regression fails loudly.
async fn expect_completion(
    rx: &mut mpsc::UnboundedReceiver<ferriskey::PushInfo>,
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("timed out waiting for PUBLISH on {COMPLETION_CHANNEL}");
        }
        let push = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for PUBLISH on {COMPLETION_CHANNEL}"))
            .expect("push channel closed unexpectedly");
        if push.kind != PushKind::Message {
            continue; // SUBSCRIBE confirmation etc.
        }
        // RESP3 Message: [channel, payload]
        let payload = match push.data.get(1) {
            Some(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
            Some(Value::SimpleString(s)) => s.clone(),
            other => panic!("unexpected Message payload shape: {other:?}"),
        };
        return payload;
    }
}

/// Expire a runnable (never-claimed) flow-bound execution and assert
/// the completion PUBLISH carries outcome="expired" and the right
/// execution_id + flow_id. Covers the main gap from #44 — without the
/// fix, the subscriber never sees a message on this code path.
#[tokio::test]
#[serial_test::serial]
async fn expire_runnable_flow_bound_publishes_to_dag_completions() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    // Build a flow-bound execution. The test drives ff_expire_execution
    // directly, so the exact `execution_deadline` value isn't load-bearing —
    // it just has to be present so ff_create_execution populates the index.
    // Minting via `ExecutionId::for_flow` pins the exec to the flow's
    // partition (RFC-011) so the exec_core HSET of flow_id below is
    // sufficient to make the terminal HSET path trigger the PUBLISH branch.
    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &config());
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let now_ms = TimestampMs::now().0;
    let deadline_at = (now_ms + 1).to_string();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&lane_id),
        ctx.noop(), // idem_key placeholder
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        LANE.to_owned(),
        "issue_44_test".to_owned(),
        "0".to_owned(),
        "e2e-test".to_owned(),
        "{}".to_owned(),
        r#"{"test":"issue_44"}"#.to_owned(),
        String::new(), // delay_until
        String::new(), // dedup_ttl_ms
        "{}".to_owned(),
        deadline_at,
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution");
    assert_ok(&raw, "ff_create_execution");

    // Stamp flow_id on exec_core: ff_create_execution doesn't take a
    // flow_id argument; flows populate it via ff_add_execution_to_flow.
    // Skip the full flow scaffold — the only thing the PUBLISH branch
    // cares about is `is_set(core.flow_id)`.
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core().as_str())
        .arg("flow_id")
        .arg(fid.to_string().as_str())
        .execute()
        .await
        .unwrap();

    // Expire.
    let expire_keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&WorkerInstanceId::new("")),
        idx.lane_active(&lane_id),
        idx.lane_terminal(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_suspended(&lane_id),
        idx.suspension_timeout(),
        ctx.suspension_current(),
    ];
    let expire_args: Vec<String> = vec![eid.to_string(), "execution_deadline".to_owned()];
    let ekr: Vec<&str> = expire_keys.iter().map(|s| s.as_str()).collect();
    let ear: Vec<&str> = expire_args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_expire_execution", &ekr, &ear)
        .await
        .expect("FCALL ff_expire_execution");
    assert_ok(&raw, "ff_expire_execution");

    // Await + parse the PUBLISH.
    let payload = expect_completion(&mut rx, Duration::from_secs(2)).await;
    let v: serde_json::Value = serde_json::from_str(&payload)
        .unwrap_or_else(|e| panic!("payload not JSON: {e}; raw={payload}"));
    assert_eq!(
        v.get("execution_id").and_then(|x| x.as_str()),
        Some(eid.to_string().as_str()),
        "payload: {payload}"
    );
    assert_eq!(
        v.get("flow_id").and_then(|x| x.as_str()),
        Some(fid.to_string().as_str()),
        "payload: {payload}"
    );
    assert_eq!(
        v.get("outcome").and_then(|x| x.as_str()),
        Some("expired"),
        "payload: {payload}"
    );
}

/// Standalone (no flow_id) expired executions must NOT publish. This
/// keeps the Batch C invariant: standalone execs never have downstream
/// edges, so emitting would be wasted bandwidth and a misleading signal
/// to dependency-resolution dashboards.
#[tokio::test]
#[serial_test::serial]
async fn expire_standalone_does_not_publish() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    let eid = tc.new_execution_id();
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let now_ms = TimestampMs::now().0;
    let deadline_at = (now_ms + 1).to_string();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&lane_id),
        ctx.noop(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        LANE.to_owned(),
        "issue_44_standalone".to_owned(),
        "0".to_owned(),
        "e2e-test".to_owned(),
        "{}".to_owned(),
        r#"{"test":"standalone"}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        deadline_at,
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution standalone");
    assert_ok(&raw, "ff_create_execution");

    // No HSET flow_id — standalone by construction.
    let expire_keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&WorkerInstanceId::new("")),
        idx.lane_active(&lane_id),
        idx.lane_terminal(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_suspended(&lane_id),
        idx.suspension_timeout(),
        ctx.suspension_current(),
    ];
    let expire_args: Vec<String> = vec![eid.to_string(), "execution_deadline".to_owned()];
    let ekr: Vec<&str> = expire_keys.iter().map(|s| s.as_str()).collect();
    let ear: Vec<&str> = expire_args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_expire_execution", &ekr, &ear)
        .await
        .expect("FCALL ff_expire_execution");
    assert_ok(&raw, "ff_expire_execution");

    // Wait briefly; expect NO Message frame. A short timeout keeps the
    // test cheap while still catching a regression that would produce
    // a spurious PUBLISH.
    let settled = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let push = rx.recv().await.expect("channel open");
            if push.kind == PushKind::Message {
                return push;
            }
        }
    })
    .await;
    assert!(
        settled.is_err(),
        "standalone expire must NOT publish to ff:dag:completions"
    );
}
