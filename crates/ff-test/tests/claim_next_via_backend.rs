//! Integration coverage for `FlowFabricWorker::claim_next_via_backend`
//! (v0.14 P1.3 — un-gated scheduler-bypass scanner).
//!
//! Pins the basic round-trip on a live Valkey fixture. The goal is to
//! confirm the un-gated method claims through the trait path
//! end-to-end identically to the legacy `claim_next` back-compat alias
//! — not to re-test the claim primitive's semantics (covered by
//! `engine_backend_deliver_signal.rs` et al).
//!
//! Run with: cargo test -p ff-test --test claim_next_via_backend -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const LANE: &str = "cnvb-lane";
const NS: &str = "cnvb-ns";
const WORKER: &str = "cnvb-worker";
const WORKER_INST: &str = "cnvb-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn build_worker() -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: Some(ff_test::fixtures::backend_config_from_env()),
        worker_id: WorkerId::new(WORKER),
        worker_instance_id: WorkerInstanceId::new(WORKER_INST),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 10,
        partition_config: None,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

async fn seed_eligible_execution(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, &config());
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
        "cnvb".to_owned(),
        "0".to_owned(),
        "cnvb-runner".to_owned(),
        "{}".to_owned(),
        r#"{"test":true}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("ff_create_execution");
}

#[tokio::test]
#[serial_test::serial]
async fn claim_next_via_backend_claims_eligible_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    seed_eligible_execution(&tc, &eid).await;

    let worker = build_worker().await;
    // Scan multiple times — the scanner covers PARTITION_SCAN_CHUNK
    // partitions per call; our eid may not be on the first window.
    let max_polls = ((config().num_flow_partitions as usize) / 32) + 2;
    let mut claimed = None;
    for _ in 0..max_polls {
        if let Some(task) = worker.claim_next_via_backend().await.expect("claim ok") {
            claimed = Some(task);
            break;
        }
    }
    let task = claimed.expect("claim_next_via_backend must land the eligible execution");
    assert_eq!(task.execution_id(), &eid);
    task.complete(Some(b"done".to_vec()))
        .await
        .expect("complete");
}

#[tokio::test]
#[serial_test::serial]
async fn claim_next_via_backend_returns_none_when_idle() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let worker = build_worker().await;
    // One poll on an empty cluster — expect None (no eligible work in
    // this partition window). Full-sweep not needed to pin the idle
    // semantic.
    let result = worker.claim_next_via_backend().await.expect("claim ok");
    assert!(
        result.is_none(),
        "idle cluster must return Ok(None), got Some(ClaimedTask)"
    );
}
