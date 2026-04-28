//! RFC-012 Stage 1c T2: integration coverage for
//! [`ff_core::engine_backend::EngineBackend::list_edges`] on the
//! Valkey-backed impl.
//!
//! The ff-sdk free-fns `list_incoming_edges` / `list_outgoing_edges`
//! retain their internal pipeline at Stage 1c-T2 (zero-behavior-change
//! holdover); this test proves the new trait method returns the SAME
//! edge set for the SAME flow, so the Stage 1c-T3 migration can
//! point callers at the trait with confidence.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_list_edges \
//!       -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::contracts::EdgeDirection;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{PartitionConfig, execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

const LANE: &str = "list-edges-lane";
const NS: &str = "list-edges-ns";

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
        worker_id: WorkerId::new(format!("list-edges-worker-{suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("list-edges-inst-{suffix}")),
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

async fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    // Reuse the TestCluster's dialed client + test partition config so
    // the backend and the SDK worker both hit the same keyspace under
    // the same routing rules. `from_client_and_partitions` wraps the
    // client into an `Arc<ValkeyBackend>`; upcast to
    // `Arc<dyn EngineBackend>` via the trait-object coercion.
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
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_flow", &key_refs, &arg_refs)
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
        "list-edges-test".to_owned(),
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
    // KEYS order per flowfabric.lua `ff_add_execution_to_flow`:
    // (1) flow_core, (2) members_set, (3) flow_index, (4) exec_core.
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

/// Trait `list_edges` (Outgoing + Incoming) returns the same edge set
/// as the ff-sdk free-fn pair for a fan-out flow with 2 outgoing +
/// 1 incoming edge on the subject nodes.
#[tokio::test]
async fn list_edges_trait_matches_sdk_free_fn() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("parity").await;
    let backend = build_backend(&tc).await;

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let mid = create_and_add_member(&tc, &fid).await;
    let other = create_and_add_member(&tc, &fid).await;
    // 3 adds → graph_revision 3, then +1 per staged edge.
    let e1 = stage_edge(&tc, &fid, &up, &mid, 3).await;
    let e2 = stage_edge(&tc, &fid, &up, &other, 4).await;

    // Outgoing: `up` → {mid, other}.
    let mut trait_out = backend
        .list_edges(
            &fid,
            EdgeDirection::Outgoing {
                from_node: up.clone(),
            },
        )
        .await
        .expect("trait list_edges Outgoing");
    let mut sdk_out = worker
        .list_outgoing_edges(&up)
        .await
        .expect("sdk list_outgoing_edges");
    trait_out.sort_by(|a, b| a.edge_id.to_string().cmp(&b.edge_id.to_string()));
    sdk_out.sort_by(|a, b| a.edge_id.to_string().cmp(&b.edge_id.to_string()));
    assert_eq!(
        trait_out, sdk_out,
        "trait list_edges(Outgoing) must match ff-sdk list_outgoing_edges"
    );
    assert_eq!(trait_out.len(), 2, "expected 2 outgoing edges");
    let ids: Vec<_> = trait_out.iter().map(|e| e.edge_id.clone()).collect();
    assert!(ids.contains(&e1));
    assert!(ids.contains(&e2));

    // Incoming: `mid` ← {up}.
    let trait_in = backend
        .list_edges(
            &fid,
            EdgeDirection::Incoming {
                to_node: mid.clone(),
            },
        )
        .await
        .expect("trait list_edges Incoming");
    let sdk_in = worker
        .list_incoming_edges(&mid)
        .await
        .expect("sdk list_incoming_edges");
    assert_eq!(
        trait_in, sdk_in,
        "trait list_edges(Incoming) must match ff-sdk list_incoming_edges"
    );
    assert_eq!(trait_in.len(), 1, "expected 1 incoming edge on mid");
    assert_eq!(trait_in[0].edge_id, e1);
    assert_eq!(trait_in[0].upstream_execution_id, up);
    assert_eq!(trait_in[0].downstream_execution_id, mid);

    // Isolated node `other` has 0 incoming listed under `mid`'s flow.
    let trait_iso = backend
        .list_edges(
            &fid,
            EdgeDirection::Outgoing {
                from_node: mid.clone(),
            },
        )
        .await
        .expect("trait list_edges Outgoing for leaf");
    assert!(
        trait_iso.is_empty(),
        "leaf node has no outgoing edges; got {trait_iso:?}"
    );
}
