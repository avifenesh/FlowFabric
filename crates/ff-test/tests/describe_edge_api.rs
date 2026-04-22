//! Integration tests for `FlowFabricWorker::describe_edge`,
//! `list_incoming_edges`, and `list_outgoing_edges` (issue #58.3).
//!
//! Exercises the typed edge snapshot reads against a live Valkey:
//! stages edges via `ff_stage_dependency_edge` (after scaffolding
//! flow + members) and asserts the resulting `EdgeSnapshot` matches
//! the engine's on-disk edge hash + adjacency sets.
//!
//! Run with:
//!   cargo test -p ff-test --test describe_edge_api -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, flow_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

const LANE: &str = "desc-edge-lane";
const NS: &str = "desc-edge-ns";

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
        backend: ff_test::fixtures::backend_config_from_env(),
        worker_id: WorkerId::new(format!("desc-edge-worker-{name_suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("desc-edge-inst-{name_suffix}")),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

/// Create a flow via FCALL.
async fn create_flow(tc: &TestCluster, fid: &FlowId) {
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![ctx.core(), ctx.members(), fidx.flow_index()];
    let now_ms = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        fid.to_string(),
        "dag".to_owned(),
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

/// Create a bare execution and attach it to a flow. Returns the fresh
/// `ExecutionId`, co-located with the flow's partition.
async fn create_and_add_member(tc: &TestCluster, fid: &FlowId) -> ExecutionId {
    let config = test_config();
    let eid = ExecutionId::for_flow(fid, &config);
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    // 1. ff_create_execution (bare-minimum, no flow_id stamp yet).
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
        "edge_test_kind".to_owned(),
        "0".to_owned(),
        "desc-edge-test".to_owned(),
        "{}".to_owned(),
        "{}".to_owned(),
        String::new(), // delay_until
        String::new(), // dedup_ttl_ms
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution");

    // 2. ff_add_execution_to_flow — stamps flow_id + adds to members_set.
    let fctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);
    let keys: Vec<String> = vec![
        fctx.core(),
        fctx.members(),
        fidx.flow_index(),
        ctx.core(),
    ];
    let args: Vec<String> = vec![
        fid.to_string(),
        eid.to_string(),
        TimestampMs::now().0.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_add_execution_to_flow", &kr, &ar)
        .await
        .expect("FCALL ff_add_execution_to_flow");

    eid
}

/// Stage a dependency edge. Returns the fresh `EdgeId`.
async fn stage_edge(
    tc: &TestCluster,
    fid: &FlowId,
    up: &ExecutionId,
    down: &ExecutionId,
    expected_graph_revision: u64,
    data_passing_ref: &str,
) -> EdgeId {
    let edge_id = EdgeId::new();
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let fctx = FlowKeyContext::new(&partition, fid);

    let keys: Vec<String> = vec![
        fctx.core(),
        fctx.members(),
        fctx.edge(&edge_id),
        fctx.outgoing(up),
        fctx.incoming(down),
        fctx.grant(&edge_id.to_string()),
    ];
    let args: Vec<String> = vec![
        fid.to_string(),
        edge_id.to_string(),
        up.to_string(),
        down.to_string(),
        "success_only".to_owned(),
        data_passing_ref.to_owned(),
        expected_graph_revision.to_string(),
        TimestampMs::now().0.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_stage_dependency_edge", &kr, &ar)
        .await
        .expect("FCALL ff_stage_dependency_edge");
    edge_id
}

// ─── Tests ───

#[tokio::test]
async fn describe_missing_edge_returns_none() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("missing").await;

    let fid = FlowId::new();
    let edge = EdgeId::new();
    let snap = worker
        .describe_edge(&fid, &edge)
        .await
        .expect("describe_edge should not error on missing");
    assert!(snap.is_none(), "expected Ok(None) for missing edge");
}

#[tokio::test]
async fn describe_freshly_staged_edge_round_trips() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("staged").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let down = create_and_add_member(&tc, &fid).await;
    // After two add_member calls: graph_revision bumped twice from 0 → 2.
    let edge = stage_edge(&tc, &fid, &up, &down, 2, "").await;

    let snap = worker
        .describe_edge(&fid, &edge)
        .await
        .expect("describe_edge succeeds")
        .expect("snapshot present for staged edge");

    assert_eq!(snap.edge_id, edge);
    assert_eq!(snap.flow_id, fid);
    assert_eq!(snap.upstream_execution_id, up);
    assert_eq!(snap.downstream_execution_id, down);
    assert_eq!(snap.dependency_kind, "success_only");
    assert_eq!(snap.satisfaction_condition, "all_required");
    assert!(snap.data_passing_ref.is_none());
    assert_eq!(snap.edge_state, "pending");
    assert!(snap.created_at.0 > 0);
    assert_eq!(snap.created_by, "engine");
}

#[tokio::test]
async fn describe_edge_preserves_data_passing_ref() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("dpref").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let down = create_and_add_member(&tc, &fid).await;
    let edge = stage_edge(&tc, &fid, &up, &down, 2, "artifact://hash-abc").await;

    let snap = worker
        .describe_edge(&fid, &edge)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snap.data_passing_ref.as_deref(), Some("artifact://hash-abc"));
}

#[tokio::test]
async fn list_edges_empty_for_isolated_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("isolated").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let eid = create_and_add_member(&tc, &fid).await;

    let out = worker.list_outgoing_edges(&eid).await.unwrap();
    let incoming = worker.list_incoming_edges(&eid).await.unwrap();
    assert!(out.is_empty(), "isolated exec has no outgoing edges");
    assert!(incoming.is_empty(), "isolated exec has no incoming edges");
}

#[tokio::test]
async fn list_edges_empty_for_standalone_execution() {
    // An ExecutionId whose exec_core has no flow_id (standalone) must
    // return an empty Vec — the adjacency set lives under a flow, and
    // a standalone exec has none.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("standalone").await;

    // Use ::solo to keep it legibly standalone.
    let config = test_config();
    let eid = ExecutionId::solo(&LaneId::new(LANE), &config);
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
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
        "edge_test_kind".to_owned(),
        "0".to_owned(),
        "desc-edge-test".to_owned(),
        "{}".to_owned(),
        "{}".to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution");

    let out = worker.list_outgoing_edges(&eid).await.unwrap();
    let incoming = worker.list_incoming_edges(&eid).await.unwrap();
    assert!(out.is_empty());
    assert!(incoming.is_empty());
}

#[tokio::test]
async fn list_edges_returns_all_adjacent_edges() {
    // Fan-out pattern: up → mid, up → other. list_outgoing_edges(up)
    // should return both. list_incoming_edges(mid) should return one.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("fanout").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let mid = create_and_add_member(&tc, &fid).await;
    let other = create_and_add_member(&tc, &fid).await;
    // 3 members → graph_revision == 3.
    let e1 = stage_edge(&tc, &fid, &up, &mid, 3, "").await;
    // After first edge, revision → 4.
    let e2 = stage_edge(&tc, &fid, &up, &other, 4, "").await;

    let out = worker.list_outgoing_edges(&up).await.unwrap();
    assert_eq!(out.len(), 2, "upstream has 2 outgoing edges");
    // Sort deterministically for comparison (SMEMBERS order is unspecified;
    // EdgeId has no Ord derive so compare via string form).
    let mut got_ids: Vec<String> = out.iter().map(|s| s.edge_id.to_string()).collect();
    got_ids.sort();
    let mut expected: Vec<String> = vec![e1.to_string(), e2.to_string()];
    expected.sort();
    assert_eq!(got_ids, expected);
    // Both snapshots must point back to the same upstream.
    for snap in &out {
        assert_eq!(snap.upstream_execution_id, up);
        assert_eq!(snap.flow_id, fid);
        assert_eq!(snap.edge_state, "pending");
    }

    let incoming = worker.list_incoming_edges(&mid).await.unwrap();
    assert_eq!(incoming.len(), 1, "mid has exactly one incoming edge");
    assert_eq!(incoming[0].edge_id, e1);
    assert_eq!(incoming[0].upstream_execution_id, up);
    assert_eq!(incoming[0].downstream_execution_id, mid);

    let other_incoming = worker.list_incoming_edges(&other).await.unwrap();
    assert_eq!(other_incoming.len(), 1);
    assert_eq!(other_incoming[0].edge_id, e2);
}

#[tokio::test]
async fn list_edges_detects_adjacency_endpoint_drift() {
    // Stamp a stale edge_id into an adjacency SET for an execution
    // that is NOT actually the edge's endpoint. The fix for the
    // Copilot review-comment catches this as corruption: the SET
    // claims the edge is outgoing from `innocent`, but the edge
    // hash's stored upstream is `up`. Must surface as Config.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("drift").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let down = create_and_add_member(&tc, &fid).await;
    let innocent = create_and_add_member(&tc, &fid).await;
    let edge = stage_edge(&tc, &fid, &up, &down, 3, "").await;

    // Directly SADD the edge_id into innocent's outgoing set.
    let config = test_config();
    let partition = flow_partition(&fid, &config);
    let fctx = FlowKeyContext::new(&partition, &fid);
    let _: i64 = tc
        .client()
        .cmd("SADD")
        .arg(fctx.outgoing(&innocent))
        .arg(edge.to_string())
        .execute()
        .await
        .unwrap();

    let err = worker
        .list_outgoing_edges(&innocent)
        .await
        .expect_err("endpoint drift must surface");
    match err {
        ff_sdk::SdkError::Config { message: msg, .. } => {
            assert!(msg.contains("adjacency"), "msg: {msg}");
            assert!(
                msg.contains("stored endpoint"),
                "msg should name drift: {msg}"
            );
        }
        other => panic!("expected Config, got {other:?}"),
    }
}

#[tokio::test]
async fn describe_edge_corrupt_state_surfaces_error() {
    // A future FF field rename or on-disk drift lands a non-FF-owned
    // unknown key. Must fail loud, matching the strict-parse posture.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("corrupt").await;

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let down = create_and_add_member(&tc, &fid).await;
    let edge = stage_edge(&tc, &fid, &up, &down, 2, "").await;

    let config = test_config();
    let partition = flow_partition(&fid, &config);
    let ctx = FlowKeyContext::new(&partition, &fid);
    let _: Value = tc
        .client()
        .cmd("HSET")
        .arg(ctx.edge(&edge))
        .arg("bogus_future_field")
        .arg("v")
        .execute()
        .await
        .unwrap();

    let err = worker
        .describe_edge(&fid, &edge)
        .await
        .expect_err("unknown edge_hash field must surface as error");
    match err {
        ff_sdk::SdkError::Config { message: msg, .. } => {
            assert!(msg.contains("bogus_future_field"), "msg: {msg}");
        }
        other => panic!("expected SdkError::Config, got {other:?}"),
    }
}
