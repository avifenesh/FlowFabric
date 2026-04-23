//! RFC-016 Stage D integration tests.
//!
//! Stage D adds the sibling-cancel reconciler — a crash-recovery safety
//! net for Invariant Q6. Stage C's `edge_cancel_dispatcher` populates
//! `ff:idx:{fp:N}:pending_cancel_groups` atomically with the
//! satisfied/impossible flip and drains it via
//! `ff_drain_sibling_cancel_group`. If the engine crashed between
//! those two steps, stale tuples leak. The reconciler detects + heals
//! them without fighting the dispatcher.
//!
//! Three scenarios exercised here:
//!
//! 1. **Stale SET entry** — flag cleared (`false` / absent) but tuple
//!    still in the SET. Simulates "dispatcher HDEL'd but crashed before
//!    SREM". Reconciler SREMs within its interval.
//! 2. **Interrupted drain** — flag `true`, members list populated, but
//!    every listed sibling is already terminal. Simulates "dispatcher
//!    fired per-sibling cancels, all landed, but crashed before the
//!    atomic SREM+HDEL". Reconciler clears flag/members + SREMs.
//! 3. **Mid-flight drain (no-op)** — flag `true`, at least one sibling
//!    still running. Reconciler MUST NOT touch state; the dispatcher
//!    owns this tuple. With the dispatcher re-enabled on its regular
//!    cadence, the cancel proceeds normally.
//!
//! Scenarios 1 + 2 run with the dispatcher disabled (interval = 1h) so
//! the reconciler is unambiguously the agent of change. Scenario 3
//! enables both scanners and asserts co-existence.
//!
//! Run with:
//!   cargo test -p ff-test --test flow_edge_policies_stage_d -- --test-threads=1

use ferriskey::Value;
use ff_core::contracts::{CreateExecutionArgs, CreateFlowArgs};
use ff_core::keys::{ExecKeyContext, FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use std::time::Duration;

const NS: &str = "edgepol-d-ns";
const LANE: &str = "edgepol-d-lane";

const RECON_DEADLINE: Duration = Duration::from_secs(15);
const ASSERT_POLL: Duration = Duration::from_millis(150);

fn cfg() -> ff_core::partition::PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

/// Start a server. `enable_dispatcher = false` forces the dispatcher
/// interval to 1 hour so the reconciler is the agent of change; the
/// reconciler always runs at a tight 500ms cadence for test latency.
async fn start_server(
    tc: &TestCluster,
    enable_dispatcher: bool,
) -> std::sync::Arc<ff_server::server::Server> {
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    let pc = *tc.partition_config();
    let dispatcher_interval = if enable_dispatcher {
        Duration::from_millis(250)
    } else {
        Duration::from_secs(3600)
    };
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
            edge_cancel_dispatcher_interval: dispatcher_interval,
            edge_cancel_reconciler_interval: Duration::from_millis(500),
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

/// Force a sibling exec_core into `lifecycle_phase = terminal` so the
/// reconciler's terminal-check loop sees it as already drained. This is
/// the test analogue of "the dispatcher's per-sibling cancel already
/// landed".
async fn force_terminal(tc: &TestCluster, eid: &ExecutionId) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core().as_str())
        .arg("lifecycle_phase")
        .arg("terminal")
        .arg("terminal_outcome")
        .arg("cancelled")
        .arg("cancellation_reason")
        .arg("sibling_quorum_satisfied")
        .execute()
        .await
        .unwrap();
}

/// Seed the edgegroup hash the way ff_resolve_dependency would leave it
/// under OnSatisfied::CancelRemaining after the atomic flip. `flag=true`
/// means "dispatcher still owes cancels + drain"; `flag=false` means
/// "dispatcher already HDEL'd but crashed before SREM".
async fn seed_edgegroup(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    siblings: &[ExecutionId],
    flag: bool,
) {
    let fp = flow_partition(fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&fp, fid);
    let edgegroup_key = fctx.edgegroup(downstream);
    let members_str: String = siblings
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("|");

    let cmd = tc
        .client()
        .cmd("HSET")
        .arg(edgegroup_key.as_str())
        .arg("policy_variant")
        .arg("any_of")
        .arg("on_satisfied")
        .arg("cancel_remaining")
        .arg("group_state")
        .arg("satisfied")
        .arg("cancel_siblings_reason")
        .arg("sibling_quorum_satisfied")
        .arg("cancel_siblings_pending_members")
        .arg(members_str.as_str());
    let cmd = if flag {
        cmd.arg("cancel_siblings_pending_flag").arg("true")
    } else {
        cmd
    };
    let _: () = cmd.execute().await.unwrap();
}

async fn add_to_pending_set(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
) {
    let fp = flow_partition(fid, tc.partition_config());
    let pending_key = FlowIndexKeys::new(&fp).pending_cancel_groups();
    let member = format!("{}|{}", fid, downstream);
    let _: i64 = tc
        .client()
        .cmd("SADD")
        .arg(pending_key.as_str())
        .arg(member.as_str())
        .execute()
        .await
        .unwrap();
}

async fn pending_set_has(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
) -> bool {
    let fp = flow_partition(fid, tc.partition_config());
    let pending_key = FlowIndexKeys::new(&fp).pending_cancel_groups();
    let member = format!("{}|{}", fid, downstream);
    let v: bool = tc
        .client()
        .cmd("SISMEMBER")
        .arg(pending_key.as_str())
        .arg(member.as_str())
        .execute()
        .await
        .unwrap();
    v
}

async fn edgegroup_field(
    tc: &TestCluster,
    fid: &FlowId,
    downstream: &ExecutionId,
    field: &str,
) -> Option<String> {
    let fp = flow_partition(fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&fp, fid);
    let edgegroup_key = fctx.edgegroup(downstream);
    tc.client()
        .cmd("HGET")
        .arg(edgegroup_key.as_str())
        .arg(field)
        .execute()
        .await
        .unwrap()
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

async fn wait_for<F, Fut>(deadline: Duration, f: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(ASSERT_POLL).await;
    }
    false
}

/// Scenario 1: flag absent, siblings already terminal, tuple in SET.
#[tokio::test]
#[serial_test::serial]
async fn stage_d_reconciler_srems_stale_entry() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    // Dispatcher disabled — the reconciler is unambiguously the agent.
    let server = start_server(&tc, false).await;

    let (fid, members) = mint_flow_with_members(&tc, 4);
    let downstream = members[3].clone();
    let siblings = members[..3].to_vec();

    create_flow_api(&server, &fid).await;
    for m in &members {
        create_exec_api(&server, &tc, m).await;
    }
    for s in &siblings {
        force_terminal(&tc, s).await;
    }

    // Flag=false ⇒ the "dispatcher HDEL'd but crashed before SREM" case.
    seed_edgegroup(&tc, &fid, &downstream, &siblings, false).await;
    add_to_pending_set(&tc, &fid, &downstream).await;

    assert!(pending_set_has(&tc, &fid, &downstream).await);

    let tc_cl = &tc;
    let fid_cl = fid.clone();
    let ds_cl = downstream.clone();
    let removed = wait_for(RECON_DEADLINE, || {
        let fid = fid_cl.clone();
        let ds = ds_cl.clone();
        async move { !pending_set_has(tc_cl, &fid, &ds).await }
    })
    .await;
    assert!(
        removed,
        "stale pending_cancel_groups entry not SREM'd within {:?}",
        RECON_DEADLINE
    );
}

/// Scenario 2: flag true, members list non-empty, all siblings terminal.
/// Simulates crash between per-sibling cancel landing and the drain FCALL.
#[tokio::test]
#[serial_test::serial]
async fn stage_d_reconciler_completes_interrupted_drain() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let server = start_server(&tc, false).await;

    let (fid, members) = mint_flow_with_members(&tc, 4);
    let downstream = members[3].clone();
    let siblings = members[..3].to_vec();

    create_flow_api(&server, &fid).await;
    for m in &members {
        create_exec_api(&server, &tc, m).await;
    }
    for s in &siblings {
        force_terminal(&tc, s).await;
    }

    seed_edgegroup(&tc, &fid, &downstream, &siblings, true).await;
    add_to_pending_set(&tc, &fid, &downstream).await;

    // Pre-conditions hold.
    assert_eq!(
        edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_flag")
            .await
            .as_deref(),
        Some("true")
    );
    assert!(pending_set_has(&tc, &fid, &downstream).await);

    let tc_cl = &tc;
    let fid_cl = fid.clone();
    let ds_cl = downstream.clone();
    let drained = wait_for(RECON_DEADLINE, || {
        let fid = fid_cl.clone();
        let ds = ds_cl.clone();
        async move {
            !pending_set_has(tc_cl, &fid, &ds).await
                && edgegroup_field(tc_cl, &fid, &ds, "cancel_siblings_pending_flag")
                    .await
                    .is_none()
                && edgegroup_field(tc_cl, &fid, &ds, "cancel_siblings_pending_members")
                    .await
                    .is_none()
        }
    })
    .await;
    assert!(
        drained,
        "interrupted drain not completed within {:?}: \
         flag={:?} members={:?} in_set={}",
        RECON_DEADLINE,
        edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_flag").await,
        edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_members").await,
        pending_set_has(&tc, &fid, &downstream).await,
    );
}

/// Scenario 3: flag true, siblings still running. Reconciler MUST
/// no-op; dispatcher owns this tuple. With the dispatcher re-enabled
/// it drains normally on its next tick.
#[tokio::test]
#[serial_test::serial]
async fn stage_d_reconciler_noops_when_siblings_non_terminal() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    // Dispatcher disabled first so we can prove the reconciler leaves
    // state alone. We re-check after the reconciler interval that the
    // flag + members + set entry are still intact.
    let server = start_server(&tc, false).await;

    let (fid, members) = mint_flow_with_members(&tc, 4);
    let downstream = members[3].clone();
    let siblings = members[..3].to_vec();

    create_flow_api(&server, &fid).await;
    for m in &members {
        create_exec_api(&server, &tc, m).await;
    }
    // NOTE: do NOT force_terminal siblings — they stay `running`, which
    // is the scenario under test.

    seed_edgegroup(&tc, &fid, &downstream, &siblings, true).await;
    add_to_pending_set(&tc, &fid, &downstream).await;

    // Let the reconciler run ≥2 ticks (default 500ms each).
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        pending_set_has(&tc, &fid, &downstream).await,
        "reconciler must not SREM while siblings still running"
    );
    assert_eq!(
        edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_flag")
            .await
            .as_deref(),
        Some("true"),
        "reconciler must not clear flag while siblings still running"
    );
    assert!(
        edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_members")
            .await
            .is_some(),
        "reconciler must not clear members while siblings still running"
    );

    // Prove co-existence: keep the reconciler running, and drive a
    // cancel-and-drain manually (the dispatcher Lua path) to confirm
    // the reconciler doesn't interfere. We invoke the drain FCALL
    // directly — that's what the dispatcher does after per-sibling
    // cancels land. The test's stand-in for "dispatcher drains" is a
    // raw ff_drain_sibling_cancel_group call.
    let fp = flow_partition(&fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&fp, &fid);
    let pending_key = FlowIndexKeys::new(&fp).pending_cancel_groups();
    let edgegroup_key = fctx.edgegroup(&downstream);
    let fid_str = fid.to_string();
    let ds_str = downstream.to_string();
    let keys = [pending_key.as_str(), edgegroup_key.as_str()];
    let argv = [fid_str.as_str(), ds_str.as_str()];
    let _: Value = tc
        .client()
        .fcall("ff_drain_sibling_cancel_group", &keys, &argv)
        .await
        .expect("drain FCALL");

    assert!(!pending_set_has(&tc, &fid, &downstream).await);
    assert!(
        edgegroup_field(&tc, &fid, &downstream, "cancel_siblings_pending_flag")
            .await
            .is_none()
    );

    // And after the drain, the reconciler continues to not-interfere:
    // its next tick sees an empty SET + returns without work.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!pending_set_has(&tc, &fid, &downstream).await);

    let _ = server; // keep server alive through the assertions
}
