//! Trait-boundary parity tests for
//! [`ff_core::engine_backend::EngineBackend::describe_execution`] and
//! [`ff_core::engine_backend::EngineBackend::describe_flow`] on the
//! Valkey-backed impl.
//!
//! Per Worker-TTT's `rfcs/drafts/post-stage-1c-test-coverage.md` audit,
//! these two read-only describe ops were exercised only via the
//! ff-sdk wrappers (`FlowFabricWorker::describe_execution` /
//! `describe_flow`) — never directly against the
//! `Arc<dyn EngineBackend>` trait object. Post-Stage-1c-T3 the SDK
//! wrappers are thin forwarders (`snapshot.rs:42-57`), but the tests
//! still went in through `FlowFabricWorker`. A future backend impl
//! (Postgres) would be exercised at the trait surface only, so we need
//! trait-boundary coverage that pins the same output.
//!
//! This test stages an execution + flow via FCALL, reads both from
//! the trait directly (`ValkeyBackend` via
//! `from_client_and_partitions`) and via the SDK wrapper, and asserts
//! the snapshots match byte-for-byte — mirrors the precedent T2
//! added in `engine_backend_list_edges.rs`.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_describe \
//!       -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{PartitionConfig, execution_partition, flow_partition};
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

const LANE: &str = "desc-trait-lane";
const NS: &str = "desc-trait-ns";

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
        backend: ff_test::fixtures::backend_config_from_env(),
        worker_id: WorkerId::new(format!("desc-trait-worker-{suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("desc-trait-inst-{suffix}")),
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

async fn create_execution(tc: &TestCluster, eid: &ExecutionId, tags_json: &str) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
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
        "desc_trait_kind".to_owned(),
        "0".to_owned(),
        "desc-trait-test".to_owned(),
        "{}".to_owned(),
        String::new(),
        String::new(),
        String::new(),
        tags_json.to_owned(),
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
}

async fn create_flow(tc: &TestCluster, fid: &FlowId, flow_kind: &str) {
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![ctx.core(), ctx.members(), fidx.flow_index()];
    let args: Vec<String> = vec![
        fid.to_string(),
        flow_kind.to_owned(),
        NS.to_owned(),
        TimestampMs::now().0.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_flow", &kr, &ar)
        .await
        .expect("FCALL ff_create_flow");
}

// ─── Tests ───

/// Trait `describe_execution` returns the same `ExecutionSnapshot`
/// as the SDK wrapper for a freshly-created execution with tags.
#[tokio::test]
async fn describe_execution_trait_matches_sdk() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("exec-parity").await;
    let backend = build_backend(&tc);

    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    let tags_json = r#"{"cairn.task_id":"t-www","cairn.project":"ff"}"#;
    create_execution(&tc, &eid, tags_json).await;

    let trait_snap = backend
        .describe_execution(&eid)
        .await
        .expect("trait describe_execution")
        .expect("trait snapshot present");
    let sdk_snap = worker
        .describe_execution(&eid)
        .await
        .expect("sdk describe_execution")
        .expect("sdk snapshot present");

    assert_eq!(
        trait_snap, sdk_snap,
        "trait describe_execution must match sdk describe_execution byte-for-byte"
    );
    // Spot-check the fields the test itself controls so an
    // unrelated field-drift regression surfaces loudly here rather
    // than only in the parity equality above.
    assert_eq!(trait_snap.execution_id, eid);
    assert_eq!(trait_snap.public_state, PublicState::Waiting);
    assert_eq!(trait_snap.lane_id.as_str(), LANE);
    assert_eq!(trait_snap.namespace.as_str(), NS);
    assert_eq!(trait_snap.tags.len(), 2);
}

/// Trait `describe_execution` returns `Ok(None)` on a fresh id —
/// matching the SDK wrapper's contract.
#[tokio::test]
async fn describe_execution_trait_missing_returns_none() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("exec-missing").await;
    let backend = build_backend(&tc);

    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    let trait_snap = backend
        .describe_execution(&eid)
        .await
        .expect("trait describe_execution on missing");
    let sdk_snap = worker
        .describe_execution(&eid)
        .await
        .expect("sdk describe_execution on missing");
    assert!(trait_snap.is_none(), "trait returns None on missing");
    assert!(sdk_snap.is_none(), "sdk returns None on missing");
    assert_eq!(trait_snap, sdk_snap);
}

/// Trait `describe_flow` returns the same `FlowSnapshot` as the SDK
/// wrapper for a freshly-created flow.
#[tokio::test]
async fn describe_flow_trait_matches_sdk() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("flow-parity").await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    create_flow(&tc, &fid, "dag").await;

    let trait_snap = backend
        .describe_flow(&fid)
        .await
        .expect("trait describe_flow")
        .expect("trait snapshot present");
    let sdk_snap = worker
        .describe_flow(&fid)
        .await
        .expect("sdk describe_flow")
        .expect("sdk snapshot present");

    assert_eq!(
        trait_snap, sdk_snap,
        "trait describe_flow must match sdk describe_flow byte-for-byte"
    );
    assert_eq!(trait_snap.flow_id, fid);
    assert_eq!(trait_snap.flow_kind, "dag");
    assert_eq!(trait_snap.namespace.as_str(), NS);
    assert_eq!(trait_snap.public_flow_state, "open");
    assert_eq!(trait_snap.node_count, 0);
    assert_eq!(trait_snap.edge_count, 0);
}

/// Trait `describe_flow` returns `Ok(None)` on a fresh id — matches
/// the SDK wrapper's contract.
#[tokio::test]
async fn describe_flow_trait_missing_returns_none() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("flow-missing").await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    let trait_snap = backend
        .describe_flow(&fid)
        .await
        .expect("trait describe_flow on missing");
    let sdk_snap = worker
        .describe_flow(&fid)
        .await
        .expect("sdk describe_flow on missing");
    assert!(trait_snap.is_none());
    assert!(sdk_snap.is_none());
    assert_eq!(trait_snap, sdk_snap);
}
