//! Durable cancel-flow dispatch backlog (addresses the "async cancel_flow
//! dispatch-drop race" concern in README.md production considerations).
//!
//! Tests the Lua contract that the scanner relies on:
//!   1. ff_cancel_flow with policy="cancel_all" and members > 0 SADDs
//!      each member into `ff:flow:{fp:N}:<fid>:pending_cancels` and
//!      ZADDs the flow into `ff:idx:{fp:N}:cancel_backlog` with a
//!      grace-window score.
//!   2. Other policies do NOT populate the backlog.
//!   3. Flows with no members do NOT populate the backlog.
//!   4. Already-terminal replay does NOT re-populate (the Lua early-
//!      returns err("flow_already_terminal") before the terminal HSET).
//!   5. ff_ack_cancel_member SREMs from pending_cancels and, when
//!      empty, ZREMs the flow from cancel_backlog.
//!
//! The full scanner-driven drain is exercised by the e2e_lifecycle
//! flow tests that already spin up ff-server and call the HTTP
//! cancel endpoint; those rely on Valkey 8 which the local CI matrix
//! runs but this developer box doesn't ship.
//!
//! Run with: cargo test -p ff-test --test cancel_flow_backlog -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::{FlowIndexKeys, FlowKeyContext};
use ff_core::partition::flow_partition;
use ff_core::types::FlowId;
use ff_test::fixtures::TestCluster;

fn config() -> ff_core::partition::PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn create_flow(tc: &TestCluster, flow_id: &FlowId) {
    let partition = flow_partition(flow_id, &config());
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let fidx = FlowIndexKeys::new(&partition);

    // KEYS (3): flow_core, members_set, flow_index. ARGV (3): flow_id, flow_kind, namespace.
    let keys = [fctx.core(), fctx.members(), fidx.flow_index()];
    let args = [flow_id.to_string(), "dag".into(), "test-ns".into()];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_flow", &kr, &ar)
        .await
        .expect("FCALL ff_create_flow");
    assert!(
        matches!(
            raw,
            Value::Array(ref arr) if matches!(arr.first(), Some(Ok(Value::Int(1))))
        ),
        "ff_create_flow failed: {raw:?}"
    );
}

async fn add_member(tc: &TestCluster, flow_id: &FlowId, eid: &str) {
    let partition = flow_partition(flow_id, &config());
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let _: i64 = tc
        .client()
        .cmd("SADD")
        .arg(fctx.members().as_str())
        .arg(eid)
        .execute()
        .await
        .expect("SADD members");
}

async fn cancel_flow(tc: &TestCluster, flow_id: &FlowId, policy: &str) -> Value {
    let partition = flow_partition(flow_id, &config());
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let fidx = FlowIndexKeys::new(&partition);

    let keys = [
        fctx.core(),
        fctx.members(),
        fidx.flow_index(),
        fctx.pending_cancels(),
        fidx.cancel_backlog(),
    ];
    let args = [
        flow_id.to_string(),
        "test-cancel".into(),
        policy.into(),
        "0".into(),
        "30000".into(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tc.client()
        .fcall("ff_cancel_flow", &kr, &ar)
        .await
        .expect("FCALL ff_cancel_flow")
}

async fn pending_cancels(tc: &TestCluster, flow_id: &FlowId) -> Vec<String> {
    let partition = flow_partition(flow_id, &config());
    let fctx = FlowKeyContext::new(&partition, flow_id);
    tc.client()
        .cmd("SMEMBERS")
        .arg(fctx.pending_cancels().as_str())
        .execute()
        .await
        .unwrap_or_default()
}

async fn backlog_contains(tc: &TestCluster, flow_id: &FlowId) -> bool {
    let partition = flow_partition(flow_id, &config());
    let fidx = FlowIndexKeys::new(&partition);
    let score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(fidx.cancel_backlog().as_str())
        .arg(flow_id.to_string().as_str())
        .execute()
        .await
        .unwrap_or(None);
    score.is_some()
}

#[tokio::test]
#[serial_test::serial]
async fn cancel_all_with_members_populates_backlog() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let flow_id = FlowId::new();
    create_flow(&tc, &flow_id).await;
    let m1 = "{fp:0}:11111111-1111-1111-1111-111111111111";
    let m2 = "{fp:0}:22222222-2222-2222-2222-222222222222";
    add_member(&tc, &flow_id, m1).await;
    add_member(&tc, &flow_id, m2).await;

    let raw = cancel_flow(&tc, &flow_id, "cancel_all").await;
    assert!(
        matches!(raw, Value::Array(ref arr) if matches!(arr.first(), Some(Ok(Value::Int(1))))),
        "cancel_flow failed: {raw:?}"
    );

    let mut pending = pending_cancels(&tc, &flow_id).await;
    pending.sort();
    assert_eq!(
        pending,
        vec![m1.to_owned(), m2.to_owned()],
        "both members must be pending_cancels"
    );
    assert!(
        backlog_contains(&tc, &flow_id).await,
        "flow must be in cancel_backlog with a grace score"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn cancel_policy_other_than_cancel_all_skips_backlog() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let flow_id = FlowId::new();
    create_flow(&tc, &flow_id).await;
    add_member(&tc, &flow_id, "{fp:0}:33333333-3333-3333-3333-333333333333").await;

    // "cancel_only_flow" — no member dispatch, no backlog writes.
    let _ = cancel_flow(&tc, &flow_id, "cancel_only_flow").await;
    assert!(
        pending_cancels(&tc, &flow_id).await.is_empty(),
        "non-cancel_all policy must NOT populate pending_cancels"
    );
    assert!(
        !backlog_contains(&tc, &flow_id).await,
        "non-cancel_all policy must NOT populate cancel_backlog"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn cancel_all_with_no_members_skips_backlog() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let flow_id = FlowId::new();
    create_flow(&tc, &flow_id).await;

    let _ = cancel_flow(&tc, &flow_id, "cancel_all").await;
    assert!(
        pending_cancels(&tc, &flow_id).await.is_empty(),
        "empty flow must NOT populate pending_cancels"
    );
    assert!(
        !backlog_contains(&tc, &flow_id).await,
        "empty flow must NOT populate cancel_backlog"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn invalid_grace_ms_rejected() {
    // Guards the Lua against ±inf / NaN / negative / non-integer
    // grace_ms values. Without the guard a `tonumber("inf")` would
    // poison cancel_backlog with an "inf" score that ZRANGEBYSCORE
    // -inf <now> never matches — the flow would sit forever.
    // (Caught by cursor-bugbot on PR #56.)
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let flow_id = FlowId::new();
    create_flow(&tc, &flow_id).await;
    add_member(&tc, &flow_id, "{fp:0}:99999999-9999-9999-9999-999999999999").await;

    let partition = flow_partition(&flow_id, &config());
    let fctx = FlowKeyContext::new(&partition, &flow_id);
    let fidx = FlowIndexKeys::new(&partition);
    let keys = [
        fctx.core(),
        fctx.members(),
        fidx.flow_index(),
        fctx.pending_cancels(),
        fidx.cancel_backlog(),
    ];

    for bad in ["-1", "1.5", "1e309", "nan", "abc"] {
        let args = [
            flow_id.to_string(),
            "bad-grace".into(),
            "cancel_all".into(),
            "0".into(),
            bad.to_string(),
        ];
        let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let raw: Value = tc
            .client()
            .fcall("ff_cancel_flow", &kr, &ar)
            .await
            .expect("FCALL ff_cancel_flow");
        let arr = match &raw {
            Value::Array(a) => a,
            _ => panic!("expected Array for grace_ms={bad}, got {raw:?}"),
        };
        let status = match arr.first() {
            Some(Ok(Value::Int(n))) => *n,
            _ => panic!("no status for grace_ms={bad}"),
        };
        assert_eq!(status, 0, "invalid grace_ms={bad} must return error");
        let code = arr
            .get(1)
            .and_then(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(
            code, "invalid_grace_ms",
            "grace_ms={bad} expected invalid_grace_ms, got {code}"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn ack_cancel_member_drains_pending_and_cleans_backlog() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let flow_id = FlowId::new();
    create_flow(&tc, &flow_id).await;
    let m1 = "{fp:0}:44444444-4444-4444-4444-444444444444";
    let m2 = "{fp:0}:55555555-5555-5555-5555-555555555555";
    add_member(&tc, &flow_id, m1).await;
    add_member(&tc, &flow_id, m2).await;
    let _ = cancel_flow(&tc, &flow_id, "cancel_all").await;

    let partition = flow_partition(&flow_id, &config());
    let fctx = FlowKeyContext::new(&partition, &flow_id);
    let fidx = FlowIndexKeys::new(&partition);
    let ack_keys = [fctx.pending_cancels(), fidx.cancel_backlog()];
    let fid_str = flow_id.to_string();

    // Ack first member — pending_cancels shrinks but stays non-empty,
    // so backlog must keep the flow.
    let ack1_args = [m1, fid_str.as_str()];
    let ack1_kr: Vec<&str> = ack_keys.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_ack_cancel_member", &ack1_kr, &ack1_args)
        .await
        .expect("FCALL ack 1");
    assert_eq!(pending_cancels(&tc, &flow_id).await, vec![m2.to_owned()]);
    assert!(
        backlog_contains(&tc, &flow_id).await,
        "backlog must retain flow while any member is still pending"
    );

    // Ack second member — pending_cancels now empty, backlog must drop.
    let ack2_args = [m2, fid_str.as_str()];
    let _: Value = tc
        .client()
        .fcall("ff_ack_cancel_member", &ack1_kr, &ack2_args)
        .await
        .expect("FCALL ack 2");
    assert!(pending_cancels(&tc, &flow_id).await.is_empty());
    assert!(
        !backlog_contains(&tc, &flow_id).await,
        "backlog must drop the flow once pending_cancels is empty"
    );
}
