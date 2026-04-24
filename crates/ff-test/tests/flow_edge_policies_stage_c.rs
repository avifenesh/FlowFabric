//! RFC-016 Stage C integration tests.
//!
//! Stage C lights up the sibling-cancel dispatcher on top of the Stage
//! B AnyOf/Quorum+CancelRemaining flag. Tests here assert end-to-end:
//!
//! 1. AnyOf{CancelRemaining} fan-out 3: first success flips the group
//!    to `satisfied`, the dispatcher scans within ~2s and cancels the
//!    other two siblings -- both reach `terminal_outcome = cancelled`
//!    with `cancellation_reason = sibling_quorum_satisfied`.
//! 2. Quorum(3/5)+CancelRemaining: 3 successes fire the group, the
//!    remaining 2 still-running siblings are cancelled within ~2s.
//! 3. Impossible-quorum (Quorum(3/5)+CancelRemaining with 3 failures,
//!    2 untouched): survivors are cancelled with reason
//!    `sibling_quorum_impossible`.
//! 4. AnyOf{LetRun}: first success fires the group BUT the dispatcher
//!    never touches siblings -- they remain non-terminal even after
//!    the scanner has had multiple ticks. LetRun is pure.
//!
//! Run with:
//!   cargo test -p ff-test --test flow_edge_policies_stage_c -- --test-threads=1
//!
//! Scanner default is 1s (`EngineConfig::edge_cancel_dispatcher_interval`);
//! the assertions wait up to 3s to absorb a single tick's worth of
//! jitter + the bounded cancel round-trip.

use ferriskey::Value;
use ff_core::contracts::{
    ApplyDependencyToChildArgs, CreateExecutionArgs, CreateFlowArgs,
    EdgeDependencyPolicy, OnSatisfied, StageDependencyEdgeArgs,
};
use ff_core::keys::{ExecKeyContext, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use std::time::Duration;

const NS: &str = "edgepol-c-ns";
const LANE: &str = "edgepol-c-lane";

const ASSERT_DEADLINE: Duration = Duration::from_secs(3);
const ASSERT_POLL: Duration = Duration::from_millis(100);

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
            edge_cancel_dispatcher_interval: Duration::from_millis(250),
            ..Default::default()
        },
        skip_library_load: false,
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
        worker_id: WorkerId::new("edgepol-c-worker"),
        worker_instance_id: WorkerInstanceId::new("edgepol-c-inst"),
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

async fn create_flow_api(server: &ff_server::server::Server, fid: &FlowId) {
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

async fn create_exec_api(
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
    let pending_cancel_groups_set =
        ff_core::keys::FlowIndexKeys::new(&partition).pending_cancel_groups();

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

async fn read_exec_field(
    tc: &TestCluster,
    eid: &ExecutionId,
    field: &str,
) -> Option<String> {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    tc.client()
        .cmd("HGET")
        .arg(ctx.core().as_str())
        .arg(field)
        .execute()
        .await
        .unwrap()
}

async fn wait_for<F, Fut>(f: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while start.elapsed() < ASSERT_DEADLINE {
        if f().await {
            return true;
        }
        tokio::time::sleep(ASSERT_POLL).await;
    }
    false
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

    create_flow_api(server, &fid).await;
    for m in &members {
        create_exec_api(server, tc, m).await;
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
async fn stage_c_any_of_cancel_remaining_terminates_siblings() {
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

    for (i, sib) in upstreams.iter().enumerate().skip(1) {
        let sib_cl = sib.clone();
        let tc_cl = &tc;
        let terminated = wait_for(|| async {
            read_exec_field(tc_cl, &sib_cl, "terminal_outcome")
                .await
                .as_deref()
                == Some("cancelled")
        })
        .await;
        assert!(
            terminated,
            "sibling #{i} did not reach terminal=cancelled within deadline"
        );
        let reason = read_exec_field(&tc, sib, "cancellation_reason").await;
        assert_eq!(
            reason.as_deref(),
            Some("sibling_quorum_satisfied"),
            "sibling #{i} has wrong cancellation_reason: {reason:?}"
        );
    }

    assert_eq!(
        read_exec_field(&tc, &downstream, "eligibility_state")
            .await
            .as_deref(),
        Some("eligible_now"),
        "downstream eligibility must survive sibling cancels"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_c_quorum_cancel_remaining_terminates_stragglers() {
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

    for i in 0..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "success").await;
    }

    for i in 3..5 {
        let sib_cl = upstreams[i].clone();
        let tc_cl = &tc;
        let terminated = wait_for(|| async {
            read_exec_field(tc_cl, &sib_cl, "terminal_outcome")
                .await
                .as_deref()
                == Some("cancelled")
        })
        .await;
        assert!(terminated, "straggler #{i} did not cancel within deadline");
        let reason =
            read_exec_field(&tc, &upstreams[i], "cancellation_reason").await;
        assert_eq!(
            reason.as_deref(),
            Some("sibling_quorum_satisfied"),
            "straggler #{i} wrong reason: {reason:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn stage_c_impossible_quorum_cancels_survivors_with_impossible_reason() {
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

    for i in 0..3 {
        fcall_resolve_with(&tc, &fid, &downstream, &upstreams[i], "failed").await;
    }

    for i in 3..5 {
        let sib_cl = upstreams[i].clone();
        let tc_cl = &tc;
        let terminated = wait_for(|| async {
            read_exec_field(tc_cl, &sib_cl, "terminal_outcome")
                .await
                .as_deref()
                == Some("cancelled")
        })
        .await;
        assert!(
            terminated,
            "survivor #{i} did not cancel on impossible quorum"
        );
        let reason =
            read_exec_field(&tc, &upstreams[i], "cancellation_reason").await;
        assert_eq!(
            reason.as_deref(),
            Some("sibling_quorum_impossible"),
            "survivor #{i} wrong reason: {reason:?}"
        );
    }

    assert_eq!(
        read_exec_field(&tc, &downstream, "terminal_outcome")
            .await
            .as_deref(),
        Some("skipped")
    );
}

#[tokio::test]
#[serial_test::serial]
async fn stage_c_let_run_never_cancels_siblings() {
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

    tokio::time::sleep(Duration::from_secs(2)).await;

    for i in 1..3 {
        let term = read_exec_field(&tc, &upstreams[i], "terminal_outcome").await;
        assert_ne!(
            term.as_deref(),
            Some("cancelled"),
            "LetRun must never touch sibling #{i}, got terminal={term:?}"
        );
    }
}
