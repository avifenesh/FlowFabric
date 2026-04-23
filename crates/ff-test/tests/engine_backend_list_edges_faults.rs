//! Trait-boundary fault-path coverage for
//! [`ff_core::engine_backend::EngineBackend::list_edges`] on the
//! Valkey-backed impl.
//!
//! Per Worker-TTT's `rfcs/drafts/post-stage-1c-test-coverage.md`
//! audit, the existing `engine_backend_list_edges.rs` (Stage 1c-T2)
//! pins the happy-path trait parity, and SDK-layer tests cover the
//! "unknown flow" + adjacency-drift paths via
//! `FlowFabricWorker::list_{incoming,outgoing}_edges`. This file adds
//! the missing trait-layer coverage:
//!
//! 1. `list_edges(non_existent_flow, ...)` → `Ok(Vec::new())` at the
//!    trait (SMEMBERS on an absent adjacency SET returns empty; the
//!    trait does NOT resolve the flow id on the caller's behalf).
//! 2. Adjacency drift — SADD a phantom edge id to the outgoing SET,
//!    leave the edge hash absent → `EngineError::Validation {
//!    kind: Corruption, .. }` (pins the loud-fail contract on
//!    staged-SET / edge-hash drift).
//! 3. Adjacency drift — stage a legitimate edge, then DEL the edge
//!    hash while leaving the adjacency SET intact → same Corruption
//!    contract.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_list_edges_faults \
//!       -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::contracts::EdgeDirection;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext, IndexKeys};
use ff_core::partition::{PartitionConfig, execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

const LANE: &str = "list-edges-faults-lane";
const NS: &str = "list-edges-faults-ns";

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
        "faults_kind".to_owned(),
        "0".to_owned(),
        "list-edges-faults".to_owned(),
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

// ─── Tests ───

/// `list_edges` on a flow id that was never created returns an
/// empty `Vec` — SMEMBERS on an absent adjacency SET yields `[]` and
/// the trait surfaces that as `Ok(Vec::new())` with no error. Pins
/// the "unknown flow id is not an error at the trait boundary"
/// contract (the SDK wrapper's `resolve_flow_id` short-circuit lives
/// above the trait; the trait itself MUST tolerate a missing flow).
#[tokio::test]
async fn list_edges_trait_missing_flow_returns_empty() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let backend = build_backend(&tc);

    // Unseeded flow id — `ff_create_flow` never called.
    let ghost_fid = FlowId::new();
    // Any execution id works here; SMEMBERS will be empty on the
    // `outgoing:<eid>` key regardless.
    let ghost_eid = ExecutionId::for_flow(&ghost_fid, &test_config());

    let out = backend
        .list_edges(
            &ghost_fid,
            EdgeDirection::Outgoing {
                from_node: ghost_eid.clone(),
            },
        )
        .await
        .expect("trait list_edges on missing flow must not error");
    assert!(out.is_empty(), "missing flow → empty Vec, got {out:?}");

    let inc = backend
        .list_edges(&ghost_fid, EdgeDirection::Incoming { to_node: ghost_eid })
        .await
        .expect("trait list_edges Incoming on missing flow must not error");
    assert!(inc.is_empty(), "missing flow → empty Vec, got {inc:?}");
}

/// Phantom adjacency drift: SADD a never-staged edge id into the
/// outgoing SET, leave the edge hash absent → trait surfaces
/// `EngineError::Validation { kind: Corruption, .. }`. Pins the
/// loud-fail contract FF relies on (edge hashes are write-once; an
/// adjacency-set entry with no hash is key corruption).
#[tokio::test]
async fn list_edges_trait_phantom_adjacency_entry_is_corruption() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;

    // SADD a valid-looking but never-staged edge id into the
    // outgoing SET. The edge_hash key is never written.
    let phantom_edge = EdgeId::new();
    let config = test_config();
    let partition = flow_partition(&fid, &config);
    let fctx = FlowKeyContext::new(&partition, &fid);
    let _: Value = tc
        .client()
        .cmd("SADD")
        .arg(fctx.outgoing(&up))
        .arg(phantom_edge.to_string().as_str())
        .execute()
        .await
        .expect("SADD phantom edge id");

    let err = backend
        .list_edges(
            &fid,
            EdgeDirection::Outgoing {
                from_node: up.clone(),
            },
        )
        .await
        .expect_err("phantom adjacency entry must surface as error");
    match err {
        EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail,
        } => {
            assert!(
                detail.contains(&phantom_edge.to_string()),
                "detail must name the phantom edge id: {detail}"
            );
        }
        other => panic!("expected Validation::Corruption, got {other:?}"),
    }
}

/// Adjacency-hash drift: stage a real edge, then DEL its edge hash
/// while leaving the outgoing SET intact → trait surfaces
/// `EngineError::Validation { kind: Corruption, .. }`. Exercises
/// the same guard as the phantom-entry case but starting from a
/// staged-then-deleted state, matching the "operator ran DEL on the
/// edge hash" failure mode.
#[tokio::test]
async fn list_edges_trait_drift_missing_edge_hash_is_corruption() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    create_flow(&tc, &fid).await;
    let up = create_and_add_member(&tc, &fid).await;
    let down = create_and_add_member(&tc, &fid).await;
    // 2 adds → graph_revision 2.
    let edge_id = stage_edge(&tc, &fid, &up, &down, 2).await;

    // Baseline: trait returns the staged edge.
    let baseline = backend
        .list_edges(
            &fid,
            EdgeDirection::Outgoing {
                from_node: up.clone(),
            },
        )
        .await
        .expect("baseline list_edges");
    assert_eq!(baseline.len(), 1);
    assert_eq!(baseline[0].edge_id, edge_id);

    // Drift: DEL the edge hash; adjacency SET still references it.
    let config = test_config();
    let partition = flow_partition(&fid, &config);
    let fctx = FlowKeyContext::new(&partition, &fid);
    let _: Value = tc
        .client()
        .cmd("DEL")
        .arg(fctx.edge(&edge_id))
        .execute()
        .await
        .expect("DEL edge_hash");

    let err = backend
        .list_edges(&fid, EdgeDirection::Outgoing { from_node: up })
        .await
        .expect_err("adjacency-hash drift must surface as error");
    match err {
        EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail,
        } => {
            assert!(
                detail.contains(&edge_id.to_string()),
                "detail must name the edge id: {detail}"
            );
        }
        other => panic!("expected Validation::Corruption, got {other:?}"),
    }
}
