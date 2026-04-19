//! RFC-011 §7.3.1 atomicity tests for `ff_add_execution_to_flow`.
//!
//! Pre-phase-3: `add_execution_to_flow` was a two-phase
//! (FCALL-then-HSET) sequence that could orphan on crash. Phase-3
//! collapsed it into a single atomic FCALL with exec_core as KEYS[4].
//! These 5 tests pin the atomicity invariants and the validates-
//! before-writing discipline that makes the Lua-level rollback work.
//!
//! Run: `cargo test -p ff-test --test e2e_flow_atomicity -- --test-threads=1`

use ff_core::contracts::{AddExecutionToFlowArgs, CreateFlowArgs};
use ff_core::keys::ExecKeyContext;
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const NS: &str = "atomicity-ns";

fn mint_flow_and_exec(tc: &TestCluster) -> (FlowId, ExecutionId) {
    let flow_id = FlowId::new();
    let fp = flow_partition(&flow_id, tc.partition_config());
    let eid = tc.new_execution_id_on_partition(fp.index);
    (flow_id, eid)
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
        lanes: vec![LaneId::new("atomicity-lane")],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: pc,
            lanes: vec![LaneId::new("atomicity-lane")],
            ..Default::default()
        },
        skip_library_load: true,
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

async fn create_flow(
    server: &ff_server::server::Server,
    flow_id: &FlowId,
) {
    server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "test".into(),
            namespace: Namespace::new(NS),
            now: TimestampMs::now(),
        })
        .await
        .expect("create_flow");
}

async fn fcall_create_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
) {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = ff_core::keys::IndexKeys::new(&partition);
    let now = TimestampMs::now();
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&LaneId::new("atomicity-lane")),
        idx.all_executions(),
        idx.execution_deadline(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        "atomicity-lane".to_owned(),
        "test".to_owned(),
        "{}".to_owned(),
        "{}".to_owned(),
        String::new(),
        String::new(),
        "0".to_owned(),
        "tester".to_owned(),
        String::new(),
        String::new(),
        String::new(),
        now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tc.client()
        .fcall::<ferriskey::Value>("ff_create_execution", &kr, &ar)
        .await
        .expect("fcall ff_create_execution");
}

async fn hget_flow_id(tc: &TestCluster, eid: &ExecutionId) -> Option<String> {
    let partition = execution_partition(eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let v: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(ctx.core())
        .arg("flow_id")
        .execute()
        .await
        .expect("HGET flow_id");
    v.filter(|s| !s.is_empty())
}

async fn sismember_flow(
    tc: &TestCluster,
    flow_id: &FlowId,
    eid: &ExecutionId,
) -> bool {
    let fp = flow_partition(flow_id, tc.partition_config());
    let members_key = format!(
        "ff:flow:{tag}:{fid}:members",
        tag = fp.hash_tag(),
        fid = flow_id
    );
    let v: bool = tc
        .client()
        .cmd("SISMEMBER")
        .arg(&members_key)
        .arg(eid.to_string())
        .execute()
        .await
        .expect("SISMEMBER");
    v
}

#[tokio::test]
#[serial_test::serial]
async fn test_atomicity_happy_path_commits_both_writes() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let (flow_id, eid) = mint_flow_and_exec(&tc);
    let server = start_server(&tc).await;

    create_flow(&server, &flow_id).await;
    fcall_create_execution(&tc, &eid).await;

    assert_eq!(hget_flow_id(&tc, &eid).await, None);
    assert!(!sismember_flow(&tc, &flow_id, &eid).await);

    server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("add_execution_to_flow");

    assert_eq!(
        hget_flow_id(&tc, &eid).await,
        Some(flow_id.to_string()),
        "exec_core.flow_id must be stamped atomically with membership"
    );
    assert!(
        sismember_flow(&tc, &flow_id, &eid).await,
        "exec must be in members_set atomically with flow_id stamp"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_atomicity_flow_not_found_commits_nothing() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let (flow_id, eid) = mint_flow_and_exec(&tc);
    let server = start_server(&tc).await;

    fcall_create_execution(&tc, &eid).await;

    let err = server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect_err("add_execution_to_flow must error when flow does not exist");
    let msg = err.to_string();
    assert!(
        msg.contains("flow_not_found") || msg.contains("flow not found"),
        "error must name flow_not_found path; got {msg}"
    );

    assert_eq!(
        hget_flow_id(&tc, &eid).await,
        None,
        "exec_core.flow_id must NOT be stamped when the Lua early-return fires"
    );
    assert!(
        !sismember_flow(&tc, &flow_id, &eid).await,
        "exec must NOT be in members_set when the Lua early-return fires"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_atomicity_repeat_call_is_idempotent() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let (flow_id, eid) = mint_flow_and_exec(&tc);
    let server = start_server(&tc).await;

    create_flow(&server, &flow_id).await;
    fcall_create_execution(&tc, &eid).await;

    server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("first add");

    server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("second add");

    let fp = flow_partition(&flow_id, tc.partition_config());
    let flow_core_key = format!(
        "ff:flow:{tag}:{fid}:core",
        tag = fp.hash_tag(),
        fid = flow_id
    );
    let nc: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(&flow_core_key)
        .arg("node_count")
        .execute()
        .await
        .expect("HGET node_count");
    assert_eq!(nc.as_deref(), Some("1"), "node_count must not double-increment");

    assert_eq!(hget_flow_id(&tc, &eid).await, Some(flow_id.to_string()));
}

#[tokio::test]
#[serial_test::serial]
async fn test_atomicity_concurrent_distinct_execs() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let flow_id = FlowId::new();
    let fp = flow_partition(&flow_id, tc.partition_config());
    let eid_a = tc.new_execution_id_on_partition(fp.index);
    let eid_b = tc.new_execution_id_on_partition(fp.index);

    let server = start_server(&tc).await;
    create_flow(&server, &flow_id).await;
    fcall_create_execution(&tc, &eid_a).await;
    fcall_create_execution(&tc, &eid_b).await;

    let fa = flow_id.clone();
    let fb = flow_id.clone();
    let ea = eid_a.clone();
    let eb = eid_b.clone();
    let sa = server.clone();
    let sb = server.clone();
    let (ra, rb) = tokio::join!(
        async move {
            sa.add_execution_to_flow(&AddExecutionToFlowArgs {
                flow_id: fa,
                execution_id: ea,
                now: TimestampMs::now(),
            })
            .await
        },
        async move {
            sb.add_execution_to_flow(&AddExecutionToFlowArgs {
                flow_id: fb,
                execution_id: eb,
                now: TimestampMs::now(),
            })
            .await
        },
    );
    ra.expect("concurrent add a");
    rb.expect("concurrent add b");

    assert!(sismember_flow(&tc, &flow_id, &eid_a).await);
    assert!(sismember_flow(&tc, &flow_id, &eid_b).await);
    assert_eq!(hget_flow_id(&tc, &eid_a).await, Some(flow_id.to_string()));
    assert_eq!(hget_flow_id(&tc, &eid_b).await, Some(flow_id.to_string()));

    let flow_core_key = format!(
        "ff:flow:{tag}:{fid}:core",
        tag = fp.hash_tag(),
        fid = flow_id
    );
    let nc: Option<String> = tc
        .client()
        .cmd("HGET")
        .arg(&flow_core_key)
        .arg("node_count")
        .execute()
        .await
        .expect("HGET node_count");
    assert_eq!(
        nc.as_deref(),
        Some("2"),
        "concurrent adds must not lose an update"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_atomicity_flow_id_visible_to_subsequent_reads() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let (flow_id, eid) = mint_flow_and_exec(&tc);
    let server = start_server(&tc).await;

    create_flow(&server, &flow_id).await;
    fcall_create_execution(&tc, &eid).await;

    server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("add_execution_to_flow");

    for attempt in 0..3 {
        assert_eq!(
            hget_flow_id(&tc, &eid).await,
            Some(flow_id.to_string()),
            "HGET attempt {attempt} must see the flow_id stamp immediately"
        );
    }
}

/// Cross-review YELLOW 2 (W1 + W3 convergent): pre-flight
/// partition-mismatch check — a wrong-partition caller must see a
/// typed `ServerError::PartitionMismatch` instead of a raw Valkey
/// CROSSSLOT / Lua-lib error.
///
/// Deliberately mints exec on a DIFFERENT partition than the flow.
/// Under the post-RFC-011 §7.3 co-location contract this is a
/// caller-side mint error; `add_execution_to_flow` catches it at
/// the API boundary before the FCALL runs.
#[tokio::test]
#[serial_test::serial]
async fn test_atomicity_partition_mismatch_pre_flight_check() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Pick a FlowId whose partition index we can compute, then mint
    // the exec on a DIFFERENT partition.
    let flow_id = FlowId::new();
    let fp = flow_partition(&flow_id, tc.partition_config());
    let wrong_partition = (fp.index + 1) % tc.partition_config().num_flow_partitions;
    let eid = tc.new_execution_id_on_partition(wrong_partition);

    let server = start_server(&tc).await;
    create_flow(&server, &flow_id).await;
    fcall_create_execution(&tc, &eid).await;

    let err = server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect_err("wrong-partition exec must fail at pre-flight");

    // Typed PartitionMismatch — not CROSSSLOT / Lua-lib / other raw
    // downstream error.
    assert!(
        matches!(err, ff_server::server::ServerError::PartitionMismatch(_)),
        "expected ServerError::PartitionMismatch, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("partition"),
        "error message must mention partition: {msg}"
    );

    // Neither write should have happened (pre-flight guards run
    // before the FCALL).
    assert_eq!(
        hget_flow_id(&tc, &eid).await,
        None,
        "pre-flight rejection must not mutate exec_core"
    );
    assert!(
        !sismember_flow(&tc, &flow_id, &eid).await,
        "pre-flight rejection must not add to members_set"
    );
}
