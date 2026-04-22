//! Bridge-event gap: `ff_expire_suspension` terminal branches
//! (fail / cancel / expire) must `PUBLISH` to `ff:dag:completions`
//! when the execution is flow-bound, matching the push-based DAG
//! promotion pattern used by `ff_complete_execution` /
//! `ff_fail_execution` / `ff_cancel_execution` /
//! `ff_expire_execution`. Without this, a flow-bound execution whose
//! suspension times out only unblocks children via the 15s
//! `dependency_reconciler` safety net.
//!
//! Run with: cargo test -p ff-test --test expire_suspension_publish_dag -- --test-threads=1

use std::time::Duration;

use ferriskey::value::ProtocolVersion;
use ferriskey::{Client, ClientBuilder, PushKind, Value};
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use tokio::sync::mpsc;

const COMPLETION_CHANNEL: &str = "ff:dag:completions";
const LANE: &str = "#bridge-suspension-lane";

fn config() -> ff_core::partition::PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

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

/// Build a dedicated RESP3 client subscribed to COMPLETION_CHANNEL.
/// Mirrors the helper in `expire_publish_dag.rs`.
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
            continue;
        }
        let payload = match push.data.get(1) {
            Some(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
            Some(Value::SimpleString(s)) => s.clone(),
            other => panic!("unexpected Message payload shape: {other:?}"),
        };
        return payload;
    }
}

/// Seed a minimal "suspended flow-bound execution with a past
/// timeout" state directly into Valkey, then call
/// `ff_expire_suspension` and verify the PUBLISH fires with the
/// expected outcome.
///
/// The Lua terminal branch only reads:
///   - core.lifecycle_phase == "suspended"
///   - suspension_current.{suspension_id, timeout_at, timeout_behavior}
///   - core.flow_id (for the PUBLISH gate)
/// so we can skip the full claim/suspend scaffold.
async fn seed_and_expire(
    tc: &TestCluster,
    eid: &ExecutionId,
    fid: Option<&FlowId>,
    timeout_behavior: &str,
) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wp_id = WaitpointId::new();

    let now_ms = TimestampMs::now().0;
    let past_ms = now_ms - 1000;
    let suspension_id = uuid::Uuid::new_v4().to_string();

    // Seed exec_core as suspended + flow-bound (if requested).
    let core_key = ctx.core();
    let mut core_fields: Vec<(&str, String)> = vec![
        ("execution_id", eid.to_string()),
        ("lifecycle_phase", "suspended".to_owned()),
        ("ownership_state", "unowned".to_owned()),
        ("eligibility_state", "not_applicable".to_owned()),
        ("blocking_reason", "waiting_for_signal".to_owned()),
        ("terminal_outcome", "none".to_owned()),
        ("attempt_state", "attempt_interrupted".to_owned()),
        ("public_state", "suspended".to_owned()),
        ("current_suspension_id", suspension_id.clone()),
        ("current_waitpoint_id", wp_id.to_string()),
        ("current_attempt_index", "0".to_owned()),
        ("priority", "0".to_owned()),
        ("created_at", now_ms.to_string()),
        ("last_mutation_at", now_ms.to_string()),
    ];
    if let Some(f) = fid {
        core_fields.push(("flow_id", f.to_string()));
    }
    let mut hset = tc.client().cmd("HSET");
    hset = hset.arg(core_key.as_str());
    for (k, v) in &core_fields {
        hset = hset.arg(*k).arg(v.as_str());
    }
    let _: i64 = hset.execute().await.unwrap();

    // Seed suspension_current.
    let susp_key = ctx.suspension_current();
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(susp_key.as_str())
        .arg("suspension_id")
        .arg(suspension_id.as_str())
        .arg("timeout_at")
        .arg(past_ms.to_string().as_str())
        .arg("timeout_behavior")
        .arg(timeout_behavior)
        .arg("reason_code")
        .arg("test")
        .execute()
        .await
        .unwrap();

    // Seed waitpoint_hash + wp_condition so the close-sub-objects
    // HSETs in the Lua succeed (HSET creates them if absent, so this
    // is belt-and-suspenders — harmless either way).
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(ctx.waitpoint(&wp_id).as_str())
        .arg("state")
        .arg("active")
        .execute()
        .await
        .unwrap();
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(ctx.waitpoint_condition(&wp_id).as_str())
        .arg("closed")
        .arg("0")
        .execute()
        .await
        .unwrap();

    // Add to suspension_timeout_zset + lane_suspended so the Lua's
    // ZREMs see the expected starting state.
    let _: i64 = tc
        .client()
        .cmd("ZADD")
        .arg(idx.suspension_timeout().as_str())
        .arg(past_ms.to_string().as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .unwrap();
    let _: i64 = tc
        .client()
        .cmd("ZADD")
        .arg(idx.lane_suspended(&lane_id).as_str())
        .arg(past_ms.to_string().as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .unwrap();

    // Call ff_expire_suspension — same KEYS layout as
    // e2e_lifecycle::fcall_expire_suspension (KEYS 12).
    let att_idx = AttemptIndex::new(0);
    let keys: Vec<String> = vec![
        ctx.core(),                         // 1
        ctx.suspension_current(),           // 2
        ctx.waitpoint(&wp_id),              // 3
        ctx.waitpoint_condition(&wp_id),    // 4
        ctx.attempt_hash(att_idx),          // 5
        ctx.stream_meta(att_idx),           // 6
        idx.suspension_timeout(),           // 7
        idx.lane_suspended(&lane_id),       // 8
        idx.lane_terminal(&lane_id),        // 9
        idx.lane_eligible(&lane_id),        // 10
        idx.lane_delayed(&lane_id),         // 11
        ctx.lease_history(),                // 12
    ];
    let args: Vec<String> = vec![eid.to_string()];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_expire_suspension", &kr, &ar)
        .await
        .expect("FCALL ff_expire_suspension");
    assert_ok(&raw, "ff_expire_suspension");
}

#[tokio::test]
#[serial_test::serial]
async fn expire_suspension_fail_flow_bound_publishes_failed() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &config());
    seed_and_expire(&tc, &eid, Some(&fid), "fail").await;

    let payload = expect_completion(&mut rx, Duration::from_secs(2)).await;
    let v: serde_json::Value = serde_json::from_str(&payload)
        .unwrap_or_else(|e| panic!("payload not JSON: {e}; raw={payload}"));
    assert_eq!(
        v.get("execution_id").and_then(|x| x.as_str()),
        Some(eid.to_string().as_str()),
    );
    assert_eq!(
        v.get("flow_id").and_then(|x| x.as_str()),
        Some(fid.to_string().as_str()),
    );
    assert_eq!(
        v.get("outcome").and_then(|x| x.as_str()),
        Some("failed"),
        "payload: {payload}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn expire_suspension_cancel_flow_bound_publishes_cancelled() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &config());
    seed_and_expire(&tc, &eid, Some(&fid), "cancel").await;

    let payload = expect_completion(&mut rx, Duration::from_secs(2)).await;
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(
        v.get("outcome").and_then(|x| x.as_str()),
        Some("cancelled"),
        "payload: {payload}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn expire_suspension_expire_flow_bound_publishes_expired() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    let fid = FlowId::new();
    let eid = ExecutionId::for_flow(&fid, &config());
    seed_and_expire(&tc, &eid, Some(&fid), "expire").await;

    let payload = expect_completion(&mut rx, Duration::from_secs(2)).await;
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    assert_eq!(
        v.get("outcome").and_then(|x| x.as_str()),
        Some("expired"),
        "payload: {payload}"
    );
}

/// Standalone (no flow_id) suspension expiry must NOT publish —
/// same invariant as `expire_publish_dag::expire_standalone_does_not_publish`.
#[tokio::test]
#[serial_test::serial]
async fn expire_suspension_standalone_does_not_publish() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    let eid = tc.new_execution_id();
    seed_and_expire(&tc, &eid, None, "fail").await;

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
        "standalone suspension-timeout must NOT publish to ff:dag:completions"
    );
}
