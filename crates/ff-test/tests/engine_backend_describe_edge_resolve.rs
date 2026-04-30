//! Issue #160: integration coverage for the two `EngineBackend` trait
//! methods introduced to let `ff-sdk::snapshot` retire its raw-HGET
//! helpers:
//!
//! - [`ff_core::engine_backend::EngineBackend::describe_edge`]
//! - [`ff_core::engine_backend::EngineBackend::resolve_execution_flow_id`]
//!
//! Mirrors the precedent set by `engine_backend_describe.rs` and
//! `engine_backend_list_edges.rs`: stage a flow + execution + edge
//! via FCALL, read both through the trait object and through the
//! ff-sdk wrapper, assert the two agree.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_describe_edge_resolve \
//!       -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{PartitionConfig, execution_partition, flow_partition};
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

async fn build_worker(suffix: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: Some(ff_test::fixtures::backend_config_from_env()),
        worker_id: WorkerId::new(format!("desc-edge-worker-{suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("desc-edge-inst-{suffix}")),
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

fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config())
}

async fn create_flow(tc: &TestCluster, fid: &FlowId) {
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);
    let keys: Vec<String> = vec![ctx.core(), ctx.members(), fidx.flow_index()];
    let now_ms = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![fid.to_string(), "dag".to_owned(), NS.to_owned(), now_ms];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_flow", &kr, &ar)
        .await
        .expect("FCALL ff_create_flow");
}

async fn create_and_add_member(tc: &TestCluster, fid: &FlowId) -> ExecutionId {
    let config = test_config();
    let eid = ExecutionId::for_flow(fid, &config);
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

    let fpart = flow_partition(fid, &config);
    let fctx = FlowKeyContext::new(&fpart, fid);
    let fidx = FlowIndexKeys::new(&fpart);
    let keys: Vec<String> = vec![fctx.core(), fctx.members(), fidx.flow_index(), ctx.core()];
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

async fn stage_edge(
    tc: &TestCluster,
    fid: &FlowId,
    up: &ExecutionId,
    down: &ExecutionId,
    expected_graph_revision: u64,
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
        String::new(),
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

/// Trait `describe_edge` returns the same snapshot as the ff-sdk
/// wrapper. Also covers the `Ok(None)` branch (unstaged edge).
#[tokio::test]
async fn describe_edge_trait_matches_sdk() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("desc").await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let down = create_and_add_member(&tc, &fid).await;
    let e1 = stage_edge(&tc, &fid, &up, &down, 2).await;

    let trait_snap = backend
        .describe_edge(&fid, &e1)
        .await
        .expect("trait describe_edge")
        .expect("edge hash should be present");
    let sdk_snap = worker
        .describe_edge(&fid, &e1)
        .await
        .expect("sdk describe_edge")
        .expect("edge hash should be present");
    assert_eq!(trait_snap, sdk_snap, "trait must match SDK wrapper");
    assert_eq!(trait_snap.edge_id, e1);
    assert_eq!(trait_snap.upstream_execution_id, up);
    assert_eq!(trait_snap.downstream_execution_id, down);

    // Ok(None) path — unstaged edge id under the same flow.
    let absent = EdgeId::new();
    let trait_missing = backend
        .describe_edge(&fid, &absent)
        .await
        .expect("trait describe_edge on absent");
    let sdk_missing = worker
        .describe_edge(&fid, &absent)
        .await
        .expect("sdk describe_edge on absent");
    assert!(trait_missing.is_none());
    assert!(sdk_missing.is_none());
}

/// Trait `resolve_execution_flow_id` returns the member's owning flow
/// and `None` for a standalone execution id that was never created.
#[tokio::test]
async fn resolve_execution_flow_id_trait_roundtrip() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let _worker = build_worker("resolve").await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let eid = create_and_add_member(&tc, &fid).await;

    let resolved = backend
        .resolve_execution_flow_id(&eid)
        .await
        .expect("trait resolve_execution_flow_id");
    assert_eq!(
        resolved,
        Some(fid.clone()),
        "member's flow_id must round-trip through the trait"
    );

    // Absent exec_core ⇒ Ok(None). Mint a fresh ExecutionId that we
    // deliberately never create, keeping it co-located under the same
    // flow partition so the routing touches a real key slot.
    let config = test_config();
    let unseen = ExecutionId::for_flow(&fid, &config);
    let absent = backend
        .resolve_execution_flow_id(&unseen)
        .await
        .expect("trait resolve on absent exec_core");
    assert!(
        absent.is_none(),
        "absent exec_core must surface as Ok(None), got {absent:?}"
    );
}
