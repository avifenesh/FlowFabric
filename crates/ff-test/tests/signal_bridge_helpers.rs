//! Integration coverage for `ff_sdk::signal_bridge::verify_and_deliver`.
//!
//! Exercises the three branches cairn's signal-bridge consumers care
//! about — UnknownWaitpoint, TokenMismatch, and the happy
//! verify-then-deliver path — against a live Valkey fixture. Seeding
//! mirrors `engine_backend_deliver_signal.rs`.
//!
//! Run with: cargo test -p ff-test --test signal_bridge_helpers -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_sdk::signal_bridge::{self, SignalBridgeError};
use ff_sdk::task::{Signal, SignalOutcome};
use ff_test::fixtures::TestCluster;

const LANE: &str = "sbh-lane";
const NS: &str = "sbh-ns";
const WORKER: &str = "sbh-worker";
const WORKER_INST: &str = "sbh-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

fn backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), config())
}

async fn build_worker(suffix: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: Some(ff_test::fixtures::backend_config_from_env()),
        worker_id: WorkerId::new(format!("{WORKER}-{suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("{WORKER_INST}-{suffix}")),
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

fn callback_signal(token: WaitpointToken) -> Signal {
    Signal {
        signal_name: "sbh_signal".to_owned(),
        signal_category: "external".to_owned(),
        payload: Some(b"{\"ok\":true}".to_vec()),
        source_type: "webhook".to_owned(),
        source_identity: "sbh-tester".to_owned(),
        idempotency_key: None,
        waitpoint_token: token,
    }
}

async fn create_and_claim(tc: &TestCluster, eid: &ExecutionId) -> (String, String, String) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

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
        "sbh".to_owned(),
        "0".to_owned(),
        "sbh-runner".to_owned(),
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

    let grant_keys: Vec<String> = vec![ctx.core(), ctx.claim_grant(), idx.lane_eligible(&lane_id)];
    let grant_args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = grant_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = grant_args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_issue_claim_grant", &kr, &ar)
        .await
        .expect("ff_issue_claim_grant");

    let lease_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let att_idx = AttemptIndex::new(0);
    let lease_ttl_ms: u64 = 30_000;
    let renew_before = lease_ttl_ms * 2 / 3;
    let claim_keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        ctx.attempt_hash(att_idx),
        ctx.attempt_usage(att_idx),
        ctx.attempt_policy(att_idx),
        ctx.attempts(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];
    let claim_args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        lease_id.clone(),
        lease_ttl_ms.to_string(),
        renew_before.to_string(),
        attempt_id.clone(),
        "{}".to_owned(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = claim_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = claim_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_claim_execution", &kr, &ar)
        .await
        .expect("ff_claim_execution");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("claim: expected Array"),
    };
    let epoch = match arr.get(3) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => panic!("claim: no epoch"),
    };
    (lease_id, epoch, attempt_id)
}

async fn suspend_and_get_token(
    tc: &TestCluster,
    eid: &ExecutionId,
    lease_id: &str,
    epoch: &str,
    attempt_id: &str,
) -> (WaitpointId, WaitpointToken) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("sbh:{wp_uuid}");

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        idx.suspension_timeout(),
        idx.pending_waitpoint_expiry(),
        idx.lane_active(&lane_id),
        idx.lane_suspended(&lane_id),
        ctx.waitpoints(),
        ctx.waitpoint_condition(&wp_id),
        idx.attempt_timeout(),
        idx.waitpoint_hmac_secrets(),
    ];
    let resume_condition = serde_json::json!({
        "condition_type": "signal_set",
        "required_signal_names": ["sbh_signal"],
        "signal_match_mode": "any",
        "minimum_signal_count": 1,
        "timeout_behavior": "fail",
        "allow_operator_override": true,
    })
    .to_string();
    let resume_policy = serde_json::json!({
        "resume_target": "runnable",
        "close_waitpoint_on_resume": true,
        "consume_matched_signals": true,
        "retain_signal_buffer_until_closed": true,
    })
    .to_string();

    let args: Vec<String> = vec![
        eid.to_string(),
        "0".to_owned(),
        attempt_id.to_owned(),
        lease_id.to_owned(),
        epoch.to_owned(),
        uuid::Uuid::new_v4().to_string(),
        wp_id.to_string(),
        wp_key,
        "waiting_for_signal".to_owned(),
        "worker".to_owned(),
        String::new(),
        resume_condition,
        resume_policy,
        String::new(),
        "0".to_owned(),
        "fail".to_owned(),
        "1000".to_owned(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_suspend_execution", &kr, &ar)
        .await
        .expect("ff_suspend_execution");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("suspend: expected Array"),
    };
    let token = match arr.get(5) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        _ => panic!("suspend: no token: {raw:?}"),
    };
    (wp_id, WaitpointToken::new(token))
}

fn tamper(t: &WaitpointToken) -> WaitpointToken {
    let mut chars: Vec<char> = t.as_str().chars().collect();
    let last = chars.last_mut().expect("non-empty token");
    *last = match *last {
        'a' => 'b',
        '0' => '1',
        _ => '0',
    };
    WaitpointToken::new(chars.into_iter().collect::<String>())
}

#[tokio::test]
#[serial_test::serial]
async fn verify_and_deliver_happy_path_resumes_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, token) = suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let backend = backend(&tc);
    let worker = build_worker("happy").await;

    let outcome = signal_bridge::verify_and_deliver(
        backend.as_ref(),
        &worker,
        &eid,
        &wp_id,
        &token,
        callback_signal(token.clone()),
    )
    .await
    .expect("verify_and_deliver should succeed on valid token");

    match outcome {
        SignalOutcome::TriggeredResume { .. } => {}
        other => panic!("expected TriggeredResume on first valid delivery, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn verify_and_deliver_rejects_tampered_token() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, token) = suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let backend = backend(&tc);
    let worker = build_worker("tamper").await;

    let bad = tamper(&token);
    let err = signal_bridge::verify_and_deliver(
        backend.as_ref(),
        &worker,
        &eid,
        &wp_id,
        &bad,
        callback_signal(bad.clone()),
    )
    .await
    .expect_err("tampered token must be rejected");

    match err {
        SignalBridgeError::TokenMismatch => {}
        other => panic!("expected TokenMismatch, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn verify_and_deliver_reports_unknown_waitpoint() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Create an execution but do NOT suspend: no waitpoint exists,
    // so `read_waitpoint_token` returns `Ok(None)`.
    let eid = tc.new_execution_id();
    let _ = create_and_claim(&tc, &eid).await;

    let backend = backend(&tc);
    let worker = build_worker("unknown").await;

    let missing_wp = WaitpointId::new();
    let any_token = WaitpointToken::new("k1:deadbeef".to_owned());

    let err = signal_bridge::verify_and_deliver(
        backend.as_ref(),
        &worker,
        &eid,
        &missing_wp,
        &any_token,
        callback_signal(any_token.clone()),
    )
    .await
    .expect_err("unknown waitpoint must be rejected");

    match err {
        SignalBridgeError::UnknownWaitpoint => {}
        other => panic!("expected UnknownWaitpoint, got {other:?}"),
    }
}
