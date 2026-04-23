//! RFC-016 Stage A integration test.
//!
//! Two scenarios:
//!
//! 1. **Explicit `AllOf` via `set_edge_group_policy`.** Stage the
//!    policy before the first `add_dependency`; stage 3 upstream→1
//!    downstream edges; drive each upstream to `success` via
//!    `ff_resolve_dependency`; assert `describe_flow.edge_groups`
//!    reports `n=3, satisfied=3, state=Satisfied` and that the
//!    downstream is eligible.
//!
//! 2. **Backward-compat shim.** Seed a flow that *never* calls
//!    `set_edge_group_policy` (the path all existing flows take
//!    before this release). Stage the same 3→1 shape; resolve all 3
//!    upstreams; assert the downstream is eligible AND the snapshot
//!    still reports AllOf / Satisfied (served either from the
//!    dual-written edgegroup hash or, for flows that pre-date this
//!    release, the legacy `deps_meta` shim).
//!
//! The original Stage-A "rejects AnyOf/Quorum" test was retired when
//! Stage B lifted that restriction; see `flow_edge_policies_stage_b.rs`
//! for the new coverage (including the narrower `k == 0` rejection).
//!
//! Run with:
//!   cargo test -p ff-test --test flow_edge_policies_stage_a -- --test-threads=1

use ferriskey::Value;
use ff_core::contracts::{
    ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    EdgeDependencyPolicy, EdgeGroupState, StageDependencyEdgeArgs,
};
use ff_core::keys::{ExecKeyContext, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const NS: &str = "edgepol-ns";
const LANE: &str = "edgepol-lane";

fn cfg() -> ff_core::partition::PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn start_server(
    tc: &TestCluster,
) -> std::sync::Arc<ff_server::server::Server> {
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    let pc = *tc.partition_config();
    let config = ff_server::config::ServerConfig {
        host,
        port,
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        partition_config: pc,
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: pc,
            lanes: vec![LaneId::new(LANE)],
            ..Default::default()
        },
        skip_library_load: false,
        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
    };
    let server = ff_server::server::Server::start(config)
        .await
        .expect("Server::start");
    std::sync::Arc::new(server)
}

async fn build_worker() -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: ff_test::fixtures::backend_config_from_env(),
        worker_id: WorkerId::new("edgepol-worker"),
        worker_instance_id: WorkerInstanceId::new("edgepol-inst"),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

fn mint_flow_with_members(
    tc: &TestCluster,
    n: usize,
) -> (FlowId, Vec<ExecutionId>) {
    let fid = FlowId::new();
    let fp = flow_partition(&fid, tc.partition_config());
    let members = (0..n)
        .map(|_| tc.new_execution_id_on_partition(fp.index))
        .collect();
    (fid, members)
}

async fn create_flow(server: &ff_server::server::Server, fid: &FlowId) {
    server
        .create_flow(&CreateFlowArgs {
            flow_id: fid.clone(),
            flow_kind: "edgepol".into(),
            namespace: Namespace::new(NS),
            now: TimestampMs::now(),
        })
        .await
        .expect("create_flow");
}

async fn create_exec(
    server: &ff_server::server::Server,
    tc: &TestCluster,
    eid: &ExecutionId,
) {
    let partition_id = execution_partition(eid, tc.partition_config()).index;
    server
        .create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE),
            execution_kind: "edgepol".into(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".into()),
            priority: 0,
            creator_identity: "tester".into(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id,
            now: TimestampMs::now(),
        })
        .await
        .expect("create_execution");
}

async fn add_member(
    server: &ff_server::server::Server,
    fid: &FlowId,
    eid: &ExecutionId,
) {
    server
        .add_execution_to_flow(&ff_core::contracts::AddExecutionToFlowArgs {
            flow_id: fid.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("add_execution_to_flow");
}

async fn stage_and_apply(
    server: &ff_server::server::Server,
    fid: &FlowId,
    upstream: &ExecutionId,
    downstream: &ExecutionId,
    expected_rev: u64,
) -> u64 {
    let edge_id = EdgeId::new();
    let now = TimestampMs::now();
    let staged = server
        .stage_dependency_edge(&StageDependencyEdgeArgs {
            flow_id: fid.clone(),
            edge_id: edge_id.clone(),
            upstream_execution_id: upstream.clone(),
            downstream_execution_id: downstream.clone(),
            dependency_kind: "success_only".into(),
            data_passing_ref: None,
            expected_graph_revision: expected_rev,
            now,
        })
        .await
        .expect("stage_dependency_edge");
    let new_rev = match staged {
        ff_core::contracts::StageDependencyEdgeResult::Staged {
            new_graph_revision,
            ..
        } => new_graph_revision,
    };
    server
        .apply_dependency_to_child(&ApplyDependencyToChildArgs {
            flow_id: fid.clone(),
            edge_id: edge_id.clone(),
            downstream_execution_id: downstream.clone(),
            upstream_execution_id: upstream.clone(),
            graph_revision: new_rev,
            dependency_kind: "success_only".into(),
            data_passing_ref: None,
            now,
        })
        .await
        .expect("apply_dependency_to_child");
    new_rev
}

/// Direct ff_resolve_dependency FCALL — drives the downstream's
/// dep-hash state machine without needing the worker/claim/complete
/// path. Finds the matching edge by scanning the downstream's `in:`
/// adjacency SET.
async fn fcall_resolve(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    upstream: &ExecutionId,
) {
    let partition = flow_partition(fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&partition, fid);
    let ep = execution_partition(downstream, tc.partition_config());
    let ctx = ExecKeyContext::new(&ep, downstream);
    let idx = ff_core::keys::IndexKeys::new(&ep);

    let edge_ids: Vec<String> = tc
        .client()
        .cmd("SMEMBERS")
        .arg(fctx.incoming(downstream).as_str())
        .execute()
        .await
        .expect("SMEMBERS incoming");

    let mut matching_edge: Option<String> = None;
    for eid in &edge_ids {
        let parsed = match EdgeId::parse(eid) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let upstream_id: Option<String> = tc
            .client()
            .cmd("HGET")
            .arg(fctx.edge(&parsed).as_str())
            .arg("upstream_execution_id")
            .execute()
            .await
            .unwrap();
        if upstream_id.as_deref() == Some(upstream.to_string().as_str()) {
            matching_edge = Some(eid.clone());
            break;
        }
    }
    let edge_str = matching_edge.expect("no edge matching upstream found");

    let edge_parsed = EdgeId::parse(&edge_str).unwrap();
    let upstream_ctx = ExecKeyContext::new(&ep, upstream);
    let att = AttemptIndex::new(0);
    let lane = LaneId::new(LANE);
    let edgegroup = fctx.edgegroup(downstream);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.deps_meta(),
        ctx.deps_unresolved(),
        ctx.dep_edge(&edge_parsed),
        idx.lane_eligible(&lane),
        idx.lane_terminal(&lane),
        idx.lane_blocked_dependencies(&lane),
        ctx.attempt_hash(att),
        ctx.stream_meta(att),
        ctx.payload(),
        upstream_ctx.result(),
        edgegroup,
    ];
    let now = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![edge_str, "success".into(), now];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_resolve_dependency", &kr, &ar)
        .await
        .expect("FCALL ff_resolve_dependency");
}

async fn seed_partition_config(tc: &TestCluster) {
    let c = cfg();
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(c.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(c.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(c.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn stage_a_allof_explicit_policy_succeeds_when_all_upstreams_succeed() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, members) = mint_flow_with_members(&tc, 4);
    let (a, b, c, d) = (&members[0], &members[1], &members[2], &members[3]);

    create_flow(&server, &fid).await;
    for m in &members {
        create_exec(&server, &tc, m).await;
        add_member(&server, &fid, m).await;
    }

    // Declare AllOf explicitly BEFORE staging any edge for d.
    worker
        .set_edge_group_policy(&fid, d, EdgeDependencyPolicy::all_of())
        .await
        .expect("set_edge_group_policy AllOf");

    // create_flow=1 rev + 4 add_member bumps = 5. But add_execution
    // may not bump graph_revision today — thread the authoritative
    // revision through successive stage calls.
    // Read current graph_revision off flow_core.
    let current_rev: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(FlowKeyContext::new(&flow_partition(&fid, &cfg()), &fid).core().as_str())
        .arg("graph_revision")
        .execute()
        .await
        .unwrap();
    let mut rev: u64 = current_rev.and_then(|s| s.parse().ok()).unwrap_or(0);
    rev = stage_and_apply(&server, &fid, a, d, rev).await;
    rev = stage_and_apply(&server, &fid, b, d, rev).await;
    let _ = stage_and_apply(&server, &fid, c, d, rev).await;

    for up in [a, b, c] {
        fcall_resolve(&tc, &fid, d, up).await;
    }

    let snap = worker
        .describe_flow(&fid)
        .await
        .expect("describe_flow")
        .expect("flow present");
    let eg = snap
        .edge_groups
        .iter()
        .find(|g| &g.downstream_execution_id == d)
        .expect("edge group for d present");

    assert_eq!(eg.policy, EdgeDependencyPolicy::AllOf);
    assert_eq!(eg.total_deps, 3, "n=3");
    assert_eq!(eg.satisfied_count, 3, "succeeded=3");
    assert_eq!(eg.group_state, EdgeGroupState::Satisfied);

    let partition = execution_partition(d, &cfg());
    let ctx = ExecKeyContext::new(&partition, d);
    let elig: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(ctx.core().as_str())
        .arg("eligibility_state")
        .execute()
        .await
        .unwrap();
    assert_eq!(
        elig.as_deref(),
        Some("eligible_now"),
        "AllOf satisfied → downstream eligible"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_a_backward_compat_shim_allof_without_explicit_policy() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, members) = mint_flow_with_members(&tc, 4);
    let (a, b, c, d) = (&members[0], &members[1], &members[2], &members[3]);

    create_flow(&server, &fid).await;
    for m in &members {
        create_exec(&server, &tc, m).await;
        add_member(&server, &fid, m).await;
    }

    // NO set_edge_group_policy call — existing-flow path.
    let current_rev: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(FlowKeyContext::new(&flow_partition(&fid, &cfg()), &fid).core().as_str())
        .arg("graph_revision")
        .execute()
        .await
        .unwrap();
    let mut rev: u64 = current_rev.and_then(|s| s.parse().ok()).unwrap_or(0);
    rev = stage_and_apply(&server, &fid, a, d, rev).await;
    rev = stage_and_apply(&server, &fid, b, d, rev).await;
    let _ = stage_and_apply(&server, &fid, c, d, rev).await;

    for up in [a, b, c] {
        fcall_resolve(&tc, &fid, d, up).await;
    }

    let snap = worker
        .describe_flow(&fid)
        .await
        .expect("describe_flow")
        .expect("flow present");
    let eg = snap
        .edge_groups
        .iter()
        .find(|g| &g.downstream_execution_id == d)
        .expect("edge group for d present (via dual-write or shim)");
    assert_eq!(eg.policy, EdgeDependencyPolicy::AllOf);
    assert_eq!(eg.total_deps, 3);
    assert_eq!(eg.group_state, EdgeGroupState::Satisfied);

    let partition = execution_partition(d, &cfg());
    let ctx = ExecKeyContext::new(&partition, d);
    let elig: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(ctx.core().as_str())
        .arg("eligibility_state")
        .execute()
        .await
        .unwrap();
    assert_eq!(
        elig.as_deref(),
        Some("eligible_now"),
        "backward-compat: AllOf satisfaction still fires without set_edge_group_policy"
    );
}

// NOTE: The original Stage-A-only `stage_a_rejects_any_of_and_quorum`
// test lived here to pin the Stage A closed-door stance (AnyOf/Quorum
// rejected with a typed validation error). RFC-016 Stage B intentionally
// flips that gate to ACCEPT AnyOf/Quorum — the assertion is obsolete.
// Stage B coverage for the AnyOf/Quorum paths and the narrower
// `k == 0` validation lives in `flow_edge_policies_stage_b.rs`.
