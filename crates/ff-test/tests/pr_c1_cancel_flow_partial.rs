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
//!   cleanly). A second member id is created whose `attempt_hash` key
//!   is planted as a *string* (SET instead of HSET). Because the real
//!   execution's lifecycle_phase is `active` with a `current_attempt_index`
//!   set, the Lua `ff_cancel_execution` proceeds to `HSET attempt_hash …`
//!   which returns `WRONGTYPE` — a non-retryable transport error that
//!   bubbles out as `ServerError::Valkey(...)`. That is NOT a
//!   terminal-ack code, so the wait loop records one failed member.
//!
//!   Note: `execution_not_active` / `execution_not_found` are treated
//!   as idempotent ack-worthy success (matching the cancel-reconciler's
//!   behaviour), so a pure ghost-id injection would *not* produce a
//!   PartiallyCancelled outcome under the current (correct) contract.
//!
//! Run with: cargo test -p ff-test --test pr_c1_cancel_flow_partial -- \
//!           --test-threads=1

use ff_core::contracts::{
    AddExecutionToFlowArgs, CancelFlowArgs, CancelFlowResult,
    CreateExecutionArgs, CreateFlowArgs,
};
use ff_core::keys::{ExecKeyContext, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
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
        backend: ff_server::config::BackendKind::default(),
        postgres: Default::default(),
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

    // 2. Inject a broken member: create a second real execution, then
    //    corrupt its `attempt_hash` key by SET-ing it as a string. The
    //    Lua `ff_cancel_execution` proceeds past the lifecycle check
    //    (phase="active", operator_override bypasses lease checks) and
    //    hits `HSET K.attempt_hash …` because `current_attempt_index` is
    //    set — HSET on a string returns WRONGTYPE, the FCALL bubbles as
    //    `ServerError::Valkey(...)`, which is NOT a terminal-ack code
    //    and is therefore recorded as a failed member.
    let broken_eid = ExecutionId::for_flow(&flow_id, &config);
    assert_ne!(broken_eid, real_eid, "broken id should be distinct");
    server
        .create_execution(&CreateExecutionArgs {
            execution_id: broken_eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE),
            execution_kind: "broken-member".to_owned(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".to_owned()),
            priority: 0,
            creator_identity: "pr-c1".to_owned(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: ff_core::partition::execution_partition(&broken_eid, &config).index,
            now,
        })
        .await
        .expect("create_execution broken");
    server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: broken_eid.clone(),
            now,
        })
        .await
        .expect("add broken member");

    // Flip the broken execution to lifecycle_phase="active" with a
    // current_attempt_index so the cancel Lua takes the active-path
    // HSET on attempt_hash.
    let bpart = execution_partition(&broken_eid, &config);
    let bctx = ExecKeyContext::new(&bpart, &broken_eid);
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(bctx.core().as_str())
        .arg("lifecycle_phase")
        .arg("active")
        .arg("ownership_state")
        .arg("leased")
        .arg("current_attempt_index")
        .arg("1")
        .execute()
        .await
        .expect("HSET broken core active");

    // Plant the attempt_hash as a STRING (not a hash). Any HSET on it
    // will return WRONGTYPE.
    let attempt_key = bctx.attempt_hash(AttemptIndex::new(1));
    let _: String = tc
        .client()
        .cmd("SET")
        .arg(attempt_key.as_str())
        .arg("wrongtype-sentinel")
        .execute()
        .await
        .expect("SET attempt_hash as string");

    // Touch the flow's members set via the public key to make the
    // FlowKeyContext usage below not just for sanity — keep the
    // helper around for assertion lookups.
    let fpart = flow_partition(&flow_id, &config);
    let _fctx = FlowKeyContext::new(&fpart, &flow_id);

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
                broken_eid.to_string(),
                "failed id should be the broken member"
            );
        }
        other => panic!(
            "expected PartiallyCancelled (broken member cancel failed), got {other:?}"
        ),
    }

    // Sanity: the real member was still cancelled.
    assert_eq!(
        server.get_execution_state(&real_eid).await.unwrap(),
        PublicState::Cancelled,
        "real member should be cancelled even though the broken one failed"
    );

    server.shutdown().await;
}
