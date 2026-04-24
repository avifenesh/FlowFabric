//! RFC-016 Stage B integration tests.
//!
//! Stage B lights up the AnyOf / Quorum resolver on top of the Stage A
//! edgegroup hash. Stage B does NOT ship the sibling-cancel dispatcher
//! -- Stage C lands that. Tests here assert:
//!
//! 1. AnyOf{CancelRemaining}: first upstream success -> downstream
//!    eligible + `cancel_siblings_pending_flag=true`. Siblings are
//!    NOT terminated (Stage C's job).
//! 2. AnyOf{LetRun}: first success -> eligible + flag NOT set.
//! 3. Quorum(3, CancelRemaining) of 5: third success -> eligible +
//!    flag=true.
//! 4. Quorum(3, LetRun) of 5: third success -> eligible + flag unset.
//! 5. Impossible-quorum + CancelRemaining: 3 of 5 fail, quorum=3 ->
//!    downstream skipped + flag=true on the still-running sibling's
//!    edgegroup.
//! 6. Impossible-quorum + LetRun: same shape, flag unset, surviving
//!    sibling still free to run to terminal.
//! 7. k=0 rejected at `set_edge_group_policy` entry point.
//! 8. Once-fired: subsequent successes after the firing point do NOT
//!    re-transition the downstream.
//!
//! Run with:
//!   cargo test -p ff-test --test flow_edge_policies_stage_b -- --test-threads=1

use ferriskey::Value;
use ff_core::contracts::{
    ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    EdgeDependencyPolicy, OnSatisfied, StageDependencyEdgeArgs,
};
use ff_core::keys::{ExecKeyContext, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const NS: &str = "edgepol-b-ns";
const LANE: &str = "edgepol-b-lane";

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
    let config = ff_server::config::ServerConfig {        valkey: ff_server::config::ValkeyServerConfig { host: host, port: port, tls: ff_test::fixtures::env_flag("FF_TLS"), cluster: ff_test::fixtures::env_flag("FF_CLUSTER"), skip_library_load: false },





        partition_config: pc,
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: pc,
            lanes: vec![LaneId::new(LANE)],
            ..Default::default()
        },

        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
        backend: ff_server::config::BackendKind::default(),
        postgres: Default::default(),
    
};
    let server = ff_server::server::Server::start(config)
        .await
        .expect("Server::start");
    std::sync::Arc::new(server)
}

async fn build_worker() -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: ff_test::fixtures::backend_config_from_env(),
        worker_id: WorkerId::new("edgepol-b-worker"),
        worker_instance_id: WorkerInstanceId::new("edgepol-b-inst"),
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

/// Drive one edge's resolve with a caller-supplied upstream_outcome.
async fn fcall_resolve_with(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    upstream: &ExecutionId,
    upstream_outcome: &str,
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
    let incoming_set = fctx.incoming(downstream);
    let pending_cancel_groups_set = ff_core::keys::FlowIndexKeys::new(&partition)
        .pending_cancel_groups();

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
        incoming_set,
        pending_cancel_groups_set,
    ];
    let now = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        edge_str,
        upstream_outcome.into(),
        now,
        fid.to_string(),
        downstream.to_string(),
    ];
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

async fn read_edgegroup_field(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    field: &str,
) -> Option<String> {
    let partition = flow_partition(fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&partition, fid);
    tc.client()
        .cmd("HGET")
        .arg(fctx.edgegroup(downstream).as_str())
        .arg(field)
        .execute()
        .await
        .unwrap()
}

async fn read_exec_field(
    tc: &TestCluster,
    downstream: &ExecutionId,
    field: &str,
) -> Option<String> {
    let partition = execution_partition(downstream, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, downstream);
    tc.client()
        .cmd("HGET")
        .arg(ctx.core().as_str())
        .arg(field)
        .execute()
        .await
        .unwrap()
}

async fn setup_fanin(
    tc: &TestCluster,
    server: &ff_server::server::Server,
    worker: &ff_sdk::FlowFabricWorker,
    upstream_n: usize,
    policy: EdgeDependencyPolicy,
) -> (FlowId, Vec<ExecutionId>, ExecutionId) {
    let (fid, members) = mint_flow_with_members(tc, upstream_n + 1);
    let downstream = members[upstream_n].clone();
    let upstreams: Vec<ExecutionId> = members[..upstream_n].to_vec();

    create_flow(server, &fid).await;
    for m in &members {
        create_exec(server, tc, m).await;
        add_member(server, &fid, m).await;
    }

    worker
        .set_edge_group_policy(&fid, &downstream, policy)
        .await
        .expect("set_edge_group_policy");

    let current_rev: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(
            FlowKeyContext::new(&flow_partition(&fid, &cfg()), &fid)
                .core()
                .as_str(),
        )
        .arg("graph_revision")
        .execute()
        .await
        .unwrap();
    let mut rev: u64 = current_rev.and_then(|s| s.parse().ok()).unwrap_or(0);
    for u in &upstreams {
        rev = stage_and_apply(server, &fid, u, &downstream, rev).await;
    }

    (fid, upstreams, downstream)
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_any_of_cancel_remaining_first_success_fires_and_flags() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        3,
        EdgeDependencyPolicy::any_of(OnSatisfied::cancel_remaining()),
    )
    .await;

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;

    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now"),
        "AnyOf satisfied on first success"
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("satisfied")
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_flag")
            .await
            .as_deref(),
        Some("true"),
        "CancelRemaining must flag siblings"
    );

    for u in &upstreams[1..] {
        let term = read_exec_field(&tc, u, "terminal_outcome").await;
        assert_ne!(
            term.as_deref(),
            Some("cancelled"),
            "Stage B must NOT terminate siblings -- that is Stage C"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_any_of_let_run_first_success_fires_without_flag() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        3,
        EdgeDependencyPolicy::any_of(OnSatisfied::let_run()),
    )
    .await;

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;

    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now")
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("satisfied")
    );
    let flag = read_edgegroup_field(
        &tc,
        &fid,
        &downstream,
        "cancel_siblings_pending_flag",
    )
    .await;
    assert!(
        flag.is_none() || flag.as_deref() == Some(""),
        "LetRun must NEVER set cancel flag, got {flag:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_quorum_cancel_remaining_fires_at_k_and_flags() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        5,
        EdgeDependencyPolicy::quorum(3, OnSatisfied::cancel_remaining()),
    )
    .await;

    for i in 0..2 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("blocked_by_dependencies"),
        "Quorum(3) still blocked at 2 successes"
    );

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[2], "success").await;
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now")
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("satisfied")
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_flag")
            .await
            .as_deref(),
        Some("true")
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_quorum_let_run_fires_at_k_without_flag() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        5,
        EdgeDependencyPolicy::quorum(3, OnSatisfied::let_run()),
    )
    .await;

    for i in 0..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now")
    );
    let flag = read_edgegroup_field(
        &tc,
        &fid,
        &downstream,
        "cancel_siblings_pending_flag",
    )
    .await;
    assert!(
        flag.is_none() || flag.as_deref() == Some(""),
        "LetRun never flags, got {flag:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_impossible_quorum_cancel_remaining_skips_and_flags() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        5,
        EdgeDependencyPolicy::quorum(3, OnSatisfied::cancel_remaining()),
    )
    .await;

    // 3 failures of 5, quorum=3. failed(3) > n-k = 5-3 = 2 -> impossible.
    for i in 0..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "failed").await;
    }
    assert_eq!(
        read_exec_field(&tc, &downstream, "terminal_outcome").await.as_deref(),
        Some("skipped"),
        "impossible quorum short-circuits downstream to skipped"
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("impossible")
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_flag")
            .await
            .as_deref(),
        Some("true"),
        "impossible + CancelRemaining still flags surviving siblings"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_impossible_quorum_let_run_skips_without_flag() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        5,
        EdgeDependencyPolicy::quorum(3, OnSatisfied::let_run()),
    )
    .await;

    for i in 0..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "failed").await;
    }
    assert_eq!(
        read_exec_field(&tc, &downstream, "terminal_outcome").await.as_deref(),
        Some("skipped")
    );
    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("impossible")
    );
    let flag = read_edgegroup_field(
        &tc,
        &fid,
        &downstream,
        "cancel_siblings_pending_flag",
    )
    .await;
    assert!(
        flag.is_none() || flag.as_deref() == Some(""),
        "LetRun is pure: no flag on impossible either, got {flag:?}"
    );

    let term = read_exec_field(&tc, &upstreams[3], "terminal_outcome").await;
    assert_ne!(
        term.as_deref(),
        Some("cancelled"),
        "LetRun must leave surviving siblings alive"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_rejects_quorum_k_zero() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let fid = FlowId::new();
    create_flow(&server, &fid).await;
    let fp_idx = flow_partition(&fid, tc.partition_config()).index;
    let d = tc.new_execution_id_on_partition(fp_idx);

    let bad = EdgeDependencyPolicy::quorum(0, OnSatisfied::cancel_remaining());
    let result = worker.set_edge_group_policy(&fid, &d, bad).await;
    match result {
        Err(ff_sdk::SdkError::Engine(boxed)) => match *boxed {
            ff_core::engine_error::EngineError::Validation {
                kind: ff_core::engine_error::ValidationKind::InvalidInput,
                ref detail,
            } => {
                assert!(
                    detail.contains("k must be >= 1") || detail.contains("quorum k"),
                    "unexpected detail: {detail}"
                );
            }
            other => panic!("expected Validation(InvalidInput), got: {other:?}"),
        },
        other => panic!("expected Engine(Validation), got: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn stage_b_any_of_once_fired_ignores_subsequent_successes() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc).await;
    let worker = build_worker().await;

    let (fid, upstreams, downstream) = setup_fanin(
        &tc,
        &server,
        &worker,
        3,
        EdgeDependencyPolicy::any_of(OnSatisfied::let_run()),
    )
    .await;

    fcall_resolve_with(&tc, &fid, &downstream, &upstreams[0], "success").await;
    let satisfied_at_1 =
        read_edgegroup_field(&tc, &fid, &downstream, "satisfied_at").await;
    assert!(satisfied_at_1.is_some(), "satisfied_at should be set");

    for i in 1..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }

    assert_eq!(
        read_edgegroup_field(&tc, &fid, &downstream, "group_state").await.as_deref(),
        Some("satisfied"),
        "group_state remains satisfied after late successes"
    );
    let succeeded =
        read_edgegroup_field(&tc, &fid, &downstream, "succeeded").await;
    assert_eq!(
        succeeded.as_deref(),
        Some("3"),
        "counter still advances for telemetry"
    );
    let satisfied_at_2 =
        read_edgegroup_field(&tc, &fid, &downstream, "satisfied_at").await;
    assert_eq!(
        satisfied_at_1, satisfied_at_2,
        "satisfied_at is one-shot (not overwritten)"
    );
    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state").await.as_deref(),
        Some("eligible_now")
    );
}
