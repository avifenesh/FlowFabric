//! PR-C1 regression (#63): `cancel_flow_wait` must not report `Cancelled`
//! when one or more member cancellations failed.
//!
//! Pre-fix:
//!   The sync-wait branch swallows per-member errors (logs a warning and
//!   returns `CancelFlowResult::Cancelled` with the full member list),
//!   contradicting the `?wait=true` guarantee that every member is in a
//!   terminal state on return.
//!
//! Post-fix:
//!   When at least one member cancel fails, the call returns
//!   `CancelFlowResult::PartiallyCancelled` carrying the failed member
//!   id(s) so the caller can retry or alert.
//!
//! Failure injection:
//!   We add a real, valid member execution to the flow (which cancels
//!   cleanly), and SADD a *second* flow-partitioned id into the members
//!   set without calling `create_execution` for it. The Lua
//!   `ff_cancel_execution` for the ghost id returns non-success
//!   (`execution_not_found`), `cancel_member_execution` propagates the
//!   `ServerError`, and the wait loop observes one Ok and one Err.
//!
//! Run with: cargo test -p ff-test --test pr_c1_cancel_flow_partial -- \
//!           --test-threads=1

use ff_core::contracts::{
    AddExecutionToFlowArgs, CancelFlowArgs, CancelFlowResult,
    CreateExecutionArgs, CreateFlowArgs,
};
use ff_core::keys::FlowKeyContext;
use ff_core::partition::flow_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_server::config::ServerConfig;
use ff_server::server::Server;
use ff_test::fixtures::{TestCluster, TEST_PARTITION_CONFIG};

const LANE: &str = "pr-c1-cancel-lane";
const NS: &str = "pr-c1-cancel-ns";

async fn start_test_server() -> Server {
    let config = TEST_PARTITION_CONFIG;
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    let tls = std::env::var("FF_TLS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let cluster = std::env::var("FF_CLUSTER")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let server_config = ServerConfig {
        host,
        port,
        tls,
        cluster,
        partition_config: config,
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "0.0.0.0:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: config,
            lanes: vec![LaneId::new(LANE)],
            ..Default::default()
        },
        skip_library_load: true, // TestCluster::connect() already loaded
        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
    };
    Server::start(server_config).await.expect("Server::start")
}

#[tokio::test]
#[serial_test::serial]
async fn cancel_flow_wait_reports_partial_on_member_failure() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = start_test_server().await;
    let config = TEST_PARTITION_CONFIG;
    let now = TimestampMs::now();

    // 1. Create flow + one real member.
    let flow_id = FlowId::new();
    server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "pr-c1-partial".to_owned(),
            namespace: Namespace::new(NS),
            now,
        })
        .await
        .expect("create_flow");

    let real_eid = ExecutionId::for_flow(&flow_id, &config);
    server
        .create_execution(&CreateExecutionArgs {
            execution_id: real_eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE),
            execution_kind: "real-member".to_owned(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".to_owned()),
            priority: 0,
            creator_identity: "pr-c1".to_owned(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: ff_core::partition::execution_partition(&real_eid, &config).index,
            now,
        })
        .await
        .expect("create_execution real");
    server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: real_eid.clone(),
            now,
        })
        .await
        .expect("add real member");

    // 2. Inject a ghost member: construct a second flow-partitioned id and
    //    SADD it directly into the members set *without* creating its
    //    execution core. The Lua cancel call for it will return
    //    `execution_not_found`, causing cancel_member_execution to Err.
    let ghost_eid = ExecutionId::for_flow(&flow_id, &config);
    assert_ne!(ghost_eid, real_eid, "ghost id should be distinct");
    let fpart = flow_partition(&flow_id, &config);
    let fctx = FlowKeyContext::new(&fpart, &flow_id);
    let _: i64 = tc
        .client()
        .cmd("SADD")
        .arg(fctx.members().as_str())
        .arg(ghost_eid.to_string().as_str())
        .execute()
        .await
        .expect("SADD ghost");

    // 3. cancel_flow_wait — expect PartiallyCancelled with ghost_eid in
    //    failed_member_execution_ids.
    let result = server
        .cancel_flow_wait(&CancelFlowArgs {
            flow_id: flow_id.clone(),
            reason: "pr-c1-test".to_owned(),
            cancellation_policy: "cancel_all".to_owned(),
            now: TimestampMs::now(),
        })
        .await
        .expect("cancel_flow_wait should not error, it should return PartiallyCancelled");

    match &result {
        CancelFlowResult::PartiallyCancelled {
            cancellation_policy,
            member_execution_ids,
            failed_member_execution_ids,
        } => {
            assert_eq!(cancellation_policy, "cancel_all");
            assert_eq!(member_execution_ids.len(), 2);
            assert_eq!(
                failed_member_execution_ids.len(),
                1,
                "expected exactly one failed member, got {failed_member_execution_ids:?}"
            );
            assert_eq!(
                failed_member_execution_ids[0],
                ghost_eid.to_string(),
                "failed id should be the ghost member"
            );
        }
        other => panic!(
            "expected PartiallyCancelled (ghost member cancel failed), got {other:?}"
        ),
    }

    // Sanity: the real member was still cancelled.
    assert_eq!(
        server.get_execution_state(&real_eid).await.unwrap(),
        PublicState::Cancelled,
        "real member should be cancelled even though the ghost one failed"
    );

    server.shutdown().await;
}
