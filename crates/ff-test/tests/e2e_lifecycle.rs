//! End-to-end integration tests for the FlowFabric Phase 1 lifecycle.
//!
//! Requires a live Valkey server. Uses raw FCALL calls to exercise the
//! Lua functions directly. Once typed ff-script wrappers are ready,
//! these tests serve as the ground truth for wrapper behavior.
//!
//! Run with: cargo test -p ff-test --test e2e_lifecycle

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::assertions::*;
use ff_test::fixtures::TestCluster;

// ─── Helper: build a small partition config for tests ───

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

// ─── Helper: FCALL wrappers for raw calls ───

/// Call ff_create_execution and return (execution_id, public_state).
async fn fcall_create_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    namespace: &str,
    lane: &str,
    kind: &str,
    priority: i32,
) -> (String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    let lane_id = LaneId::new(lane);
    let eligible_key = idx.lane_eligible(&lane_id);

    // KEYS (8): core, payload, policy, tags, eligible_zset, idem_key, deadline_zset, all_exec
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        eligible_key,
        ctx.noop(), // idem_key placeholder (must share {p:N} hash tag for cluster)
        idx.execution_deadline(),
        idx.all_executions(),
    ];

    // ARGV (13): eid, namespace, lane, kind, priority, creator, policy_json,
    //            input_payload, delay_until, dedup_ttl_ms, tags_json,
    //            execution_deadline_at, partition_id
    let args: Vec<String> = vec![
        eid.to_string(),
        namespace.to_owned(),
        lane.to_owned(),
        kind.to_owned(),
        priority.to_string(),
        "e2e-test".to_owned(),
        "{}".to_owned(),         // policy_json
        r#"{"test":true}"#.to_owned(), // input_payload
        String::new(),           // delay_until
        String::new(),           // dedup_ttl_ms
        "{}".to_owned(),         // tags_json
        String::new(),           // execution_deadline_at
        partition.index.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_create_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_execution failed");

    parse_ok_fields(&raw, "ff_create_execution")
}

/// Call ff_create_execution with a delay.
#[allow(dead_code)]
async fn fcall_create_delayed_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    namespace: &str,
    lane: &str,
    delay_until_ms: i64,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    let lane_id = LaneId::new(lane);
    let delayed_key = idx.lane_delayed(&lane_id);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        delayed_key, // scheduling_zset = delayed for this case
        String::new(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];

    let args: Vec<String> = vec![
        eid.to_string(),
        namespace.to_owned(),
        lane.to_owned(),
        "delayed_test".to_owned(),
        "0".to_owned(), // priority
        "e2e-test".to_owned(),
        "{}".to_owned(),
        r#"{"delayed":true}"#.to_owned(),
        delay_until_ms.to_string(), // delay_until
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_create_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_execution (delayed) failed");

    let (_, status) = parse_ok_fields(&raw, "ff_create_execution (delayed)");
    assert_eq!(status, "delayed");
}

/// Call ff_issue_claim_grant. Returns () on success.
async fn fcall_issue_claim_grant(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_id: &str,
    worker_instance_id: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    // KEYS (3): exec_core, claim_grant, eligible_zset
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
    ];

    // ARGV (9): eid, worker_id, worker_instance_id, lane, cap_hash, grant_ttl,
    //           route_json, admission, worker_capabilities_csv
    let args: Vec<String> = vec![
        eid.to_string(),
        worker_id.to_owned(),
        worker_instance_id.to_owned(),
        lane.to_owned(),
        String::new(),        // capability_hash
        "5000".to_owned(),    // grant_ttl_ms
        String::new(),        // route_snapshot_json
        String::new(),        // admission_summary
        String::new(),        // worker_capabilities_csv (empty: match-any path)
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_issue_claim_grant", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_issue_claim_grant failed");

    assert_ok(&raw, "ff_issue_claim_grant");
}

/// Call ff_claim_execution. Returns (lease_id, lease_epoch, attempt_index, attempt_id).
async fn fcall_claim_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_id: &str,
    worker_instance_id: &str,
    lease_ttl_ms: u64,
) -> (String, String, String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let wid = WorkerInstanceId::new(worker_instance_id);

    let lease_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();

    // Read total_attempt_count to derive next attempt index.
    // The Lua uses total_attempt_count as the new index, so the caller must
    // construct attempt_hash/usage/policy keys at that index.
    let total_str: Option<String> = tc
        .hget(&ctx.core(), "total_attempt_count")
        .await;
    let next_idx = total_str
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let att_idx = AttemptIndex::new(next_idx);

    // KEYS (14): exec_core, claim_grant, eligible_zset, lease_expiry,
    //            worker_leases, attempt_hash, attempt_usage, attempt_policy,
    //            attempts_zset, lease_current, lease_history, active_index,
    //            attempt_timeout, execution_deadline
    let keys: Vec<String> = vec![
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

    // ARGV (12): eid, worker_id, worker_instance_id, lane, cap_hash,
    //            lease_id, lease_ttl_ms, renew_before_ms, attempt_id,
    //            attempt_policy_json, attempt_timeout_ms, execution_deadline_at
    let renew_before = lease_ttl_ms * 2 / 3;
    let args: Vec<String> = vec![
        eid.to_string(),
        worker_id.to_owned(),
        worker_instance_id.to_owned(),
        lane.to_owned(),
        String::new(),            // capability_hash
        lease_id.clone(),
        lease_ttl_ms.to_string(),
        renew_before.to_string(),
        attempt_id.clone(),
        "{}".to_owned(),          // attempt_policy_json
        String::new(),            // attempt_timeout_ms
        String::new(),            // execution_deadline_at
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_claim_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_claim_execution failed");

    // Parse: {1, "OK", lease_id, epoch, expires_at, attempt_id, attempt_index, attempt_type}
    let arr = expect_success_array(&raw, "ff_claim_execution");
    let lease_id_ret = field_str(arr, 2);
    let epoch_ret = field_str(arr, 3);
    let attempt_id_ret = field_str(arr, 5);
    let attempt_idx_ret = field_str(arr, 6);

    (lease_id_ret, epoch_ret, attempt_idx_ret, attempt_id_ret)
}

/// Call ff_renew_lease. Returns new_expires_at.
async fn fcall_renew_lease(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: &str,
    attempt_id: &str,
    lease_id: &str,
    lease_epoch: &str,
    lease_ttl_ms: u64,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    // KEYS (4): exec_core, lease_current, lease_history, lease_expiry
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
    ];

    // ARGV (7): eid, attempt_index, attempt_id, lease_id, lease_epoch, ttl, grace
    let args: Vec<String> = vec![
        eid.to_string(),
        attempt_index.to_owned(),
        attempt_id.to_owned(),
        lease_id.to_owned(),
        lease_epoch.to_owned(),
        lease_ttl_ms.to_string(),
        "5000".to_owned(), // grace_ms
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_renew_lease", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_renew_lease failed");

    assert_ok(&raw, "ff_renew_lease");
}

/// Call ff_complete_execution.
async fn fcall_complete_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_instance_id: &str,
    lease_id: &str,
    lease_epoch: &str,
    attempt_id: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let wid = WorkerInstanceId::new(worker_instance_id);

    let att_idx = AttemptIndex::new(0);

    // KEYS (12): exec_core, attempt_hash, lease_expiry, worker_leases,
    //            terminal_zset, lease_current, lease_history, active_index,
    //            stream_meta, result_key, attempt_timeout, execution_deadline
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(att_idx),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        idx.lane_terminal(&lane_id),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&lane_id),
        ctx.stream_meta(att_idx),
        ctx.result(),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];

    // ARGV (5): eid, lease_id, lease_epoch, attempt_id, result_payload
    let args: Vec<String> = vec![
        eid.to_string(),
        lease_id.to_owned(),
        lease_epoch.to_owned(),
        attempt_id.to_owned(),
        r#"{"status":"ok"}"#.to_owned(), // result_payload
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_complete_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_complete_execution failed");

    assert_ok(&raw, "ff_complete_execution");
}

/// Call ff_cancel_execution with operator override (no lease required).
async fn fcall_cancel_execution_operator(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    reason: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    let att_idx = AttemptIndex::new(0);
    let wp_id = WaitpointId::new(); // placeholder

    // KEYS (21) for ff_cancel_execution
    let keys: Vec<String> = vec![
        ctx.core(),                          // 1
        ctx.attempt_hash(att_idx),           // 2
        ctx.stream_meta(att_idx),            // 3
        ctx.lease_current(),                 // 4
        ctx.lease_history(),                 // 5
        idx.lease_expiry(),                  // 6
        idx.worker_leases(&WorkerInstanceId::new("")), // 7
        ctx.suspension_current(),            // 8
        ctx.waitpoint(&wp_id),               // 9
        ctx.waitpoint_condition(&wp_id),     // 10
        idx.suspension_timeout(),            // 11
        idx.lane_terminal(&lane_id),         // 12
        idx.attempt_timeout(),               // 13
        idx.execution_deadline(),            // 14
        idx.lane_eligible(&lane_id),         // 15
        idx.lane_delayed(&lane_id),          // 16
        idx.lane_blocked_dependencies(&lane_id), // 17
        idx.lane_blocked_budget(&lane_id),   // 18
        idx.lane_blocked_quota(&lane_id),    // 19
        idx.lane_blocked_route(&lane_id),    // 20
        idx.lane_blocked_operator(&lane_id), // 21
    ];

    // ARGV (5): eid, reason, source, lease_id, lease_epoch
    let args: Vec<String> = vec![
        eid.to_string(),
        reason.to_owned(),
        "operator_override".to_owned(),
        String::new(), // no lease_id needed for operator override
        String::new(), // no lease_epoch
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_cancel_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_cancel_execution failed");

    assert_ok(&raw, "ff_cancel_execution");
}

/// Call ff_delay_execution.
async fn fcall_delay_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    lease_id: &str,
    lease_epoch: &str,
    attempt_id: &str,
    delay_until_ms: i64,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    let att_idx = AttemptIndex::new(0);

    // KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
    //           lease_expiry, worker_leases, active_index, delayed_zset, attempt_timeout
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(att_idx),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&WorkerInstanceId::new("")),
        idx.lane_active(&lane_id),
        idx.lane_delayed(&lane_id),
        idx.attempt_timeout(),
    ];

    // ARGV (5): eid, lease_id, lease_epoch, attempt_id, delay_until
    let args: Vec<String> = vec![
        eid.to_string(),
        lease_id.to_owned(),
        lease_epoch.to_owned(),
        attempt_id.to_owned(),
        delay_until_ms.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_delay_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_delay_execution failed");

    assert_ok(&raw, "ff_delay_execution");
}

/// Call ff_change_priority.
async fn fcall_change_priority(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    new_priority: i32,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    // KEYS (2): exec_core, eligible_zset
    let keys: Vec<String> = vec![ctx.core(), idx.lane_eligible(&lane_id)];

    // ARGV (2): eid, new_priority
    let args: Vec<String> = vec![eid.to_string(), new_priority.to_string()];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_change_priority", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_change_priority failed");

    assert_ok(&raw, "ff_change_priority");
}

// ─── FCALL result parsing helpers ───

/// Check that a raw FCALL result is an error with the given code.
fn assert_err(raw: &Value, expected_code: &str, function_name: &str) {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => panic!("{function_name}: expected Array, got {raw:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        other => panic!("{function_name}: expected Int status, got {other:?}"),
    };
    assert_eq!(
        status, 0,
        "{function_name}: expected status=0 (error), got status={status}. Full: {raw:?}"
    );
    let code = field_str(arr, 1);
    assert_eq!(
        code, expected_code,
        "{function_name}: expected error code '{expected_code}', got '{code}'. Full: {raw:?}"
    );
}

fn assert_ok(raw: &Value, function_name: &str) {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => panic!("{function_name}: expected Array, got {raw:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        other => panic!("{function_name}: expected Int status, got {other:?}"),
    };
    assert_eq!(
        status, 1,
        "{function_name}: expected status=1 (OK), got status={status}. Full: {raw:?}"
    );
}

fn parse_ok_fields(raw: &Value, function_name: &str) -> (String, String) {
    let arr = expect_success_array(raw, function_name);
    (field_str(arr, 2), field_str(arr, 3))
}

fn expect_success_array<'a>(
    raw: &'a Value,
    function_name: &str,
) -> &'a Vec<Result<Value, ferriskey::Error>> {
    let arr = match raw {
        Value::Array(arr) => arr,
        _ => panic!("{function_name}: expected Array, got {raw:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        other => panic!("{function_name}: expected Int status, got {other:?}"),
    };
    assert_eq!(
        status, 1,
        "{function_name}: expected status=1, got {status}. Full: {raw:?}"
    );
    arr
}

fn field_str(arr: &[Result<Value, ferriskey::Error>], index: usize) -> String {
    match arr.get(index) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => String::new(),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// TESTS
// ═══════════════════════════════════════════════════════════════════════

const LANE: &str = "e2e-test-lane";
const NS: &str = "e2e-ns";
const WORKER: &str = "e2e-worker";
const WORKER_INST: &str = "e2e-worker-inst-1";

/// Full lifecycle: create → claim → renew → complete → verify terminal.
#[tokio::test]
#[serial_test::serial]
async fn test_create_claim_complete_lifecycle() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    // 1. Create execution
    let (ret_eid, public_state) = fcall_create_execution(&tc, &eid, NS, LANE, "llm_call", 0).await;
    assert_eq!(ret_eid, eid.to_string());
    assert_eq!(public_state, "waiting");

    // 2. Verify: runnable/waiting, in eligible index
    assert_execution_state(
        &tc,
        &eid,
        &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
            ownership_state: Some(ff_core::state::OwnershipState::Unowned),
            eligibility_state: Some(ff_core::state::EligibilityState::EligibleNow),
            public_state: Some(ff_core::state::PublicState::Waiting),
            ..Default::default()
        },
    )
    .await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_in_all_executions(&tc, &eid).await;

    // 3. Issue claim grant
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;

    // 4. Claim execution
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    assert!(!lease_id.is_empty(), "lease_id should be non-empty");
    assert_eq!(epoch, "1"); // first claim = epoch 1
    assert_eq!(att_idx, "0"); // first attempt = index 0

    // 5. Verify: active/leased, lease exists, attempt exists
    assert_execution_state(
        &tc,
        &eid,
        &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
            ownership_state: Some(ff_core::state::OwnershipState::Leased),
            eligibility_state: Some(ff_core::state::EligibilityState::NotApplicable),
            blocking_reason: Some(ff_core::state::BlockingReason::None),
            attempt_state: Some(ff_core::state::AttemptState::RunningAttempt),
            public_state: Some(ff_core::state::PublicState::Active),
            ..Default::default()
        },
    )
    .await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;
    assert_lease_active(&tc, &eid).await;
    assert_attempt_exists(&tc, &eid, AttemptIndex::new(0)).await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "started").await;
    assert_attempt_type(&tc, &eid, AttemptIndex::new(0), "initial").await;

    // Verify index membership: NOT in eligible, IS in active
    assert_not_in_eligible(&tc, &eid, &LaneId::new(LANE)).await;
    assert_in_active(&tc, &eid, &LaneId::new(LANE)).await;

    // 6. Renew lease
    fcall_renew_lease(&tc, &eid, &att_idx, &attempt_id, &lease_id, &epoch, 30_000).await;

    // 7. Complete execution
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lease_id, &epoch, &attempt_id).await;

    // 8. Verify: terminal/completed
    assert_execution_state(
        &tc,
        &eid,
        &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
            ownership_state: Some(ff_core::state::OwnershipState::Unowned),
            terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
            attempt_state: Some(ff_core::state::AttemptState::AttemptTerminal),
            public_state: Some(ff_core::state::PublicState::Completed),
            ..Default::default()
        },
    )
    .await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;
    assert_lease_absent(&tc, &eid).await;
    assert_in_terminal(&tc, &eid, &LaneId::new(LANE)).await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "ended_success").await;

    // Verify result was stored
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let result: Option<String> = tc.client().get(&ctx.result()).await.unwrap();
    assert_eq!(result.as_deref(), Some(r#"{"status":"ok"}"#));
}

/// Create, don't claim, cancel from waiting. Verify terminal/cancelled.
#[tokio::test]
#[serial_test::serial]
async fn test_create_cancel_from_waiting() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create
    fcall_create_execution(&tc, &eid, NS, LANE, "cancel_test", 0).await;

    // Cancel with operator override (no lease needed for runnable)
    fcall_cancel_execution_operator(&tc, &eid, LANE, "test cancellation").await;

    // Verify: terminal/cancelled
    assert_execution_state(
        &tc,
        &eid,
        &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
            terminal_outcome: Some(ff_core::state::TerminalOutcome::Cancelled),
            public_state: Some(ff_core::state::PublicState::Cancelled),
            ..Default::default()
        },
    )
    .await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;

    // Should be in terminal index, NOT in eligible
    assert_in_terminal(&tc, &eid, &LaneId::new(LANE)).await;
    assert_not_in_eligible(&tc, &eid, &LaneId::new(LANE)).await;
    assert_lease_absent(&tc, &eid).await;
}

/// Create, claim, cancel with operator_override from active. Verify terminal, lease cleared.
#[tokio::test]
#[serial_test::serial]
async fn test_create_claim_cancel_from_active() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "active_cancel", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (_lease_id, _epoch, _, _) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Verify active
    assert_lease_active(&tc, &eid).await;

    // Cancel with operator override
    fcall_cancel_execution_operator(&tc, &eid, LANE, "operator cancelled active").await;

    // Verify: terminal/cancelled, lease gone
    assert_execution_state(
        &tc,
        &eid,
        &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
            terminal_outcome: Some(ff_core::state::TerminalOutcome::Cancelled),
            public_state: Some(ff_core::state::PublicState::Cancelled),
            ..Default::default()
        },
    )
    .await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_lease_absent(&tc, &eid).await;
    assert_in_terminal(&tc, &eid, &LaneId::new(LANE)).await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "ended_cancelled").await;
}

/// Create, claim, delay. Verify delayed index, lease released, attempt paused.
#[tokio::test]
#[serial_test::serial]
async fn test_delay_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "delay_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, _, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Delay to far future
    let delay_until = TimestampMs::now().0 + 60_000; // 60s from now
    fcall_delay_execution(&tc, &eid, LANE, &lease_id, &epoch, &attempt_id, delay_until).await;

    // Verify: runnable/delayed, lease gone, attempt paused
    assert_execution_state(
        &tc,
        &eid,
        &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
            ownership_state: Some(ff_core::state::OwnershipState::Unowned),
            eligibility_state: Some(ff_core::state::EligibilityState::NotEligibleUntilTime),
            blocking_reason: Some(ff_core::state::BlockingReason::WaitingForDelay),
            attempt_state: Some(ff_core::state::AttemptState::AttemptInterrupted),
            public_state: Some(ff_core::state::PublicState::Delayed),
            ..Default::default()
        },
    )
    .await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;
    assert_lease_absent(&tc, &eid).await;

    // Should be in delayed index, NOT in eligible or active
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    assert_in_index(&tc, &idx.lane_delayed(&lane_id), &eid).await;
    assert_not_in_eligible(&tc, &eid, &lane_id).await;

    // Attempt should be suspended (paused, not ended)
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "suspended").await;
}

/// Create, verify eligible score, change_priority, verify new score.
#[tokio::test]
#[serial_test::serial]
async fn test_change_priority() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    // Create with priority 0
    fcall_create_execution(&tc, &eid, NS, LANE, "priority_test", 0).await;

    // Read score from eligible ZSET
    let partition = execution_partition(&eid, &config);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let eligible_key = idx.lane_eligible(&lane_id);

    let score_before = tc.zscore(&eligible_key, &eid.to_string()).await;
    assert!(score_before.is_some(), "should be in eligible set");
    let score_before = score_before.unwrap();

    // Change priority to 10 (higher = more urgent = lower score)
    fcall_change_priority(&tc, &eid, LANE, 10).await;

    let score_after = tc.zscore(&eligible_key, &eid.to_string()).await;
    assert!(score_after.is_some(), "should still be in eligible set");
    let score_after = score_after.unwrap();

    // Higher priority → lower score (ZPOPMIN gives highest priority first)
    assert!(
        score_after < score_before,
        "score should decrease with higher priority: before={score_before}, after={score_after}"
    );

    // Verify priority field updated on core
    let core_key = ExecKeyContext::new(&partition, &eid).core();
    let priority_str = tc.hget(&core_key, "priority").await;
    assert_eq!(priority_str.as_deref(), Some("10"));
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE 2 TESTS: fail, retry, reclaim, expire
// ═══════════════════════════════════════════════════════════════════════

/// Helper: Call ff_fail_execution. Returns the sub-status string.
#[allow(clippy::too_many_arguments)]
async fn fcall_fail_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_instance_id: &str,
    lease_id: &str,
    lease_epoch: &str,
    attempt_id: &str,
    failure_reason: &str,
    failure_category: &str,
    retry_policy_json: &str,
) -> String {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let wid = WorkerInstanceId::new(worker_instance_id);

    let att_idx = AttemptIndex::new(0);

    // KEYS (12): exec_core, attempt_hash, lease_expiry, worker_leases,
    //            terminal_zset, delayed_zset, lease_current, lease_history,
    //            active_index, stream_meta, attempt_timeout, execution_deadline
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(att_idx),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        idx.lane_terminal(&lane_id),
        idx.lane_delayed(&lane_id),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&lane_id),
        ctx.stream_meta(att_idx),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];

    // ARGV (7)
    let args: Vec<String> = vec![
        eid.to_string(),
        lease_id.to_owned(),
        lease_epoch.to_owned(),
        attempt_id.to_owned(),
        failure_reason.to_owned(),
        failure_category.to_owned(),
        retry_policy_json.to_owned(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_fail_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_fail_execution failed");

    let arr = expect_success_array(&raw, "ff_fail_execution");
    field_str(arr, 2)
}

/// Helper: Call ff_promote_delayed.
async fn fcall_promote_delayed(tc: &TestCluster, eid: &ExecutionId, lane: &str) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let now = TimestampMs::now();

    // KEYS (3): exec_core, delayed_zset, eligible_zset
    let keys: Vec<String> = vec![
        ctx.core(),
        idx.lane_delayed(&lane_id),
        idx.lane_eligible(&lane_id),
    ];

    let args: Vec<String> = vec![eid.to_string(), now.to_string()];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_promote_delayed", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_promote_delayed failed");

    assert_ok(&raw, "ff_promote_delayed");
}

/// Helper: Call ff_mark_lease_expired_if_due.
async fn fcall_mark_lease_expired(tc: &TestCluster, eid: &ExecutionId) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    // KEYS (4): exec_core, lease_current, lease_expiry, lease_history
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.lease_current(),
        idx.lease_expiry(),
        ctx.lease_history(),
    ];

    let args: Vec<String> = vec![eid.to_string()];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_mark_lease_expired_if_due", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_mark_lease_expired_if_due failed");

    assert_ok(&raw, "ff_mark_lease_expired_if_due");
}

/// Helper: Call ff_issue_reclaim_grant.
async fn fcall_issue_reclaim_grant(
    tc: &TestCluster,
    eid: &ExecutionId,
    worker_id: &str,
    worker_instance_id: &str,
    lane: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    // KEYS (3): exec_core, claim_grant, lease_expiry
    let keys: Vec<String> = vec![ctx.core(), ctx.claim_grant(), idx.lease_expiry()];

    // ARGV (9): …, worker_capabilities_csv (empty → match-any)
    let args: Vec<String> = vec![
        eid.to_string(),
        worker_id.to_owned(),
        worker_instance_id.to_owned(),
        lane.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        String::new(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_issue_reclaim_grant", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_issue_reclaim_grant failed");

    assert_ok(&raw, "ff_issue_reclaim_grant");
}

/// Helper: Call ff_reclaim_execution. Returns (lease_id, epoch, attempt_index).
async fn fcall_reclaim_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_id: &str,
    worker_instance_id: &str,
    old_attempt_index: AttemptIndex,
    lease_ttl_ms: u64,
) -> (String, String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let wid = WorkerInstanceId::new(worker_instance_id);

    let lease_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let new_att_idx = AttemptIndex::new(old_attempt_index.0 + 1);

    // KEYS (14): exec_core, claim_grant, old_attempt, old_stream_meta,
    //            new_attempt, new_attempt_usage, attempts_zset,
    //            lease_current, lease_history, lease_expiry,
    //            worker_leases, active_index, attempt_timeout, execution_deadline
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        ctx.attempt_hash(old_attempt_index),
        ctx.stream_meta(old_attempt_index),
        ctx.attempt_hash(new_att_idx),
        ctx.attempt_usage(new_att_idx),
        ctx.attempts(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        idx.lane_active(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];

    // ARGV (8)
    let args: Vec<String> = vec![
        eid.to_string(),
        worker_id.to_owned(),
        worker_instance_id.to_owned(),
        lane.to_owned(),
        lease_id.clone(),
        lease_ttl_ms.to_string(),
        attempt_id,
        "{}".to_owned(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_reclaim_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_reclaim_execution failed");

    let arr = expect_success_array(&raw, "ff_reclaim_execution");
    (field_str(arr, 2), field_str(arr, 3), field_str(arr, 6))
}

/// Helper: Call ff_expire_execution.
async fn fcall_expire_execution(tc: &TestCluster, eid: &ExecutionId, lane: &str, reason: &str) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    let att_idx = AttemptIndex::new(0);

    // KEYS (14): exec_core, attempt_hash, stream_meta, lease_current,
    //            lease_history, lease_expiry, worker_leases, active_index,
    //            terminal_zset, attempt_timeout, execution_deadline,
    //            suspended_zset, suspension_timeout, suspension_current
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(att_idx),
        ctx.stream_meta(att_idx),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&WorkerInstanceId::new("")),
        idx.lane_active(&lane_id),
        idx.lane_terminal(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
        idx.lane_suspended(&lane_id),
        idx.suspension_timeout(),
        ctx.suspension_current(),
    ];

    let args: Vec<String> = vec![eid.to_string(), reason.to_owned()];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_expire_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_expire_execution failed");

    assert_ok(&raw, "ff_expire_execution");
}

// ─── Phase 2 test cases ───

/// Fail with retry policy → delayed → promote → re-claim → verify retry attempt.
#[tokio::test]
#[serial_test::serial]
async fn test_fail_with_retry() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create with a retry policy (max_retries=3, fixed 10ms backoff for fast test)
    let policy_json = r#"{"retry_policy":{"max_retries":3,"backoff":{"type":"fixed","delay_ms":10},"retryable_categories":[]}}"#;
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    // Create execution (with retry policy in policy key)
    fcall_create_execution(&tc, &eid, NS, LANE, "retry_test", 0).await;
    // Overwrite policy with retry config
    tc.client()
        .set(&ctx.policy(), policy_json)
        .await
        .expect("SET policy failed");

    // Claim
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, _att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Fail with retryable error
    let sub_status = fcall_fail_execution(
        &tc, &eid, LANE, WORKER_INST,
        &lease_id, &epoch, &attempt_id,
        "provider_error", "provider_error",
        r#"{"max_retries":3,"backoff":{"type":"fixed","delay_ms":10}}"#,
    ).await;
    assert_eq!(sub_status, "retry_scheduled");

    // Verify: runnable/delayed, attempt 0 ended_failure, retry_count=1
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        eligibility_state: Some(ff_core::state::EligibilityState::NotEligibleUntilTime),
        blocking_reason: Some(ff_core::state::BlockingReason::WaitingForRetryBackoff),
        attempt_state: Some(ff_core::state::AttemptState::PendingRetryAttempt),
        public_state: Some(ff_core::state::PublicState::Delayed),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_lease_absent(&tc, &eid).await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "ended_failure").await;

    let retry_count = tc.hget(&ctx.core(), "retry_count").await;
    assert_eq!(retry_count.as_deref(), Some("1"));

    // In delayed index
    assert_in_index(&tc, &idx.lane_delayed(&lane_id), &eid).await;

    // Wait for backoff then promote (10ms backoff + buffer)
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    fcall_promote_delayed(&tc, &eid, LANE).await;

    // Now eligible again
    assert_execution_state(&tc, &eid, &ExpectedState {
        eligibility_state: Some(ff_core::state::EligibilityState::EligibleNow),
        public_state: Some(ff_core::state::PublicState::Waiting),
        ..Default::default()
    }).await;
    assert_in_eligible(&tc, &eid, &lane_id).await;

    // Re-claim: should create attempt 1 with type=retry
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (_lease_id2, epoch2, att_idx2, _attempt_id2) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    assert_eq!(att_idx2, "1", "second attempt should be index 1");
    assert_eq!(epoch2, "2", "second lease should be epoch 2");
    // Verify the core state is correct
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
        ownership_state: Some(ff_core::state::OwnershipState::Leased),
        attempt_state: Some(ff_core::state::AttemptState::RunningAttempt),
        ..Default::default()
    }).await;
    // Verify attempt 1 exists with correct type
    assert_attempt_exists(&tc, &eid, AttemptIndex::new(1)).await;
    assert_attempt_type(&tc, &eid, AttemptIndex::new(1), "retry").await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(1), "started").await;
}

/// Fail without retry policy (or max_retries=0) → terminal failed.
#[tokio::test]
#[serial_test::serial]
async fn test_fail_terminal() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim (no retry policy)
    fcall_create_execution(&tc, &eid, NS, LANE, "terminal_fail", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, _att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Fail with no retry policy → terminal
    let sub_status = fcall_fail_execution(
        &tc, &eid, LANE, WORKER_INST,
        &lease_id, &epoch, &attempt_id,
        "unrecoverable_error", "worker_error",
        "", // no retry policy
    ).await;
    assert_eq!(sub_status, "terminal_failed");

    // Verify: terminal/failed
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Failed),
        public_state: Some(ff_core::state::PublicState::Failed),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;
    assert_lease_absent(&tc, &eid).await;
    assert_in_terminal(&tc, &eid, &LaneId::new(LANE)).await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "ended_failure").await;
}

/// Reclaim an expired lease: create → claim → force expire → reclaim → verify new attempt.
#[tokio::test]
#[serial_test::serial]
async fn test_reclaim_expired_lease() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim with very short TTL (100ms)
    fcall_create_execution(&tc, &eid, NS, LANE, "reclaim_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (_lease_id, epoch, att_idx, _attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 100).await;

    assert_eq!(epoch, "1");
    assert_eq!(att_idx, "0");

    // Wait for lease to expire
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Scanner: mark lease expired
    fcall_mark_lease_expired(&tc, &eid).await;

    // Verify: active/lease_expired_reclaimable
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
        ownership_state: Some(ff_core::state::OwnershipState::LeaseExpiredReclaimable),
        ..Default::default()
    }).await;

    // Issue reclaim grant
    let reclaim_worker = "reclaim-worker-inst";
    fcall_issue_reclaim_grant(&tc, &eid, WORKER, reclaim_worker, LANE).await;

    // Reclaim execution
    let (_new_lease_id, new_epoch, new_att_idx) = fcall_reclaim_execution(
        &tc, &eid, LANE, WORKER, reclaim_worker,
        AttemptIndex::new(0), 30_000,
    ).await;

    assert_eq!(new_epoch, "2", "reclaim should increment epoch");
    assert_eq!(new_att_idx, "1", "reclaim should create attempt index 1");

    // Verify: active/leased again with new epoch
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
        ownership_state: Some(ff_core::state::OwnershipState::Leased),
        attempt_state: Some(ff_core::state::AttemptState::RunningAttempt),
        public_state: Some(ff_core::state::PublicState::Active),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_lease_active(&tc, &eid).await;
    assert_lease_epoch(&tc, &eid, 2).await;

    // Old attempt is interrupted_reclaimed
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "interrupted_reclaimed").await;
    // New attempt is reclaim type
    assert_attempt_type(&tc, &eid, AttemptIndex::new(1), "reclaim").await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(1), "started").await;
}

/// Expire an active execution via ff_expire_execution.
#[tokio::test]
#[serial_test::serial]
async fn test_expire_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "expire_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (_lease_id, _epoch, _att_idx, _attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Expire the execution (simulates attempt_timeout scanner)
    fcall_expire_execution(&tc, &eid, LANE, "attempt_timeout").await;

    // Verify: terminal/expired
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Expired),
        public_state: Some(ff_core::state::PublicState::Expired),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;
    assert_lease_absent(&tc, &eid).await;
    assert_in_terminal(&tc, &eid, &LaneId::new(LANE)).await;
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "ended_failure").await;

    // Verify failure_reason captures the timeout type
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let fr = tc.hget(&ctx.core(), "failure_reason").await;
    assert_eq!(fr.as_deref(), Some("attempt_timeout"));
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE 3 TESTS: suspend, signal, resume, waitpoint
// ═══════════════════════════════════════════════════════════════════════

// ─── Phase 3 FCALL helpers ───

/// Build a resume condition JSON for the given signal names.
fn build_resume_condition_json(signal_names: &[&str], timeout_behavior: &str) -> String {
    serde_json::json!({
        "condition_type": "signal_set",
        "required_signal_names": signal_names,
        "signal_match_mode": if signal_names.len() <= 1 { "any" } else { "all" },
        "minimum_signal_count": 1,
        "timeout_behavior": timeout_behavior,
        "allow_operator_override": true,
    }).to_string()
}

fn build_resume_policy_json() -> String {
    serde_json::json!({
        "resume_target": "runnable",
        "close_waitpoint_on_resume": true,
        "consume_matched_signals": true,
        "retain_signal_buffer_until_closed": true,
    }).to_string()
}

/// Call ff_suspend_execution.
/// Returns (suspension_id, waitpoint_id, waitpoint_key, sub_status).
#[allow(clippy::too_many_arguments)]
async fn fcall_suspend_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_instance_id: &str,
    lease_id: &str,
    lease_epoch: &str,
    attempt_index: &str,
    attempt_id: &str,
    reason_code: &str,
    resume_condition_json: &str,
    timeout_at: Option<i64>,
    timeout_behavior: &str,
) -> (String, String, String, String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let wid = WorkerInstanceId::new(worker_instance_id);

    let att_idx = AttemptIndex::new(attempt_index.parse::<u32>().unwrap_or(0));
    let suspension_id = uuid::Uuid::new_v4().to_string();
    let waitpoint_id_uuid = uuid::Uuid::new_v4();
    let waitpoint_id_str = waitpoint_id_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(waitpoint_id_uuid);
    let waitpoint_key = format!("wpk:{waitpoint_id_str}");

    // KEYS (17): exec_core, attempt_record, lease_current, lease_history,
    //            lease_expiry, worker_leases, suspension_current, waitpoint_hash,
    //            waitpoint_signals, suspension_timeout, pending_wp_expiry,
    //            active_index, suspended_index, waitpoint_history, wp_condition,
    //            attempt_timeout, hmac_secrets
    let keys: Vec<String> = vec![
        ctx.core(),                              // 1
        ctx.attempt_hash(att_idx),               // 2
        ctx.lease_current(),                     // 3
        ctx.lease_history(),                     // 4
        idx.lease_expiry(),                      // 5
        idx.worker_leases(&wid),                 // 6
        ctx.suspension_current(),                // 7
        ctx.waitpoint(&wp_id),                   // 8
        ctx.waitpoint_signals(&wp_id),           // 9
        idx.suspension_timeout(),                // 10
        idx.pending_waitpoint_expiry(),          // 11
        idx.lane_active(&lane_id),               // 12
        idx.lane_suspended(&lane_id),            // 13
        ctx.waitpoints(),                        // 14
        ctx.waitpoint_condition(&wp_id),         // 15
        idx.attempt_timeout(),                   // 16
        idx.waitpoint_hmac_secrets(),            // 17
    ];

    // ARGV (17): execution_id, attempt_index, attempt_id, lease_id,
    //            lease_epoch, suspension_id, waitpoint_id, waitpoint_key,
    //            reason_code, requested_by, timeout_at, resume_condition_json,
    //            resume_policy_json, continuation_metadata_pointer,
    //            use_pending_waitpoint, timeout_behavior, lease_history_maxlen
    let args: Vec<String> = vec![
        eid.to_string(),                                        // 1
        attempt_index.to_owned(),                               // 2
        attempt_id.to_owned(),                                  // 3
        lease_id.to_owned(),                                    // 4
        lease_epoch.to_owned(),                                 // 5
        suspension_id.clone(),                                  // 6
        waitpoint_id_str.clone(),                               // 7
        waitpoint_key.clone(),                                  // 8
        reason_code.to_owned(),                                 // 9
        "worker".to_owned(),                                    // 10
        timeout_at.map_or(String::new(), |t| t.to_string()),   // 11
        resume_condition_json.to_owned(),                       // 12
        build_resume_policy_json(),                             // 13
        String::new(),                                          // 14 continuation_metadata_ptr
        "0".to_owned(),                                         // 15 use_pending_waitpoint
        timeout_behavior.to_owned(),                            // 16
        "1000".to_owned(),                                      // 17 lease_history_maxlen
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_suspend_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_suspend_execution failed");

    let arr = expect_success_array(&raw, "ff_suspend_execution");
    let sub_status = field_str(arr, 1); // "OK" or "ALREADY_SATISFIED"
    // Field index 5 in the response is the minted HMAC token (RFC-004
    // §Waitpoint Security). Echoed for tests that will deliver signals.
    let waitpoint_token = field_str(arr, 5);
    (suspension_id, waitpoint_id_str, waitpoint_key, sub_status, waitpoint_token)
}

/// Call ff_deliver_signal.
/// Returns (signal_id, effect).
#[allow(clippy::too_many_arguments)]
async fn fcall_deliver_signal(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    waitpoint_id_str: &str,
    signal_name: &str,
    signal_category: &str,
    idempotency_key: &str,
    waitpoint_token: &str,
) -> (String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    let signal_id = uuid::Uuid::new_v4().to_string();
    let wp_id = WaitpointId::parse(waitpoint_id_str).unwrap();
    let sig_id = ff_core::types::SignalId::parse(&signal_id).unwrap();
    let now = TimestampMs::now();

    let idem_key = if idempotency_key.is_empty() {
        ctx.noop() // placeholder sharing {p:N} hash tag for cluster mode
    } else {
        ctx.signal_dedup(&wp_id, idempotency_key)
    };

    // KEYS (14): exec_core, wp_condition, wp_signals_stream,
    //            exec_signals_zset, signal_hash, signal_payload,
    //            idem_key, waitpoint_hash, suspension_current,
    //            eligible_zset, suspended_zset, delayed_zset,
    //            suspension_timeout_zset, hmac_secrets
    let keys: Vec<String> = vec![
        ctx.core(),                              // 1
        ctx.waitpoint_condition(&wp_id),         // 2
        ctx.waitpoint_signals(&wp_id),           // 3
        ctx.exec_signals(),                      // 4
        ctx.signal(&sig_id),                     // 5
        ctx.signal_payload(&sig_id),             // 6
        idem_key,                                // 7
        ctx.waitpoint(&wp_id),                   // 8
        ctx.suspension_current(),                // 9
        idx.lane_eligible(&lane_id),             // 10
        idx.lane_suspended(&lane_id),            // 11
        idx.lane_delayed(&lane_id),              // 12
        idx.suspension_timeout(),                // 13
        idx.waitpoint_hmac_secrets(),            // 14
    ];

    // ARGV (18): signal_id, execution_id, waitpoint_id, signal_name,
    //            signal_category, source_type, source_identity,
    //            payload, payload_encoding, idempotency_key,
    //            correlation_id, target_scope, created_at,
    //            dedup_ttl_ms, resume_delay_ms, signal_maxlen,
    //            max_signals_per_execution, waitpoint_token
    let args: Vec<String> = vec![
        signal_id.clone(),                   // 1
        eid.to_string(),                     // 2
        waitpoint_id_str.to_owned(),         // 3
        signal_name.to_owned(),              // 4
        signal_category.to_owned(),          // 5
        "external_api".to_owned(),           // 6 source_type
        "e2e-test".to_owned(),               // 7 source_identity
        String::new(),                       // 8 payload
        "json".to_owned(),                   // 9 payload_encoding
        idempotency_key.to_owned(),          // 10
        String::new(),                       // 11 correlation_id
        "waitpoint".to_owned(),              // 12 target_scope
        now.to_string(),                     // 13 created_at
        "86400000".to_owned(),               // 14 dedup_ttl_ms
        "0".to_owned(),                      // 15 resume_delay_ms
        "1000".to_owned(),                   // 16 signal_maxlen
        "10000".to_owned(),                  // 17 max_signals_per_execution
        waitpoint_token.to_owned(),          // 18 waitpoint_token
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_deliver_signal", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_deliver_signal failed");

    let arr = expect_success_array(&raw, "ff_deliver_signal");
    let ret_signal_id = field_str(arr, 2);
    let effect = field_str(arr, 3);
    (ret_signal_id, effect)
}

/// Call ff_expire_suspension. Timeout behavior is read from the suspension's
/// resume_condition_json by the Lua script.
async fn fcall_expire_suspension(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    attempt_index: u32,
    waitpoint_id_str: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let att_idx = AttemptIndex::new(attempt_index);
    let wp_id = WaitpointId::parse(waitpoint_id_str).unwrap();

    // KEYS (12): exec_core, suspension_current, waitpoint_hash, wp_condition,
    //            attempt_hash, stream_meta, suspension_timeout,
    //            suspended_zset, terminal_zset, eligible_zset, delayed_zset,
    //            lease_history
    let keys: Vec<String> = vec![
        ctx.core(),                         // 1
        ctx.suspension_current(),           // 2
        ctx.waitpoint(&wp_id),              // 3
        ctx.waitpoint_condition(&wp_id),    // 4
        ctx.attempt_hash(att_idx),          // 5
        ctx.stream_meta(att_idx),           // 6
        idx.suspension_timeout(),           // 7
        idx.lane_suspended(&lane_id),       // 8
        idx.lane_terminal(&lane_id),        // 9
        idx.lane_eligible(&lane_id),        // 10
        idx.lane_delayed(&lane_id),         // 11
        ctx.lease_history(),                // 12
    ];

    let args: Vec<String> = vec![eid.to_string()];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_expire_suspension", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_expire_suspension failed");

    assert_ok(&raw, "ff_expire_suspension");
}

/// Call ff_create_pending_waitpoint.
/// Returns (waitpoint_id, waitpoint_key, waitpoint_token).
async fn fcall_create_pending_waitpoint(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: &str,
    expires_in_ms: u64,
) -> (String, String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    let waitpoint_id_uuid = uuid::Uuid::new_v4();
    let waitpoint_id_str = waitpoint_id_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(waitpoint_id_uuid);
    let waitpoint_key = format!("wpk:{waitpoint_id_str}");

    let expires_at = TimestampMs::now().0 + expires_in_ms as i64;

    // KEYS (4): exec_core, waitpoint_hash, pending_wp_expiry_zset, hmac_secrets
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint(&wp_id),
        idx.pending_waitpoint_expiry(),
        idx.waitpoint_hmac_secrets(),
    ];

    // ARGV (5): execution_id, attempt_index, waitpoint_id, waitpoint_key, expires_at
    let args: Vec<String> = vec![
        eid.to_string(),             // 1
        attempt_index.to_owned(),    // 2
        waitpoint_id_str.clone(),    // 3
        waitpoint_key.clone(),       // 4
        expires_at.to_string(),      // 5
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_create_pending_waitpoint", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_pending_waitpoint failed");

    let arr = expect_success_array(&raw, "ff_create_pending_waitpoint");
    // Response: {1, "OK", waitpoint_id, waitpoint_key, waitpoint_token}
    let waitpoint_token = field_str(arr, 4);
    (waitpoint_id_str, waitpoint_key, waitpoint_token)
}

/// Call ff_buffer_signal_for_pending_waitpoint.
/// Returns (signal_id, effect).
async fn fcall_buffer_signal(
    tc: &TestCluster,
    eid: &ExecutionId,
    waitpoint_id_str: &str,
    signal_name: &str,
    idempotency_key: &str,
    waitpoint_token: &str,
) -> (String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    let signal_id = uuid::Uuid::new_v4().to_string();
    let wp_id = WaitpointId::parse(waitpoint_id_str).unwrap();
    let sig_id = ff_core::types::SignalId::parse(&signal_id).unwrap();
    let now = TimestampMs::now();

    let idem_key = if idempotency_key.is_empty() {
        ctx.noop() // placeholder sharing {p:N} hash tag for cluster mode
    } else {
        ctx.signal_dedup(&wp_id, idempotency_key)
    };

    // KEYS (9): exec_core, wp_condition, wp_signals_stream,
    //           exec_signals_zset, signal_hash, signal_payload, idem_key,
    //           waitpoint_hash, hmac_secrets
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint_condition(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        ctx.exec_signals(),
        ctx.signal(&sig_id),
        ctx.signal_payload(&sig_id),
        idem_key,
        ctx.waitpoint(&wp_id),
        idx.waitpoint_hmac_secrets(),
    ];

    // ARGV (18): same layout as ff_deliver_signal + waitpoint_token
    let args: Vec<String> = vec![
        signal_id.clone(),                   // 1
        eid.to_string(),                     // 2
        waitpoint_id_str.to_owned(),         // 3
        signal_name.to_owned(),              // 4 signal_name
        "callback".to_owned(),               // 5 signal_category
        "external_api".to_owned(),           // 6 source_type
        "e2e-test".to_owned(),               // 7 source_identity
        String::new(),                       // 8 payload
        "json".to_owned(),                   // 9 payload_encoding
        idempotency_key.to_owned(),          // 10 idempotency_key
        String::new(),                       // 11 correlation_id
        "waitpoint".to_owned(),              // 12 target_scope
        now.to_string(),                     // 13 created_at
        "86400000".to_owned(),               // 14 dedup_ttl_ms
        "0".to_owned(),                      // 15 resume_delay_ms (unused by buffer)
        "1000".to_owned(),                   // 16 signal_maxlen
        "10000".to_owned(),                  // 17 max_signals
        waitpoint_token.to_owned(),          // 18 waitpoint_token
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_buffer_signal_for_pending_waitpoint", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_buffer_signal_for_pending_waitpoint failed");

    let arr = expect_success_array(&raw, "ff_buffer_signal_for_pending_waitpoint");
    (field_str(arr, 2), field_str(arr, 3))
}

/// Call ff_claim_resumed_execution.
/// Returns (lease_id, lease_epoch, attempt_index, attempt_id).
async fn fcall_claim_resumed_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_id: &str,
    worker_instance_id: &str,
    lease_ttl_ms: u64,
) -> (String, String, String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let wid = WorkerInstanceId::new(worker_instance_id);

    let lease_id = uuid::Uuid::new_v4().to_string();

    // Read current attempt index from exec_core
    let att_idx_str: Option<String> = tc.hget(&ctx.core(), "current_attempt_index").await;
    let att_idx_val = att_idx_str.as_deref().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let att_idx = AttemptIndex::new(att_idx_val);

    // KEYS (11): exec_core, claim_grant, eligible_zset, lease_expiry,
    //            worker_leases, existing_attempt_hash, lease_current,
    //            lease_history, active_index, attempt_timeout, execution_deadline
    let keys: Vec<String> = vec![
        ctx.core(),                              // 1
        ctx.claim_grant(),                       // 2
        idx.lane_eligible(&lane_id),             // 3
        idx.lease_expiry(),                      // 4
        idx.worker_leases(&wid),                 // 5
        ctx.attempt_hash(att_idx),               // 6
        ctx.lease_current(),                     // 7
        ctx.lease_history(),                     // 8
        idx.lane_active(&lane_id),               // 9
        idx.attempt_timeout(),                   // 10
        idx.execution_deadline(),                // 11
    ];

    // ARGV (8): eid, worker_id, worker_instance_id, lane,
    //           capability_snapshot_hash, lease_id, lease_ttl_ms,
    //           remaining_attempt_timeout_ms
    let args: Vec<String> = vec![
        eid.to_string(),                     // 1
        worker_id.to_owned(),                // 2
        worker_instance_id.to_owned(),       // 3
        lane.to_owned(),                     // 4
        String::new(),                       // 5 capability_snapshot_hash
        lease_id.clone(),                    // 6
        lease_ttl_ms.to_string(),            // 7
        String::new(),                       // 8 remaining_attempt_timeout_ms
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_claim_resumed_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_claim_resumed_execution failed");

    let arr = expect_success_array(&raw, "ff_claim_resumed_execution");
    let lease_id_ret = field_str(arr, 2);
    let epoch_ret = field_str(arr, 3);
    let attempt_id_ret = field_str(arr, 5);
    let attempt_idx_ret = field_str(arr, 6);

    (lease_id_ret, epoch_ret, attempt_idx_ret, attempt_id_ret)
}

/// Call ff_cancel_execution for a suspended execution.
/// Needs real waitpoint_id so the Lua can close the waitpoint.
async fn fcall_cancel_suspended_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    attempt_index: u32,
    waitpoint_id_str: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);
    let att_idx = AttemptIndex::new(attempt_index);
    let wp_id = WaitpointId::parse(waitpoint_id_str).unwrap();

    // KEYS (21) for ff_cancel_execution — with real waitpoint keys
    let keys: Vec<String> = vec![
        ctx.core(),                                        // 1
        ctx.attempt_hash(att_idx),                         // 2
        ctx.stream_meta(att_idx),                          // 3
        ctx.lease_current(),                               // 4
        ctx.lease_history(),                               // 5
        idx.lease_expiry(),                                // 6
        idx.worker_leases(&WorkerInstanceId::new("")),     // 7
        ctx.suspension_current(),                          // 8
        ctx.waitpoint(&wp_id),                             // 9
        ctx.waitpoint_condition(&wp_id),                   // 10
        idx.suspension_timeout(),                          // 11
        idx.lane_terminal(&lane_id),                       // 12
        idx.attempt_timeout(),                             // 13
        idx.execution_deadline(),                          // 14
        idx.lane_eligible(&lane_id),                       // 15
        idx.lane_delayed(&lane_id),                        // 16
        idx.lane_blocked_dependencies(&lane_id),           // 17
        idx.lane_blocked_budget(&lane_id),                 // 18
        idx.lane_blocked_quota(&lane_id),                  // 19
        idx.lane_blocked_route(&lane_id),                  // 20
        idx.lane_blocked_operator(&lane_id),               // 21
    ];

    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        eid.to_string(),
        "test_cancellation".to_owned(),
        "operator_override".to_owned(),
        String::new(), // no lease_id for operator override
        String::new(), // no lease_epoch
        String::new(), // no attempt_id — operator override
        now.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_cancel_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_cancel_execution (suspended) failed");

    assert_ok(&raw, "ff_cancel_execution (suspended)");
}

// ─── Phase 3 test cases ───

/// Create → claim → suspend → signal → resume → re-claim (same attempt).
#[tokio::test]
#[serial_test::serial]
async fn test_suspend_and_signal_resume() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // 1. Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "suspend_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    assert_eq!(att_idx, "0");

    // 2. Suspend
    let resume_cond = build_resume_condition_json(&["approval_result"], "fail");
    let (suspension_id, waitpoint_id, _waitpoint_key, sub_status, wp_token) =
        fcall_suspend_execution(
            &tc, &eid, LANE, WORKER_INST,
            &lease_id, &epoch, &att_idx, &attempt_id,
            "waiting_for_approval", &resume_cond, None, "fail",
        ).await;
    assert_eq!(sub_status, "OK");

    // 3. Verify suspended state
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Suspended),
        ownership_state: Some(ff_core::state::OwnershipState::Unowned),
        eligibility_state: Some(ff_core::state::EligibilityState::NotApplicable),
        blocking_reason: Some(ff_core::state::BlockingReason::WaitingForApproval),
        attempt_state: Some(ff_core::state::AttemptState::AttemptInterrupted),
        public_state: Some(ff_core::state::PublicState::Suspended),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;
    assert_lease_absent(&tc, &eid).await;

    // Verify suspension record exists
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let susp_sid = tc.hget(&ctx.suspension_current(), "suspension_id").await;
    assert_eq!(susp_sid.as_deref(), Some(suspension_id.as_str()));

    // Verify waitpoint is active
    let wp_id = WaitpointId::parse(&waitpoint_id).unwrap();
    let wp_state = tc.hget(&ctx.waitpoint(&wp_id), "state").await;
    assert_eq!(wp_state.as_deref(), Some("active"));

    // Verify in suspended index, not in eligible/active
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    assert_in_index(&tc, &idx.lane_suspended(&lane_id), &eid).await;
    assert_not_in_eligible(&tc, &eid, &lane_id).await;

    // 4. Deliver signal matching the condition
    let (signal_id, effect) = fcall_deliver_signal(
        &tc, &eid, LANE, &waitpoint_id,
        "approval_result", "approval", "", &wp_token,
    ).await;
    assert!(!signal_id.is_empty());
    assert_eq!(effect, "resume_condition_satisfied");

    // 5. Verify resumed to runnable
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        ownership_state: Some(ff_core::state::OwnershipState::Unowned),
        eligibility_state: Some(ff_core::state::EligibilityState::EligibleNow),
        attempt_state: Some(ff_core::state::AttemptState::AttemptInterrupted),
        public_state: Some(ff_core::state::PublicState::Waiting),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_in_eligible(&tc, &eid, &lane_id).await;

    // 6. Re-claim via claim_resumed_execution (same attempt continues)
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (_lease_id2, epoch2, att_idx2, attempt_id2) =
        fcall_claim_resumed_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Same attempt index (0) — resume does NOT create a new attempt
    assert_eq!(att_idx2, "0", "resumed execution should keep same attempt index");
    assert_eq!(epoch2, "2", "new claim should increment epoch");
    assert!(!attempt_id2.is_empty());

    // Verify back to active
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
        ownership_state: Some(ff_core::state::OwnershipState::Leased),
        attempt_state: Some(ff_core::state::AttemptState::RunningAttempt),
        public_state: Some(ff_core::state::PublicState::Active),
        ..Default::default()
    }).await;
}

/// Suspend with short timeout + behavior=fail → verify terminal/failed.
#[tokio::test]
#[serial_test::serial]
async fn test_suspend_timeout_fail() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "timeout_fail", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Suspend with timeout in the past (triggers immediate expiry)
    let past_timeout = TimestampMs::now().0 - 1000; // 1s ago
    let resume_cond = build_resume_condition_json(&["approval"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub, _wp_token) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST,
        &lease_id, &epoch, &att_idx, &attempt_id,
        "waiting_for_approval", &resume_cond, Some(past_timeout), "fail",
    ).await;

    // Trigger suspension timeout scanner
    fcall_expire_suspension(&tc, &eid, LANE, att_idx.parse().unwrap_or(0), &waitpoint_id).await;

    // Verify: terminal/failed (timeout behavior = fail)
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Failed),
        public_state: Some(ff_core::state::PublicState::Failed),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_in_terminal(&tc, &eid, &LaneId::new(LANE)).await;
}

/// Suspend with timeout + behavior=auto_resume → verify runnable (not terminal).
#[tokio::test]
#[serial_test::serial]
async fn test_suspend_timeout_auto_resume() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "timeout_resume", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Suspend with auto_resume timeout behavior
    // Lua checks behavior == "auto_resume" (not the long form)
    let past_timeout = TimestampMs::now().0 - 1000;
    let resume_cond = build_resume_condition_json(&["approval"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub, _wp_token) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST,
        &lease_id, &epoch, &att_idx, &attempt_id,
        "waiting_for_approval", &resume_cond, Some(past_timeout), "auto_resume",
    ).await;

    // Trigger suspension timeout
    fcall_expire_suspension(&tc, &eid, LANE, att_idx.parse().unwrap_or(0), &waitpoint_id).await;

    // Verify: runnable/eligible (auto_resume), NOT terminal
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        eligibility_state: Some(ff_core::state::EligibilityState::EligibleNow),
        public_state: Some(ff_core::state::PublicState::Waiting),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_in_eligible(&tc, &eid, &LaneId::new(LANE)).await;
}

/// Pending waitpoint → buffer signal → suspend (activates pending wp) →
/// buffered signal satisfies condition → immediate resume (ALREADY_SATISFIED).
#[tokio::test]
#[serial_test::serial]
async fn test_pending_waitpoint_with_buffered_signal() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "pending_wp_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // 1. Create pending waitpoint while still active
    let (waitpoint_id, _waitpoint_key, wp_token) = fcall_create_pending_waitpoint(
        &tc, &eid, &att_idx, 60_000,
    ).await;

    // 2. Buffer a signal against the pending waitpoint (before suspension)
    let (_sig_id, _effect) = fcall_buffer_signal(
        &tc, &eid, &waitpoint_id, "callback_result", "", &wp_token,
    ).await;

    // 3. Now suspend with the pending waitpoint — condition matches buffered signal
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);
    let wp_id = WaitpointId::parse(&waitpoint_id).unwrap();

    let suspension_id = uuid::Uuid::new_v4().to_string();
    let resume_cond = build_resume_condition_json(&["callback_result"], "fail");

    // ff_suspend_execution with use_pending_waitpoint=1
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(att_idx.parse().unwrap_or(0))),
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
    ];

    // ARGV (17): execution_id, attempt_index, attempt_id, lease_id, lease_epoch,
    //            suspension_id, waitpoint_id, waitpoint_key, reason_code,
    //            requested_by, timeout_at, resume_condition_json, resume_policy_json,
    //            continuation_metadata_pointer, use_pending_waitpoint,
    //            timeout_behavior, lease_history_maxlen
    let args: Vec<String> = vec![
        eid.to_string(),                                          // 1
        att_idx.clone(),                                          // 2 attempt_index
        attempt_id.clone(),                                       // 3 attempt_id
        lease_id.clone(),                                         // 4 lease_id
        epoch.clone(),                                            // 5 lease_epoch
        suspension_id.clone(),                                    // 6
        waitpoint_id.clone(),                                     // 7
        format!("wpk:{waitpoint_id}"),                           // 8
        "waiting_for_callback".to_owned(),                       // 9 reason_code
        "worker".to_owned(),                                      // 10 requested_by
        String::new(),                                            // 11 timeout_at (none)
        resume_cond,                                              // 12 resume_condition_json
        build_resume_policy_json(),                               // 13 resume_policy_json
        String::new(),                                            // 14 continuation_metadata_ptr
        "1".to_owned(),                                           // 15 use_pending_waitpoint = 1
        "fail".to_owned(),                                        // 16 timeout_behavior
        "1000".to_owned(),                                        // 17 lease_history_maxlen
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_suspend_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_suspend_execution (pending wp) failed");

    let arr = expect_success_array(&raw, "ff_suspend_execution (pending wp)");
    let sub_status = field_str(arr, 1);

    // The buffered signal should have satisfied the condition immediately
    assert_eq!(sub_status, "ALREADY_SATISFIED",
        "buffered signal should satisfy the condition — expect ALREADY_SATISFIED");

    // Execution should still be active (lease not released since condition was met)
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
        ownership_state: Some(ff_core::state::OwnershipState::Leased),
        ..Default::default()
    }).await;
}

/// Deliver same signal twice with same idempotency key → second is DUPLICATE.
#[tokio::test]
#[serial_test::serial]
async fn test_signal_idempotency() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim + suspend
    fcall_create_execution(&tc, &eid, NS, LANE, "idem_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Use 2 matchers so first signal doesn't trigger resume
    let resume_cond = build_resume_condition_json(&["sig_a", "sig_b"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub, wp_token) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST,
        &lease_id, &epoch, &att_idx, &attempt_id,
        "waiting_for_signal", &resume_cond, None, "fail",
    ).await;

    // First signal delivery
    let (sig_id_1, effect_1) = fcall_deliver_signal(
        &tc, &eid, LANE, &waitpoint_id,
        "sig_a", "callback", "dedup-key-1", &wp_token,
    ).await;
    assert!(!sig_id_1.is_empty());
    assert_eq!(effect_1, "appended_to_waitpoint",
        "first signal should be appended (condition not yet fully satisfied)");

    // Second delivery with same idempotency key → should be deduplicated
    // We construct a separate FCALL with a NEW signal_id but same idem key
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wp_id = WaitpointId::parse(&waitpoint_id).unwrap();

    let dup_signal_id = uuid::Uuid::new_v4().to_string();
    let dup_sig_id = ff_core::types::SignalId::parse(&dup_signal_id).unwrap();
    let now = TimestampMs::now();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint_condition(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        ctx.exec_signals(),
        ctx.signal(&dup_sig_id),
        ctx.signal_payload(&dup_sig_id),
        ctx.signal_dedup(&wp_id, "dedup-key-1"),
        ctx.waitpoint(&wp_id),
        ctx.suspension_current(),
        idx.lane_eligible(&lane_id),
        idx.lane_suspended(&lane_id),
        idx.lane_delayed(&lane_id),
        idx.suspension_timeout(),
        idx.waitpoint_hmac_secrets(),
    ];

    let args: Vec<String> = vec![
        dup_signal_id.clone(),               // 1 signal_id
        eid.to_string(),                     // 2 execution_id
        waitpoint_id.clone(),                // 3 waitpoint_id
        "sig_a".to_owned(),                  // 4 signal_name
        "callback".to_owned(),               // 5 signal_category
        "external_api".to_owned(),           // 6 source_type
        "e2e-test".to_owned(),               // 7 source_identity
        String::new(),                       // 8 payload
        "json".to_owned(),                   // 9 payload_encoding
        "dedup-key-1".to_owned(),            // 10 idempotency_key (same!)
        String::new(),                       // 11 correlation_id
        "waitpoint".to_owned(),              // 12 target_scope
        now.to_string(),                     // 13 created_at
        "86400000".to_owned(),               // 14 dedup_ttl_ms
        "0".to_owned(),                      // 15 resume_delay_ms
        "1000".to_owned(),                   // 16 signal_maxlen
        "10000".to_owned(),                  // 17 max_signals
        wp_token.clone(),                    // 18 waitpoint_token
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_deliver_signal", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_deliver_signal (dup) failed");

    let arr = expect_success_array(&raw, "ff_deliver_signal (dup)");
    let sub_status_2 = field_str(arr, 1);
    assert_eq!(sub_status_2, "DUPLICATE", "second signal with same idem key should be DUPLICATE");

    // Execution should still be suspended (only 1 of 2 matchers satisfied)
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Suspended),
        ..Default::default()
    }).await;
}

/// Cancel a suspended execution → verify terminal/cancelled, suspension closed.
#[tokio::test]
#[serial_test::serial]
async fn test_cancel_suspended_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim + suspend
    fcall_create_execution(&tc, &eid, NS, LANE, "cancel_susp", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    let resume_cond = build_resume_condition_json(&["approval"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub, _wp_token) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST,
        &lease_id, &epoch, &att_idx, &attempt_id,
        "waiting_for_approval", &resume_cond, None, "fail",
    ).await;

    // Verify suspended
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Suspended),
        ..Default::default()
    }).await;

    // Cancel the suspended execution
    fcall_cancel_suspended_execution(
        &tc, &eid, LANE, att_idx.parse().unwrap_or(0), &waitpoint_id,
    ).await;

    // Verify: terminal/cancelled
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Cancelled),
        public_state: Some(ff_core::state::PublicState::Cancelled),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;
    assert_in_terminal(&tc, &eid, &LaneId::new(LANE)).await;

    // Verify suspension record is closed
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let close_reason = tc.hget(&ctx.suspension_current(), "close_reason").await;
    assert_eq!(close_reason.as_deref(), Some("cancelled"));

    // Verify waitpoint is closed
    let wp_id = WaitpointId::parse(&waitpoint_id).unwrap();
    let wp_state = tc.hget(&ctx.waitpoint(&wp_id), "state").await;
    assert_eq!(wp_state.as_deref(), Some("closed"));
    let wp_close = tc.hget(&ctx.waitpoint(&wp_id), "close_reason").await;
    assert_eq!(wp_close.as_deref(), Some("cancelled"));

    // Verify attempt ended
    assert_attempt_state(&tc, &eid, AttemptIndex::new(0), "ended_cancelled").await;
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE 4 TESTS: streaming (append_frame)
// ═══════════════════════════════════════════════════════════════════════

// ─── Phase 4 FCALL helpers ───

/// Call ff_append_frame. Returns (entry_id, frame_count).
#[allow(clippy::too_many_arguments)]
async fn fcall_append_frame(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: &str,
    lease_id: &str,
    lease_epoch: &str,
    attempt_id: &str,
    frame_type: &str,
    payload: &str,
    retention_maxlen: u64,
) -> (String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let att_idx = AttemptIndex::new(attempt_index.parse::<u32>().unwrap_or(0));
    let now = TimestampMs::now();

    // KEYS (3): exec_core, stream_data, stream_meta
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.stream(att_idx),
        ctx.stream_meta(att_idx),
    ];

    // ARGV (13): execution_id, attempt_index, lease_id, lease_epoch,
    //            frame_type, ts, payload, encoding, correlation_id,
    //            source, retention_maxlen, attempt_id, max_payload_bytes
    let args: Vec<String> = vec![
        eid.to_string(),              // 1
        attempt_index.to_owned(),     // 2
        lease_id.to_owned(),          // 3
        lease_epoch.to_owned(),       // 4
        frame_type.to_owned(),        // 5
        now.to_string(),              // 6 ts
        payload.to_owned(),           // 7
        "utf8".to_owned(),            // 8 encoding
        String::new(),                // 9 correlation_id
        "worker".to_owned(),          // 10 source
        retention_maxlen.to_string(), // 11
        attempt_id.to_owned(),        // 12
        "65536".to_owned(),           // 13 max_payload_bytes
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_append_frame", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_append_frame failed");

    let arr = expect_success_array(&raw, "ff_append_frame");
    (field_str(arr, 2), field_str(arr, 3))
}

/// Try ff_append_frame, return the raw result (for error testing).
#[allow(clippy::too_many_arguments)]
async fn fcall_append_frame_raw(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: &str,
    lease_id: &str,
    lease_epoch: &str,
    attempt_id: &str,
    frame_type: &str,
    payload: &str,
) -> Value {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let att_idx = AttemptIndex::new(attempt_index.parse::<u32>().unwrap_or(0));
    let now = TimestampMs::now();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.stream(att_idx),
        ctx.stream_meta(att_idx),
    ];

    let args: Vec<String> = vec![
        eid.to_string(),
        attempt_index.to_owned(),
        lease_id.to_owned(),
        lease_epoch.to_owned(),
        frame_type.to_owned(),
        now.to_string(),
        payload.to_owned(),
        "utf8".to_owned(),
        String::new(),
        "worker".to_owned(),
        "0".to_owned(), // no maxlen
        attempt_id.to_owned(),
        "65536".to_owned(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    tc.client()
        .fcall("ff_append_frame", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_append_frame (raw) failed")
}

// ─── Phase 4 test cases ───

/// Append frames during execution, verify stream_meta, complete, verify stream readable.
#[tokio::test]
#[serial_test::serial]
async fn test_append_frame_during_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "stream_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Append first frame
    let (entry_id1, count1) = fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "log", r#"{"msg":"hello"}"#, 0,
    ).await;
    assert!(!entry_id1.is_empty(), "entry_id should be non-empty");
    assert_eq!(count1, "1");

    // Append second frame
    let (entry_id2, count2) = fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "output", r#"{"result":"done"}"#, 0,
    ).await;
    assert!(!entry_id2.is_empty());
    assert_eq!(count2, "2");
    assert_ne!(entry_id1, entry_id2, "each frame gets a unique entry_id");

    // Verify stream_meta
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let att = AttemptIndex::new(att_idx.parse().unwrap_or(0));

    let frame_count = tc.hget(&ctx.stream_meta(att), "frame_count").await;
    assert_eq!(frame_count.as_deref(), Some("2"));

    let last_seq = tc.hget(&ctx.stream_meta(att), "last_sequence").await;
    assert_eq!(last_seq.as_deref(), Some(entry_id2.as_str()));

    let closed_at = tc.hget(&ctx.stream_meta(att), "closed_at").await;
    assert_eq!(closed_at.as_deref(), Some(""), "stream should still be open");

    // Complete the execution
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lease_id, &epoch, &attempt_id).await;

    // Stream should still be readable (XLEN) even after completion
    let stream_key = ctx.stream(att);
    let xlen: i64 = tc.client()
        .cmd("XLEN").arg(&stream_key).execute().await
        .expect("XLEN failed");
    assert_eq!(xlen, 2, "stream should have 2 frames after completion");
}

/// Append frame after execution is completed → expect stream_closed error.
#[tokio::test]
#[serial_test::serial]
async fn test_append_frame_after_complete() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();

    // Create + claim + complete
    fcall_create_execution(&tc, &eid, NS, LANE, "closed_stream", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lease_id, &epoch, &attempt_id).await;

    // Try to append — should fail since execution is terminal
    let raw = fcall_append_frame_raw(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "log", "should fail",
    ).await;

    // Expect error status (0)
    let arr = match &raw {
        Value::Array(arr) => arr,
        _ => panic!("expected Array, got {raw:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        other => panic!("expected Int, got {other:?}"),
    };
    assert_eq!(status, 0, "append after complete should fail");

    let error_code = field_str(arr, 1);
    assert_eq!(error_code, "stream_closed",
        "error should be stream_closed, got: {error_code}");
}

/// Append with MAXLEN trimming — verify stream length capped.
#[tokio::test]
#[serial_test::serial]
async fn test_stream_maxlen_trimming() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "maxlen_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Append 200 frames with retention_maxlen=50.
    // Valkey's XTRIM MAXLEN ~ (approximate) trims at the listpack node
    // level (~100 entries per node by default). We need enough entries
    // to span multiple nodes before approximate trim kicks in.
    for i in 0..200 {
        fcall_append_frame(
            &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
            "log", &format!("frame_{i}"), 50,
        ).await;
    }

    // Verify stream length is trimmed from 200
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let att = AttemptIndex::new(att_idx.parse().unwrap_or(0));
    let stream_key = ctx.stream(att);

    let xlen: i64 = tc.client()
        .cmd("XLEN").arg(&stream_key).execute().await
        .expect("XLEN failed");

    // With MAXLEN ~50 and 200 entries, approximate trim should drop
    // at least one full node. Expect significantly less than 200.
    assert!(
        xlen < 200,
        "stream should be trimmed from 200 frames, got {xlen}"
    );
    assert!(
        xlen >= 10,
        "stream should retain entries near maxlen target, got {xlen}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE 5 TESTS: budget, quota, block/unblock
// ═══════════════════════════════════════════════════════════════════════

/// Helper: create a budget via ff_create_budget FCALL.
/// Uses a fixed {b:0} tag for test simplicity.
async fn fcall_create_budget(
    tc: &TestCluster,
    budget_id: &str,
    hard_limits: &[(&str, u64)],
    soft_limits: &[(&str, u64)],
) -> Vec<String> {
    fcall_create_budget_full(tc, budget_id, hard_limits, soft_limits, 0).await
}

/// Helper: create a budget via ff_create_budget with reset interval.
async fn fcall_create_budget_full(
    tc: &TestCluster,
    budget_id: &str,
    hard_limits: &[(&str, u64)],
    soft_limits: &[(&str, u64)],
    reset_interval_ms: u64,
) -> Vec<String> {
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let resets_key = "ff:idx:{b:0}:budget_resets".to_string();
    let policies_index = "ff:idx:{b:0}:budget_policies".to_string();

    let now = TimestampMs::now();

    // Build ARGV: budget_id, scope_type, scope_id, enforcement_mode,
    //   on_hard_limit, on_soft_limit, reset_interval_ms, now_ms,
    //   dimension_count, dim_1..dim_N, hard_1..hard_N, soft_1..soft_N
    let dim_count = hard_limits.len();
    let mut args: Vec<String> = vec![
        budget_id.to_owned(),
        "lane".to_owned(),
        "test-lane".to_owned(),
        "strict".to_owned(),
        "fail".to_owned(),
        "warn".to_owned(),
        reset_interval_ms.to_string(),
        now.to_string(),
        dim_count.to_string(),
    ];
    for (dim, _) in hard_limits {
        args.push((*dim).to_owned());
    }
    for (_, limit) in hard_limits {
        args.push(limit.to_string());
    }
    for i in 0..dim_count {
        // Match soft limits by index, default 0 if not provided
        let soft = soft_limits.get(i).map(|(_, v)| *v).unwrap_or(0);
        args.push(soft.to_string());
    }

    let keys: Vec<String> = vec![def_key, limits_key, usage_key, resets_key, policies_index];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_create_budget", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_budget failed");

    parse_ok_result(&raw)
}

/// Helper: create a quota policy via ff_create_quota_policy FCALL.
/// Uses a fixed {q:0} tag for test simplicity.
async fn fcall_create_quota_policy(
    tc: &TestCluster,
    policy_id: &str,
    window_seconds: u64,
    max_requests_per_window: u64,
    max_concurrent: u64,
) -> Vec<String> {
    let def_key = format!("ff:quota:{{q:0}}:{policy_id}");
    let window_key = format!("ff:quota:{{q:0}}:{policy_id}:window:requests");
    let concurrency_key = format!("ff:quota:{{q:0}}:{policy_id}:concurrency");
    let admitted_set_key = format!("ff:quota:{{q:0}}:{policy_id}:admitted_set");
    let policies_index_key = "ff:idx:{q:0}:quota_policies".to_owned();

    let now = TimestampMs::now();
    let keys: Vec<String> = vec![
        def_key, window_key, concurrency_key, admitted_set_key, policies_index_key,
    ];
    let args: Vec<String> = vec![
        policy_id.to_owned(),
        window_seconds.to_string(),
        max_requests_per_window.to_string(),
        max_concurrent.to_string(),
        now.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_create_quota_policy", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_quota_policy failed");

    parse_ok_result(&raw)
}

/// Parse {1, status, ...fields} → vec of string fields (status + rest).
fn parse_ok_result(raw: &Value) -> Vec<String> {
    match raw {
        Value::Array(arr) => {
            arr.iter().skip(1).filter_map(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                Ok(Value::Int(n)) => Some(n.to_string()),
                _ => None,
            }).collect()
        }
        _ => panic!("expected Array result, got: {raw:?}"),
    }
}

/// Call ff_report_usage_and_check. Returns the raw result array elements.
async fn fcall_report_usage(
    tc: &TestCluster,
    budget_id: &str,
    dimensions: &[(&str, u64)],
) -> Vec<String> {
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");

    let keys: Vec<String> = vec![usage_key, limits_key, def_key];

    let now = TimestampMs::now();
    let dim_count = dimensions.len();

    // ARGV: dim_count, dim_1..dim_N, delta_1..delta_N, now_ms
    let mut args: Vec<String> = vec![dim_count.to_string()];
    for (dim, _) in dimensions {
        args.push((*dim).to_owned());
    }
    for (_, delta) in dimensions {
        args.push(delta.to_string());
    }
    args.push(now.to_string());

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_report_usage_and_check", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_report_usage_and_check failed");

    // Parse result array — standard format {1, status, ...}
    // Skip the status code (index 0) so callers see [status, field1, field2, ...]
    match &raw {
        Value::Array(arr) => {
            let status_code = match arr.first() {
                Some(Ok(Value::Int(n))) => *n,
                _ => panic!("ff_report_usage_and_check: expected Int status code, got {raw:?}"),
            };
            assert_eq!(status_code, 1, "ff_report_usage_and_check returned error: {raw:?}");
            arr.iter().skip(1).filter_map(|v| match v {
                Ok(Value::BulkString(b)) => Some(String::from_utf8_lossy(b).into_owned()),
                Ok(Value::SimpleString(s)) => Some(s.clone()),
                Ok(Value::Int(n)) => Some(n.to_string()),
                _ => None,
            }).collect()
        }
        _ => panic!("ff_report_usage_and_check: expected Array, got {raw:?}"),
    }
}

/// Call ff_reset_budget.
async fn fcall_reset_budget(tc: &TestCluster, budget_id: &str) {
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let resets_key = "ff:idx:{b:0}:budget_resets".to_owned();

    let keys: Vec<String> = vec![def_key, usage_key, resets_key];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![budget_id.to_owned(), now.to_string()];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_reset_budget", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_reset_budget failed");

    assert_ok(&raw, "ff_reset_budget");
}

// ─── Phase 5 test cases ───

/// Report usage: 60 OK, then 50 more → HARD_BREACH (total 110 > limit 100).
#[tokio::test]
#[serial_test::serial]
async fn test_budget_report_and_breach() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let budget_id = "test-budget-1";
    fcall_create_budget(&tc, budget_id,
        &[("total_tokens", 100)],
        &[("total_tokens", 80)],
    ).await;

    // Report 60 tokens → should be OK
    let result1 = fcall_report_usage(&tc, budget_id, &[("total_tokens", 60)]).await;
    assert_eq!(result1[0], "OK", "60 tokens should be under limit. Got: {result1:?}");

    // Report 25 more (total 85) → should be SOFT_BREACH (> 80 soft limit)
    let result2 = fcall_report_usage(&tc, budget_id, &[("total_tokens", 25)]).await;
    assert_eq!(result2[0], "SOFT_BREACH", "85 tokens should breach soft limit 80. Got: {result2:?}");
    assert_eq!(result2[1], "total_tokens");

    // Report 50 more (would be 135 total) → should be HARD_BREACH (> 100)
    // Note: check-before-increment means usage stays at 85, NOT 135
    let result3 = fcall_report_usage(&tc, budget_id, &[("total_tokens", 50)]).await;
    assert_eq!(result3[0], "HARD_BREACH", "135 tokens should breach hard limit 100. Got: {result3:?}");
    assert_eq!(result3[1], "total_tokens");

    // Verify usage is still 85 (hard breach does NOT increment)
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let usage: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("total_tokens")
        .execute().await.unwrap();
    assert_eq!(usage.as_deref(), Some("85"), "usage should be 85 (hard breach did not increment)");

    // ── Multi-dimension test: if one dimension breaches, NONE get incremented ──
    let budget_id2 = "test-budget-multi";
    fcall_create_budget(&tc, budget_id2,
        &[("tokens", 1000), ("cost", 50)],
        &[],
    ).await;

    // Report: tokens=100 (under 1000), cost=60 (over 50) → HARD_BREACH on cost
    let multi_result = fcall_report_usage(&tc, budget_id2,
        &[("tokens", 100), ("cost", 60)],
    ).await;
    assert_eq!(multi_result[0], "HARD_BREACH", "cost exceeds limit. Got: {multi_result:?}");
    assert_eq!(multi_result[1], "cost");

    // Verify NEITHER dimension was incremented
    let usage_key2 = format!("ff:budget:{{b:0}}:{budget_id2}:usage");
    let tokens_usage: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key2).arg("tokens")
        .execute().await.unwrap();
    // tokens should be 0 or None — NOT 100
    let tokens_val = tokens_usage.as_deref().unwrap_or("0");
    assert_eq!(tokens_val, "0", "tokens should NOT be incremented when cost breaches: got {tokens_val}");
}

/// Create budget, report usage, reset, verify usage zeroed.
#[tokio::test]
#[serial_test::serial]
async fn test_budget_reset() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let budget_id = "test-budget-reset";
    fcall_create_budget_full(&tc, budget_id,
        &[("total_tokens", 1000)],
        &[],
        3_600_000, // 1 hour reset interval
    ).await;

    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");

    // Report 500 tokens
    let result = fcall_report_usage(&tc, budget_id, &[("total_tokens", 500)]).await;
    assert_eq!(result[0], "OK");

    // Verify usage is 500
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let usage: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("total_tokens")
        .execute().await.unwrap();
    assert_eq!(usage.as_deref(), Some("500"));

    // Reset the budget
    fcall_reset_budget(&tc, budget_id).await;

    // Verify usage is zeroed
    let usage_after: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("total_tokens")
        .execute().await.unwrap();
    assert_eq!(usage_after.as_deref(), Some("0"), "usage should be 0 after reset");

    // Verify reset_count incremented
    let reset_count: Option<String> = tc.client()
        .cmd("HGET").arg(&def_key).arg("reset_count")
        .execute().await.unwrap();
    assert_eq!(reset_count.as_deref(), Some("1"));

    // Verify next_reset_at in resets ZSET (score = now + interval_ms)
    let resets_key = "ff:idx:{b:0}:budget_resets";
    let score: Option<f64> = tc.zscore(resets_key, budget_id).await;
    assert!(score.is_some(), "budget should be in resets ZSET after reset");
    // Score should be roughly now + 3600000 (1 hour)
    let now_ms = TimestampMs::now().0 as f64;
    let diff = score.unwrap() - now_ms;
    assert!(
        diff > 3_500_000.0 && diff < 3_700_000.0,
        "next_reset_at score should be ~3600000ms from now, got diff={diff}"
    );
}

/// Block execution for budget, verify blocked state, unblock, verify eligible.
#[tokio::test]
#[serial_test::serial]
async fn test_block_and_unblock() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    // Create execution (eligible)
    fcall_create_execution(&tc, &eid, NS, LANE, "block_test", 0).await;
    assert_in_eligible(&tc, &eid, &LaneId::new(LANE)).await;

    // Block for budget
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let now = TimestampMs::now();

    let keys: Vec<String> = vec![
        ctx.core(),
        idx.lane_eligible(&lane_id),
        idx.lane_blocked_budget(&lane_id),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        "waiting_for_budget".to_owned(),
        "budget test-budget: total_tokens 100/100".to_owned(),
        now.to_string(),
    ];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_block_execution_for_admission", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_block_execution_for_admission failed");
    assert_ok(&raw, "ff_block_execution_for_admission");

    // Verify: blocked_by_budget, in blocked:budget index, NOT in eligible
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        eligibility_state: Some(ff_core::state::EligibilityState::BlockedByBudget),
        blocking_reason: Some(ff_core::state::BlockingReason::WaitingForBudget),
        public_state: Some(ff_core::state::PublicState::RateLimited),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_not_in_eligible(&tc, &eid, &lane_id).await;
    assert_in_index(&tc, &idx.lane_blocked_budget(&lane_id), &eid).await;

    // Unblock
    let unblock_keys: Vec<String> = vec![
        ctx.core(),
        idx.lane_blocked_budget(&lane_id),
        idx.lane_eligible(&lane_id),
    ];
    let unblock_args: Vec<String> = vec![
        eid.to_string(),
        now.to_string(),
        "waiting_for_budget".to_owned(),
    ];
    let key_refs2: Vec<&str> = unblock_keys.iter().map(|s| s.as_str()).collect();
    let arg_refs2: Vec<&str> = unblock_args.iter().map(|s| s.as_str()).collect();
    let raw2: Value = tc.client()
        .fcall("ff_unblock_execution", &key_refs2, &arg_refs2)
        .await
        .expect("FCALL ff_unblock_execution failed");
    assert_ok(&raw2, "ff_unblock_execution");

    // Verify: eligible_now, back in eligible index
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        eligibility_state: Some(ff_core::state::EligibilityState::EligibleNow),
        blocking_reason: Some(ff_core::state::BlockingReason::WaitingForWorker),
        public_state: Some(ff_core::state::PublicState::Waiting),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_in_eligible(&tc, &eid, &lane_id).await;

    // ── Stale unblock rejection: wrong expected_blocking_reason ──
    // Create another execution and block it for quota
    let eid2 = tc.new_execution_id();
    fcall_create_execution(&tc, &eid2, NS, LANE, "stale_unblock", 0).await;

    let partition2 = execution_partition(&eid2, &config);
    let ctx2 = ExecKeyContext::new(&partition2, &eid2);
    let idx2 = IndexKeys::new(&partition2);
    let now2 = TimestampMs::now();

    // Block for quota
    let block_keys2: Vec<String> = vec![
        ctx2.core(),
        idx2.lane_eligible(&lane_id),
        idx2.lane_blocked_quota(&lane_id),
    ];
    let block_args2: Vec<String> = vec![
        eid2.to_string(), "waiting_for_quota".to_owned(),
        "quota test: rate limit".to_owned(), now2.to_string(),
    ];
    let bk2: Vec<&str> = block_keys2.iter().map(|s| s.as_str()).collect();
    let ba2: Vec<&str> = block_args2.iter().map(|s| s.as_str()).collect();
    let _: Value = tc.client()
        .fcall("ff_block_execution_for_admission", &bk2, &ba2)
        .await.expect("block for quota failed");

    // Try to unblock with WRONG reason (waiting_for_budget instead of waiting_for_quota)
    let unblock_keys2: Vec<String> = vec![
        ctx2.core(),
        idx2.lane_blocked_quota(&lane_id),
        idx2.lane_eligible(&lane_id),
    ];
    let unblock_args2: Vec<String> = vec![
        eid2.to_string(), now2.to_string(),
        "waiting_for_budget".to_owned(), // WRONG reason
    ];
    let uk2: Vec<&str> = unblock_keys2.iter().map(|s| s.as_str()).collect();
    let ua2: Vec<&str> = unblock_args2.iter().map(|s| s.as_str()).collect();
    let raw_stale: Value = tc.client()
        .fcall("ff_unblock_execution", &uk2, &ua2)
        .await.expect("unblock call failed");

    // Should be rejected — status 0 (error)
    let arr_stale = match &raw_stale {
        Value::Array(arr) => arr,
        _ => panic!("expected Array, got {raw_stale:?}"),
    };
    let status_stale = match arr_stale.first() {
        Some(Ok(Value::Int(n))) => *n,
        other => panic!("expected Int, got {other:?}"),
    };
    assert_eq!(status_stale, 0, "unblock with wrong reason should be rejected");

    // Execution should still be blocked
    assert_execution_state(&tc, &eid2, &ExpectedState {
        eligibility_state: Some(ff_core::state::EligibilityState::BlockedByQuota),
        blocking_reason: Some(ff_core::state::BlockingReason::WaitingForQuota),
        ..Default::default()
    }).await;
}

/// Quota admission: admit 2 (limit=2), 3rd denied. Wait for window, admit again.
#[tokio::test]
#[serial_test::serial]
async fn test_quota_admission() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let policy_id = "test-quota-1";
    let window_key = format!("ff:quota:{{q:0}}:{policy_id}:window:requests");
    let concurrency_key = format!("ff:quota:{{q:0}}:{policy_id}:concurrency");
    let def_key = format!("ff:quota:{{q:0}}:{policy_id}");
    let admitted_set_key = format!("ff:quota:{{q:0}}:{policy_id}:admitted_set");

    // Create quota policy: window=1s, rate limit=2, no concurrency cap
    fcall_create_quota_policy(&tc, policy_id, 1, 2, 0).await;

    // Admit execution 1 → ADMITTED
    let eid1 = tc.new_execution_id();
    let guard1 = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{}", eid1);
    let now1 = TimestampMs::now();
    let keys1: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard1, admitted_set_key.clone()];
    let args1: Vec<String> = vec![
        now1.to_string(), "1".to_owned(), "2".to_owned(), // window=1s, limit=2
        "0".to_owned(), eid1.to_string(), "0".to_owned(), // no concurrency cap, no jitter
    ];
    let kr1: Vec<&str> = keys1.iter().map(|s| s.as_str()).collect();
    let ar1: Vec<&str> = args1.iter().map(|s| s.as_str()).collect();
    let raw1: Value = tc.client()
        .fcall("ff_check_admission_and_record", &kr1, &ar1)
        .await.expect("FCALL admission 1 failed");
    let result1 = parse_admission_result(&raw1);
    assert_eq!(result1, "ADMITTED");

    // Admit execution 2 → ADMITTED
    let eid2 = tc.new_execution_id();
    let guard2 = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{}", eid2);
    let now2 = TimestampMs::now();
    let keys2: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard2, admitted_set_key.clone()];
    let args2: Vec<String> = vec![
        now2.to_string(), "1".to_owned(), "2".to_owned(),
        "0".to_owned(), eid2.to_string(), "0".to_owned(),
    ];
    let kr2: Vec<&str> = keys2.iter().map(|s| s.as_str()).collect();
    let ar2: Vec<&str> = args2.iter().map(|s| s.as_str()).collect();
    let raw2: Value = tc.client()
        .fcall("ff_check_admission_and_record", &kr2, &ar2)
        .await.expect("FCALL admission 2 failed");
    assert_eq!(parse_admission_result(&raw2), "ADMITTED");

    // Admit execution 3 → RATE_EXCEEDED (limit=2 per 1s window)
    let eid3 = tc.new_execution_id();
    let guard3 = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{}", eid3);
    let now3 = TimestampMs::now();
    let keys3: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard3, admitted_set_key.clone()];
    let args3: Vec<String> = vec![
        now3.to_string(), "1".to_owned(), "2".to_owned(),
        "0".to_owned(), eid3.to_string(), "0".to_owned(),
    ];
    let kr3: Vec<&str> = keys3.iter().map(|s| s.as_str()).collect();
    let ar3: Vec<&str> = args3.iter().map(|s| s.as_str()).collect();
    let raw3: Value = tc.client()
        .fcall("ff_check_admission_and_record", &kr3, &ar3)
        .await.expect("FCALL admission 3 failed");
    assert_eq!(parse_admission_result(&raw3), "RATE_EXCEEDED");

    // Wait 1.1s for window to expire
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // Admit execution 3 again → should be ADMITTED (window expired)
    let now4 = TimestampMs::now();
    let guard3b = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{}", eid3);
    let keys4: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard3b, admitted_set_key.clone()];
    let args4: Vec<String> = vec![
        now4.to_string(), "1".to_owned(), "2".to_owned(),
        "0".to_owned(), eid3.to_string(), "0".to_owned(),
    ];
    let kr4: Vec<&str> = keys4.iter().map(|s| s.as_str()).collect();
    let ar4: Vec<&str> = args4.iter().map(|s| s.as_str()).collect();
    let raw4: Value = tc.client()
        .fcall("ff_check_admission_and_record", &kr4, &ar4)
        .await.expect("FCALL admission 4 failed");
    assert_eq!(parse_admission_result(&raw4), "ADMITTED");
}

/// Helper to parse admission result (domain-specific format).
fn parse_admission_result(raw: &Value) -> String {
    match raw {
        Value::Array(arr) if !arr.is_empty() => {
            match &arr[0] {
                Ok(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
                Ok(Value::SimpleString(s)) => s.clone(),
                _ => "UNKNOWN".to_owned(),
            }
        }
        _ => panic!("expected Array, got {raw:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE 6 TESTS: flow coordination and dependencies
// ═══════════════════════════════════════════════════════════════════════

async fn set_flow_id_on_exec(tc: &TestCluster, eid: &ExecutionId, flow_id: &str) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    tc.client().cmd("HSET").arg(ctx.core()).arg("flow_id").arg(flow_id)
        .execute::<i64>().await.expect("HSET flow_id");
}

/// FCALL ff_create_flow — returns flow_id or "already_satisfied".
async fn fcall_create_flow(tc: &TestCluster, flow_id: &str) -> String {
    let prefix = format!("ff:flow:{{fp:0}}:{flow_id}");
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        "ff:idx:{fp:0}:flow_index".to_string(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        flow_id.to_owned(), "test".to_owned(), NS.to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_create_flow", &kr, &ar)
        .await
        .expect("FCALL ff_create_flow");
    let arr = expect_success_array(&raw, "ff_create_flow");
    field_str(arr, 2)
}

/// FCALL ff_add_execution_to_flow — returns (eid_or_status, node_count).
///
/// Post-RFC-011 phase-3: FCALL is an atomic single-commit and takes
/// exec_core as KEYS[4]. `execution_id` MUST be pinned to the same
/// partition as this helper's hardcoded `{fp:0}` flow partition —
/// mint via `tc.new_execution_id_on_partition(0)`. Passing a solo-
/// routed exec whose hash-tag ≠ `{fp:0}` fails (cross-slot on cluster,
/// Lua-lib error on standalone when the exec_core key lives in a
/// different logical partition index).
async fn fcall_add_execution_to_flow(
    tc: &TestCluster, flow_id: &str, execution_id: &ExecutionId,
) -> (String, String) {
    let prefix = format!("ff:flow:{{fp:0}}:{flow_id}");
    // exec_core co-located with {fp:0} per the mint-side contract
    // above. Construct via ExecKeyContext so the key format matches
    // production (ff:exec:{fp:N}:<eid>:core).
    let exec_partition = ff_core::partition::execution_partition(
        execution_id,
        tc.partition_config(),
    );
    let ectx = ff_core::keys::ExecKeyContext::new(&exec_partition, execution_id);
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        "ff:idx:{fp:0}:flow_index".to_string(),
        ectx.core(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        flow_id.to_owned(), execution_id.to_string(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_add_execution_to_flow", &kr, &ar)
        .await
        .expect("FCALL ff_add_execution_to_flow");
    let arr = expect_success_array(&raw, "ff_add_execution_to_flow");
    (field_str(arr, 2), field_str(arr, 3))
}

/// FCALL ff_cancel_flow — returns (cancellation_policy, member_eids).
async fn fcall_cancel_flow(
    tc: &TestCluster, flow_id: &str, reason: &str, policy: &str,
) -> (String, Vec<String>) {
    let prefix = format!("ff:flow:{{fp:0}}:{flow_id}");
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        "ff:idx:{fp:0}:flow_index".to_string(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        flow_id.to_owned(), reason.to_owned(), policy.to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_cancel_flow", &kr, &ar)
        .await
        .expect("FCALL ff_cancel_flow");
    let arr = expect_success_array(&raw, "ff_cancel_flow");
    let cancellation_policy = field_str(arr, 2);
    let mut members = Vec::new();
    let mut i = 3;
    loop {
        let s = field_str(arr, i);
        if s.is_empty() {
            break;
        }
        members.push(s);
        i += 1;
    }
    (cancellation_policy, members)
}

/// Higher-level helper: create flow + add members + set flow_id on exec_core.
///
/// Post-RFC-011 phase-3: `ff_add_execution_to_flow`'s step-1 guard
/// requires exec_core to exist (`EXISTS K.exec_core`). Pre-seed a
/// minimal exec_core hash before calling the FCALL.
async fn setup_flow_via_fcall(tc: &TestCluster, flow_id: &str, members: &[&ExecutionId]) {
    fcall_create_flow(tc, flow_id).await;
    for eid in members {
        ensure_exec_core_exists(tc, eid).await;
        fcall_add_execution_to_flow(tc, flow_id, eid).await;
        set_flow_id_on_exec(tc, eid, flow_id).await;
    }
}

/// Minimal exec_core row creator for tests that skip the full
/// `ff_create_execution` FCALL surface. HSETs a single field so
/// `EXISTS` returns true; sufficient for the post-phase-3
/// `ff_add_execution_to_flow` exec-existence guard. Idempotent.
async fn ensure_exec_core_exists(tc: &TestCluster, eid: &ExecutionId) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    tc.client()
        .cmd("HSET")
        .arg(ctx.core())
        .arg("execution_id")
        .arg(eid.to_string())
        .execute::<i64>()
        .await
        .expect("HSET minimal exec_core");
}

async fn fcall_apply_dependency(
    tc: &TestCluster, downstream: &ExecutionId, flow_id: &str,
    edge_id: &str, upstream: &ExecutionId,
) {
    fcall_apply_dependency_with_data_passing(
        tc, downstream, flow_id, edge_id, upstream, "",
    ).await;
}

async fn fcall_apply_dependency_with_data_passing(
    tc: &TestCluster, downstream: &ExecutionId, flow_id: &str,
    edge_id: &str, upstream: &ExecutionId, data_passing_ref: &str,
) {
    let config = test_config();
    let p = execution_partition(downstream, &config);
    let ctx = ExecKeyContext::new(&p, downstream);
    let idx = IndexKeys::new(&p);
    let lane_id = LaneId::new(LANE);
    let eid_p = ff_core::types::EdgeId::parse(edge_id).unwrap();
    let keys: Vec<String> = vec![
        ctx.core(), ctx.deps_meta(), ctx.deps_unresolved(),
        ctx.dep_edge(&eid_p), idx.lane_eligible(&lane_id),
        idx.lane_blocked_dependencies(&lane_id),
        ctx.deps_all_edges(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        flow_id.to_owned(), edge_id.to_owned(), upstream.to_string(),
        "1".to_owned(), "success_only".to_owned(),
        data_passing_ref.to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_apply_dependency_to_child", &kr, &ar)
        .await.expect("FCALL apply_dep");
    assert_ok(&raw, "apply_dep");
}

async fn fcall_resolve_dependency(
    tc: &TestCluster, downstream: &ExecutionId, upstream: &ExecutionId,
    edge_id: &str, outcome: &str,
) -> String {
    let config = test_config();
    let p = execution_partition(downstream, &config);
    let ctx = ExecKeyContext::new(&p, downstream);
    let upstream_ctx = ExecKeyContext::new(&p, upstream);
    let idx = IndexKeys::new(&p);
    let lane_id = LaneId::new(LANE);
    let eid_p = ff_core::types::EdgeId::parse(edge_id).unwrap();
    let att = AttemptIndex::new(0);
    let keys: Vec<String> = vec![
        ctx.core(), ctx.deps_meta(), ctx.deps_unresolved(),
        ctx.dep_edge(&eid_p), idx.lane_eligible(&lane_id),
        idx.lane_terminal(&lane_id), idx.lane_blocked_dependencies(&lane_id),
        ctx.attempt_hash(att), ctx.stream_meta(att),
        ctx.payload(), upstream_ctx.result(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        edge_id.to_owned(), outcome.to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_resolve_dependency", &kr, &ar)
        .await.expect("FCALL resolve_dep");
    let arr = expect_success_array(&raw, "resolve_dep");
    field_str(arr, 2)
}

/// Linear chain A->B->C: complete A, B unblocked, complete B, C unblocked.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_linear_chain() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-chain";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let c = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_bc = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "chain_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "chain_b", 0).await;
    fcall_create_execution(&tc, &c, NS, LANE, "chain_c", 0).await;
    setup_flow_via_fcall(&tc, fid, &[&a, &b, &c]).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    fcall_apply_dependency(&tc, &c, fid, &e_bc, &b).await;
    // B,C blocked; A eligible
    assert_execution_state(&tc, &b, &ExpectedState {
        eligibility_state: Some(ff_core::state::EligibilityState::BlockedByDependencies),
        public_state: Some(ff_core::state::PublicState::WaitingChildren),
        ..Default::default()
    }).await;
    assert_in_eligible(&tc, &a, &LaneId::new(LANE)).await;
    // Complete A, resolve A->B
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    assert_eq!(fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await, "satisfied");
    assert_in_eligible(&tc, &b, &LaneId::new(LANE)).await;
    // Complete B, resolve B->C
    fcall_issue_claim_grant(&tc, &b, LANE, WORKER, WORKER_INST).await;
    let (lb, eb, _, ab) = fcall_claim_execution(&tc, &b, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &b, LANE, WORKER_INST, &lb, &eb, &ab).await;
    assert_eq!(fcall_resolve_dependency(&tc, &c, &b, &e_bc, "success").await, "satisfied");
    assert_in_eligible(&tc, &c, &LaneId::new(LANE)).await;
    // Complete C
    fcall_issue_claim_grant(&tc, &c, LANE, WORKER, WORKER_INST).await;
    let (lc, ec, _, ac) = fcall_claim_execution(&tc, &c, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &c, LANE, WORKER_INST, &lc, &ec, &ac).await;
    for eid in [&a, &b, &c] {
        assert_execution_state(&tc, eid, &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
            terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
            ..Default::default()
        }).await;
    }
}

/// Server-side data_passing_ref resolution (Batch C item 3).
/// Chain A->B with data_passing_ref on the edge. After A completes,
/// B's input_payload should equal A's result (the canned
/// `{"status":"ok"}` string that fcall_complete_execution writes).
#[tokio::test]
#[serial_test::serial]
async fn test_flow_data_passing_ref_injects_on_satisfy() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-dataref";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "dp_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "dp_b", 0).await;
    setup_flow_via_fcall(&tc, fid, &[&a, &b]).await;
    // Stage the edge with a non-empty data_passing_ref to enable copy.
    fcall_apply_dependency_with_data_passing(
        &tc, &b, fid, &e_ab, &a, "result",
    ).await;

    // Complete A with the canned {"status":"ok"} result_payload baked
    // into fcall_complete_execution.
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) =
        fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    assert_eq!(
        fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await,
        "satisfied"
    );

    // Verify B's payload now holds A's result bytes (not the original
    // create_execution payload).
    let config = test_config();
    let p = execution_partition(&b, &config);
    let ctx = ExecKeyContext::new(&p, &b);
    let payload: Option<String> = tc.client()
        .cmd("GET")
        .arg(ctx.payload())
        .execute()
        .await
        .expect("GET b.payload");
    assert_eq!(
        payload.as_deref(),
        Some(r#"{"status":"ok"}"#),
        "data_passing_ref should have copied A's result into B's payload"
    );
}

/// Control test for data_passing_ref: when the edge is staged with an
/// EMPTY data_passing_ref, B's payload must NOT be overwritten by A's
/// result. Guards against an accidental copy on every satisfy.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_data_passing_ref_empty_no_copy() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-nodataref";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "np_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "np_b", 0).await;

    // Write a known payload on B up-front so we can assert it survives.
    let config = test_config();
    let p = execution_partition(&b, &config);
    let ctx = ExecKeyContext::new(&p, &b);
    let _: () = tc.client()
        .cmd("SET")
        .arg(ctx.payload())
        .arg("b_original_payload")
        .execute()
        .await
        .expect("SET b.payload");

    setup_flow_via_fcall(&tc, fid, &[&a, &b]).await;
    // Edge has NO data_passing_ref — injection must be skipped.
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;

    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) =
        fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    assert_eq!(
        fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await,
        "satisfied"
    );

    let payload: Option<String> = tc.client()
        .cmd("GET")
        .arg(ctx.payload())
        .execute()
        .await
        .expect("GET b.payload");
    assert_eq!(
        payload.as_deref(),
        Some("b_original_payload"),
        "empty data_passing_ref must preserve B's original payload"
    );
}

/// Fan-out: A->B, A->C. Complete A, both B and C unblocked.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_fan_out() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-fanout";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let c = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_ac = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "fo_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "fo_b", 0).await;
    fcall_create_execution(&tc, &c, NS, LANE, "fo_c", 0).await;
    setup_flow_via_fcall(&tc, fid, &[&a, &b, &c]).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    fcall_apply_dependency(&tc, &c, fid, &e_ac, &a).await;
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await;
    fcall_resolve_dependency(&tc, &c, &a, &e_ac, "success").await;
    assert_in_eligible(&tc, &b, &LaneId::new(LANE)).await;
    assert_in_eligible(&tc, &c, &LaneId::new(LANE)).await;
}

/// Failure propagation: A->B->C. A ok, B fails, C skipped.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_dependency_failure_propagation() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-fail";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let c = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_bc = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "fp_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "fp_b", 0).await;
    fcall_create_execution(&tc, &c, NS, LANE, "fp_c", 0).await;
    setup_flow_via_fcall(&tc, fid, &[&a, &b, &c]).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    fcall_apply_dependency(&tc, &c, fid, &e_bc, &b).await;
    // Complete A, resolve A->B
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await;
    // Fail B
    fcall_issue_claim_grant(&tc, &b, LANE, WORKER, WORKER_INST).await;
    let (lb, eb, _, ab) = fcall_claim_execution(&tc, &b, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_fail_execution(&tc, &b, LANE, WORKER_INST, &lb, &eb, &ab, "err", "worker_error", "").await;
    // Resolve B->C as "failed" -> C becomes skipped
    assert_eq!(fcall_resolve_dependency(&tc, &c, &b, &e_bc, "failed").await, "impossible");
    assert_execution_state(&tc, &c, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Skipped),
        public_state: Some(ff_core::state::PublicState::Skipped),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &c).await;
    assert_in_terminal(&tc, &c, &LaneId::new(LANE)).await;
}

/// Resolve idempotency: resolve same edge twice -> second is already_resolved.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_resolve_idempotency() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-idem";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "ri_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "ri_b", 0).await;
    setup_flow_via_fcall(&tc, fid, &[&a, &b]).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    assert_eq!(fcall_resolve_dependency(&tc, &b, &a, &e_ab, "failed").await, "impossible");
    // Second resolve -> already_resolved
    assert_eq!(fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await, "already_resolved");
    // B still skipped
    assert_execution_state(&tc, &b, &ExpectedState {
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Skipped),
        ..Default::default()
    }).await;
}

/// Full integration: flow A->B, complete both.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_with_execution_lifecycle() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-full";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "fl_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "fl_b", 0).await;
    setup_flow_via_fcall(&tc, fid, &[&a, &b]).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    assert_in_eligible(&tc, &a, &LaneId::new(LANE)).await;
    assert_execution_state(&tc, &b, &ExpectedState {
        eligibility_state: Some(ff_core::state::EligibilityState::BlockedByDependencies),
        ..Default::default()
    }).await;
    // Complete A, resolve, claim+complete B
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await;
    assert_in_eligible(&tc, &b, &LaneId::new(LANE)).await;
    fcall_issue_claim_grant(&tc, &b, LANE, WORKER, WORKER_INST).await;
    let (lb, eb, _, ab) = fcall_claim_execution(&tc, &b, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &b, LANE, WORKER_INST, &lb, &eb, &ab).await;
    for eid in [&a, &b] {
        assert_execution_state(&tc, eid, &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
            terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
            ..Default::default()
        }).await;
    }
}

/// Replay after skip: A->B. Fail A -> B skipped. Replay B -> blocked_by_dependencies.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_replay_after_skip() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-replay";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    let config = test_config();

    fcall_create_execution(&tc, &a, NS, LANE, "rep_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "rep_b", 0).await;
    setup_flow_via_fcall(&tc, fid, &[&a, &b]).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;

    // Fail A (terminal)
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_fail_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa, "err", "worker_error", "").await;

    // Resolve A->B as "failed" -> B becomes skipped
    fcall_resolve_dependency(&tc, &b, &a, &e_ab, "failed").await;
    assert_execution_state(&tc, &b, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Skipped),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &b).await;

    // Replay B via ff_replay_execution (skipped flow member path)
    let p_b = execution_partition(&b, &config);
    let ctx_b = ExecKeyContext::new(&p_b, &b);
    let idx_b = IndexKeys::new(&p_b);
    let lane_id = LaneId::new(LANE);
    let edge_parsed = ff_core::types::EdgeId::parse(&e_ab).unwrap();

    // KEYS (4+N): exec_core, terminal_zset, eligible_zset, lease_history,
    //             blocked_deps_zset, deps_meta, deps_unresolved, dep_edge_0
    let replay_keys: Vec<String> = vec![
        ctx_b.core(),                              // 1
        idx_b.lane_terminal(&lane_id),             // 2
        idx_b.lane_eligible(&lane_id),             // 3
        ctx_b.lease_history(),                     // 4
        idx_b.lane_blocked_dependencies(&lane_id), // 5
        ctx_b.deps_meta(),                         // 6
        ctx_b.deps_unresolved(),                   // 7
        ctx_b.dep_edge(&edge_parsed),              // 8
    ];
    // ARGV (2+N): execution_id, now_ms, edge_id_0
    let now = TimestampMs::now();
    let replay_args: Vec<String> = vec![
        b.to_string(),      // 1
        now.to_string(),     // 2
        e_ab.clone(),        // 3 edge_id
    ];
    let rk: Vec<&str> = replay_keys.iter().map(|s| s.as_str()).collect();
    let ra: Vec<&str> = replay_args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_replay_execution", &rk, &ra)
        .await.expect("FCALL ff_replay_execution");
    assert_ok(&raw, "ff_replay_execution");

    // B should now be runnable/blocked_by_dependencies (waiting for A to resolve again)
    assert_execution_state(&tc, &b, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        eligibility_state: Some(ff_core::state::EligibilityState::BlockedByDependencies),
        blocking_reason: Some(ff_core::state::BlockingReason::WaitingForChildren),
        attempt_state: Some(ff_core::state::AttemptState::PendingReplayAttempt),
        public_state: Some(ff_core::state::PublicState::WaitingChildren),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &b).await;

    // Verify dep edge was reset from impossible back to unsatisfied
    let dep_state: Option<String> = tc.hget(
        &ctx_b.dep_edge(&edge_parsed), "state",
    ).await;
    assert_eq!(dep_state.as_deref(), Some("unsatisfied"),
        "dep edge should be reset to unsatisfied after replay");

    // Verify B is in blocked:deps index, NOT terminal
    assert_in_index(&tc, &idx_b.lane_blocked_dependencies(&lane_id), &b).await;
    assert_not_in_index(&tc, &idx_b.lane_terminal(&lane_id), &b).await;
}

// ─── Priority clamp + ordering ─────────────────────────────────────────

/// Verify that priority values outside [0, 9000] are clamped, and that
/// higher-priority executions appear first in the eligible ZSET (lower score).
#[tokio::test]
#[serial_test::serial]
async fn test_priority_clamp_and_ordering() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Create execution with priority=10000 (should be clamped to 9000)
    let high = tc.new_execution_id();
    let (_, _) = fcall_create_execution(
        &tc, &high, "ns_prio", "default", "clamp_test", 10000,
    ).await;

    let config = test_config();
    let partition_high = execution_partition(&high, &config);
    let ctx_high = ExecKeyContext::new(&partition_high, &high);

    // Read stored priority — should be 9000, not 10000
    let stored_priority: Option<String> = tc.hget(&ctx_high.core(), "priority").await;
    assert_eq!(
        stored_priority.as_deref(),
        Some("9000"),
        "priority=10000 should be clamped to 9000"
    );

    // Create execution with priority=-5 (should be clamped to 0)
    let neg = tc.new_execution_id();
    let (_, _) = fcall_create_execution(
        &tc, &neg, "ns_prio", "default", "clamp_test", -5,
    ).await;

    let partition_neg = execution_partition(&neg, &config);
    let ctx_neg = ExecKeyContext::new(&partition_neg, &neg);

    let stored_neg: Option<String> = tc.hget(&ctx_neg.core(), "priority").await;
    assert_eq!(
        stored_neg.as_deref(),
        Some("0"),
        "priority=-5 should be clamped to 0"
    );

    // Create execution with priority=50 (should be stored as-is)
    let low = tc.new_execution_id();
    let (_, _) = fcall_create_execution(
        &tc, &low, "ns_prio", "default", "clamp_test", 50,
    ).await;

    let partition_low = execution_partition(&low, &config);
    let ctx_low = ExecKeyContext::new(&partition_low, &low);

    let stored_low: Option<String> = tc.hget(&ctx_low.core(), "priority").await;
    assert_eq!(
        stored_low.as_deref(),
        Some("50"),
        "priority=50 should be stored as-is"
    );

    // Verify ordering in eligible ZSET: higher priority = lower score = first
    // If high and low are on the same partition, we can compare scores directly.
    // The score formula is: -(priority * 1_000_000_000_000) + created_at
    // priority=9000 should have a MUCH lower score than priority=50.
    if partition_high.index == partition_low.index {
        let idx = IndexKeys::new(&partition_high);
        let lane_id = LaneId::new("default");
        let eligible_key = idx.lane_eligible(&lane_id);

        // ZSCORE for both
        let score_high: Option<String> = tc.client()
            .cmd("ZSCORE")
            .arg(&eligible_key)
            .arg(high.to_string().as_str())
            .execute()
            .await
            .unwrap_or(None);

        let score_low: Option<String> = tc.client()
            .cmd("ZSCORE")
            .arg(&eligible_key)
            .arg(low.to_string().as_str())
            .execute()
            .await
            .unwrap_or(None);

        if let (Some(sh), Some(sl)) = (score_high, score_low) {
            let sh_f: f64 = sh.parse().unwrap();
            let sl_f: f64 = sl.parse().unwrap();
            assert!(
                sh_f < sl_f,
                "priority=9000 (score={sh_f}) should have lower score than priority=50 (score={sl_f})"
            );
        }
    }
}

// ─── Phase 7: SDK concurrency enforcement ───

/// Verify that FlowFabricWorker.max_concurrent_tasks is enforced via semaphore.
/// Create 3 executions. Claim 2 (max). 3rd claim returns None. Complete one.
/// 4th claim succeeds (permit freed).
#[tokio::test]
#[serial_test::serial]
async fn test_max_concurrent_tasks_enforcement() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Write the test partition config to Valkey so the SDK worker reads the
    // same 4-partition layout that fcall_create_execution uses.
    let config = test_config();
    let config_key = "ff:config:partitions";
    let _: () = tc.client().cmd("HSET")
        .arg(config_key)
        .arg("num_flow_partitions")
        .arg(config.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(config.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(config.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();

    // Create 3 eligible executions via raw FCALL
    let eids: Vec<ExecutionId> = (0..3).map(|_| tc.new_execution_id()).collect();
    for eid in &eids {
        fcall_create_execution(&tc, eid, NS, LANE, "concurrency_test", 0).await;
    }

    // Build a worker with max_concurrent_tasks = 2
    let worker_config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new("concurrency-test-worker"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new("concurrency-test-inst"),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 2,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config).await.unwrap();

    // Claim 1: should succeed
    let task1 = worker.claim_next().await.unwrap();
    assert!(task1.is_some(), "claim 1 should succeed (0/2 active)");
    let task1 = task1.unwrap();

    // Claim 2: should succeed
    let task2 = worker.claim_next().await.unwrap();
    assert!(task2.is_some(), "claim 2 should succeed (1/2 active)");
    let task2 = task2.unwrap();

    // Claim 3: should return None (at capacity, 2/2 active)
    let task3 = worker.claim_next().await.unwrap();
    assert!(task3.is_none(), "claim 3 should return None (2/2 active — at capacity)");

    // Complete task1 — frees one permit
    task1.complete(Some(b"done".to_vec())).await.unwrap();

    // Claim 4: should now succeed (1/2 active after completion)
    let task4 = worker.claim_next().await.unwrap();
    assert!(task4.is_some(), "claim 4 should succeed after completing task1 (1/2 active)");

    // Cleanup: complete remaining tasks
    task2.complete(Some(b"done".to_vec())).await.unwrap();
    task4.unwrap().complete(Some(b"done".to_vec())).await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// ROUND 5: execution_deadline expiry on runnable (unclaimed) execution
// ═══════════════════════════════════════════════════════════════════════

/// Expire a RUNNABLE (never-claimed) execution via ff_expire_execution.
/// Exercises the execution_deadline scanner's primary use case: absolute
/// deadline fires while the execution is still waiting in the eligible set.
/// Verifies: terminal/expired state, ZREM from eligible, ZADD to terminal,
/// execution_deadline index cleaned, no attempt ended (none existed).
#[tokio::test]
#[serial_test::serial]
async fn test_execution_deadline_expire_runnable() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    // Create execution with an absolute deadline (1 ms from now — already expired)
    let now_ms = TimestampMs::now().0;
    let deadline_at = (now_ms + 1).to_string();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&lane_id),
        ctx.noop(), // idem_key placeholder (cluster-safe)
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        LANE.to_owned(),
        "deadline_test".to_owned(),
        "0".to_owned(),
        "e2e-test".to_owned(),
        "{}".to_owned(),
        r#"{"test":"deadline"}"#.to_owned(),
        String::new(),      // delay_until
        String::new(),      // dedup_ttl_ms
        "{}".to_owned(),    // tags_json
        deadline_at,        // execution_deadline_at
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution with deadline");
    assert_ok(&raw, "ff_create_execution");

    // Verify: in eligible set
    assert_in_eligible(&tc, &eid, &lane_id).await;

    // Verify: in execution_deadline index
    let deadline_key = idx.execution_deadline();
    let score: Option<String> = tc.client()
        .cmd("ZSCORE")
        .arg(&deadline_key)
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .unwrap_or(None);
    assert!(score.is_some(), "execution should be in execution_deadline index");

    // Expire via ff_expire_execution (simulates execution_deadline scanner)
    fcall_expire_execution(&tc, &eid, LANE, "execution_deadline").await;

    // Verify: terminal/expired
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Expired),
        public_state: Some(ff_core::state::PublicState::Expired),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_public_state_consistent(&tc, &eid).await;

    // Verify: NOT in eligible set (runnable path cleaned it up)
    assert_not_in_eligible(&tc, &eid, &lane_id).await;

    // Verify: IN terminal set
    assert_in_terminal(&tc, &eid, &lane_id).await;

    // Verify: execution_deadline index cleaned
    let score_after: Option<String> = tc.client()
        .cmd("ZSCORE")
        .arg(&deadline_key)
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .unwrap_or(None);
    assert!(score_after.is_none(), "should be removed from execution_deadline index");

    // Verify: attempt_state is pending_first_attempt (no attempt existed)
    let att_state = tc.hget(&ctx.core(), "attempt_state").await;
    assert_eq!(att_state.as_deref(), Some("pending_first_attempt"),
        "runnable execution should keep pending_first_attempt");

    // Verify: failure_reason captures the deadline type
    let fr = tc.hget(&ctx.core(), "failure_reason").await;
    assert_eq!(fr.as_deref(), Some("execution_deadline"));
}

// ─── Phase 7: Negative / error path tests ───

/// Verify error paths that production users will hit.
/// These ensure errors return structured codes (not panics or empty arrays).
#[tokio::test]
#[serial_test::serial]
async fn test_error_paths() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let config = test_config();

    // ── 1. Claim grant on nonexistent execution → execution_not_found ──
    let fake_eid = tc.new_execution_id();
    let partition = execution_partition(&fake_eid, &config);
    let ctx = ExecKeyContext::new(&partition, &fake_eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
    ];
    let args: Vec<String> = vec![
        fake_eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        String::new(), // worker_capabilities_csv
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_issue_claim_grant", &kr, &ar)
        .await
        .expect("FCALL should not fail at transport level");
    assert_err(&raw, "execution_not_found", "ff_issue_claim_grant on nonexistent");

    // ── 2. Complete with wrong lease_id → stale_lease ──
    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "error_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, _att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Try complete with wrong lease_id
    let partition2 = execution_partition(&eid, &config);
    let ctx2 = ExecKeyContext::new(&partition2, &eid);
    let idx2 = IndexKeys::new(&partition2);
    let wrong_keys: Vec<String> = vec![
        ctx2.core(),
        ctx2.attempt_hash(AttemptIndex::new(0)),
        idx2.lease_expiry(),
        idx2.worker_leases(&WorkerInstanceId::new(WORKER_INST)),
        idx2.lane_terminal(&lane_id),
        ctx2.lease_current(),
        ctx2.lease_history(),
        idx2.lane_active(&lane_id),
        ctx2.stream_meta(AttemptIndex::new(0)),
        ctx2.result(),
        idx2.attempt_timeout(),
        idx2.execution_deadline(),
    ];
    let wrong_args: Vec<String> = vec![
        eid.to_string(),
        "wrong-lease-id-00000000".to_owned(),  // WRONG lease_id
        epoch.clone(),
        attempt_id.clone(),
        r#"{"done":true}"#.to_owned(),
    ];
    let wkr: Vec<&str> = wrong_keys.iter().map(|s| s.as_str()).collect();
    let war: Vec<&str> = wrong_args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_complete_execution", &wkr, &war)
        .await
        .expect("FCALL should not fail at transport level");
    assert_err(&raw, "stale_lease", "ff_complete_execution with wrong lease_id");

    // Clean up: complete with correct lease
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lease_id, &epoch, &attempt_id).await;

    // ── 3. Create with duplicate dedup key → DUPLICATE ──
    let eid3 = tc.new_execution_id();
    let partition3 = execution_partition(&eid3, &config);
    let ctx3 = ExecKeyContext::new(&partition3, &eid3);
    let idx3 = IndexKeys::new(&partition3);
    let idem_key = format!("ff:dedup:{}:test-idem-key", partition3.hash_tag());

    let dedup_keys: Vec<String> = vec![
        ctx3.core(),
        ctx3.payload(),
        ctx3.policy(),
        ctx3.tags(),
        idx3.lane_eligible(&lane_id),
        idem_key.clone(),
        idx3.execution_deadline(),
        idx3.all_executions(),
    ];
    let dedup_args: Vec<String> = vec![
        eid3.to_string(),
        NS.to_owned(),
        LANE.to_owned(),
        "dedup_test".to_owned(),
        "0".to_owned(),
        "e2e-test".to_owned(),
        "{}".to_owned(),
        r#"{"test":"dedup"}"#.to_owned(),
        String::new(),       // delay_until
        "60000".to_owned(),  // dedup_ttl_ms
        "{}".to_owned(),     // tags_json
        String::new(),       // execution_deadline_at
        partition3.index.to_string(),
    ];
    let dkr: Vec<&str> = dedup_keys.iter().map(|s| s.as_str()).collect();
    let dar: Vec<&str> = dedup_args.iter().map(|s| s.as_str()).collect();
    let raw1: Value = tc.client()
        .fcall("ff_create_execution", &dkr, &dar)
        .await
        .expect("first create should succeed");
    assert_ok(&raw1, "ff_create_execution (first)");

    // Second create with SAME execution ID → DUPLICATE (hits EXISTS guard)
    let raw2: Value = tc.client()
        .fcall("ff_create_execution", &dkr, &dar)
        .await
        .expect("second create should return DUPLICATE");
    // DUPLICATE returns {1, "DUPLICATE", existing_eid} — status=1
    let arr2 = expect_success_array(&raw2, "ff_create_execution (duplicate)");
    let dup_status = field_str(arr2, 1);
    assert_eq!(dup_status, "DUPLICATE", "second create should return DUPLICATE");

    // ── 4. Fail with max retries exhausted → terminal_failed ──
    let eid4 = tc.new_execution_id();
    // Create with a retry policy that has max_retries=0
    fcall_create_execution(&tc, &eid4, NS, LANE, "retry_test", 0).await;

    // Set a policy that allows 0 retries
    let partition4 = execution_partition(&eid4, &config);
    let ctx4 = ExecKeyContext::new(&partition4, &eid4);
    let policy = r#"{"retry_policy":{"max_retries":0}}"#;
    let _: () = tc.client()
        .cmd("SET")
        .arg(ctx4.policy())
        .arg(policy)
        .execute()
        .await
        .unwrap();

    // Claim the execution
    fcall_issue_claim_grant(&tc, &eid4, LANE, WORKER, WORKER_INST).await;
    let (lease_id4, epoch4, _att_idx4, attempt_id4) =
        fcall_claim_execution(&tc, &eid4, LANE, WORKER, WORKER_INST, 30_000).await;

    // Fail it — max_retries=0 means terminal immediately
    let idx4 = IndexKeys::new(&partition4);
    let fail_keys: Vec<String> = vec![
        ctx4.core(),
        ctx4.attempt_hash(AttemptIndex::new(0)),
        idx4.lease_expiry(),
        idx4.worker_leases(&WorkerInstanceId::new(WORKER_INST)),
        idx4.lane_terminal(&lane_id),
        idx4.lane_delayed(&lane_id),
        ctx4.lease_current(),
        ctx4.lease_history(),
        idx4.lane_active(&lane_id),
        ctx4.stream_meta(AttemptIndex::new(0)),
        idx4.attempt_timeout(),
        idx4.execution_deadline(),
    ];
    let fail_args: Vec<String> = vec![
        eid4.to_string(),
        lease_id4,
        epoch4,
        attempt_id4,
        "test_failure".to_owned(),
        "transient".to_owned(),
        r#"{"max_retries":0}"#.to_owned(),
    ];
    let fkr: Vec<&str> = fail_keys.iter().map(|s| s.as_str()).collect();
    let far: Vec<&str> = fail_args.iter().map(|s| s.as_str()).collect();
    let raw4: Value = tc.client()
        .fcall("ff_fail_execution", &fkr, &far)
        .await
        .expect("fail should succeed");
    // Should return ok("terminal_failed")
    let arr4 = expect_success_array(&raw4, "ff_fail_execution (terminal)");
    let outcome = field_str(arr4, 2);
    assert_eq!(outcome, "terminal_failed", "max_retries=0 should go terminal");

    // Verify terminal state
    assert_execution_state(&tc, &eid4, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Failed),
        public_state: Some(ff_core::state::PublicState::Failed),
        ..Default::default()
    }).await;
}

// ═══════════════════════════════════════════════════════════════════════
// ROUND 7: ff-scheduler integration test
// ═══════════════════════════════════════════════════════════════════════

/// Test the ff-scheduler claim_for_worker() path end-to-end.
///
/// Creates 3 executions with different priorities on the same lane.
/// Uses the Scheduler struct (not raw FCALL) to issue claim grants.
/// Verifies priority ordering: highest priority execution is granted first.
/// Then consumes the grant via ff_claim_execution to prove it's valid.
#[tokio::test]
#[serial_test::serial]
async fn test_scheduler_claim_priority_ordering() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let config = test_config();
    let lane_id = LaneId::new(LANE);
    let worker_id = ff_core::types::WorkerId::new(WORKER);
    let wiid = ff_core::types::WorkerInstanceId::new(WORKER_INST);

    // Create 3 executions with different priorities on the SAME partition.
    // The scheduler iterates partitions sequentially and takes the top candidate
    // per partition — priority ordering is only guaranteed within a partition.
    // Generate UUIDs until all 3 hash to the same execution partition.
    let (eid_low, eid_mid, eid_high) = {
        let mut low;
        let mut mid;
        let mut high;
        loop {
            low = tc.new_execution_id();
            mid = tc.new_execution_id();
            high = tc.new_execution_id();
            let p_low = ff_core::partition::execution_partition(&low, &config);
            let p_mid = ff_core::partition::execution_partition(&mid, &config);
            let p_high = ff_core::partition::execution_partition(&high, &config);
            if p_low.index == p_mid.index && p_mid.index == p_high.index {
                break;
            }
        }
        (low, mid, high)
    };

    fcall_create_execution(&tc, &eid_low, NS, LANE, "sched_test", 10).await;
    // Small sleep to ensure different created_at for tiebreaking
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    fcall_create_execution(&tc, &eid_mid, NS, LANE, "sched_test", 100).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    fcall_create_execution(&tc, &eid_high, NS, LANE, "sched_test", 1000).await;

    // Use the Scheduler to issue claim grants
    let scheduler = ff_scheduler::claim::Scheduler::new(
        tc.client().clone(),
        config,
    );
    let no_caps = std::collections::BTreeSet::<String>::new();

    // First grant should go to highest priority (1000)
    let grant1 = scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &no_caps, 5000)
        .await
        .expect("scheduler claim 1 should not error");
    let grant1 = grant1.expect("should find an eligible execution");
    assert_eq!(
        grant1.execution_id, eid_high,
        "first grant should go to priority=1000 execution"
    );

    // Consume the grant to transition execution out of eligible
    fcall_claim_execution(
        &tc, &grant1.execution_id, LANE, WORKER, WORKER_INST, 30_000,
    ).await;

    // Second grant should go to next highest priority (100)
    let grant2 = scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &no_caps, 5000)
        .await
        .expect("scheduler claim 2 should not error");
    let grant2 = grant2.expect("should find second eligible execution");
    assert_eq!(
        grant2.execution_id, eid_mid,
        "second grant should go to priority=100 execution"
    );

    // Consume grant2
    fcall_claim_execution(
        &tc, &grant2.execution_id, LANE, WORKER, WORKER_INST, 30_000,
    ).await;

    // Third grant should go to lowest priority (10)
    let grant3 = scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &no_caps, 5000)
        .await
        .expect("scheduler claim 3 should not error");
    let grant3 = grant3.expect("should find third eligible execution");
    assert_eq!(
        grant3.execution_id, eid_low,
        "third grant should go to priority=10 execution"
    );

    // Consume grant3
    fcall_claim_execution(
        &tc, &grant3.execution_id, LANE, WORKER, WORKER_INST, 30_000,
    ).await;

    // Fourth claim: nothing left
    let grant4 = scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &no_caps, 5000)
        .await
        .expect("scheduler claim 4 should not error");
    assert!(grant4.is_none(), "no more eligible executions");
}

/// End-to-end: `Scheduler::claim_for_worker` → `FlowFabricWorker::
/// claim_from_grant` → live `ClaimedTask`. This is the production
/// entry path for consumers that cannot enable the
/// `insecure-direct-claim` feature (cairn, in particular) — the
/// scheduler does admission control and hands off a grant; the SDK
/// consumes the grant without touching the eligible ZSET directly.
///
/// Asserts:
///  1. the grant issued by the scheduler is consumable by
///     `claim_from_grant` (no shape mismatch between the two layers).
///  2. the returned `ClaimedTask` carries the correct attempt_index
///     (0 for a fresh claim), execution_id, and kind/tags.
///  3. `complete()` drops the lease cleanly — proving the concurrency
///     permit was acquired + transferred properly (otherwise the task
///     drop would panic on a non-transferred renewal handle).
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_grant_via_scheduler() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "claim_from_grant_test", 500).await;

    // Issue grant via the Scheduler — not via raw FCALL — so the
    // test exercises the scheduler-to-SDK handoff shape.
    let scheduler = ff_scheduler::claim::Scheduler::new(
        tc.client().clone(),
        test_config(),
    );
    let lane_id = LaneId::new(LANE);
    let worker_id = ff_core::types::WorkerId::new(WORKER);
    let wiid = ff_core::types::WorkerInstanceId::new(WORKER_INST);
    let no_caps = std::collections::BTreeSet::<String>::new();

    let grant = scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &no_caps, 30_000)
        .await
        .expect("scheduler should not error")
        .expect("the created execution should be eligible");
    assert_eq!(grant.execution_id, eid, "grant should point at our execution");

    // Hand the grant to the SDK worker. `build_reclaim_test_worker`
    // configures lanes=[LANE] and max_concurrent_tasks=16 — enough
    // headroom that the semaphore is never the gating factor here.
    let worker = build_reclaim_test_worker(WORKER, WORKER_INST).await;
    let claimed = worker
        .claim_from_grant(lane_id.clone(), grant)
        .await
        .expect("claim_from_grant should succeed on a fresh grant");

    assert_eq!(*claimed.execution_id(), eid, "claimed task should carry our eid");
    assert_eq!(
        claimed.attempt_index().0, 0,
        "fresh claim should have attempt_index 0"
    );
    assert_eq!(claimed.execution_kind(), "claim_from_grant_test");

    // Drop-clean contract: complete() releases the lease, renewal
    // task, and concurrency permit.
    claimed.complete(Some(b"ok".to_vec())).await
        .expect("complete should succeed");

    // State sanity: execution terminal + no active lease.
    assert_execution_state(&tc, &eid, &ExpectedState {
        public_state: Some(ff_core::state::PublicState::Completed),
        ..Default::default()
    }).await;
}

/// Saturation guard: when the worker's `max_concurrent_tasks` is
/// fully committed, `claim_from_grant` must return
/// `SdkError::WorkerAtCapacity` WITHOUT calling
/// `ff_claim_execution` — otherwise the grant would be atomically
/// consumed by the Lua FCALL even though the SDK can't run the
/// work, and the next scheduler tick would see a stuck execution.
///
/// Asserts:
///   1. A worker with `max_concurrent_tasks=1` plus one permit in
///      flight refuses the second grant with
///      `SdkError::WorkerAtCapacity`.
///   2. The grant stays valid — a different worker can consume it.
///   3. `SdkError::WorkerAtCapacity.is_retryable()` is `true`
///      (sanity).
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_grant_rejects_at_capacity() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    // Two separate executions so that if the first claim somehow
    // releases, we still catch the saturation on the second one.
    // Generate UUIDs until both hash to the SAME execution partition —
    // otherwise the scheduler's jittered partition scan may pick them
    // up in a different order than priority dictates (the scheduler
    // visits partitions linearly, priority-ordering is only guaranteed
    // within a single partition).
    let (eid_a, eid_b) = {
        let config = test_config();
        loop {
            let a = tc.new_execution_id();
            let b = tc.new_execution_id();
            let pa = ff_core::partition::execution_partition(&a, &config);
            let pb = ff_core::partition::execution_partition(&b, &config);
            if pa.index == pb.index {
                break (a, b);
            }
        }
    };
    fcall_create_execution(&tc, &eid_a, NS, LANE, "cap_test", 100).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    fcall_create_execution(&tc, &eid_b, NS, LANE, "cap_test", 50).await;

    // Build a worker with max_concurrent_tasks=1 so the second claim
    // is guaranteed to saturate. Use the standard worker identity
    // so both grants are issued to this worker_id.
    let worker_config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new(WORKER),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(WORKER_INST),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config).await.unwrap();

    let scheduler = ff_scheduler::claim::Scheduler::new(
        tc.client().clone(),
        test_config(),
    );
    let lane_id = LaneId::new(LANE);
    let worker_id_t = ff_core::types::WorkerId::new(WORKER);
    let wiid_t = ff_core::types::WorkerInstanceId::new(WORKER_INST);
    let no_caps = std::collections::BTreeSet::<String>::new();

    // 1st grant (highest priority on the shared partition = eid_a).
    // Succeeds and holds the sole permit.
    let g1 = scheduler
        .claim_for_worker(&lane_id, &worker_id_t, &wiid_t, &no_caps, 30_000)
        .await
        .unwrap()
        .expect("eligible execution present");
    assert_eq!(
        g1.execution_id, eid_a,
        "same-partition priority: eid_a (100) comes before eid_b (50)"
    );

    let task1 = worker
        .claim_from_grant(lane_id.clone(), g1)
        .await
        .expect("first claim should take the permit");

    // 2nd grant (eid_b). Semaphore is drained — claim_from_grant
    // must refuse without consuming the grant.
    let g2 = scheduler
        .claim_for_worker(&lane_id, &worker_id_t, &wiid_t, &no_caps, 30_000)
        .await
        .unwrap()
        .expect("second eligible execution present");
    assert_eq!(g2.execution_id, eid_b);
    let g2_key = g2.grant_key.clone();

    match worker.claim_from_grant(lane_id.clone(), g2).await {
        Err(ff_sdk::SdkError::WorkerAtCapacity) => {}
        Err(other) => panic!("expected WorkerAtCapacity, got {other:?}"),
        Ok(_) => panic!("saturated worker should not claim"),
    }

    // Retryability sanity check.
    assert!(
        ff_sdk::SdkError::WorkerAtCapacity.is_retryable(),
        "capacity saturation is transient"
    );

    // Grant unharmed: the grant key still exists in Valkey (FCALL
    // was not called, so the grant hash was not atomically deleted).
    let exists: i64 = tc.client()
        .cmd("EXISTS")
        .arg(&g2_key)
        .execute()
        .await
        .expect("EXISTS on grant_key");
    assert_eq!(exists, 1, "grant_key must still exist after capacity reject");

    // Stronger proof that the grant is not just present but still
    // live: spin up a second FlowFabricWorker with available
    // capacity and have it consume the same grant. If
    // claim_from_grant had leaked the FCALL through on the first
    // worker, ff_claim_execution would have atomically consumed
    // the grant key and this second claim would return
    // `InvalidClaimGrant`. Ok here is proof of liveness.
    //
    // Same worker_id / worker_instance_id so Lua's worker match
    // on the grant passes — we're modelling a retry from a fresh
    // worker process with free capacity, not a different identity.
    let second_worker_config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new(WORKER),
        // Distinct instance id so the alive-key SET NX guard in
        // `FlowFabricWorker::connect` doesn't reject us as a
        // duplicate — same worker_id, new process identity.
        worker_instance_id: ff_core::types::WorkerInstanceId::new("cap-second-inst"),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 16,
    };
    let second_worker = ff_sdk::FlowFabricWorker::connect(second_worker_config)
        .await
        .expect("second worker should connect");

    // Reconstruct the grant struct by hand — the original `g2` was
    // moved into the saturated call. The grant values are
    // unchanged; we just rebuild the Rust wrapper around the same
    // Valkey key.
    let rebuilt_g2 = ff_core::contracts::ClaimGrant {
        execution_id: eid_b.clone(),
        partition: ff_core::partition::execution_partition(&eid_b, &test_config()),
        grant_key: g2_key.clone(),
        // expires_at_ms is advisory on the consumer side; Lua
        // enforces the real expiry via its own TIME read, so
        // copying any value from the pre-rejection grant is fine.
        expires_at_ms: u64::MAX,
    };

    let task2 = second_worker
        .claim_from_grant(lane_id.clone(), rebuilt_g2)
        .await
        .expect("second worker with free capacity must consume the still-live grant");
    assert_eq!(*task2.execution_id(), eid_b);

    // Cleanup both tasks so the test leaves the cluster in a
    // serial-safe state.
    task1.complete(None).await.unwrap();
    task2.complete(None).await.unwrap();
}

// ─── Phase 7: Integration tests ───

/// Budget enforcement e2e: report 5 tokens (OK), report 6 more (HARD_BREACH),
/// verify breach metadata recorded.
#[tokio::test]
#[serial_test::serial]
async fn test_budget_enforcement_with_breach_metadata() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let budget_id = "breach-meta-budget";
    fcall_create_budget(&tc, budget_id, &[("tokens", 10)], &[]).await;

    // Report 5 tokens → OK
    let r1 = fcall_report_usage(&tc, budget_id, &[("tokens", 5)]).await;
    assert_eq!(r1[0], "OK", "5 tokens within limit 10. Got: {r1:?}");

    // Verify usage is 5
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let usage: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("tokens")
        .execute().await.unwrap();
    assert_eq!(usage.as_deref(), Some("5"));

    // Report 6 more (total 11 > limit 10) → HARD_BREACH
    let r2 = fcall_report_usage(&tc, budget_id, &[("tokens", 6)]).await;
    assert_eq!(r2[0], "HARD_BREACH", "11 tokens > limit 10. Got: {r2:?}");
    assert_eq!(r2[1], "tokens", "breach should be on 'tokens' dim");
    assert_eq!(r2[2], "5", "current_usage should be 5. Got: {r2:?}");
    assert_eq!(r2[3], "10", "hard_limit should be 10. Got: {r2:?}");

    // Verify usage is STILL 5 (check-before-increment: no overshoot)
    let usage2: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("tokens")
        .execute().await.unwrap();
    assert_eq!(usage2.as_deref(), Some("5"), "breach should not increment usage");

    // Verify breach metadata
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let breach_count: Option<String> = tc.client()
        .cmd("HGET").arg(&def_key).arg("breach_count")
        .execute().await.unwrap();
    assert_eq!(breach_count.as_deref(), Some("1"), "breach_count should be 1");

    let last_breach_dim: Option<String> = tc.client()
        .cmd("HGET").arg(&def_key).arg("last_breach_dim")
        .execute().await.unwrap();
    assert_eq!(last_breach_dim.as_deref(), Some("tokens"));
}

/// Stream payload size enforcement: 1KB OK, 64KB OK, 65KB rejected.
#[tokio::test]
#[serial_test::serial]
async fn test_stream_payload_size_enforcement() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "stream_size_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // 1KB payload → OK
    let payload_1k = "x".repeat(1024);
    let (stream_id, _) = fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "delta", &payload_1k, 10000,
    ).await;
    assert!(!stream_id.is_empty(), "1KB append should succeed");

    // 64KB payload (exactly at limit) → OK
    let payload_64k = "y".repeat(65536);
    let (stream_id2, _) = fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "delta", &payload_64k, 10000,
    ).await;
    assert!(!stream_id2.is_empty(), "64KB append should succeed");

    // 65KB payload (over limit) → should fail with retention_limit_exceeded
    let payload_65k = "z".repeat(65537);
    let raw = fcall_append_frame_raw(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "delta", &payload_65k,
    ).await;
    assert_err(&raw, "retention_limit_exceeded", "ff_append_frame 65KB");

    // Verify stream has exactly 2 frames
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let att = AttemptIndex::new(att_idx.parse().unwrap());
    let frame_count: Option<String> = tc.client()
        .cmd("HGET").arg(ctx.stream_meta(att)).arg("frame_count")
        .execute().await.unwrap();
    assert_eq!(frame_count.as_deref(), Some("2"), "only 2 frames should exist");

    // Use previously-unused assert_stream_exists helper
    assert_stream_exists(&tc, &eid, att).await;
}

/// Quota concurrency enforcement: max_concurrent=2, admit 2, reject 3rd.
#[tokio::test]
#[serial_test::serial]
async fn test_quota_concurrency_enforcement() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let policy_id = "concurrency-quota";
    let window_key = format!("ff:quota:{{q:0}}:{policy_id}:window:requests");
    let concurrency_key = format!("ff:quota:{{q:0}}:{policy_id}:concurrency");
    let def_key = format!("ff:quota:{{q:0}}:{policy_id}");

    // Create quota policy: window=60s, no rate limit, concurrency cap=2
    fcall_create_quota_policy(&tc, policy_id, 60, 0, 2).await;

    // Admit E1 → ADMITTED (concurrency_cap=2, no rate limit)
    let e1 = tc.new_execution_id();
    let r1 = fcall_admit(&tc, policy_id, &e1, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r1, "ADMITTED");

    // Admit E2 → ADMITTED
    let e2 = tc.new_execution_id();
    let r2 = fcall_admit(&tc, policy_id, &e2, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r2, "ADMITTED");

    // Admit E3 → CONCURRENCY_EXCEEDED
    let e3 = tc.new_execution_id();
    let r3 = fcall_admit(&tc, policy_id, &e3, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r3, "CONCURRENCY_EXCEEDED", "3rd admission should be rejected (cap=2)");

    // Simulate E1 completing: decrement concurrency counter
    let _: i64 = tc.client()
        .cmd("DECR").arg(&concurrency_key)
        .execute().await.unwrap();

    // Admit E3 again → ADMITTED (one slot freed)
    let r4 = fcall_admit(&tc, policy_id, &e3, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r4, "ADMITTED", "after DECR, 3rd should be admitted");
}

/// Helper for quota admission tests.
async fn fcall_admit(
    tc: &TestCluster,
    policy_id: &str,
    eid: &ExecutionId,
    window_key: &str,
    concurrency_key: &str,
    def_key: &str,
) -> String {
    let guard = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{eid}");
    let admitted_set = format!("ff:quota:{{q:0}}:{policy_id}:admitted_set");
    let now = TimestampMs::now();
    let keys: Vec<String> = vec![
        window_key.to_owned(), concurrency_key.to_owned(),
        def_key.to_owned(), guard, admitted_set,
    ];
    let args: Vec<String> = vec![
        now.to_string(), "60".to_owned(), "0".to_owned(),
        "2".to_owned(), eid.to_string(), "0".to_owned(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_check_admission_and_record", &kr, &ar)
        .await.expect("FCALL admission failed");
    parse_admission_result(&raw)
}

/// Golden path: exercises EVERY major operation in sequence.
/// create → claim → update_progress → append_frame → renew_lease → suspend → signal → resume → complete
#[tokio::test]
#[serial_test::serial]
async fn test_golden_path_all_methods() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let lane_id = LaneId::new(LANE);

    // 1. Create
    fcall_create_execution(&tc, &eid, NS, LANE, "golden_path", 0).await;

    // 2. Claim
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // 3. Update progress (exercises the R6 ARGV fix)
    let keys_p: Vec<String> = vec![ctx.core()];
    let args_p: Vec<String> = vec![
        eid.to_string(), lease_id.clone(), epoch.clone(),
        "50".to_owned(), "halfway there".to_owned(),
    ];
    let kr_p: Vec<&str> = keys_p.iter().map(|s| s.as_str()).collect();
    let ar_p: Vec<&str> = args_p.iter().map(|s| s.as_str()).collect();
    let raw_p: Value = tc.client()
        .fcall("ff_update_progress", &kr_p, &ar_p)
        .await.expect("FCALL ff_update_progress");
    assert_ok(&raw_p, "ff_update_progress");

    // Verify progress stored correctly (not UUID corruption from pre-R6 bug)
    let pct = tc.hget(&ctx.core(), "progress_pct").await;
    assert_eq!(pct.as_deref(), Some("50"), "progress_pct should be '50', not a UUID");
    let msg = tc.hget(&ctx.core(), "progress_message").await;
    assert_eq!(msg.as_deref(), Some("halfway there"));

    // 4. Append frame
    let (stream_id, count) = fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "delta", r#"{"token":"hello"}"#, 10000,
    ).await;
    assert!(!stream_id.is_empty());
    assert_eq!(count, "1");

    // 5. Renew lease
    fcall_renew_lease(&tc, &eid, &att_idx, &attempt_id, &lease_id, &epoch, 30_000).await;

    // 6. Suspend
    let resume_cond = build_resume_condition_json(&["continue"], "fail");
    let (_susp_id, wp_id_str, _wp_key, sub_status, wp_token) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST, &lease_id, &epoch, &att_idx, &attempt_id,
        "waiting_for_signal", &resume_cond, None, "fail",
    ).await;
    assert_eq!(sub_status, "OK");

    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Suspended),
        attempt_state: Some(ff_core::state::AttemptState::AttemptInterrupted),
        ..Default::default()
    }).await;

    // Verify blocking_detail contains the reason code
    let detail = tc.hget(&ctx.core(), "blocking_detail").await.unwrap_or_default();
    assert!(detail.contains("waiting_for_signal"),
        "blocking_detail should contain 'waiting_for_signal', got: {detail}");

    // 7. Deliver signal → triggers resume
    let (sig_id, effect) = fcall_deliver_signal(
        &tc, &eid, LANE, &wp_id_str, "continue", "api", "", &wp_token,
    ).await;
    assert!(!sig_id.is_empty());
    assert_eq!(effect, "resume_condition_satisfied");

    // Verify runnable/eligible after resume
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        eligibility_state: Some(ff_core::state::EligibilityState::EligibleNow),
        attempt_state: Some(ff_core::state::AttemptState::AttemptInterrupted),
        ..Default::default()
    }).await;

    // 8. Re-claim (resumed execution — same attempt continues)
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id2, epoch2, att_idx2, attempt_id2) =
        fcall_claim_resumed_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    assert_eq!(att_idx2, "0", "resumed claim should reuse attempt 0");
    assert_eq!(epoch2, "2", "resumed claim should be epoch 2");

    // 9. Complete
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lease_id2, &epoch2, &attempt_id2).await;

    // Final state: terminal/completed
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
        public_state: Some(ff_core::state::PublicState::Completed),
        attempt_state: Some(ff_core::state::AttemptState::AttemptTerminal),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
    assert_in_terminal(&tc, &eid, &lane_id).await;
}

// ═══════════════════════════════════════════════════════════════════════
// ROUND 7: Full flow lifecycle with edge staging + cycle detection
// ═══════════════════════════════════════════════════════════════════════

/// Helper: stage a dependency edge via ff_stage_dependency_edge on {fp:0}.
async fn fcall_stage_dependency_edge(
    tc: &TestCluster, flow_id: &str,
    edge_id: &str, upstream: &ExecutionId, downstream: &ExecutionId,
    expected_rev: &str,
) -> String {
    let prefix = format!("ff:flow:{{fp:0}}:{flow_id}");
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        format!("{prefix}:edge:{edge_id}"),
        format!("{prefix}:out:{upstream}"),
        format!("{prefix}:in:{downstream}"),
        format!("{prefix}:grant:{edge_id}"),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        flow_id.to_owned(), edge_id.to_owned(),
        upstream.to_string(), downstream.to_string(),
        "success_only".to_owned(), String::new(),
        expected_rev.to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_stage_dependency_edge", &kr, &ar)
        .await
        .expect("FCALL ff_stage_dependency_edge");
    let arr = expect_success_array(&raw, "ff_stage_dependency_edge");
    field_str(arr, 3)
}

/// Helper: stage edge that should fail; returns error code.
async fn fcall_stage_dependency_edge_expect_err(
    tc: &TestCluster, flow_id: &str,
    edge_id: &str, upstream: &ExecutionId, downstream: &ExecutionId,
    expected_rev: &str,
) -> String {
    let prefix = format!("ff:flow:{{fp:0}}:{flow_id}");
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        format!("{prefix}:edge:{edge_id}"),
        format!("{prefix}:out:{upstream}"),
        format!("{prefix}:in:{downstream}"),
        format!("{prefix}:grant:{edge_id}"),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        flow_id.to_owned(), edge_id.to_owned(),
        upstream.to_string(), downstream.to_string(),
        "success_only".to_owned(), String::new(),
        expected_rev.to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_stage_dependency_edge", &kr, &ar)
        .await
        .expect("FCALL should not fail at transport level");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("expected array"),
    };
    field_str(arr, 1)
}

/// Full flow lifecycle with edge staging and cycle detection.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_full_lifecycle_with_staging() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let fid = "flow-r7";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let c = tc.new_execution_id_on_partition(0);
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_bc = uuid::Uuid::new_v4().to_string();
    let lane_id = LaneId::new(LANE);

    // 1. Create 3 executions
    fcall_create_execution(&tc, &a, NS, LANE, "r7_a", 5).await;
    fcall_create_execution(&tc, &b, NS, LANE, "r7_b", 5).await;
    fcall_create_execution(&tc, &c, NS, LANE, "r7_c", 5).await;

    // 2. Create flow + members
    setup_flow_via_fcall(&tc, fid, &[&a, &b, &c]).await;

    // 3. Stage edges (graph_revision starts at 3 after adding 3 members)
    let rev1 = fcall_stage_dependency_edge(&tc, fid, &e_ab, &a, &b, "3").await;
    assert_eq!(rev1, "4");
    let rev2 = fcall_stage_dependency_edge(&tc, fid, &e_bc, &b, &c, "4").await;
    assert_eq!(rev2, "5");

    // 4. Cycle detection: C->A rejected
    let err = fcall_stage_dependency_edge_expect_err(
        &tc, fid, &uuid::Uuid::new_v4().to_string(), &c, &a, "5",
    ).await;
    assert_eq!(err, "cycle_detected");

    // 5. Apply deps
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    fcall_apply_dependency(&tc, &c, fid, &e_bc, &b).await;
    assert_in_eligible(&tc, &a, &lane_id).await;
    assert_not_in_eligible(&tc, &b, &lane_id).await;
    assert_not_in_eligible(&tc, &c, &lane_id).await;

    // 6. Complete A, resolve A->B, B eligible
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    assert_eq!(fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await, "satisfied");
    assert_in_eligible(&tc, &b, &lane_id).await;
    assert_not_in_eligible(&tc, &c, &lane_id).await;

    // 7. Complete B, resolve B->C, C eligible
    fcall_issue_claim_grant(&tc, &b, LANE, WORKER, WORKER_INST).await;
    let (lb, eb, _, ab) = fcall_claim_execution(&tc, &b, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &b, LANE, WORKER_INST, &lb, &eb, &ab).await;
    assert_eq!(fcall_resolve_dependency(&tc, &c, &b, &e_bc, "success").await, "satisfied");
    assert_in_eligible(&tc, &c, &lane_id).await;

    // 8. Complete C
    fcall_issue_claim_grant(&tc, &c, LANE, WORKER, WORKER_INST).await;
    let (lc, ec, _, ac) = fcall_claim_execution(&tc, &c, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &c, LANE, WORKER_INST, &lc, &ec, &ac).await;

    // 9. All terminal/success
    for eid in [&a, &b, &c] {
        assert_execution_state(&tc, eid, &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
            terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
            public_state: Some(ff_core::state::PublicState::Completed),
            ..Default::default()
        }).await;
        assert_state_vector_complete(&tc, eid).await;
        assert_in_terminal(&tc, eid, &lane_id).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ROUND 9: SDK suspend → signal → resume → re-claim end-to-end
// ═══════════════════════════════════════════════════════════════════════

/// Full SDK suspend/resume cycle: claim → suspend → deliver_signal → re-claim → complete.
/// Tests the R8 fix (claim_resumed_execution fallback in claim_next).
#[tokio::test]
#[serial_test::serial]
async fn test_sdk_suspend_signal_resume_reclaim() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Write partition config for SDK
    let config = test_config();
    let config_key = "ff:config:partitions";
    let _: () = tc.client().cmd("HSET")
        .arg(config_key)
        .arg("num_flow_partitions").arg(config.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions").arg(config.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions").arg(config.num_quota_partitions.to_string().as_str())
        .execute().await.unwrap();

    // Create an execution via raw FCALL
    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "sdk_suspend_test", 0).await;

    // Build SDK worker
    let worker_config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new("suspend-test-worker"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new("suspend-test-inst"),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 10,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config).await.unwrap();

    // 1. Claim via SDK
    let task1 = worker.claim_next().await.unwrap();
    assert!(task1.is_some(), "should claim the execution");
    let task1 = task1.unwrap();
    let eid_claimed = task1.execution_id().clone();
    assert_eq!(eid_claimed, eid);

    // 2. Suspend with a signal condition
    let outcome = task1.suspend(
        "waiting_for_signal",
        &[ff_sdk::task::ConditionMatcher { signal_name: "test_signal".into() }],
        Some(60_000), // 60s timeout
        ff_sdk::task::TimeoutBehavior::Fail,
    ).await.unwrap();

    let (waitpoint_id, _waitpoint_key, waitpoint_token) = match outcome {
        ff_sdk::task::SuspendOutcome::Suspended {
            waitpoint_id, waitpoint_key, waitpoint_token, ..
        } => (waitpoint_id, waitpoint_key, waitpoint_token),
        ff_sdk::task::SuspendOutcome::AlreadySatisfied { .. } => {
            panic!("should not be already satisfied");
        }
    };

    // Verify execution is suspended
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Suspended),
        public_state: Some(ff_core::state::PublicState::Suspended),
        ..Default::default()
    }).await;

    // 3. Deliver signal to resume
    let signal = ff_sdk::task::Signal {
        signal_name: "test_signal".into(),
        signal_category: "test".into(),
        payload: Some(b"hello".to_vec()),
        source_type: "test".into(),
        source_identity: "test-runner".into(),
        idempotency_key: None,
        waitpoint_token,
    };
    let sig_outcome = worker.deliver_signal(&eid, &waitpoint_id, signal).await.unwrap();
    match sig_outcome {
        ff_sdk::task::SignalOutcome::TriggeredResume { .. } => {}
        other => panic!("expected TriggeredResume, got {other:?}"),
    }

    // Verify execution is now runnable/eligible (resumed)
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        eligibility_state: Some(ff_core::state::EligibilityState::EligibleNow),
        public_state: Some(ff_core::state::PublicState::Waiting),
        ..Default::default()
    }).await;

    // 4. Re-claim via SDK (tests R8 claim_resumed_execution fallback)
    let task2 = worker.claim_next().await.unwrap();
    assert!(task2.is_some(), "should re-claim the resumed execution via claim_resumed path");
    let task2 = task2.unwrap();
    assert_eq!(*task2.execution_id(), eid, "should be the same execution");

    // 5. Complete
    task2.complete(Some(b"done after resume".to_vec())).await.unwrap();

    // Verify terminal
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
        public_state: Some(ff_core::state::PublicState::Completed),
        ..Default::default()
    }).await;
    assert_state_vector_complete(&tc, &eid).await;
}

// ═══════════════════════════════════════════════════════════════════════
// ROUND 9: Quota reconciler self-healing test
// ═══════════════════════════════════════════════════════════════════════

/// Test that the quota reconciler corrects a drifted concurrency counter.
///
/// 1. Admit 2 executions (concurrency cap 2)
/// 2. Verify counter = 2
/// 3. Manually corrupt counter to 5 (simulating missed DECRs)
/// 4. Run quota_reconciler scan
/// 5. Verify counter corrected to 2 (matches live admitted:* guard keys)
/// 6. Admit a 3rd → should be rejected (cap is 2, counter is 2)
#[tokio::test]
#[serial_test::serial]
async fn test_quota_reconciler_self_healing() {
    use ff_engine::scanner::Scanner;

    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let policy_id = "reconciler-test-quota";
    let tag = "{q:0}";
    let window_key = format!("ff:quota:{}:{}:window:requests", tag, policy_id);
    let concurrency_key = format!("ff:quota:{}:{}:concurrency", tag, policy_id);
    let def_key = format!("ff:quota:{}:{}", tag, policy_id);

    // Create quota via FCALL (registers in policies_index for reconciler discovery)
    fcall_create_quota_policy(&tc, policy_id, 60, 0, 2).await;

    // Admit E1 and E2
    let e1 = tc.new_execution_id();
    let e2 = tc.new_execution_id();
    let r1 = fcall_admit(&tc, policy_id, &e1, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r1, "ADMITTED");
    let r2 = fcall_admit(&tc, policy_id, &e2, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r2, "ADMITTED");

    // Verify counter is 2
    let counter: Option<String> = tc.client()
        .cmd("GET").arg(&concurrency_key)
        .execute().await.unwrap();
    assert_eq!(counter.as_deref(), Some("2"), "counter should be 2 after 2 admissions");

    // Corrupt counter to 5 (simulates missed DECRs from completed executions)
    let _: () = tc.client()
        .cmd("SET").arg(&concurrency_key).arg("5")
        .execute().await.unwrap();
    let corrupted: Option<String> = tc.client()
        .cmd("GET").arg(&concurrency_key)
        .execute().await.unwrap();
    assert_eq!(corrupted.as_deref(), Some("5"), "counter should be corrupted to 5");

    // Verify 3rd admission is rejected (counter=5 >= cap=2)
    let e3 = tc.new_execution_id();
    let r3 = fcall_admit(&tc, policy_id, &e3, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r3, "CONCURRENCY_EXCEEDED", "should be rejected with corrupted counter");

    // Run quota reconciler scan on partition 0
    let reconciler = ff_engine::scanner::quota_reconciler::QuotaReconciler::new(
        std::time::Duration::from_secs(30),
    );
    let result = reconciler.scan_partition(tc.client(), 0).await;
    assert!(result.errors == 0, "reconciler should not error");

    // Verify counter was corrected to 2 (only 2 live admitted:* guard keys)
    let fixed: Option<String> = tc.client()
        .cmd("GET").arg(&concurrency_key)
        .execute().await.unwrap();
    assert_eq!(
        fixed.as_deref(), Some("2"),
        "reconciler should correct counter from 5 to 2 (2 live guard keys)"
    );

    // Now 3rd admission should still be rejected (2/2 active)
    let r4 = fcall_admit(&tc, policy_id, &e3, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r4, "CONCURRENCY_EXCEEDED", "still at capacity after reconciliation");
}

/// SDK smoke test: exercises fail(), cancel(), update_progress(), append_frame()
/// through the actual SDK, not raw FCALL helpers.
#[tokio::test]
#[serial_test::serial]
async fn test_sdk_all_methods_smoke() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Write partition config so SDK reads the test config
    let config = test_config();
    let config_key = "ff:config:partitions";
    let _: () = tc.client().cmd("HSET")
        .arg(config_key)
        .arg("num_flow_partitions")
        .arg(config.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(config.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(config.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();

    let worker_config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new("sdk-smoke-worker"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new("sdk-smoke-inst"),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 10,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config).await.unwrap();

    // ── Test 1: update_progress + append_frame + complete (happy path) ──
    let eid1 = tc.new_execution_id();
    fcall_create_execution(&tc, &eid1, NS, LANE, "sdk_smoke_1", 0).await;
    let task1 = worker.claim_next().await.unwrap().expect("should claim eid1");

    // update_progress via SDK (was broken pre-R6 — UUID in progress_pct)
    task1.update_progress(75, "three quarters").await.unwrap();
    let partition1 = execution_partition(&eid1, &config);
    let ctx1 = ExecKeyContext::new(&partition1, &eid1);
    let pct = tc.hget(&ctx1.core(), "progress_pct").await;
    assert_eq!(pct.as_deref(), Some("75"), "progress_pct via SDK should be '75'");
    let msg = tc.hget(&ctx1.core(), "progress_message").await;
    assert_eq!(msg.as_deref(), Some("three quarters"));

    // append_frame via SDK
    let frame_result = task1.append_frame("delta", b"hello world", None).await.unwrap();
    assert!(!frame_result.stream_id.is_empty(), "stream_id should be non-empty");
    assert_eq!(frame_result.frame_count, 1);

    // complete via SDK
    task1.complete(Some(b"sdk_result".to_vec())).await.unwrap();
    assert_execution_state(&tc, &eid1, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
        ..Default::default()
    }).await;

    // ── Test 2: fail() via SDK → terminal ──
    let eid2 = tc.new_execution_id();
    fcall_create_execution(&tc, &eid2, NS, LANE, "sdk_smoke_2", 0).await;
    let task2 = worker.claim_next().await.unwrap().expect("should claim eid2");

    let fail_outcome = task2.fail("test_error", "transient").await.unwrap();
    assert_eq!(fail_outcome, ff_sdk::FailOutcome::TerminalFailed,
        "no retry policy → terminal failed");
    assert_execution_state(&tc, &eid2, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Failed),
        ..Default::default()
    }).await;

    // ── Test 3: cancel() via SDK ──
    let eid3 = tc.new_execution_id();
    fcall_create_execution(&tc, &eid3, NS, LANE, "sdk_smoke_3", 0).await;
    let task3 = worker.claim_next().await.unwrap().expect("should claim eid3");

    task3.cancel("user_requested").await.unwrap();
    assert_execution_state(&tc, &eid3, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Cancelled),
        ..Default::default()
    }).await;
}

/// Verify that claim_execution extracts attempt_index correctly (field position fix).
/// On retry, attempt_index should be 1 (not 0 from misread expires_at).
#[tokio::test]
#[serial_test::serial]
async fn test_sdk_claim_retry_attempt_index() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Write partition config
    let config = test_config();
    let _: () = tc.client().cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(config.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(config.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(config.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();

    let worker_config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new("retry-idx-worker"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new("retry-idx-inst"),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 10,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config).await.unwrap();

    // Create execution with retry policy
    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "retry_idx_test", 0).await;
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let _: () = tc.client()
        .cmd("SET")
        .arg(ctx.policy())
        .arg(r#"{"retry_policy":{"max_retries":3,"backoff":{"type":"fixed","delay_ms":10}}}"#)
        .execute()
        .await
        .unwrap();

    // Claim attempt 0 via SDK
    let task = worker.claim_next().await.unwrap().expect("should claim");
    assert_eq!(task.attempt_index(), AttemptIndex::new(0), "first attempt should be index 0");

    // Fail it (triggers retry)
    let outcome = task.fail("test_retry", "transient").await.unwrap();
    assert!(matches!(outcome, ff_sdk::FailOutcome::RetryScheduled { .. }),
        "should schedule retry, got: {outcome:?}");

    // Wait for backoff + promote
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    fcall_promote_delayed(&tc, &eid, LANE).await;

    // Claim attempt 1 via SDK — THIS is the critical test.
    // Pre-fix: attempt_index would be 0 (misread from expires_at).
    // Post-fix: attempt_index should be 1.
    let task2 = worker.claim_next().await.unwrap().expect("should claim retry");
    assert_eq!(task2.attempt_index(), AttemptIndex::new(1),
        "retry attempt should be index 1 (not 0 from misread expires_at)");

    // Verify attempt type is retry
    let att_type = tc.hget(
        &ctx.core(), "current_attempt_index"
    ).await;
    assert_eq!(att_type.as_deref(), Some("1"), "current_attempt_index should be 1");

    // Complete it
    task2.complete(Some(b"done".to_vec())).await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE B TESTS: ff_create_budget, ff_create_quota_policy
// ═══════════════════════════════════════════════════════════════════════

/// Create budget → report usage (OK) → report usage (HARD_BREACH) → verify breach_count.
#[tokio::test]
#[serial_test::serial]
async fn test_budget_create_and_enforce() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let budget_id = "create-enforce-budget";

    // Create budget: tokens dimension, hard=10, soft=8
    let create_result = fcall_create_budget(&tc, budget_id,
        &[("tokens", 10)],
        &[("tokens", 8)],
    ).await;
    assert_eq!(create_result[0], "OK", "create should return OK, got: {create_result:?}");
    assert!(create_result[1].contains(budget_id),
        "should return budget_id, got: {create_result:?}");

    // Verify budget def created
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let stored_id = tc.hget(&def_key, "budget_id").await;
    assert_eq!(stored_id.as_deref(), Some(budget_id));
    let enforcement = tc.hget(&def_key, "enforcement_mode").await;
    assert_eq!(enforcement.as_deref(), Some("strict"));

    // Verify limits stored
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let hard: Option<String> = tc.client()
        .cmd("HGET").arg(&limits_key).arg("hard:tokens")
        .execute().await.unwrap();
    assert_eq!(hard.as_deref(), Some("10"));
    let soft: Option<String> = tc.client()
        .cmd("HGET").arg(&limits_key).arg("soft:tokens")
        .execute().await.unwrap();
    assert_eq!(soft.as_deref(), Some("8"));

    // Report 5 tokens → OK
    let r1 = fcall_report_usage(&tc, budget_id, &[("tokens", 5)]).await;
    assert_eq!(r1[0], "OK", "5 tokens under limit 10. Got: {r1:?}");

    // Report 6 more (total 11 > limit 10) → HARD_BREACH
    let r2 = fcall_report_usage(&tc, budget_id, &[("tokens", 6)]).await;
    assert_eq!(r2[0], "HARD_BREACH", "11 > 10 should breach. Got: {r2:?}");

    // Verify breach_count=1
    let breach_count = tc.hget(&def_key, "breach_count").await;
    assert_eq!(breach_count.as_deref(), Some("1"), "breach_count should be 1");

    // Verify usage is still 5 (check-before-increment)
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let usage: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("tokens")
        .execute().await.unwrap();
    assert_eq!(usage.as_deref(), Some("5"), "hard breach should not increment usage");
}

/// Create budget twice → second returns ALREADY_SATISFIED.
#[tokio::test]
#[serial_test::serial]
async fn test_budget_create_idempotent() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let budget_id = "idempotent-budget";

    // First create → OK
    let r1 = fcall_create_budget(&tc, budget_id,
        &[("tokens", 100)],
        &[("tokens", 80)],
    ).await;
    assert_eq!(r1[0], "OK", "first create should be OK, got: {r1:?}");

    // Second create → ALREADY_SATISFIED
    let r2 = fcall_create_budget(&tc, budget_id,
        &[("tokens", 200)], // different limits — should be ignored
        &[("tokens", 150)],
    ).await;
    assert_eq!(r2[0], "ALREADY_SATISFIED",
        "second create should be ALREADY_SATISFIED, got: {r2:?}");

    // Verify original limits preserved (not overwritten)
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let hard: Option<String> = tc.client()
        .cmd("HGET").arg(&limits_key).arg("hard:tokens")
        .execute().await.unwrap();
    assert_eq!(hard.as_deref(), Some("100"), "limits should not be overwritten on idempotent create");
}

/// Create quota policy → admit 2 (cap=2) → 3rd rejected.
#[tokio::test]
#[serial_test::serial]
async fn test_quota_create_and_enforce() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let policy_id = "create-enforce-quota";

    // Create quota policy: window=60s, no rate limit, max_concurrent=2
    let create_result = fcall_create_quota_policy(&tc, policy_id, 60, 0, 2).await;
    assert_eq!(create_result[0], "OK", "create should return OK, got: {create_result:?}");

    // Verify quota def created
    let def_key = format!("ff:quota:{{q:0}}:{policy_id}");
    let stored_id = tc.hget(&def_key, "quota_policy_id").await;
    assert_eq!(stored_id.as_deref(), Some(policy_id));
    let cap = tc.hget(&def_key, "active_concurrency_cap").await;
    assert_eq!(cap.as_deref(), Some("2"));

    // Verify concurrency counter initialized to 0
    let concurrency_key = format!("ff:quota:{{q:0}}:{policy_id}:concurrency");
    let counter: Option<String> = tc.client()
        .cmd("GET").arg(&concurrency_key)
        .execute().await.unwrap();
    assert_eq!(counter.as_deref(), Some("0"));

    // Admit E1 → ADMITTED
    let window_key = format!("ff:quota:{{q:0}}:{policy_id}:window:requests");
    let e1 = tc.new_execution_id();
    let r1 = fcall_admit(&tc, policy_id, &e1, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r1, "ADMITTED");

    // Admit E2 → ADMITTED
    let e2 = tc.new_execution_id();
    let r2 = fcall_admit(&tc, policy_id, &e2, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r2, "ADMITTED");

    // Admit E3 → CONCURRENCY_EXCEEDED
    let e3 = tc.new_execution_id();
    let r3 = fcall_admit(&tc, policy_id, &e3, &window_key, &concurrency_key, &def_key).await;
    assert_eq!(r3, "CONCURRENCY_EXCEEDED", "3rd should be rejected (cap=2)");
}

/// Create quota policy twice → second returns ALREADY_SATISFIED.
#[tokio::test]
#[serial_test::serial]
async fn test_quota_create_idempotent() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let policy_id = "idempotent-quota";

    // First create → OK
    let r1 = fcall_create_quota_policy(&tc, policy_id, 60, 100, 5).await;
    assert_eq!(r1[0], "OK", "first create should be OK, got: {r1:?}");

    // Second create → ALREADY_SATISFIED
    let r2 = fcall_create_quota_policy(&tc, policy_id, 120, 200, 10).await;
    assert_eq!(r2[0], "ALREADY_SATISFIED",
        "second create should be ALREADY_SATISFIED, got: {r2:?}");

    // Verify original config preserved
    let def_key = format!("ff:quota:{{q:0}}:{policy_id}");
    let cap = tc.hget(&def_key, "active_concurrency_cap").await;
    assert_eq!(cap.as_deref(), Some("5"), "config should not be overwritten");
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE B TESTS: flow lifecycle functions
// ═══════════════════════════════════════════════════════════════════════

/// ff_create_flow + ff_add_execution_to_flow x3 → SMEMBERS=3, node_count=3, graph_revision=3.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_create_and_membership() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let fid = "flow-create-test";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let c = tc.new_execution_id_on_partition(0);

    // Create flow
    let result = fcall_create_flow(&tc, fid).await;
    assert_eq!(result, fid, "ff_create_flow should return flow_id");

    // Verify flow_core exists with open state
    let flow_core_key = format!("ff:flow:{{fp:0}}:{fid}:core");
    let pfs: Option<String> = tc.client().cmd("HGET").arg(&flow_core_key)
        .arg("public_flow_state").execute().await.unwrap();
    assert_eq!(pfs.as_deref(), Some("open"));

    // Pre-seed exec_core rows (RFC-011 phase-3 guard requires EXISTS).
    ensure_exec_core_exists(&tc, &a).await;
    ensure_exec_core_exists(&tc, &b).await;
    ensure_exec_core_exists(&tc, &c).await;

    // Add 3 members
    let (eid1, nc1) = fcall_add_execution_to_flow(&tc, fid, &a).await;
    assert_eq!(eid1, a.to_string());
    assert_eq!(nc1, "1");

    let (eid2, nc2) = fcall_add_execution_to_flow(&tc, fid, &b).await;
    assert_eq!(eid2, b.to_string());
    assert_eq!(nc2, "2");

    let (eid3, nc3) = fcall_add_execution_to_flow(&tc, fid, &c).await;
    assert_eq!(eid3, c.to_string());
    assert_eq!(nc3, "3");

    // Verify SMEMBERS = 3
    let members_key = format!("ff:flow:{{fp:0}}:{fid}:members");
    let member_count: u32 = tc.client().cmd("SCARD").arg(&members_key)
        .execute().await.unwrap();
    assert_eq!(member_count, 3, "should have 3 members");

    // Verify node_count = 3
    let nc: Option<String> = tc.client().cmd("HGET").arg(&flow_core_key)
        .arg("node_count").execute().await.unwrap();
    assert_eq!(nc.as_deref(), Some("3"));

    // Verify graph_revision = 3
    let gr: Option<String> = tc.client().cmd("HGET").arg(&flow_core_key)
        .arg("graph_revision").execute().await.unwrap();
    assert_eq!(gr.as_deref(), Some("3"));
}

/// ff_create_flow idempotency: second call returns already_satisfied.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_create_idempotent() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let fid = "flow-idem-test";

    // First create
    let result1 = fcall_create_flow(&tc, fid).await;
    assert_eq!(result1, fid);

    // Second create — parse raw to check "already_satisfied" status
    let prefix = format!("ff:flow:{{fp:0}}:{fid}");
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        "ff:idx:{fp:0}:flow_index".to_string(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        fid.to_owned(), "test".to_owned(), NS.to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_create_flow", &kr, &ar)
        .await
        .expect("FCALL ff_create_flow 2");
    let arr = expect_success_array(&raw, "ff_create_flow idempotent");
    // ok_already_satisfied(id) → {1, "ALREADY_SATISFIED", id}
    let status_str = field_str(arr, 1);
    assert_eq!(status_str, "ALREADY_SATISFIED",
        "second ff_create_flow should return ALREADY_SATISFIED");
    let fid_returned = field_str(arr, 2);
    assert_eq!(fid_returned, fid);
}

/// flow_index self-heal: if the projector SREMs a live flow (sampled
/// all-terminal), a subsequent ff_add_execution_to_flow must re-register
/// the flow in flow_index so future projector cycles see it again.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_index_self_heal_on_add() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let fid = "flow-heal-test";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let flow_index_key = "ff:idx:{fp:0}:flow_index";

    // 1. Create flow + add one member (pre-seed exec_core for the
    //    RFC-011 phase-3 EXISTS guard).
    fcall_create_flow(&tc, fid).await;
    ensure_exec_core_exists(&tc, &a).await;
    ensure_exec_core_exists(&tc, &b).await;
    fcall_add_execution_to_flow(&tc, fid, &a).await;

    // Sanity: flow is registered in the index
    let present: bool = tc.client()
        .cmd("SISMEMBER").arg(flow_index_key).arg(fid)
        .execute().await.unwrap();
    assert!(present, "flow_index should contain fid after create");

    // 2. Simulate projector prune: SREM the flow_index entry
    let _: u32 = tc.client()
        .cmd("SREM").arg(flow_index_key).arg(fid)
        .execute().await.unwrap();
    let after_rem: bool = tc.client()
        .cmd("SISMEMBER").arg(flow_index_key).arg(fid)
        .execute().await.unwrap();
    assert!(!after_rem, "flow_index SREM simulation failed");

    // 3. Add a second member via ff_add_execution_to_flow
    fcall_add_execution_to_flow(&tc, fid, &b).await;

    // 4. Assert: flow_index now contains fid again (self-heal fired)
    let healed: bool = tc.client()
        .cmd("SISMEMBER").arg(flow_index_key).arg(fid)
        .execute().await.unwrap();
    assert!(healed, "flow_index should be self-healed by ff_add_execution_to_flow");

    // 5. Negative: self-heal must NOT resurrect a cancelled flow. Cancel,
    //    then a subsequent add attempt must fail with flow_already_terminal.
    //
    //    NOTE on flow_index ownership: cancel_flow does NOT SREM flow_index
    //    (the projector is the sole writer — it SREMs once it observes
    //    sampled==true_total all-terminal members). So we assert the
    //    negative invariant that matters for correctness: the *rejected*
    //    add MUST NOT ADD the flow back to the index if a prior SREM had
    //    dropped it. We pre-SREM the index here to simulate a projector
    //    prune that ran between cancel and the retry, then confirm the
    //    rejected add leaves it absent.
    fcall_cancel_flow(&tc, fid, "test_heal_cancel", "cancel_all").await;

    // Simulate the projector having already observed all-terminal and
    // SREMd the flow (in real life this would take a projector cycle).
    let _: u32 = tc.client()
        .cmd("SREM").arg(flow_index_key).arg(fid)
        .execute().await.unwrap();

    // Direct FCALL so we can inspect the error result without panicking.
    // Post-RFC-011 phase-3 the FCALL requires KEYS[4]=exec_core on the
    // same {fp:0} slot — pin the exec via on_partition(0). Test happens
    // to pass even with only 3 KEYS today because flow_already_terminal
    // returns before step 4 touches exec_core, but relying on Lua
    // validation order is brittle; pass all 4 KEYS as the contract
    // dictates.
    let prefix = format!("ff:flow:{{fp:0}}:{fid}");
    let c = tc.new_execution_id_on_partition(0);
    let exec_partition = ff_core::partition::execution_partition(&c, tc.partition_config());
    let ectx = ff_core::keys::ExecKeyContext::new(&exec_partition, &c);
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        flow_index_key.to_string(),
        ectx.core(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        fid.to_owned(), c.to_string(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_add_execution_to_flow", &kr, &ar)
        .await
        .expect("FCALL ff_add_execution_to_flow after cancel");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("expected array"),
    };
    let status = match &arr[0] {
        Ok(Value::Int(n)) => *n,
        _ => panic!("expected Int status"),
    };
    assert_eq!(status, 0, "add-to-terminal-flow must fail");
    assert_eq!(field_str(arr, 1), "flow_already_terminal");
    let still_absent: bool = tc.client()
        .cmd("SISMEMBER").arg(flow_index_key).arg(fid)
        .execute().await.unwrap();
    assert!(!still_absent, "terminal flow must NOT be resurrected in flow_index");
}

/// ff_cancel_flow: create flow + add members → cancel → verify state + member list returned.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_cancel() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let fid = "flow-cancel-test";
    let a = tc.new_execution_id_on_partition(0);
    let b = tc.new_execution_id_on_partition(0);
    let c = tc.new_execution_id_on_partition(0);

    // Create flow and add 3 members (pre-seed exec_core for the
    // RFC-011 phase-3 EXISTS guard).
    fcall_create_flow(&tc, fid).await;
    ensure_exec_core_exists(&tc, &a).await;
    ensure_exec_core_exists(&tc, &b).await;
    ensure_exec_core_exists(&tc, &c).await;
    fcall_add_execution_to_flow(&tc, fid, &a).await;
    fcall_add_execution_to_flow(&tc, fid, &b).await;
    fcall_add_execution_to_flow(&tc, fid, &c).await;

    // Cancel with cancel_all policy
    let (policy, members) = fcall_cancel_flow(&tc, fid, "test_cancel", "cancel_all").await;
    assert_eq!(policy, "cancel_all");
    assert_eq!(members.len(), 3, "should return 3 member eids");

    // Verify all 3 eids are in the returned list
    let mut member_set: std::collections::HashSet<String> = members.into_iter().collect();
    assert!(member_set.remove(&a.to_string()), "A in cancel list");
    assert!(member_set.remove(&b.to_string()), "B in cancel list");
    assert!(member_set.remove(&c.to_string()), "C in cancel list");

    // Verify flow is cancelled in Valkey
    let flow_core_key = format!("ff:flow:{{fp:0}}:{fid}:core");
    let pfs: Option<String> = tc.client().cmd("HGET").arg(&flow_core_key)
        .arg("public_flow_state").execute().await.unwrap();
    assert_eq!(pfs.as_deref(), Some("cancelled"));

    let reason: Option<String> = tc.client().cmd("HGET").arg(&flow_core_key)
        .arg("cancel_reason").execute().await.unwrap();
    assert_eq!(reason.as_deref(), Some("test_cancel"));

    // Second cancel should return error (already terminal)
    let prefix = format!("ff:flow:{{fp:0}}:{fid}");
    let keys: Vec<String> = vec![
        format!("{prefix}:core"),
        format!("{prefix}:members"),
        "ff:idx:{fp:0}:flow_index".to_string(),
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        fid.to_owned(), "again".to_owned(), "cancel_all".to_owned(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_cancel_flow", &kr, &ar)
        .await
        .expect("FCALL ff_cancel_flow 2");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("expected array"),
    };
    let status = match &arr[0] {
        Ok(Value::Int(n)) => *n,
        _ => panic!("expected Int status"),
    };
    assert_eq!(status, 0, "second cancel should fail");
    assert_eq!(field_str(arr, 1), "flow_already_terminal");
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE C TESTS: Server API — budget/quota
// ═══════════════════════════════════════════════════════════════════════

/// Build a Server instance suitable for testing.
/// Uses the test partition config (4/2/2/2) and connects to the local Valkey.
async fn test_server() -> ff_server::server::Server {
    use ff_server::config::ServerConfig;
    let config = test_config();
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
        skip_library_load: true, // TestCluster::connect() already loaded it
        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
    };
    ff_server::server::Server::start(server_config)
        .await
        .expect("Server::start failed")
}

/// Server::create_budget → Server::get_budget_status → verify all fields.
#[tokio::test]
#[serial_test::serial]
async fn test_server_create_budget() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let server = test_server().await;

    let budget_id = BudgetId::new();
    let now = TimestampMs::now();
    let args = ff_core::contracts::CreateBudgetArgs {
        budget_id: budget_id.clone(),
        scope_type: "lane".into(),
        scope_id: "test-lane".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 3_600_000, // 1 hour
        dimensions: vec!["tokens".into(), "cost".into()],
        hard_limits: vec![1000, 50],
        soft_limits: vec![800, 40],
        now,
    };

    // Create budget via Server API
    let result = server.create_budget(&args).await.unwrap();
    assert!(matches!(result, ff_core::contracts::CreateBudgetResult::Created { .. }),
        "expected Created, got: {result:?}");

    // Read back via Server::get_budget_status
    let status = server.get_budget_status(&budget_id).await.unwrap();
    assert_eq!(status.budget_id, budget_id.to_string());
    assert_eq!(status.scope_type, "lane");
    assert_eq!(status.scope_id, "test-lane");
    assert_eq!(status.enforcement_mode, "strict");
    assert_eq!(status.hard_limits.get("tokens"), Some(&1000));
    assert_eq!(status.hard_limits.get("cost"), Some(&50));
    assert_eq!(status.soft_limits.get("tokens"), Some(&800));
    assert_eq!(status.soft_limits.get("cost"), Some(&40));
    assert_eq!(status.breach_count, 0);
    assert_eq!(status.soft_breach_count, 0);
    assert!(status.next_reset_at.is_some(), "reset_interval_ms > 0 should set next_reset_at");
    assert!(status.created_at.is_some(), "created_at should be set");

    server.shutdown().await;
}

/// Server::create_quota_policy → ff_check_admission → verify works end-to-end.
#[tokio::test]
#[serial_test::serial]
async fn test_server_create_quota() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let server = test_server().await;

    let qid = QuotaPolicyId::new();
    let now = TimestampMs::now();
    let args = ff_core::contracts::CreateQuotaPolicyArgs {
        quota_policy_id: qid.clone(),
        window_seconds: 60,
        max_requests_per_window: 100,
        max_concurrent: 5,
        now,
    };

    // Create quota via Server API
    let result = server.create_quota_policy(&args).await.unwrap();
    assert!(matches!(result, ff_core::contracts::CreateQuotaPolicyResult::Created { .. }),
        "expected Created, got: {result:?}");

    // Verify the policy was stored correctly by reading from Valkey
    let config = test_config();
    let partition = ff_core::partition::quota_partition(&qid, &config);
    let qctx = ff_core::keys::QuotaKeyContext::new(&partition, &qid);

    let stored_cap = tc.hget(&qctx.definition(), "active_concurrency_cap").await;
    assert_eq!(stored_cap.as_deref(), Some("5"), "concurrency cap should be 5");
    let stored_window = tc.hget(&qctx.definition(), "requests_per_window_seconds").await;
    assert_eq!(stored_window.as_deref(), Some("60"), "window should be 60s");

    // Verify admission works against the policy we created
    let eid = tc.new_execution_id();
    let guard_key = qctx.admitted(&eid);
    let admit_keys: Vec<String> = vec![
        qctx.window("requests_per_window"),
        qctx.concurrency(),
        qctx.definition(),
        guard_key,
        qctx.admitted_set(),
    ];
    let admit_args: Vec<String> = vec![
        TimestampMs::now().to_string(), "60".to_owned(), "100".to_owned(),
        "5".to_owned(), eid.to_string(), "0".to_owned(),
    ];
    let kr: Vec<&str> = admit_keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = admit_args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_check_admission_and_record", &kr, &ar)
        .await
        .expect("FCALL admission check");
    let admission = parse_admission_result(&raw);
    assert_eq!(admission, "ADMITTED", "admission against server-created policy should work");

    server.shutdown().await;
}

/// Server::create_budget twice → second returns AlreadySatisfied.
#[tokio::test]
#[serial_test::serial]
async fn test_server_budget_idempotent() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let server = test_server().await;

    let budget_id = BudgetId::new();
    let now = TimestampMs::now();
    let args = ff_core::contracts::CreateBudgetArgs {
        budget_id: budget_id.clone(),
        scope_type: "lane".into(),
        scope_id: "idem-lane".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: vec!["tokens".into()],
        hard_limits: vec![100],
        soft_limits: vec![80],
        now,
    };

    // First create → Created
    let r1 = server.create_budget(&args).await.unwrap();
    assert!(matches!(r1, ff_core::contracts::CreateBudgetResult::Created { .. }),
        "first should be Created, got: {r1:?}");

    // Second create → AlreadySatisfied
    let r2 = server.create_budget(&args).await.unwrap();
    assert!(matches!(r2, ff_core::contracts::CreateBudgetResult::AlreadySatisfied { .. }),
        "second should be AlreadySatisfied, got: {r2:?}");

    // Verify original data preserved
    let status = server.get_budget_status(&budget_id).await.unwrap();
    assert_eq!(status.hard_limits.get("tokens"), Some(&100));

    server.shutdown().await;
}

/// Server flow lifecycle: create_flow → add 2 members → stage edge →
/// complete upstream → verify downstream eligible → cancel_flow remainder.
#[tokio::test]
#[serial_test::serial]
async fn test_server_flow_lifecycle() {
    use ff_core::contracts::{
        AddExecutionToFlowArgs, CancelFlowArgs, CreateFlowArgs,
    };

    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let server = test_server().await;
    let now = TimestampMs::now();

    // 1. Create flow via Server API
    let flow_id = FlowId::new();
    let create_flow_result = server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "test_lifecycle".to_owned(),
            namespace: Namespace::new(NS),
            now,
        })
        .await
        .expect("create_flow failed");
    assert!(
        matches!(create_flow_result, ff_core::contracts::CreateFlowResult::Created { .. }),
        "expected Created, got {create_flow_result:?}"
    );

    // 2. Create 2 executions co-located with the flow (RFC-011 §7.3).
    let config = test_config();
    let upstream = ExecutionId::for_flow(&flow_id, &config);
    let downstream = ExecutionId::for_flow(&flow_id, &config);
    fcall_create_execution(&tc, &upstream, NS, LANE, "upstream_task", 0).await;
    fcall_create_execution(&tc, &downstream, NS, LANE, "downstream_task", 0).await;

    // 3. Add both to flow via Server API
    let add_up = server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: upstream.clone(),
            now,
        })
        .await
        .expect("add upstream failed");
    assert!(
        matches!(add_up, ff_core::contracts::AddExecutionToFlowResult::Added { .. }),
        "expected Added, got {add_up:?}"
    );

    let add_down = server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: downstream.clone(),
            now,
        })
        .await
        .expect("add downstream failed");
    assert!(
        matches!(add_down, ff_core::contracts::AddExecutionToFlowResult::Added { .. }),
        "expected Added, got {add_down:?}"
    );

    // Verify flow_id was set on exec_core (Phase 2 of add_execution_to_flow)
    let config = test_config();
    let up_partition = execution_partition(&upstream, &config);
    let up_ctx = ExecKeyContext::new(&up_partition, &upstream);
    let stored_fid: Option<String> = tc.client().cmd("HGET").arg(up_ctx.core())
        .arg("flow_id").execute().await.unwrap();
    assert_eq!(stored_fid.as_deref(), Some(flow_id.to_string().as_str()),
        "flow_id should be set on upstream exec_core");

    // 4. Stage dependency edge (upstream → downstream) — partition-aware
    let edge_id = uuid::Uuid::new_v4().to_string();
    let fpart = ff_core::partition::flow_partition(&flow_id, &config);
    let fctx = ff_core::keys::FlowKeyContext::new(&fpart, &flow_id);
    let flow_id_str = flow_id.to_string();
    {
        let edge_eid = ff_core::types::EdgeId::parse(&edge_id).unwrap();
        let keys: Vec<String> = vec![
            fctx.core(), fctx.members(),
            fctx.edge(&edge_eid),
            fctx.outgoing(&upstream),
            fctx.incoming(&downstream),
            fctx.grant(&edge_id),
        ];
        let now_stage = TimestampMs::now();
        // graph_revision = 2 after adding 2 members
        let args: Vec<String> = vec![
            flow_id_str.clone(), edge_id.clone(),
            upstream.to_string(), downstream.to_string(),
            "success_only".to_owned(), String::new(),
            "2".to_owned(), now_stage.to_string(),
        ];
        let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let raw: Value = tc.client()
            .fcall("ff_stage_dependency_edge", &kr, &ar)
            .await
            .expect("FCALL ff_stage_dependency_edge");
        expect_success_array(&raw, "ff_stage_dependency_edge");
    }

    // 5. Apply dependency to downstream
    fcall_apply_dependency(&tc, &downstream, &flow_id_str, &edge_id, &upstream).await;

    // Verify downstream is blocked
    assert_execution_state(&tc, &downstream, &ExpectedState {
        eligibility_state: Some(ff_core::state::EligibilityState::BlockedByDependencies),
        public_state: Some(ff_core::state::PublicState::WaitingChildren),
        ..Default::default()
    }).await;

    // 6. Complete upstream execution
    fcall_issue_claim_grant(&tc, &upstream, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &upstream, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &upstream, LANE, WORKER_INST, &la, &ea, &aa).await;

    // 7. Resolve dependency → downstream becomes eligible
    let resolve_result = fcall_resolve_dependency(&tc, &downstream, &upstream, &edge_id, "success").await;
    assert_eq!(resolve_result, "satisfied");

    assert_in_eligible(&tc, &downstream, &LaneId::new(LANE)).await;

    // 8. Cancel flow via Server API — should cancel downstream.
    // Use cancel_flow_wait so the assertion on downstream state directly
    // below observes the synchronous dispatch outcome.
    let cancel_result = server
        .cancel_flow_wait(&CancelFlowArgs {
            flow_id: flow_id.clone(),
            reason: "test_done".to_owned(),
            cancellation_policy: "cancel_all".to_owned(),
            now: TimestampMs::now(),
        })
        .await
        .expect("cancel_flow failed");

    let (cancellation_policy, member_execution_ids) = match &cancel_result {
        ff_core::contracts::CancelFlowResult::Cancelled {
            cancellation_policy,
            member_execution_ids,
        } => (cancellation_policy, member_execution_ids),
        other => panic!("expected Cancelled from cancel_flow_wait, got {other:?}"),
    };
    assert_eq!(cancellation_policy, "cancel_all");
    assert_eq!(member_execution_ids.len(), 2);

    // Verify downstream is now cancelled (upstream was already terminal:success,
    // cancel_execution on it would fail — that's expected and logged as warning)
    let down_state = server.get_execution_state(&downstream).await.unwrap();
    assert_eq!(down_state, ff_core::state::PublicState::Cancelled,
        "downstream should be cancelled after cancel_flow");

    // Verify flow core state is cancelled (reuse fctx from step 4)
    let flow_state: Option<String> = tc.client().cmd("HGET").arg(fctx.core())
        .arg("public_flow_state").execute().await.unwrap();
    assert_eq!(flow_state.as_deref(), Some("cancelled"));

    server.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════
// PHASE C TESTS: Server API — execution operations (Worker-2)
// ═══════════════════════════════════════════════════════════════════════

/// Server::deliver_signal: create → claim → suspend → deliver_signal → verify resumed.
#[tokio::test]
#[serial_test::serial]
async fn test_server_deliver_signal() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;

    // 1. Create and claim
    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "signal_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, lease_epoch, _, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // 2. Suspend with resume condition
    let cond_json = build_resume_condition_json(&["test_signal"], "fail");
    let (_susp_id, wp_id_str, _wp_key, sub, wp_token) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST, &lease_id, &lease_epoch,
        "0", &attempt_id, "waiting_signal", &cond_json, None, "fail",
    ).await;
    assert_ne!(sub, "ALREADY_SATISFIED", "should suspend normally");

    // Verify suspended
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Suspended),
        public_state: Some(ff_core::state::PublicState::Suspended),
        ..Default::default()
    }).await;

    // 3. Deliver signal via Server API
    let wp_id = WaitpointId::parse(&wp_id_str).unwrap();
    let sig_id = SignalId::new();
    let signal_args = ff_core::contracts::DeliverSignalArgs {
        execution_id: eid.clone(),
        waitpoint_id: wp_id,
        signal_id: sig_id.clone(),
        signal_name: "test_signal".into(),
        signal_category: "test".into(),
        source_type: "external_api".into(),
        source_identity: "e2e-test".into(),
        payload: None,
        payload_encoding: None,
        idempotency_key: Some("idem-1".into()),
        correlation_id: None,
        target_scope: "waitpoint".into(),
        created_at: None,
        dedup_ttl_ms: Some(86_400_000),
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: ff_core::types::WaitpointToken::new(wp_token),
        now: TimestampMs::now(),
    };
    let result = server.deliver_signal(&signal_args).await.unwrap();
    match &result {
        ff_core::contracts::DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied",
                "effect should be 'resume_condition_satisfied', not the signal_id");
        }
        other => panic!("expected Accepted, got: {other:?}"),
    }

    // 4. Verify execution is back to runnable (resumed by signal)
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        public_state: Some(ff_core::state::PublicState::Waiting),
        ..Default::default()
    }).await;

    server.shutdown().await;
}

/// Server::change_priority: create → change priority → verify new priority.
#[tokio::test]
#[serial_test::serial]
async fn test_server_change_priority() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;

    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "prio_test", 100).await;

    // Verify initial priority
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let prio: Option<String> = tc.client().cmd("HGET")
        .arg(ctx.core()).arg("priority").execute().await.unwrap();
    assert_eq!(prio.as_deref(), Some("100"));

    // Change priority via Server API
    let result = server.change_priority(&eid, 500).await.unwrap();
    assert!(matches!(result, ff_core::contracts::ChangePriorityResult::Changed { .. }),
        "expected Changed, got: {result:?}");

    // Verify new priority in exec_core
    let new_prio: Option<String> = tc.client().cmd("HGET")
        .arg(ctx.core()).arg("priority").execute().await.unwrap();
    assert_eq!(new_prio.as_deref(), Some("500"));

    server.shutdown().await;
}

/// Server::get_execution: create → get full info → verify all fields.
#[tokio::test]
#[serial_test::serial]
async fn test_server_get_execution() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;

    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "get_test", 42).await;

    let info = server.get_execution(&eid).await.unwrap();

    assert_eq!(info.execution_id, eid);
    assert_eq!(info.namespace, NS);
    assert_eq!(info.lane_id, LANE);
    assert_eq!(info.priority, 42);
    assert_eq!(info.execution_kind, "get_test");
    assert_eq!(info.state_vector.lifecycle_phase, ff_core::state::LifecyclePhase::Runnable);
    assert_eq!(info.state_vector.ownership_state, ff_core::state::OwnershipState::Unowned);
    assert_eq!(info.state_vector.eligibility_state, ff_core::state::EligibilityState::EligibleNow);
    assert_eq!(info.state_vector.blocking_reason, ff_core::state::BlockingReason::WaitingForWorker);
    assert_eq!(info.state_vector.terminal_outcome, ff_core::state::TerminalOutcome::None);
    assert_eq!(info.state_vector.attempt_state, ff_core::state::AttemptState::PendingFirstAttempt);
    assert_eq!(info.public_state, ff_core::state::PublicState::Waiting);
    assert_eq!(info.current_attempt_index, 0);
    assert!(info.flow_id.is_none(), "no flow_id for standalone execution");
    assert!(!info.created_at.is_empty(), "created_at should be set");

    // Verify not-found
    let missing = tc.new_execution_id();
    let err = server.get_execution(&missing).await;
    assert!(err.is_err(), "non-existent execution should error");

    server.shutdown().await;
}

/// Server::replay_execution: create → claim → complete → replay → verify back to runnable.
#[tokio::test]
#[serial_test::serial]
async fn test_server_replay() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;

    // Create, claim, complete
    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "replay_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lid, lep, _, aid) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lid, &lep, &aid).await;

    // Verify terminal
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
        ..Default::default()
    }).await;

    // Replay via Server API
    let result = server.replay_execution(&eid).await.unwrap();
    assert!(matches!(result, ff_core::contracts::ReplayExecutionResult::Replayed {
        public_state: ff_core::state::PublicState::Waiting,
    }), "expected Replayed(Waiting), got: {result:?}");

    // Verify back to runnable
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Runnable),
        public_state: Some(ff_core::state::PublicState::Waiting),
        ..Default::default()
    }).await;

    server.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════
// ADVERSARIAL ROUND 2: Error paths and state transitions
// ═══════════════════════════════════════════════════════════════════════

/// Angle 1: Every server method with a non-existent execution_id returns clean error.
#[tokio::test]
#[serial_test::serial]
async fn test_server_methods_wrong_execution_id() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;

    let bogus = tc.new_execution_id(); // never created

    // get_execution → execution not found
    let err = server.get_execution(&bogus).await;
    assert!(err.is_err(), "get_execution on bogus should fail");
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("not found"), "should contain 'not found', got: {msg}");

    // change_priority → script error (Lua returns execution_not_found/not_eligible)
    let err = server.change_priority(&bogus, 100).await;
    assert!(err.is_err(), "change_priority on bogus should fail");

    // replay_execution → script error
    let err = server.replay_execution(&bogus).await;
    assert!(err.is_err(), "replay_execution on bogus should fail");

    // deliver_signal → script error (bogus token, bogus execution).
    // The bogus token is fine here — this test asserts the execution-not-found
    // path fails, so any token shape is acceptable.
    let signal_args = ff_core::contracts::DeliverSignalArgs {
        execution_id: bogus.clone(),
        waitpoint_id: WaitpointId::new(),
        signal_id: SignalId::new(),
        signal_name: "test".into(),
        signal_category: "test".into(),
        source_type: "test".into(),
        source_identity: "test".into(),
        payload: None,
        payload_encoding: None,
        idempotency_key: None,
        correlation_id: None,
        target_scope: "waitpoint".into(),
        created_at: None,
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: ff_core::types::WaitpointToken::new("k1:0000000000000000000000000000000000000000"),
        now: TimestampMs::now(),
    };
    let err = server.deliver_signal(&signal_args).await;
    assert!(err.is_err(), "deliver_signal on bogus should fail");

    // cancel_execution → script error
    let cancel_args = ff_core::contracts::CancelExecutionArgs {
        execution_id: bogus,
        reason: "test".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::now(),
    };
    let err = server.cancel_execution(&cancel_args).await;
    assert!(err.is_err(), "cancel_execution on bogus should fail");

    server.shutdown().await;
}

/// Angle 3: get_execution at every lifecycle phase — all 7 dims must parse.
#[tokio::test]
#[serial_test::serial]
async fn test_server_get_execution_all_phases() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;

    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "phase_test", 50).await;

    // Phase 1: RUNNABLE
    let info = server.get_execution(&eid).await.unwrap();
    assert_eq!(info.state_vector.lifecycle_phase, ff_core::state::LifecyclePhase::Runnable);
    assert_eq!(info.state_vector.ownership_state, ff_core::state::OwnershipState::Unowned);
    assert_eq!(info.state_vector.eligibility_state, ff_core::state::EligibilityState::EligibleNow);
    assert_eq!(info.state_vector.blocking_reason, ff_core::state::BlockingReason::WaitingForWorker);
    assert_eq!(info.state_vector.terminal_outcome, ff_core::state::TerminalOutcome::None);
    assert_eq!(info.state_vector.attempt_state, ff_core::state::AttemptState::PendingFirstAttempt);
    assert_eq!(info.public_state, ff_core::state::PublicState::Waiting);

    // Phase 2: ACTIVE (claim)
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lid, lep, _, aid) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    let info = server.get_execution(&eid).await.unwrap();
    assert_eq!(info.state_vector.lifecycle_phase, ff_core::state::LifecyclePhase::Active);
    assert_eq!(info.state_vector.ownership_state, ff_core::state::OwnershipState::Leased);
    assert_eq!(info.state_vector.eligibility_state, ff_core::state::EligibilityState::NotApplicable);
    assert_eq!(info.state_vector.attempt_state, ff_core::state::AttemptState::RunningAttempt);
    assert_eq!(info.public_state, ff_core::state::PublicState::Active);

    // Phase 3: SUSPENDED
    let cond_json = build_resume_condition_json(&["resume_me"], "fail");
    let (_susp_id, wp_id_str, _wp_key, _sub, wp_token) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST, &lid, &lep, "0", &aid,
        "waiting_signal", &cond_json, None, "fail",
    ).await;
    let info = server.get_execution(&eid).await.unwrap();
    assert_eq!(info.state_vector.lifecycle_phase, ff_core::state::LifecyclePhase::Suspended);
    assert_eq!(info.state_vector.ownership_state, ff_core::state::OwnershipState::Unowned);
    assert_eq!(info.state_vector.eligibility_state, ff_core::state::EligibilityState::NotApplicable);
    assert_eq!(info.state_vector.blocking_reason, ff_core::state::BlockingReason::WaitingForSignal);
    assert_eq!(info.state_vector.attempt_state, ff_core::state::AttemptState::AttemptInterrupted);
    assert_eq!(info.public_state, ff_core::state::PublicState::Suspended);

    // Phase 4: Signal → back to RUNNABLE
    fcall_deliver_signal(&tc, &eid, LANE, &wp_id_str, "resume_me", "test", "", &wp_token).await;
    let info = server.get_execution(&eid).await.unwrap();
    assert_eq!(info.state_vector.lifecycle_phase, ff_core::state::LifecyclePhase::Runnable);
    assert_eq!(info.state_vector.attempt_state, ff_core::state::AttemptState::AttemptInterrupted);
    assert_eq!(info.public_state, ff_core::state::PublicState::Waiting);

    // Phase 5: Claim resumed → ACTIVE again (must use claim_resumed, not claim)
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lid2, lep2, _, aid2) =
        fcall_claim_resumed_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    let info = server.get_execution(&eid).await.unwrap();
    assert_eq!(info.state_vector.lifecycle_phase, ff_core::state::LifecyclePhase::Active);
    assert_eq!(info.state_vector.attempt_state, ff_core::state::AttemptState::RunningAttempt);

    // Phase 6: TERMINAL (complete)
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lid2, &lep2, &aid2).await;
    let info = server.get_execution(&eid).await.unwrap();
    assert_eq!(info.state_vector.lifecycle_phase, ff_core::state::LifecyclePhase::Terminal);
    assert_eq!(info.state_vector.ownership_state, ff_core::state::OwnershipState::Unowned);
    assert_eq!(info.state_vector.eligibility_state, ff_core::state::EligibilityState::NotApplicable);
    assert_eq!(info.state_vector.blocking_reason, ff_core::state::BlockingReason::None);
    assert_eq!(info.state_vector.terminal_outcome, ff_core::state::TerminalOutcome::Success);
    assert_eq!(info.state_vector.attempt_state, ff_core::state::AttemptState::AttemptTerminal);
    assert_eq!(info.public_state, ff_core::state::PublicState::Completed);
    assert_eq!(info.priority, 50);

    server.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════
// REVIEW ROUND 2: Data integrity cycle tests
// ═══════════════════════════════════════════════════════════════════════

/// Angle 1: Budget create→use→reset→use cycle via ff_create_budget.
#[tokio::test]
#[serial_test::serial]
async fn test_budget_create_use_reset_cycle() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let bid = "cycle-budget";
    fcall_create_budget_full(&tc, bid, &[("tokens", 10)], &[("tokens", 8)], 3_600_000).await;

    let r1 = fcall_report_usage(&tc, bid, &[("tokens", 5)]).await;
    assert_eq!(r1[0], "OK", "5/10 OK. Got: {r1:?}");

    let r2 = fcall_report_usage(&tc, bid, &[("tokens", 3)]).await;
    // Lua uses strict > not >=, so 8 == soft(8) → no breach
    assert_eq!(r2[0], "OK", "8 == soft(8), strict > means no breach. Got: {r2:?}");

    let usage_key = format!("ff:budget:{{b:0}}:{bid}:usage");
    let usage: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("tokens").execute().await.unwrap();
    assert_eq!(usage.as_deref(), Some("8"));

    fcall_reset_budget(&tc, bid).await;

    let usage_after: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("tokens").execute().await.unwrap();
    assert_eq!(usage_after.as_deref(), Some("0"), "usage=0 after reset");

    let r3 = fcall_report_usage(&tc, bid, &[("tokens", 5)]).await;
    assert_eq!(r3[0], "OK", "5/10 after reset OK. Got: {r3:?}");

    let r4 = fcall_report_usage(&tc, bid, &[("tokens", 6)]).await;
    assert_eq!(r4[0], "HARD_BREACH", "11>10 breach. Got: {r4:?}");

    let usage_final: Option<String> = tc.client()
        .cmd("HGET").arg(&usage_key).arg("tokens").execute().await.unwrap();
    assert_eq!(usage_final.as_deref(), Some("5"), "breach did not increment");
}

/// Angle 2: Quota create→admit→exhaust→corrupt→reconcile→recheck cycle.
#[tokio::test]
#[serial_test::serial]
async fn test_quota_create_admit_reconcile_cycle() {
    use ff_engine::scanner::Scanner;

    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let pid = "cycle-quota";
    fcall_create_quota_policy(&tc, pid, 60, 0, 2).await;

    let conc_key = format!("ff:quota:{{q:0}}:{pid}:concurrency");
    let counter: Option<String> = tc.client()
        .cmd("GET").arg(&conc_key).execute().await.unwrap();
    assert_eq!(counter.as_deref(), Some("0"), "counter=0 from create");

    let window_key = format!("ff:quota:{{q:0}}:{pid}:window:requests");
    let def_key = format!("ff:quota:{{q:0}}:{pid}");

    let e1 = tc.new_execution_id();
    let e2 = tc.new_execution_id();
    assert_eq!(fcall_admit(&tc, pid, &e1, &window_key, &conc_key, &def_key).await, "ADMITTED");
    assert_eq!(fcall_admit(&tc, pid, &e2, &window_key, &conc_key, &def_key).await, "ADMITTED");

    let e3 = tc.new_execution_id();
    assert_eq!(fcall_admit(&tc, pid, &e3, &window_key, &conc_key, &def_key).await, "CONCURRENCY_EXCEEDED");

    let _: () = tc.client().cmd("SET").arg(&conc_key).arg("5").execute().await.unwrap();

    let reconciler = ff_engine::scanner::quota_reconciler::QuotaReconciler::new(
        std::time::Duration::from_secs(30),
    );
    let result = reconciler.scan_partition(tc.client(), 0).await;
    assert_eq!(result.errors, 0);

    let fixed: Option<String> = tc.client()
        .cmd("GET").arg(&conc_key).execute().await.unwrap();
    assert_eq!(fixed.as_deref(), Some("2"), "reconciler corrected to 2");

    assert_eq!(fcall_admit(&tc, pid, &e3, &window_key, &conc_key, &def_key).await, "CONCURRENCY_EXCEEDED");
}

/// Angle 4: Server budget status tracks create/report/reset accurately.
#[tokio::test]
#[serial_test::serial]
async fn test_server_budget_status_tracks_operations() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let server = test_server().await;
    let budget_id = BudgetId::new();

    server.create_budget(&ff_core::contracts::CreateBudgetArgs {
        budget_id: budget_id.clone(),
        scope_type: "lane".into(),
        scope_id: "track-lane".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 3_600_000,
        dimensions: vec!["tokens".into()],
        hard_limits: vec![100],
        soft_limits: vec![80],
        now: TimestampMs::now(),
    }).await.unwrap();

    // Fresh: 0 usage
    let s1 = server.get_budget_status(&budget_id).await.unwrap();
    assert!(s1.usage.is_empty(), "fresh budget empty usage, got: {:?}", s1.usage);
    assert_eq!(s1.breach_count, 0);

    // Report 5 via raw FCALL (using correct partition)
    let partition = ff_core::partition::budget_partition(&budget_id, &test_config());
    let bctx = ff_core::keys::BudgetKeyContext::new(&partition, &budget_id);
    let rk_v: Vec<String> = vec![bctx.usage(), bctx.limits(), bctx.definition()];
    let rk: Vec<&str> = rk_v.iter().map(|s| s.as_str()).collect();
    let ra1_v: Vec<String> = vec!["1".into(), "tokens".into(), "5".into(), TimestampMs::now().to_string()];
    let ra1: Vec<&str> = ra1_v.iter().map(|s| s.as_str()).collect();
    let _: Value = tc.client().fcall("ff_report_usage_and_check", &rk, &ra1).await.unwrap();

    let s2 = server.get_budget_status(&budget_id).await.unwrap();
    assert_eq!(s2.usage.get("tokens"), Some(&5));

    // Report 3 more
    let ra2_v: Vec<String> = vec!["1".into(), "tokens".into(), "3".into(), TimestampMs::now().to_string()];
    let ra2: Vec<&str> = ra2_v.iter().map(|s| s.as_str()).collect();
    let _: Value = tc.client().fcall("ff_report_usage_and_check", &rk, &ra2).await.unwrap();

    let s3 = server.get_budget_status(&budget_id).await.unwrap();
    assert_eq!(s3.usage.get("tokens"), Some(&8));

    // Reset
    let resetk_v: Vec<String> = vec![bctx.definition(), bctx.usage(), ff_core::keys::budget_resets_key(bctx.hash_tag())];
    let resetk: Vec<&str> = resetk_v.iter().map(|s| s.as_str()).collect();
    let reseta_v: Vec<String> = vec![budget_id.to_string(), TimestampMs::now().to_string()];
    let reseta: Vec<&str> = reseta_v.iter().map(|s| s.as_str()).collect();
    let _: Value = tc.client().fcall("ff_reset_budget", &resetk, &reseta).await.unwrap();

    let s4 = server.get_budget_status(&budget_id).await.unwrap();
    assert_eq!(s4.usage.get("tokens"), Some(&0), "0 after reset");

    server.shutdown().await;
}

/// Full flow lifecycle: Server create + add members + stage edges + complete chain.
#[tokio::test]
#[serial_test::serial]
async fn test_server_full_flow_lifecycle() {
    use ff_core::contracts::{AddExecutionToFlowArgs, CreateExecutionArgs, CreateFlowArgs};
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;
    let config = test_config();
    let now = TimestampMs::now();

    // Mint flow FIRST so execs can co-locate with it (RFC-011 §7.3
    // consumer contract enforced by add_execution_to_flow pre-flight).
    let flow_id = FlowId::new();
    let a = ExecutionId::for_flow(&flow_id, &config);
    let b = ExecutionId::for_flow(&flow_id, &config);
    let c = ExecutionId::for_flow(&flow_id, &config);
    for (eid, kind) in [(&a, "step_a"), (&b, "step_b"), (&c, "step_c")] {
        server.create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(), namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE), execution_kind: kind.to_owned(),
            input_payload: b"{}".to_vec(), payload_encoding: Some("json".to_owned()),
            priority: 0, creator_identity: "test".to_owned(),
            idempotency_key: None, tags: std::collections::HashMap::new(),
            policy: None, delay_until: None, execution_deadline_at: None,
            partition_id: ff_core::partition::execution_partition(eid, &config).index, now,
        }).await.expect("create_execution");
    }

    server.create_flow(&CreateFlowArgs {
        flow_id: flow_id.clone(), flow_kind: "chain".to_owned(),
        namespace: Namespace::new(NS), now,
    }).await.expect("create_flow");
    for eid in [&a, &b, &c] {
        server.add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(), execution_id: eid.clone(), now,
        }).await.expect("add member");
    }

    let fpart = ff_core::partition::flow_partition(&flow_id, &config);
    let fctx = ff_core::keys::FlowKeyContext::new(&fpart, &flow_id);
    let fid_s = flow_id.to_string();
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_bc = uuid::Uuid::new_v4().to_string();
    for (eid, up, down, rev) in [(&e_ab, &a, &b, "3"), (&e_bc, &b, &c, "4")] {
        let edge = ff_core::types::EdgeId::parse(eid).unwrap();
        let keys: Vec<String> = vec![
            fctx.core(), fctx.members(), fctx.edge(&edge),
            fctx.outgoing(up), fctx.incoming(down), fctx.grant(eid),
        ];
        let args: Vec<String> = vec![
            fid_s.clone(), eid.clone(), up.to_string(), down.to_string(),
            "success_only".to_owned(), String::new(), rev.to_owned(),
            TimestampMs::now().to_string(),
        ];
        let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
        let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let raw: Value = tc.client().fcall("ff_stage_dependency_edge", &kr, &ar)
            .await.expect("stage edge");
        expect_success_array(&raw, "stage edge");
    }
    fcall_apply_dependency(&tc, &b, &fid_s, &e_ab, &a).await;
    fcall_apply_dependency(&tc, &c, &fid_s, &e_bc, &b).await;
    assert_in_eligible(&tc, &a, &LaneId::new(LANE)).await;

    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    assert_eq!(fcall_resolve_dependency(&tc, &b, &a, &e_ab, "success").await, "satisfied");
    fcall_issue_claim_grant(&tc, &b, LANE, WORKER, WORKER_INST).await;
    let (lb, eb, _, ab) = fcall_claim_execution(&tc, &b, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &b, LANE, WORKER_INST, &lb, &eb, &ab).await;
    assert_eq!(fcall_resolve_dependency(&tc, &c, &b, &e_bc, "success").await, "satisfied");
    fcall_issue_claim_grant(&tc, &c, LANE, WORKER, WORKER_INST).await;
    let (lc, ec, _, ac) = fcall_claim_execution(&tc, &c, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &c, LANE, WORKER_INST, &lc, &ec, &ac).await;

    for eid in [&a, &b, &c] {
        assert_eq!(server.get_execution_state(eid).await.unwrap(),
            ff_core::state::PublicState::Completed, "{} should be completed", eid);
    }
    server.shutdown().await;
}

/// cancel empty flow then add member → should fail (flow_already_terminal).
#[tokio::test]
#[serial_test::serial]
async fn test_cancel_empty_flow_then_add_member() {
    use ff_core::contracts::{AddExecutionToFlowArgs, CancelFlowArgs, CreateFlowArgs};
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;
    let now = TimestampMs::now();

    let flow_id = FlowId::new();
    server.create_flow(&CreateFlowArgs {
        flow_id: flow_id.clone(), flow_kind: "empty".to_owned(),
        namespace: Namespace::new(NS), now,
    }).await.expect("create_flow");
    server.cancel_flow(&CancelFlowArgs {
        flow_id: flow_id.clone(), reason: "pre_cancel".to_owned(),
        cancellation_policy: "cancel_all".to_owned(), now,
    }).await.expect("cancel empty flow");

    let config = test_config();
    let eid = ExecutionId::for_flow(&flow_id, &config);
    fcall_create_execution(&tc, &eid, NS, LANE, "late_add", 0).await;
    let add_result = server.add_execution_to_flow(&AddExecutionToFlowArgs {
        flow_id: flow_id.clone(), execution_id: eid.clone(), now,
    }).await;
    assert!(add_result.is_err(), "add to cancelled flow should fail");
    let err_msg = format!("{}", add_result.unwrap_err());
    assert!(err_msg.contains("flow_already_terminal"), "got: {err_msg}");
    server.shutdown().await;
}

/// create_execution → create_flow → add_execution_to_flow → cancel_flow →
/// verify execution is cancelled via get_execution_state.
#[tokio::test]
#[serial_test::serial]
async fn test_server_create_cancel_roundtrip() {
    use ff_core::contracts::{
        AddExecutionToFlowArgs, CancelFlowArgs, CreateExecutionArgs, CreateFlowArgs,
    };
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let server = test_server().await;
    let config = test_config();
    let now = TimestampMs::now();

    // Mint the flow FIRST so the exec can co-locate with it —
    // RFC-011 §7.3 co-location contract, enforced by the pre-flight
    // partition check in `add_execution_to_flow`.
    let flow_id = FlowId::new();
    let eid = ExecutionId::for_flow(&flow_id, &config);
    server.create_execution(&CreateExecutionArgs {
        execution_id: eid.clone(), namespace: Namespace::new(NS),
        lane_id: LaneId::new(LANE), execution_kind: "roundtrip_task".to_owned(),
        input_payload: b"{}".to_vec(), payload_encoding: Some("json".to_owned()),
        priority: 0, creator_identity: "test".to_owned(),
        idempotency_key: None, tags: std::collections::HashMap::new(),
        policy: None, delay_until: None, execution_deadline_at: None,
        partition_id: ff_core::partition::execution_partition(&eid, &config).index, now,
    }).await.expect("create_execution");

    // Verify waiting
    assert_eq!(server.get_execution_state(&eid).await.unwrap(),
        ff_core::state::PublicState::Waiting);

    // Create flow + add member via Server
    server.create_flow(&CreateFlowArgs {
        flow_id: flow_id.clone(), flow_kind: "roundtrip".to_owned(),
        namespace: Namespace::new(NS), now,
    }).await.expect("create_flow");
    server.add_execution_to_flow(&AddExecutionToFlowArgs {
        flow_id: flow_id.clone(), execution_id: eid.clone(), now,
    }).await.expect("add member");

    // Cancel flow via Server (cancel_all dispatches cancel_execution).
    // Use the wait variant so the execution-state assertion below is
    // deterministic without a polling loop.
    let result = server.cancel_flow_wait(&CancelFlowArgs {
        flow_id: flow_id.clone(), reason: "roundtrip_test".to_owned(),
        cancellation_policy: "cancel_all".to_owned(), now: TimestampMs::now(),
    }).await.expect("cancel_flow");
    let member_execution_ids = match &result {
        ff_core::contracts::CancelFlowResult::Cancelled { member_execution_ids, .. } => {
            member_execution_ids
        }
        other => panic!("expected Cancelled from cancel_flow_wait, got {other:?}"),
    };
    assert_eq!(member_execution_ids.len(), 1);

    // Verify execution is cancelled via Server
    assert_eq!(server.get_execution_state(&eid).await.unwrap(),
        ff_core::state::PublicState::Cancelled,
        "execution should be cancelled after cancel_flow");

    // Verify flow is cancelled
    let fpart = ff_core::partition::flow_partition(&flow_id, &config);
    let fctx = ff_core::keys::FlowKeyContext::new(&fpart, &flow_id);
    let pfs: Option<String> = tc.client().cmd("HGET").arg(fctx.core())
        .arg("public_flow_state").execute().await.unwrap();
    assert_eq!(pfs.as_deref(), Some("cancelled"));

    server.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════
// FIX #5: System-level test with all 14 scanners running concurrently
// ═══════════════════════════════════════════════════════════════════════

/// System test: Engine with 14 scanners + create → delayed → promote → claim → complete.
///
/// Unlike all other tests which call individual Lua functions, this test starts
/// the full Engine (all 14 background scanners) and lets the delayed promoter
/// scanner naturally promote a delayed execution to eligible. This exercises
/// concurrent scanner timing, partition iteration, and the create→claim→complete
/// lifecycle under real background processing.
#[tokio::test]
#[serial_test::serial]
async fn test_system_engine_with_concurrent_scanners() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Start the full server (includes Engine with all 14 scanners)
    // Use short intervals so scanners fire quickly in tests
    let config = test_config();
    let server_config = {
        use ff_server::config::ServerConfig;
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
        ServerConfig {
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
                // Short intervals for test responsiveness
                delayed_promoter_interval: std::time::Duration::from_millis(100),
                lease_expiry_interval: std::time::Duration::from_millis(500),
                ..Default::default()
            },
            skip_library_load: true,
            cors_origins: vec!["*".to_owned()],
            api_token: None,
            waitpoint_hmac_secret:
                "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            waitpoint_hmac_grace_ms: 86_400_000,
            max_concurrent_stream_ops: 64,
        }
    };
    let server = ff_server::server::Server::start(server_config)
        .await
        .expect("Server::start with 14 scanners");

    // 1. Create a DELAYED execution (delay_until = 50ms from now)
    let eid = tc.new_execution_id();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let now = TimestampMs::now();
    let delay_until = TimestampMs::from_millis(now.0 + 50); // 50ms from now

    let delayed_key = idx.lane_delayed(&lane_id);
    let keys: Vec<String> = vec![
        ctx.core(), ctx.payload(), ctx.policy(), ctx.tags(),
        delayed_key.clone(), // scheduling_zset = delayed (not eligible)
        ctx.noop(), // idem_key placeholder
        idx.execution_deadline(), idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(), NS.to_owned(), LANE.to_owned(), "system_test".to_owned(),
        "0".to_owned(), "system-creator".to_owned(), "{}".to_owned(),
        "system-input".to_owned(), delay_until.0.to_string(), // delay_until
        String::new(), "{}".to_owned(), String::new(), "0".to_owned(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc.client()
        .fcall("ff_create_execution", &kr, &ar)
        .await.expect("create delayed execution");

    // 2. Verify it's in the delayed set, NOT eligible
    assert_not_in_eligible(&tc, &eid, &lane_id).await;

    // 3. Wait for the delayed promoter scanner to promote it.
    //    Scanner interval = 100ms, delay = 50ms. Should promote within ~200ms.
    let mut promoted = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let score = tc.zscore(&idx.lane_eligible(&lane_id), &eid.to_string()).await;
        if score.is_some() {
            promoted = true;
            break;
        }
    }
    assert!(promoted, "delayed promoter scanner should have promoted execution to eligible");

    // 4. Verify execution is now eligible (promoted by the background scanner)
    assert_in_eligible(&tc, &eid, &lane_id).await;

    // 5. Claim + complete through the normal FCALL path
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, _att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lease_id, &epoch, &attempt_id).await;

    // 6. Verify terminal state
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Terminal),
        terminal_outcome: Some(ff_core::state::TerminalOutcome::Success),
        public_state: Some(ff_core::state::PublicState::Completed),
        ..Default::default()
    }).await;

    server.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════
// Batch B: Capability routing (RFC-009)
// ═══════════════════════════════════════════════════════════════════════
//
// Worker caps flow through ARGV[9] of ff_issue_claim_grant. Executions
// declare required_capabilities in ExecutionPolicy.routing_requirements;
// ff_create_execution extracts a sorted CSV onto exec_core. On mismatch
// the Lua returns ("capability_mismatch", missing_csv) WITHOUT mutating
// the eligible ZSET — the execution stays queued for a matching worker.

async fn create_execution_with_caps(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    required: &[&str],
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

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

    let policy_json = if required.is_empty() {
        "{}".to_owned()
    } else {
        let caps_json = serde_json::to_string(required).unwrap();
        format!(
            r#"{{"routing_requirements":{{"required_capabilities":{caps_json}}}}}"#
        )
    };

    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        lane.to_owned(),
        "cap_test".to_owned(),
        "0".to_owned(),
        "cap-test".to_owned(),
        policy_json,
        r#"{"t":1}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution failed");
    parse_ok_fields(&raw, "ff_create_execution");
}

async fn fcall_claim_grant_with_caps(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    worker_caps: &[&str],
) -> Value {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
    ];

    let caps_csv: String = worker_caps
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");

    let args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        lane.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        caps_csv,
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tc.client()
        .fcall("ff_issue_claim_grant", &kr, &ar)
        .await
        .expect("FCALL ff_issue_claim_grant transport failure")
}

async fn assert_still_eligible(tc: &TestCluster, eid: &ExecutionId, lane: &str) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let idx = IndexKeys::new(&partition);
    let eligible_key = idx.lane_eligible(&LaneId::new(lane));
    let score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(&eligible_key)
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .expect("ZSCORE transport");
    assert!(
        score.is_some(),
        "execution {eid} should still be in eligible ZSET after capability_mismatch"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn test_capability_match_superset() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    create_execution_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    let raw = fcall_claim_grant_with_caps(&tc, &eid, LANE, &["gpu", "cuda"]).await;
    assert_ok(&raw, "capability_match_superset");
}

#[tokio::test]
#[serial_test::serial]
async fn test_capability_exact_match() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    create_execution_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    let raw = fcall_claim_grant_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    assert_ok(&raw, "capability_exact_match");
}

#[tokio::test]
#[serial_test::serial]
async fn test_capability_empty_requirement() {
    // Backwards compat: no required_capabilities → any worker matches,
    // including a worker with NO caps declared.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    create_execution_with_caps(&tc, &eid, LANE, &[]).await;
    let raw = fcall_claim_grant_with_caps(&tc, &eid, LANE, &[]).await;
    assert_ok(&raw, "capability_empty_requirement");
}

#[tokio::test]
#[serial_test::serial]
async fn test_capability_missing_one() {
    // Worker satisfies some but not all required caps → capability_mismatch.
    // Execution must stay in eligible ZSET.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    create_execution_with_caps(&tc, &eid, LANE, &["gpu", "torch>=2.3"]).await;
    let raw = fcall_claim_grant_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    assert_err(&raw, "capability_mismatch", "missing one required cap");
    assert_still_eligible(&tc, &eid, LANE).await;
}

#[tokio::test]
#[serial_test::serial]
async fn test_capability_no_overlap() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let eid = tc.new_execution_id();
    create_execution_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    let raw = fcall_claim_grant_with_caps(&tc, &eid, LANE, &["cpu"]).await;
    assert_err(&raw, "capability_mismatch", "no overlap");
    assert_still_eligible(&tc, &eid, LANE).await;
}

/// Execution-side ingress: a required capability containing a comma must
/// be rejected by `ff_create_execution`. Without this rejection the
/// Lua-side CSV subset check would silently split the token at the comma
/// and accept a worker that advertises only the prefix — a silent auth
/// bypass. See lua/execution.lua's routing_requirements extraction.
#[tokio::test]
#[serial_test::serial]
async fn test_capability_required_comma_rejected() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();
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
    // A token with an embedded comma. This MUST be rejected, otherwise
    // the wire CSV would parse as two tokens on the Lua side.
    let policy_json = r#"{"routing_requirements":{"required_capabilities":["gpu,cuda"]}}"#;
    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        LANE.to_owned(),
        "cap_comma_test".to_owned(),
        "0".to_owned(),
        "cap-test".to_owned(),
        policy_json.to_owned(),
        r#"{"t":1}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("transport");
    assert_err(&raw, "invalid_capabilities", "comma in required token");
}

/// Execution-side ingress: required_capabilities over the 4096-byte CSV
/// limit must be rejected by `ff_create_execution` rather than landing an
/// unclaimable execution with a multi-kilobyte exec_core field. Limit is
/// inclusive: exactly CAPS_MAX_BYTES accepts; CAPS_MAX_BYTES+1 rejects.
#[tokio::test]
#[serial_test::serial]
async fn test_capability_required_csv_too_large() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Generate enough tokens that the resulting sorted CSV crosses
    // CAPS_MAX_BYTES = 4096. Tokens "c000..c999" at 5 bytes + 1 comma each
    // → worst case ~6KB; use 800 such tokens to safely exceed the limit
    // while staying under CAPS_MAX_TOKENS (256) — wait, 800 > 256, so the
    // too_many_tokens guard would fire first. To hit the bytes guard, use
    // <=256 tokens with LONG strings. 32 * 200-byte tokens ≈ 6.4 KB.
    let mut caps: Vec<String> = Vec::new();
    for i in 0..32 {
        caps.push(format!("{}{}", "c".repeat(200), i));
    }
    let caps_refs: Vec<&str> = caps.iter().map(String::as_str).collect();

    let eid = tc.new_execution_id();
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let policy_json = format!(
        r#"{{"routing_requirements":{{"required_capabilities":{}}}}}"#,
        serde_json::to_string(&caps_refs).unwrap()
    );

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
        "cap_big_test".to_owned(),
        "0".to_owned(),
        "cap-test".to_owned(),
        policy_json,
        r#"{"t":1}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("transport");
    assert_err(&raw, "invalid_capabilities", "CSV too large");
}

/// ff_issue_reclaim_grant applies the same capability check as the initial
/// grant path: a reclaiming worker whose caps don't satisfy the execution's
/// required set is rejected with capability_mismatch, execution stays in
/// the lease_expiry ZSET for a different reclaimer to pick up.
#[tokio::test]
#[serial_test::serial]
async fn test_capability_reclaim_grant_mismatch() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    // Create with required cap {gpu} and get the execution into
    // lease_expired_reclaimable state.
    create_execution_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    // Initial worker matches so grant succeeds.
    let grant = fcall_claim_grant_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    assert_ok(&grant, "initial grant should succeed for gpu worker");
    // Claim with short lease TTL so we can time-expire it.
    let (_lease_id, _epoch, _att_idx, _attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 100).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    fcall_mark_lease_expired(&tc, &eid).await;

    // Attempt reclaim with a worker that advertises {cpu} only. Must be
    // rejected; execution remains lease_expired_reclaimable.
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);

    let keys: Vec<String> = vec![ctx.core(), ctx.claim_grant(), idx.lease_expiry()];
    let args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        "reclaim-cpu-worker".to_owned(),
        LANE.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        "cpu".to_owned(), // worker_capabilities_csv — missing gpu
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_issue_reclaim_grant", &kr, &ar)
        .await
        .expect("transport");
    assert_err(
        &raw,
        "capability_mismatch",
        "reclaim with non-matching caps",
    );

    // Execution should remain reclaimable for a different worker.
    assert_execution_state(
        &tc,
        &eid,
        &ExpectedState {
            lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
            ownership_state: Some(ff_core::state::OwnershipState::LeaseExpiredReclaimable),
            ..Default::default()
        },
    )
    .await;
}

// ═══════════════════════════════════════════════════════════════════════
// RFC-006 #2: STREAM READ + TAIL
// ═══════════════════════════════════════════════════════════════════════

/// Read all frames on an attempt that wrote two frames. Verifies round-trip
/// of every field written by `ff_append_frame`.
#[tokio::test]
#[serial_test::serial]
async fn test_read_stream_round_trips_append_frame_fields() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    fcall_create_execution(&tc, &eid, NS, LANE, "stream_read_rt", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "log", "hello world", 0,
    ).await;
    fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "output", "final result", 0,
    ).await;

    let result = ff_sdk::read_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(att_idx.parse().unwrap()),
        "-",
        "+",
        ff_sdk::STREAM_READ_HARD_CAP,
    )
    .await
    .expect("read_stream");
    let frames = &result.frames;

    assert_eq!(frames.len(), 2, "expected 2 frames, got {}", frames.len());
    let f0 = &frames[0];
    assert_eq!(f0.fields.get("frame_type").map(String::as_str), Some("log"));
    assert_eq!(f0.fields.get("payload").map(String::as_str), Some("hello world"));
    assert_eq!(f0.fields.get("encoding").map(String::as_str), Some("utf8"));
    assert_eq!(f0.fields.get("source").map(String::as_str), Some("worker"));
    assert!(f0.fields.contains_key("ts"), "ts field missing");

    let f1 = &frames[1];
    assert_eq!(f1.fields.get("frame_type").map(String::as_str), Some("output"));
    assert_eq!(f1.fields.get("payload").map(String::as_str), Some("final result"));

    assert_ne!(f0.id, f1.id);
}

/// Empty / never-written stream → read returns [].
#[tokio::test]
#[serial_test::serial]
async fn test_read_stream_empty_returns_empty() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    let result = ff_sdk::read_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(0),
        "-",
        "+",
        100,
    )
    .await
    .expect("read_stream on empty");
    assert!(result.frames.is_empty(), "expected empty vec, got {}", result.frames.len());
    assert!(!result.is_closed(), "never-written stream must present as open");
}

/// Write many frames, read a page with limit, then resume from the next id.
#[tokio::test]
#[serial_test::serial]
async fn test_read_stream_slice_and_resume() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    fcall_create_execution(&tc, &eid, NS, LANE, "stream_slice", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    const TOTAL: usize = 50;
    for i in 0..TOTAL {
        fcall_append_frame(
            &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
            "log", &format!("msg-{i}"), 0,
        ).await;
    }

    let att = AttemptIndex::new(att_idx.parse().unwrap());

    let page1 = ff_sdk::read_stream(tc.client(), &config, &eid, att, "-", "+", 20)
        .await
        .expect("read page1");
    assert_eq!(page1.frames.len(), 20);

    let last_id = page1.frames.last().unwrap().id.clone();
    // XRANGE start is inclusive. Bump the sequence to skip the cursor entry.
    let (ms, seq) = last_id.split_once('-').expect("entry id has ms-seq form");
    let next_seq: u64 = seq.parse::<u64>().unwrap() + 1;
    let resume_from = format!("{ms}-{next_seq}");

    let page2 = ff_sdk::read_stream(tc.client(), &config, &eid, att, &resume_from, "+", 100)
        .await
        .expect("read page2");
    assert_eq!(page2.frames.len(), TOTAL - 20);
    assert_ne!(page2.frames[0].id, last_id, "resume must skip cursor entry");
}

/// Tail on an empty stream with short block_ms returns [] near the timeout.
#[tokio::test]
#[serial_test::serial]
async fn test_tail_stream_timeout_returns_empty() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    let started = std::time::Instant::now();
    let result = ff_sdk::tail_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(0),
        "0-0",
        200,
        50,
    )
    .await
    .expect("tail_stream");
    let elapsed = started.elapsed();

    assert!(result.frames.is_empty(), "expected timeout with empty vec");
    assert!(!result.is_closed(), "never-written stream must not report closed");
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "expected to block at least ~200ms, blocked {elapsed:?}"
    );
}

/// Tail unblocks when a writer appends a frame while we're blocking.
#[tokio::test]
#[serial_test::serial]
async fn test_tail_stream_unblocks_on_write() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    fcall_create_execution(&tc, &eid, NS, LANE, "stream_tail", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    let eid_w = eid.clone();
    let att_idx_w = att_idx.clone();
    let lease_w = lease_id.clone();
    let epoch_w = epoch.clone();
    let att_id_w = attempt_id.clone();
    let writer = tokio::spawn(async move {
        let w = TestCluster::connect().await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        fcall_append_frame(
            &w, &eid_w, &att_idx_w, &lease_w, &epoch_w, &att_id_w,
            "log", "tail-unblock-payload", 0,
        ).await;
    });

    let result = ff_sdk::tail_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(att_idx.parse().unwrap()),
        "0-0",
        2_000,
        50,
    )
    .await
    .expect("tail_stream");

    writer.await.unwrap();

    assert_eq!(result.frames.len(), 1, "expected 1 frame, got {}", result.frames.len());
    assert_eq!(
        result.frames[0].fields.get("payload").map(String::as_str),
        Some("tail-unblock-payload")
    );
}

/// Regression: ferriskey auto-extends request_timeout for XREAD BLOCK by
/// `BLOCK_arg + 500ms`, so tail calls with block_ms > the client's default
/// request_timeout (5s) do NOT spuriously time out. This test proves that
/// by blocking for 7s (above 5s client request_timeout).
#[tokio::test]
#[serial_test::serial]
async fn test_tail_stream_long_block_respects_ferriskey_timeout_extension() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    let started = std::time::Instant::now();
    let result = ff_sdk::tail_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(0),
        "0-0",
        7_000, // deliberately > default 5s request_timeout
        50,
    )
    .await
    .expect("tail_stream must not surface transport timeout when ferriskey extends BLOCK timeout");
    let elapsed = started.elapsed();
    let frames = &result.frames;

    assert!(frames.is_empty(), "no writer → expected empty vec");
    assert!(
        elapsed >= std::time::Duration::from_millis(6_500),
        "expected to block ~7s, blocked {elapsed:?}"
    );
    assert!(
        elapsed <= std::time::Duration::from_millis(9_000),
        "expected to unblock near block_ms, blocked {elapsed:?}"
    );
}

/// RFC-006 #2 terminal signal: after the attempt completes, both read_stream
/// and a subsequent non-blocking tail_stream surface `closed_at` +
/// `closed_reason` so consumers can exit their polling loop without a
/// timeout fallback.
#[tokio::test]
#[serial_test::serial]
async fn test_stream_closed_signal_propagates_to_read_and_tail() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    fcall_create_execution(&tc, &eid, NS, LANE, "stream_closed", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Write a frame, complete the attempt — completion closes the stream
    // via lua/execution.lua with `closed_reason=attempt_success`.
    fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "log", "final-frame", 0,
    ).await;
    fcall_complete_execution(&tc, &eid, LANE, WORKER_INST, &lease_id, &epoch, &attempt_id).await;

    let att = AttemptIndex::new(att_idx.parse().unwrap());

    // read_stream sees the frame AND the terminal markers.
    let read = ff_sdk::read_stream(tc.client(), &config, &eid, att, "-", "+", 100)
        .await
        .expect("read_stream after complete");
    assert_eq!(read.frames.len(), 1);
    assert!(read.is_closed(), "read_stream must report closed after complete");
    assert_eq!(read.closed_reason.as_deref(), Some("attempt_success"));
    assert!(read.closed_at.is_some());

    // tail_stream with block_ms=0 at the tip returns no new frames but
    // still reports closed — this is the signal consumers poll on.
    let tip = read.frames.last().unwrap().id.clone();
    let tail = ff_sdk::tail_stream(tc.client(), &config, &eid, att, &tip, 0, 10)
        .await
        .expect("tail_stream at tip");
    assert!(tail.frames.is_empty());
    assert!(tail.is_closed(), "tail_stream must report closed after complete");
    assert_eq!(tail.closed_reason.as_deref(), Some("attempt_success"));
}

/// R4 regression: lease expiry closes stream_meta so tail consumers observe
/// the terminal signal even if no reclaim ever runs (worker OOM / node dead).
/// closed_reason=lease_expired is set on the FIRST close; later reclaim can
/// overwrite it to "reclaimed" via its own HSET.
#[tokio::test]
#[serial_test::serial]
async fn test_lease_expired_closes_stream_meta() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    fcall_create_execution(&tc, &eid, NS, LANE, "stream_lease_expire", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    // Short TTL so the lease expires while we wait.
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 200).await;

    // Write a frame so stream_meta exists (the close path skips if meta
    // was never created — a never-written attempt has no stream to close).
    fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "log", "pre-expiry frame", 0,
    ).await;

    // Wait past TTL + a margin for Valkey's server-time resolution.
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    fcall_mark_lease_expired(&tc, &eid).await;

    let att = AttemptIndex::new(att_idx.parse().unwrap());
    let result = ff_sdk::read_stream(
        tc.client(), &config, &eid, att, "-", "+", 100,
    )
    .await
    .expect("read_stream after lease expiry");

    assert_eq!(result.frames.len(), 1);
    assert!(
        result.is_closed(),
        "lease expiry must close stream_meta so tail consumers exit"
    );
    assert_eq!(result.closed_reason.as_deref(), Some("lease_expired"));
    assert!(result.closed_at.is_some());
}

/// block_ms == 0 → non-blocking peek returns immediately with available entries.
#[tokio::test]
#[serial_test::serial]
async fn test_tail_stream_no_block_returns_available() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    fcall_create_execution(&tc, &eid, NS, LANE, "stream_peek", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    fcall_append_frame(
        &tc, &eid, &att_idx, &lease_id, &epoch, &attempt_id,
        "log", "peek-1", 0,
    ).await;

    let started = std::time::Instant::now();
    let result = ff_sdk::tail_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(att_idx.parse().unwrap()),
        "0-0",
        0,
        50,
    )
    .await
    .expect("tail_stream peek");
    let elapsed = started.elapsed();
    let frames = &result.frames;

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].fields.get("payload").map(String::as_str), Some("peek-1"));
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "non-blocking peek should return fast, took {elapsed:?}"
    );
}

/// R3 regression (BUG2 `count_limit == 0` rejected at every layer).
///
/// Reject at the SDK boundary with `SdkError::Config` — never silently
/// upgraded to `STREAM_READ_HARD_CAP`.
#[tokio::test]
#[serial_test::serial]
async fn test_read_stream_rejects_zero_count_limit() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    let err = ff_sdk::read_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(0),
        "-",
        "+",
        0,
    )
    .await
    .expect_err("count_limit=0 must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("count_limit must be >= 1"),
        "unexpected error: {msg}"
    );
}

/// R3 regression (BUG2 `count_limit == 0` rejected on tail path too).
#[tokio::test]
#[serial_test::serial]
async fn test_tail_stream_rejects_zero_count_limit() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();

    let err = ff_sdk::tail_stream(
        tc.client(),
        &config,
        &eid,
        AttemptIndex::new(0),
        "0-0",
        0,
        0,
    )
    .await
    .expect_err("count_limit=0 must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("count_limit must be >= 1"),
        "unexpected error: {msg}"
    );
}

/// PR#7 regression (Copilot): direct FCALL with `count_limit > HARD_CAP`
/// must return `err("invalid_input", "count_limit_exceeds_hard_cap")` —
/// NOT a silent clamp to HARD_CAP. The SDK and server layers already
/// reject at their boundary, but a caller bypassing those (direct
/// `FCALL ff_read_attempt_stream`) still hit the silent-clamp path in
/// lua/stream.lua before this fix. RFC-006 §Input validation promises
/// symmetric rejection at both edges.
#[tokio::test]
#[serial_test::serial]
async fn test_read_stream_rejects_over_hard_cap_at_lua() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);

    // HARD_CAP in lua/stream.lua is 10_000 (mirrors STREAM_READ_HARD_CAP).
    // 10_001 must be rejected loud, not silently clamped.
    let keys: Vec<String> = vec![
        ctx.stream(AttemptIndex::new(0)),
        ctx.stream_meta(AttemptIndex::new(0)),
    ];
    let args: Vec<String> = vec![
        "-".to_owned(),
        "+".to_owned(),
        "10001".to_owned(),
    ];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_read_attempt_stream", &key_refs, &arg_refs)
        .await
        .expect("FCALL transport failed");

    // Expect failure tuple: {0, "invalid_input", "count_limit_exceeds_hard_cap"}
    let arr = match &raw {
        Value::Array(a) => a,
        other => panic!("expected Array, got {other:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        other => panic!("expected Int status, got {other:?}"),
    };
    assert_eq!(status, 0, "expected failure status, got {status}: {arr:?}");
    let code = match arr.get(1) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        other => panic!("expected error code string, got {other:?}"),
    };
    assert_eq!(code, "invalid_input", "unexpected error code: {code}");
    let detail = match arr.get(2) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        other => panic!("expected detail string, got {other:?}"),
    };
    assert_eq!(
        detail, "count_limit_exceeds_hard_cap",
        "unexpected detail: {detail}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// R2 regression tests (BUG1 + BUG2): fail-closed on malformed policy/caps
// ═══════════════════════════════════════════════════════════════════════

/// Submit an arbitrary policy_json raw and return the FCALL response.
/// Deliberately bypasses the well-formed cap helpers above so tests can
/// exercise the fail-closed parse path in ff_create_execution.
async fn fcall_create_execution_with_raw_policy(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &str,
    policy_json: &str,
) -> Value {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

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
        lane.to_owned(),
        "cap_regression".to_owned(),
        "0".to_owned(),
        "cap-test".to_owned(),
        policy_json.to_owned(),
        r#"{"t":1}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    tc.client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("transport")
}

/// R2-BUG1 regression: a malformed policy_json (trailing comma here) used
/// to slip through pcall(cjson.decode) as ok_decode=false, silently skip
/// the routing_requirements extraction, and store an execution with NO
/// required_capabilities — wildcard-claimable by anyone. Now it returns
/// invalid_policy_json and writes nothing.
#[tokio::test]
#[serial_test::serial]
async fn test_capability_malformed_policy_json_fails_closed() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let raw = fcall_create_execution_with_raw_policy(
        &tc,
        &eid,
        LANE,
        r#"{"routing_requirements":{"required_capabilities":["gpu",]}}"#,
    )
    .await;
    assert_err(&raw, "invalid_policy_json", "malformed policy JSON");

    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let exists: i64 = tc
        .client()
        .cmd("EXISTS")
        .arg(ctx.core())
        .execute()
        .await
        .expect("EXISTS transport");
    assert_eq!(
        exists, 0,
        "exec_core must not exist after invalid_policy_json"
    );
}

/// R2-BUG2 regression: a typed-config typo like ["gpu", null, 42] used to
/// silent-drop the non-strings and store ["gpu"]. That silently erased a
/// security-sensitive requirement. Now every non-string element aborts
/// the create call with invalid_capabilities.
#[tokio::test]
#[serial_test::serial]
async fn test_capability_non_string_token_fails_closed() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let cases = [
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu",null]}}"#,
            "null token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu",42]}}"#,
            "numeric token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu",{"x":1}]}}"#,
            "object token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu",""]}}"#,
            "empty token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":"gpu"}}"#,
            "required not array",
        ),
    ];
    for (policy_json, label) in cases {
        let eid = tc.new_execution_id();
        let raw = fcall_create_execution_with_raw_policy(&tc, &eid, LANE, policy_json).await;
        assert_err(&raw, "invalid_capabilities", label);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// R4: capability-mismatch block-to-route + periodic unblock
// ═══════════════════════════════════════════════════════════════════════

/// R4-BUG regression: after `ff_issue_claim_grant` returns capability_mismatch,
/// the scheduler MUST move the execution from the lane's eligible ZSET
/// into its blocked:route ZSET (eligibility_state=blocked_by_route,
/// blocking_reason=waiting_for_capable_worker). Otherwise every tick
/// re-picks the same top-of-zset and hot-loops.
#[tokio::test]
#[serial_test::serial]
async fn test_capability_mismatch_blocks_to_route_zset() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    create_execution_with_caps(&tc, &eid, LANE, &["gpu"]).await;

    let config = test_config();
    let lane_id = LaneId::new(LANE);
    let worker_id = ff_core::types::WorkerId::new(WORKER);
    let wiid = ff_core::types::WorkerInstanceId::new(WORKER_INST);
    let scheduler = ff_scheduler::claim::Scheduler::new(tc.client().clone(), config);

    // Worker has only {cpu}; execution requires {gpu} → mismatch.
    let cpu_only: std::collections::BTreeSet<String> =
        ["cpu".to_owned()].into_iter().collect();
    let grant = scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &cpu_only, 5000)
        .await
        .expect("scheduler claim should not error");
    assert!(grant.is_none(), "no matching worker → no grant returned");

    // Invariant (a)+(b): execution was moved out of eligible into blocked:route
    let partition = execution_partition(&eid, &config);
    let idx = IndexKeys::new(&partition);
    let eligible_score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(idx.lane_eligible(&lane_id).as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .expect("ZSCORE eligible");
    assert!(
        eligible_score.is_none(),
        "execution must NOT be in eligible zset after mismatch"
    );

    let blocked_score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(idx.lane_blocked_route(&lane_id).as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .expect("ZSCORE blocked:route");
    assert!(
        blocked_score.is_some(),
        "execution must be in blocked:route zset after mismatch"
    );

    // Invariant (c): exec_core fields reflect the block.
    let ctx = ExecKeyContext::new(&partition, &eid);
    let (eligibility_state, blocking_reason): (Option<String>, Option<String>) = {
        let v: Vec<Option<String>> = tc
            .client()
            .cmd("HMGET")
            .arg(ctx.core().as_str())
            .arg("eligibility_state")
            .arg("blocking_reason")
            .execute()
            .await
            .expect("HMGET");
        (v.first().cloned().flatten(), v.get(1).cloned().flatten())
    };
    assert_eq!(
        eligibility_state.as_deref(),
        Some("blocked_by_route"),
        "eligibility_state must be blocked_by_route"
    );
    assert_eq!(
        blocking_reason.as_deref(),
        Some("waiting_for_capable_worker"),
        "blocking_reason must be waiting_for_capable_worker"
    );
}

/// R4-BUG regression: the engine's unblock scanner MUST promote
/// `waiting_for_capable_worker`-blocked executions back to eligible once
/// the union of connected workers' caps covers the required set. Without
/// this, capability-blocked executions stay stuck forever — exactly the
/// bug that W3 and W1 both flagged independently in R4.
///
/// Uses the unblock scanner directly (not via a running Engine) to keep
/// the test deterministic.
#[tokio::test]
#[serial_test::serial]
async fn test_capable_worker_unblocks_route_blocked() {
    use ff_engine::scanner::Scanner;

    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let config = test_config();
    let lane_id = LaneId::new(LANE);
    let eid = tc.new_execution_id();

    // 1. Create execution with required caps {gpu}; block it via a
    //    mismatched scheduler claim.
    create_execution_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    let worker_id = ff_core::types::WorkerId::new(WORKER);
    let wiid = ff_core::types::WorkerInstanceId::new(WORKER_INST);
    let scheduler = ff_scheduler::claim::Scheduler::new(tc.client().clone(), config);
    let cpu_only: std::collections::BTreeSet<String> =
        ["cpu".to_owned()].into_iter().collect();
    scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &cpu_only, 5000)
        .await
        .expect("scheduler claim should not error");

    // 2. Simulate a capable worker registering by writing the SAME two
    //    cluster-safe entries the SDK writes on connect: the per-worker
    //    caps STRING at ff:worker:<id>:caps, AND a SADD into the global
    //    workers-index SET `ff:idx:workers`. The scanner enumerates
    //    through the index in cluster mode (a keyspace SCAN would miss
    //    workers whose caps key hashes to other shards).
    let instance_id = "capable-gpu-worker";
    let caps_key = format!("ff:worker:{instance_id}:caps");
    let _: Option<String> = tc
        .client()
        .cmd("SET")
        .arg(caps_key.as_str())
        .arg("cuda,gpu")
        .execute()
        .await
        .expect("SET worker caps");
    let _: Option<i64> = tc
        .client()
        .cmd("SADD")
        .arg("ff:idx:workers")
        .arg(instance_id)
        .execute()
        .await
        .expect("SADD workers-index");

    // 3. Run the unblock scanner for this execution's partition.
    let partition_idx = execution_partition(&eid, &config).index;
    let scanner = ff_engine::scanner::unblock::UnblockScanner::new(
        std::time::Duration::from_secs(30),
        vec![lane_id.clone()],
        config,
    );
    let _ = scanner.scan_partition(tc.client(), partition_idx).await;

    // 4. Execution should be back in eligible, off blocked:route.
    let idx = IndexKeys::new(&execution_partition(&eid, &config));
    let eligible_score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(idx.lane_eligible(&lane_id).as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .expect("ZSCORE eligible");
    assert!(
        eligible_score.is_some(),
        "execution must be promoted back to eligible after capable worker appears"
    );

    let blocked_score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(idx.lane_blocked_route(&lane_id).as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .expect("ZSCORE blocked:route");
    assert!(
        blocked_score.is_none(),
        "execution must NOT be in blocked:route after promotion"
    );
}

/// PR#7-BUG4 regression: required_capabilities tokens must be rejected by
/// ff_create_execution when they contain ASCII control bytes (0x00-0x1F,
/// 0x7F) or whitespace (0x20). Rust ingress in ff-sdk/ff-scheduler
/// already rejects these, but an admin direct-HSET or a path that
/// bypasses Rust ingress should STILL fail on the Lua side — the Lua
/// write-path validation is the last line of defense. Silent-accept of
/// "gpu\ncuda" would pin an execution as unclaimable forever.
#[tokio::test]
#[serial_test::serial]
async fn test_capability_required_control_or_whitespace_rejected() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let cases = [
        // Raw JSON escape of \n inside the cap string.
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu\ncuda"]}}"#,
            "newline in token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu\tcuda"]}}"#,
            "tab in token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu cuda"]}}"#,
            "space in token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu\u0000cuda"]}}"#,
            "NUL byte in token",
        ),
        (
            r#"{"routing_requirements":{"required_capabilities":["gpu\u007fcuda"]}}"#,
            "DEL byte in token",
        ),
    ];
    for (policy_json, label) in cases {
        let eid = tc.new_execution_id();
        let raw = fcall_create_execution_with_raw_policy(&tc, &eid, LANE, policy_json).await;
        assert_err(&raw, "invalid_capabilities", label);
    }
}

/// PR#7 follow-up regression (Copilot): if a per-worker GET errors out
/// inside `load_worker_caps_union` (transport blip, WRONGTYPE, etc.) the
/// union must NOT silently treat that worker as capless. Previously
/// `.unwrap_or(None)` merged error and empty into the same branch,
/// producing a false-negative union and keeping executions blocked even
/// though a matching worker existed.
///
/// Now the error propagates up; `check_route_cleared` treats `Err` as
/// fail-open ("we don't know → let the scheduler re-decide next tick")
/// and returns true, which promotes the blocked execution back to
/// eligible. Test shapes the failure with HSET so the subsequent GET
/// returns WRONGTYPE — a deterministic per-worker GET error we can
/// exercise without mocking Valkey.
#[tokio::test]
#[serial_test::serial]
async fn test_capable_worker_unblocks_on_get_error_failopen() {
    use ff_engine::scanner::Scanner;

    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let config = test_config();
    let lane_id = LaneId::new(LANE);
    let eid = tc.new_execution_id();

    // 1. Create execution with required caps {gpu}; block it via a
    //    mismatched scheduler claim.
    create_execution_with_caps(&tc, &eid, LANE, &["gpu"]).await;
    let worker_id = ff_core::types::WorkerId::new(WORKER);
    let wiid = ff_core::types::WorkerInstanceId::new(WORKER_INST);
    let scheduler = ff_scheduler::claim::Scheduler::new(tc.client().clone(), config);
    let cpu_only: std::collections::BTreeSet<String> =
        ["cpu".to_owned()].into_iter().collect();
    scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &cpu_only, 5000)
        .await
        .expect("scheduler claim should not error");

    // 2. Register a worker in the index but corrupt its caps key with a
    //    HSET instead of SET. A subsequent GET on that key returns
    //    WRONGTYPE — the concrete shape of "per-worker GET error" this
    //    test is guarding against.
    let instance_id = "wrongtype-gpu-worker";
    let caps_key = format!("ff:worker:{instance_id}:caps");
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(caps_key.as_str())
        .arg("corrupted")
        .arg("by-test")
        .execute()
        .await
        .expect("HSET plant WRONGTYPE");
    let _: Option<i64> = tc
        .client()
        .cmd("SADD")
        .arg("ff:idx:workers")
        .arg(instance_id)
        .execute()
        .await
        .expect("SADD workers-index");

    // 3. Run the unblock scanner. The GET on the corrupted caps key will
    //    error out inside load_worker_caps_union; that should bubble up
    //    through check_route_cleared which fails OPEN and promotes.
    let partition_idx = execution_partition(&eid, &config).index;
    let scanner = ff_engine::scanner::unblock::UnblockScanner::new(
        std::time::Duration::from_secs(30),
        vec![lane_id.clone()],
        config,
    );
    let _ = scanner.scan_partition(tc.client(), partition_idx).await;

    // 4. Execution MUST be promoted back to eligible. With the old
    //    `.unwrap_or(None)` swallow, the union would have been empty,
    //    the subset check would have failed, and the execution would
    //    still be in blocked:route.
    let idx = IndexKeys::new(&execution_partition(&eid, &config));
    let eligible_score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(idx.lane_eligible(&lane_id).as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .expect("ZSCORE eligible");
    assert!(
        eligible_score.is_some(),
        "execution must be promoted back to eligible when caps GET errors \
         (fail-open on transient errors preserves liveness)"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// FlowFabricWorker::claim_from_reclaim_grant (cairn-fabric P1B)
// ═══════════════════════════════════════════════════════════════════════

/// Write the test partition config into Valkey so the SDK's worker
/// reads the same 4-partition layout as `fcall_create_execution`.
/// Without this, `FlowFabricWorker` picks up whatever config was
/// already there (often the 256-partition default from a prior
/// server run), and the `ReclaimGrant.partition` computed from
/// `test_config()` points at a core-key that doesn't exist.
async fn write_test_partition_config(tc: &TestCluster) {
    let config = test_config();
    let _: () = tc.client().cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(config.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(config.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(config.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

/// Build a `FlowFabricWorker` configured for the suspend→signal→resume
/// flow using the standard e2e worker identity.
async fn build_reclaim_test_worker(
    worker_id: &str,
    worker_instance_id: &str,
) -> ff_sdk::FlowFabricWorker {
    let config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new(worker_id),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(worker_instance_id),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 16,
    };
    ff_sdk::FlowFabricWorker::connect(config).await.unwrap()
}

/// Drive an execution through create → claim → suspend → deliver-signal,
/// leaving it in `attempt_interrupted` with the suspended/runnable
/// transition already completed. Returns the eid plus the `ReclaimGrant`
/// a caller can pass to `claim_from_reclaim_grant`.
///
/// `grant_ttl_ms` is the TTL for the claim-grant the resumed-path
/// consumer reads. Short TTL = fast expiry test; long TTL = happy path.
async fn suspend_resume_setup_with_grant(
    tc: &TestCluster,
    eid: &ExecutionId,
    grant_ttl_ms: u64,
    grant_worker_id: &str,
    grant_worker_instance_id: &str,
) -> ff_core::contracts::ReclaimGrant {
    // 1. create + claim the initial attempt
    fcall_create_execution(tc, eid, NS, LANE, "reclaim_grant_test", 0).await;
    fcall_issue_claim_grant(tc, eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(tc, eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // 2. suspend (fail-on-timeout path to avoid timer noise)
    let resume_cond = build_resume_condition_json(&["approval_result"], "fail");
    let (_susp_id, waitpoint_id, _wp_key, _sub_status, wp_token) =
        fcall_suspend_execution(
            tc, eid, LANE, WORKER_INST,
            &lease_id, &epoch, &att_idx, &attempt_id,
            "waiting_for_approval", &resume_cond, None, "fail",
        ).await;

    // 3. deliver signal so the execution transitions to runnable +
    //    attempt_interrupted
    let (_sig_id, effect) = fcall_deliver_signal(
        tc, eid, LANE, &waitpoint_id,
        "approval_result", "approval", "", &wp_token,
    ).await;
    assert_eq!(effect, "resume_condition_satisfied");

    // 4. issue the reclaim grant — for the resumed flow, the grant is
    //    written by ff_issue_claim_grant (same key). We want a
    //    configurable TTL so expiry tests can dial it down; the existing
    //    fcall_issue_claim_grant helper uses a hardcoded 5 s TTL, so
    //    inline the FCALL here to parameterise grant_ttl_ms.
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        grant_worker_id.to_owned(),
        grant_worker_instance_id.to_owned(),
        LANE.to_owned(),
        String::new(),               // capability_hash
        grant_ttl_ms.to_string(),    // grant_ttl_ms (parameterised)
        String::new(),               // route_snapshot_json
        String::new(),               // admission_summary
        String::new(),               // worker_capabilities_csv
    ];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_issue_claim_grant", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_issue_claim_grant failed");
    assert_ok(&raw, "ff_issue_claim_grant");

    // 5. read grant_expires_at from the claim_grant hash
    let grant_key = ctx.claim_grant();
    let expires_at_str: Option<String> = tc.hget(&grant_key, "grant_expires_at").await;
    let expires_at_ms: u64 = expires_at_str
        .as_deref()
        .and_then(|s| s.parse().ok())
        .expect("grant_expires_at should be numeric");

    ff_core::contracts::ReclaimGrant {
        execution_id: eid.clone(),
        partition,
        grant_key,
        expires_at_ms,
        lane_id,
    }
}

/// Happy path + control: `claim_from_reclaim_grant` resumes the
/// suspended execution cleanly. Paired with the expiry test — both use
/// the same setup; this one consumes the grant immediately to prove
/// the flow works, the other waits out the TTL to prove the expiry
/// check fires.
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_reclaim_grant_happy_path() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    let eid = tc.new_execution_id();
    // Long TTL so there's no race with the HGET / FlowFabricWorker
    // construction below.
    let grant = suspend_resume_setup_with_grant(
        &tc, &eid, 30_000, WORKER, WORKER_INST,
    ).await;

    let worker = build_reclaim_test_worker(WORKER, WORKER_INST).await;
    let claimed = worker
        .claim_from_reclaim_grant(grant)
        .await
        .expect("happy path should succeed");

    // Resumed execution preserves attempt_index 0 — a resume re-uses
    // the existing attempt.
    assert_eq!(claimed.attempt_index().0, 0,
        "resumed execution should keep attempt_index 0");

    // Verify active state after consumption.
    assert_execution_state(&tc, &eid, &ExpectedState {
        lifecycle_phase: Some(ff_core::state::LifecyclePhase::Active),
        ownership_state: Some(ff_core::state::OwnershipState::Leased),
        attempt_state: Some(ff_core::state::AttemptState::RunningAttempt),
        public_state: Some(ff_core::state::PublicState::Active),
        ..Default::default()
    }).await;

    // Complete to release the lease.
    claimed.complete(Some(b"reclaim-done".to_vec())).await.unwrap();
}

/// Control companion to the expired-grant test: at the SAME setup
/// (short TTL) with NO sleep, the claim should succeed. Guards against
/// the expiry test passing due to a stuck-clock Lua bug rather than a
/// real TTL check.
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_reclaim_grant_immediate_control() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    let eid = tc.new_execution_id();
    // Short TTL — same knob the expiry test uses. No sleep here, so
    // the grant is still fresh when claimed.
    let grant = suspend_resume_setup_with_grant(
        &tc, &eid, 500, WORKER, WORKER_INST,
    ).await;

    let worker = build_reclaim_test_worker(WORKER, WORKER_INST).await;
    let claimed = worker
        .claim_from_reclaim_grant(grant)
        .await
        .expect("control: short-TTL grant claimed immediately should still succeed");

    claimed.complete(None).await.unwrap();
}

/// Expired grant → `ScriptError::ClaimGrantExpired`. Run AFTER the
/// control test above so the exit condition isn't confused with
/// "short TTL always fails" flakiness.
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_reclaim_grant_expired() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    let eid = tc.new_execution_id();
    // 200 ms TTL — small enough to wait out cheaply, large enough that
    // the setup FCALLs finish well inside it.
    let grant = suspend_resume_setup_with_grant(
        &tc, &eid, 200, WORKER, WORKER_INST,
    ).await;

    // Let the grant expire.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    let worker = build_reclaim_test_worker(WORKER, WORKER_INST).await;
    match worker.claim_from_reclaim_grant(grant).await {
        // Either error is acceptable:
        //   * `ClaimGrantExpired` fires when the FCALL observes the
        //     grant key still present but `grant_expires_at < now_ms`
        //     (Lua deletes the key and returns this code).
        //   * `InvalidClaimGrant` fires when Valkey's PEXPIREAT has
        //     already evicted the key before the FCALL reaches
        //     HGETALL (#grant_raw == 0).
        // Both signal the same root cause — the grant's TTL elapsed.
        Err(ff_sdk::SdkError::Script(ff_script::error::ScriptError::ClaimGrantExpired)) => {}
        Err(ff_sdk::SdkError::Script(ff_script::error::ScriptError::InvalidClaimGrant)) => {}
        Err(other) => panic!("expected ClaimGrantExpired or InvalidClaimGrant, got {other:?}"),
        Ok(_) => panic!("expired grant should not succeed"),
    }
}

/// Grant issued to worker A → worker B calls `claim_from_reclaim_grant`
/// → `ScriptError::InvalidClaimGrant`.
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_reclaim_grant_wrong_worker() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    let eid = tc.new_execution_id();
    // Grant issued for "worker-a".
    let grant = suspend_resume_setup_with_grant(
        &tc, &eid, 30_000, "worker-a", "worker-a-inst",
    ).await;

    // Build a FlowFabricWorker with a DIFFERENT worker_id.
    let worker = build_reclaim_test_worker("worker-b", "worker-b-inst").await;
    match worker.claim_from_reclaim_grant(grant).await {
        Err(ff_sdk::SdkError::Script(ff_script::error::ScriptError::InvalidClaimGrant)) => {}
        Err(other) => panic!("expected InvalidClaimGrant, got {other:?}"),
        Ok(_) => panic!("worker-id mismatch should not succeed"),
    }
}

/// Execution that was never suspended (attempt_state != attempt_interrupted)
/// → `ScriptError::NotAResumedExecution`. Builds a plain create+claim
/// execution, issues a grant, then tries to consume it via the RESUME
/// path instead of the normal claim path.
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_reclaim_grant_not_resumed() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    let eid = tc.new_execution_id();
    fcall_create_execution(&tc, &eid, NS, LANE, "not_resumed_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    // DO NOT suspend + deliver_signal. Execution is in ClaimGrant state
    // but attempt_state = NotAttempted (the initial claim hasn't fired
    // yet) — not attempt_interrupted.

    // Build the ReclaimGrant manually from the just-issued grant key.
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let grant_key = ctx.claim_grant();
    let expires_at_str: Option<String> = tc.hget(&grant_key, "grant_expires_at").await;
    let expires_at_ms: u64 = expires_at_str.as_deref().and_then(|s| s.parse().ok()).unwrap();

    let grant = ff_core::contracts::ReclaimGrant {
        execution_id: eid.clone(),
        partition,
        grant_key,
        expires_at_ms,
        lane_id: LaneId::new(LANE),
    };

    let worker = build_reclaim_test_worker(WORKER, WORKER_INST).await;
    match worker.claim_from_reclaim_grant(grant).await {
        Err(ff_sdk::SdkError::Script(ff_script::error::ScriptError::NotAResumedExecution)) => {}
        Err(ff_sdk::SdkError::Script(ff_script::error::ScriptError::ExecutionNotLeaseable)) => {
            // Acceptable alternate: some Lua orderings check
            // lifecycle_phase (runnable) before attempt_state. Either
            // error signals the same root cause — this is a non-
            // resumed execution, reject.
        }
        Err(other) => panic!("expected NotAResumedExecution or ExecutionNotLeaseable, got {other:?}"),
        Ok(_) => panic!("non-resumed execution should not succeed"),
    }
}

/// Grant is consumed atomically — a second call with the same grant
/// fails with `InvalidClaimGrant`.
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_reclaim_grant_double_consume() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    let eid = tc.new_execution_id();
    let grant = suspend_resume_setup_with_grant(
        &tc, &eid, 30_000, WORKER, WORKER_INST,
    ).await;

    let worker = build_reclaim_test_worker(WORKER, WORKER_INST).await;

    // First consumption succeeds.
    let claimed = worker
        .claim_from_reclaim_grant(grant.clone())
        .await
        .expect("first consume should succeed");

    // Second consume with the same grant handle must fail. The
    // underlying Valkey `claim_grant` key was DEL'd by the first
    // consume, AND the execution's `lifecycle_phase` transitioned
    // from `runnable` to `active`. The Lua FCALL checks
    // `lifecycle_phase == runnable` (line 524 of signal.lua) BEFORE
    // grant validation, so the error surfaced first is
    // `ExecutionNotLeaseable`. If an attacker/race ordered calls
    // differently we'd also see `InvalidClaimGrant` — accept either
    // as proof the second consume was rejected.
    match worker.claim_from_reclaim_grant(grant).await {
        Err(ff_sdk::SdkError::Script(ff_script::error::ScriptError::ExecutionNotLeaseable)) => {}
        Err(ff_sdk::SdkError::Script(ff_script::error::ScriptError::InvalidClaimGrant)) => {}
        Err(other) => panic!("expected ExecutionNotLeaseable or InvalidClaimGrant on second consume, got {other:?}"),
        Ok(_) => panic!("second consume should not succeed"),
    }

    // Cleanup: complete the still-live claim so the lease doesn't
    // linger into other serial tests.
    claimed.complete(None).await.unwrap();
}

/// Saturation guard for `claim_from_reclaim_grant` — mirrors
/// `test_claim_from_grant_rejects_at_capacity`. A worker at
/// `max_concurrent_tasks=1` with its single permit in flight must
/// refuse a second reclaim grant with `SdkError::WorkerAtCapacity`
/// WITHOUT invoking `ff_claim_resumed_execution` (so the grant key
/// is not atomically consumed).
///
/// Caught during cross-review round 2: the original
/// `claim_from_reclaim_grant` did NOT acquire a concurrency permit,
/// which on a saturated worker silently exceeded
/// `max_concurrent_tasks` and handed back a `ClaimedTask` without a
/// permit to release — violating the concurrency contract the user
/// configured. Fix mirrors W1's `claim_from_grant` saturation
/// pattern exactly.
#[tokio::test]
#[serial_test::serial]
async fn test_claim_from_reclaim_grant_rejects_at_capacity() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    write_test_partition_config(&tc).await;

    // Set up two independently-suspended executions so we have two
    // reclaim grants to try against the same worker. Both use the
    // standard e2e worker identity so both grants are targeted at
    // the same worker_id.
    let eid_a = tc.new_execution_id();
    let grant_a = suspend_resume_setup_with_grant(
        &tc, &eid_a, 30_000, WORKER, WORKER_INST,
    ).await;
    let eid_b = tc.new_execution_id();
    let grant_b = suspend_resume_setup_with_grant(
        &tc, &eid_b, 30_000, WORKER, WORKER_INST,
    ).await;
    let grant_b_key = grant_b.grant_key.clone();

    // Build a worker with max_concurrent_tasks=1 so the second
    // reclaim is guaranteed to saturate.
    let worker_config = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: ff_core::types::WorkerId::new(WORKER),
        worker_instance_id: ff_core::types::WorkerInstanceId::new(WORKER_INST),
        namespace: ff_core::types::Namespace::new(NS),
        lanes: vec![ff_core::types::LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config).await.unwrap();

    // 1st reclaim takes the sole permit.
    let task_a = worker
        .claim_from_reclaim_grant(grant_a)
        .await
        .expect("first reclaim should take the permit");

    // 2nd reclaim must refuse at capacity, BEFORE invoking the Lua
    // FCALL that would atomically consume the grant key.
    match worker.claim_from_reclaim_grant(grant_b).await {
        Err(ff_sdk::SdkError::WorkerAtCapacity) => {}
        Err(other) => panic!("expected WorkerAtCapacity, got {other:?}"),
        Ok(_) => panic!("saturated worker should not claim"),
    }

    // Retryability sanity check — same contract as claim_from_grant.
    assert!(
        ff_sdk::SdkError::WorkerAtCapacity.is_retryable(),
        "capacity saturation is transient"
    );

    // Grant unharmed: grant_b's claim_grant key is still present in
    // Valkey. If the FCALL had fired on a saturated worker we'd see
    // EXISTS=0 here (the Lua DELs the key on consume).
    let exists: i64 = tc.client()
        .cmd("EXISTS")
        .arg(&grant_b_key)
        .execute()
        .await
        .expect("EXISTS on grant_key");
    assert_eq!(exists, 1,
        "reclaim grant_key must still exist after capacity reject — \
         claim_from_reclaim_grant must not fire ff_claim_resumed_execution \
         when the semaphore is drained");

    // Cleanup so the lease from task_a doesn't linger into the next
    // serial test.
    task_a.complete(None).await.unwrap();
}
