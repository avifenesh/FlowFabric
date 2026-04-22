//! Bridge-event gap: `ff_resolve_dependency` child-skip must `PUBLISH`
//! to `ff:dag:completions` on the impossible-upstream branch when the
//! child is flow-bound. Without this, a child skipped due to an
//! impossible upstream only unblocks its own downstream edges via the
//! 15s `dependency_reconciler` safety net.
//!
//! Run with: cargo test -p ff-test --test resolve_dependency_child_skip_publish_dag -- --test-threads=1

use std::time::Duration;

use ferriskey::value::ProtocolVersion;
use ferriskey::{Client, ClientBuilder, PushKind, Value};
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use tokio::sync::mpsc;

const COMPLETION_CHANNEL: &str = "ff:dag:completions";
const LANE: &str = "#bridge-child-skip-lane";

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

    let _: Result<(), _> = client
        .cmd("SUBSCRIBE")
        .arg(COMPLETION_CHANNEL)
        .execute()
        .await;

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

/// Seed the minimal "runnable + blocked_by_dependencies child with one
/// pending edge" state, then call ff_resolve_dependency with a
/// non-success outcome and verify PUBLISH / no-PUBLISH.
async fn seed_and_resolve_impossible(
    tc: &TestCluster,
    downstream: &ExecutionId,
    fid: Option<&FlowId>,
) {
    let partition = execution_partition(downstream, &config());
    let ctx = ExecKeyContext::new(&partition, downstream);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let edge_id_str = uuid::Uuid::new_v4().to_string();
    let edge = ff_core::types::EdgeId::parse(&edge_id_str).unwrap();

    let now_ms = TimestampMs::now().0;

    // Seed exec_core.
    let mut core_fields: Vec<(&str, String)> = vec![
        ("execution_id", downstream.to_string()),
        ("lifecycle_phase", "runnable".to_owned()),
        ("ownership_state", "unowned".to_owned()),
        ("eligibility_state", "blocked_by_dependencies".to_owned()),
        ("blocking_reason", "waiting_for_dependencies".to_owned()),
        ("terminal_outcome", "none".to_owned()),
        ("attempt_state", "none".to_owned()),
        ("public_state", "waiting".to_owned()),
        ("priority", "0".to_owned()),
        ("created_at", now_ms.to_string()),
        ("last_mutation_at", now_ms.to_string()),
    ];
    if let Some(f) = fid {
        core_fields.push(("flow_id", f.to_string()));
    }
    let mut hset = tc.client().cmd("HSET");
    hset = hset.arg(ctx.core().as_str());
    for (k, v) in &core_fields {
        hset = hset.arg(*k).arg(v.as_str());
    }
    let _: i64 = hset.execute().await.unwrap();

    // Seed deps_meta with one unsatisfied required edge.
    let _: i64 = tc.client().cmd("HSET")
        .arg(ctx.deps_meta().as_str())
        .arg("unsatisfied_required_count").arg("1")
        .arg("impossible_required_count").arg("0")
        .execute().await.unwrap();

    // Seed deps_unresolved set.
    let _: i64 = tc.client().cmd("SADD")
        .arg(ctx.deps_unresolved().as_str())
        .arg(edge_id_str.as_str())
        .execute().await.unwrap();

    // Seed dep edge hash.
    let _: i64 = tc.client().cmd("HSET")
        .arg(ctx.dep_edge(&edge).as_str())
        .arg("state").arg("unresolved")
        .arg("required").arg("1")
        .arg("gate_kind").arg("success_only")
        .execute().await.unwrap();

    // Seed blocked_deps_zset membership so the ZREM is a no-op (still ok).
    let _: i64 = tc.client().cmd("ZADD")
        .arg(idx.lane_blocked_dependencies(&lane_id).as_str())
        .arg(now_ms.to_string().as_str())
        .arg(downstream.to_string().as_str())
        .execute().await.unwrap();

    // Call ff_resolve_dependency with upstream_outcome="failed" (any
    // non-"success" value triggers the impossible path).
    let att = AttemptIndex::new(0);
    let upstream_placeholder = tc.new_execution_id_on_partition(partition.index);
    let upstream_ctx = ExecKeyContext::new(&partition, &upstream_placeholder);
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.deps_meta(),
        ctx.deps_unresolved(),
        ctx.dep_edge(&edge),
        idx.lane_eligible(&lane_id),
        idx.lane_terminal(&lane_id),
        idx.lane_blocked_dependencies(&lane_id),
        ctx.attempt_hash(att),
        ctx.stream_meta(att),
        ctx.payload(),
        upstream_ctx.result(),
    ];
    let args: Vec<String> = vec![
        edge_id_str,
        "failed".to_owned(),
        now_ms.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_resolve_dependency", &kr, &ar)
        .await
        .expect("FCALL ff_resolve_dependency");
    assert_ok(&raw, "ff_resolve_dependency");
}

#[tokio::test]
#[serial_test::serial]
async fn resolve_dependency_child_skip_flow_bound_publishes_skipped() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    let fid = FlowId::new();
    let downstream = ExecutionId::for_flow(&fid, &config());
    seed_and_resolve_impossible(&tc, &downstream, Some(&fid)).await;

    let payload = expect_completion(&mut rx, Duration::from_secs(2)).await;
    let v: serde_json::Value = serde_json::from_str(&payload)
        .unwrap_or_else(|e| panic!("payload not JSON: {e}; raw={payload}"));
    assert_eq!(
        v.get("execution_id").and_then(|x| x.as_str()),
        Some(downstream.to_string().as_str()),
        "payload: {payload}"
    );
    assert_eq!(
        v.get("flow_id").and_then(|x| x.as_str()),
        Some(fid.to_string().as_str()),
        "payload: {payload}"
    );
    assert_eq!(
        v.get("outcome").and_then(|x| x.as_str()),
        Some("skipped"),
        "payload: {payload}"
    );
}

/// Standalone (no flow_id) child-skip must NOT publish. Standalone
/// executions by definition have no downstream edges; the gate is
/// symmetric with the other emit sites.
#[tokio::test]
#[serial_test::serial]
async fn resolve_dependency_child_skip_standalone_does_not_publish() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (_sub, mut rx) = subscribe_to_completions().await;

    let downstream = tc.new_execution_id();
    seed_and_resolve_impossible(&tc, &downstream, None).await;

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
        "standalone child-skip must NOT publish to ff:dag:completions"
    );
}
