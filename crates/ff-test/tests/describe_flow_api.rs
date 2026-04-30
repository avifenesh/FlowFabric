//! Integration tests for `FlowFabricWorker::describe_flow` (issue #58.2).
//!
//! Exercises the typed snapshot read against a live Valkey: creates
//! flows via `ff_create_flow`, optionally cancels them, stamps
//! namespaced-tag fields inline on flow_core, and asserts the
//! resulting `FlowSnapshot` matches the engine's on-disk state.
//!
//! Run with:
//!   cargo test -p ff-test --test describe_flow_api -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::{FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

const LANE: &str = "desc-flow-lane";
const NS: &str = "desc-flow-ns";

async fn seed_partition_config(tc: &TestCluster) {
    let cfg = test_config();
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(cfg.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(cfg.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(cfg.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

async fn build_worker(name_suffix: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: Some(ff_test::fixtures::backend_config_from_env()),
        worker_id: WorkerId::new(format!("desc-flow-worker-{name_suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("desc-flow-inst-{name_suffix}")),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    partition_config: None,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

/// Create a flow via FCALL.
async fn create_flow(tc: &TestCluster, fid: &FlowId, flow_kind: &str) {
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![ctx.core(), ctx.members(), fidx.flow_index()];
    let now_ms = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        fid.to_string(),
        flow_kind.to_owned(),
        NS.to_owned(),
        now_ms,
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let _: Value = tc
        .client()
        .fcall("ff_create_flow", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_flow failed");
}

/// Cancel a flow via FCALL.
async fn cancel_flow(
    tc: &TestCluster,
    fid: &FlowId,
    reason: &str,
    cancellation_policy: &str,
) {
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.members(),
        fidx.flow_index(),
        ctx.pending_cancels(),
        fidx.cancel_backlog(),
    ];
    let now_ms = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        fid.to_string(),
        reason.to_owned(),
        cancellation_policy.to_owned(),
        now_ms,
        String::new(), // grace_ms — default
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let _: Value = tc
        .client()
        .fcall("ff_cancel_flow", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_cancel_flow failed");
}

// ─── Tests ───

#[tokio::test]
async fn describe_missing_flow_returns_none() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("missing").await;

    // Fresh flow id — never written to Valkey.
    let fid = FlowId::new();
    let snap = worker
        .describe_flow(&fid)
        .await
        .expect("describe_flow should not error on missing");
    assert!(snap.is_none(), "expected Ok(None) for missing flow");
}

#[tokio::test]
async fn describe_freshly_created_flow_is_open() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("open").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid, "dag").await;

    let snap = worker
        .describe_flow(&fid)
        .await
        .expect("describe_flow should succeed")
        .expect("snapshot should be Some for created flow");

    assert_eq!(snap.flow_id, fid);
    assert_eq!(snap.flow_kind, "dag");
    assert_eq!(snap.namespace.as_str(), NS);
    assert_eq!(snap.public_flow_state, "open");
    assert_eq!(snap.graph_revision, 0);
    assert_eq!(snap.node_count, 0);
    assert_eq!(snap.edge_count, 0);
    assert!(snap.created_at.0 > 0);
    assert_eq!(
        snap.last_mutation_at, snap.created_at,
        "on create, last_mutation_at == created_at"
    );
    assert!(snap.cancelled_at.is_none());
    assert!(snap.cancel_reason.is_none());
    assert!(snap.cancellation_policy.is_none());
    assert!(snap.tags.is_empty());
}

#[tokio::test]
async fn describe_cancelled_flow_surfaces_cancel_fields() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("cancelled").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid, "pipeline").await;
    cancel_flow(&tc, &fid, "operator_requested", "cancel_all").await;

    let snap = worker
        .describe_flow(&fid)
        .await
        .expect("describe_flow succeeds")
        .expect("snapshot present");

    assert_eq!(snap.public_flow_state, "cancelled");
    assert!(
        snap.cancelled_at.is_some(),
        "cancelled_at populated after cancel_flow"
    );
    assert_eq!(snap.cancel_reason.as_deref(), Some("operator_requested"));
    assert_eq!(snap.cancellation_policy.as_deref(), Some("cancel_all"));
    assert!(
        snap.last_mutation_at.0 >= snap.created_at.0,
        "cancel bumps last_mutation_at forward"
    );
}

#[tokio::test]
async fn describe_flow_namespaced_tags_round_trip() {
    // Flow tags live inline on flow_core — stamp cairn.* fields
    // directly and confirm they land in the snapshot's tags map.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("tags").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid, "dag").await;

    let config = test_config();
    let partition = flow_partition(&fid, &config);
    let ctx = FlowKeyContext::new(&partition, &fid);
    let _: Value = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core())
        .arg("cairn.task_id")
        .arg("t-42")
        .arg("cairn.project")
        .arg("proj-a")
        .execute()
        .await
        .unwrap();

    let snap = worker
        .describe_flow(&fid)
        .await
        .expect("describe_flow succeeds")
        .expect("snapshot present");

    assert_eq!(snap.tags.len(), 2);
    assert_eq!(snap.tags.get("cairn.task_id").map(String::as_str), Some("t-42"));
    assert_eq!(snap.tags.get("cairn.project").map(String::as_str), Some("proj-a"));
    // BTreeMap sort order keeps enumeration deterministic.
    let ordered_keys: Vec<&str> = snap.tags.keys().map(String::as_str).collect();
    assert_eq!(ordered_keys, vec!["cairn.project", "cairn.task_id"]);
}

#[tokio::test]
async fn describe_flow_corrupt_state_surfaces_error() {
    // Defensive: an unknown non-namespaced flow_core field must fail
    // loud rather than be silently routed to tags. Pins the strict-
    // parse posture so a future "silently bucket unknowns" regression
    // is caught in CI.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("corrupt").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid, "dag").await;

    let config = test_config();
    let partition = flow_partition(&fid, &config);
    let ctx = FlowKeyContext::new(&partition, &fid);
    let _: Value = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core())
        .arg("bogus_future_field")
        .arg("v")
        .execute()
        .await
        .unwrap();

    let err = worker
        .describe_flow(&fid)
        .await
        .expect_err("unknown flat flow_core field must surface as error");
    match err {
        // RFC-012 Stage 1c T3: decoder returns
        // `EngineError::Validation { kind: Corruption, .. }` now.
        ff_sdk::SdkError::Engine(ref boxed) => match boxed.as_ref() {
            ff_core::engine_error::EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::Corruption,
                detail,
            } => {
                assert!(
                    detail.contains("bogus_future_field"),
                    "error detail must name the field: {detail}"
                );
            }
            other => panic!("expected EngineError::Validation::Corruption, got {other:?}"),
        },
        other => panic!("expected SdkError::Engine(Validation::Corruption), got {other:?}"),
    }
}
