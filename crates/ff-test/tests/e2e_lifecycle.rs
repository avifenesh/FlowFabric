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
        String::new(), // idem_key (empty = no dedup)
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

    // ARGV (8): eid, worker_id, worker_instance_id, lane, cap_hash, grant_ttl, route_json, admission
    let args: Vec<String> = vec![
        eid.to_string(),
        worker_id.to_owned(),
        worker_instance_id.to_owned(),
        lane.to_owned(),
        String::new(),        // capability_hash
        "5000".to_owned(),    // grant_ttl_ms
        String::new(),        // route_snapshot_json
        String::new(),        // admission_summary
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

    let eid = ExecutionId::new();
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

    let eid = ExecutionId::new();

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

    let eid = ExecutionId::new();

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

    let eid = ExecutionId::new();

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

    let eid = ExecutionId::new();
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

    let args: Vec<String> = vec![
        eid.to_string(),
        worker_id.to_owned(),
        worker_instance_id.to_owned(),
        lane.to_owned(),
        String::new(),
        "5000".to_owned(),
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

    let eid = ExecutionId::new();

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

    let eid = ExecutionId::new();

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

    let eid = ExecutionId::new();

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

    let eid = ExecutionId::new();

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
) -> (String, String, String, String) {
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

    // KEYS (16): exec_core, attempt_record, lease_current, lease_history,
    //            lease_expiry, worker_leases, suspension_current, waitpoint_hash,
    //            waitpoint_signals, suspension_timeout, pending_wp_expiry,
    //            active_index, suspended_index, waitpoint_history, wp_condition,
    //            attempt_timeout
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
    (suspension_id, waitpoint_id_str, waitpoint_key, sub_status)
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
        String::new()
    } else {
        ctx.signal_dedup(&wp_id, idempotency_key)
    };

    // KEYS (13): exec_core, wp_condition, wp_signals_stream,
    //            exec_signals_zset, signal_hash, signal_payload,
    //            idem_key, waitpoint_hash, suspension_current,
    //            eligible_zset, suspended_zset, delayed_zset,
    //            suspension_timeout_zset
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
    ];

    // ARGV (17): signal_id, execution_id, waitpoint_id, signal_name,
    //            signal_category, source_type, source_identity,
    //            payload, payload_encoding, idempotency_key,
    //            correlation_id, target_scope, created_at,
    //            dedup_ttl_ms, resume_delay_ms, signal_maxlen,
    //            max_signals_per_execution
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
/// Returns (waitpoint_id, waitpoint_key).
async fn fcall_create_pending_waitpoint(
    tc: &TestCluster,
    eid: &ExecutionId,
    attempt_index: &str,
    expires_in_ms: u64,
) -> (String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    let waitpoint_id_uuid = uuid::Uuid::new_v4();
    let waitpoint_id_str = waitpoint_id_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(waitpoint_id_uuid);
    let waitpoint_key = format!("wpk:{waitpoint_id_str}");

    let expires_at = TimestampMs::now().0 + expires_in_ms as i64;

    // KEYS (3): exec_core, waitpoint_hash, pending_wp_expiry_zset
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint(&wp_id),
        idx.pending_waitpoint_expiry(),
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

    assert_ok(&raw, "ff_create_pending_waitpoint");
    (waitpoint_id_str, waitpoint_key)
}

/// Call ff_buffer_signal_for_pending_waitpoint.
/// Returns (signal_id, effect).
async fn fcall_buffer_signal(
    tc: &TestCluster,
    eid: &ExecutionId,
    waitpoint_id_str: &str,
    signal_name: &str,
    idempotency_key: &str,
) -> (String, String) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);

    let signal_id = uuid::Uuid::new_v4().to_string();
    let wp_id = WaitpointId::parse(waitpoint_id_str).unwrap();
    let sig_id = ff_core::types::SignalId::parse(&signal_id).unwrap();
    let now = TimestampMs::now();

    let idem_key = if idempotency_key.is_empty() {
        String::new()
    } else {
        ctx.signal_dedup(&wp_id, idempotency_key)
    };

    // KEYS (7): exec_core, wp_condition, wp_signals_stream,
    //           exec_signals_zset, signal_hash, signal_payload, idem_key
    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint_condition(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        ctx.exec_signals(),
        ctx.signal(&sig_id),
        ctx.signal_payload(&sig_id),
        idem_key,
    ];

    // ARGV (17): same layout as ff_deliver_signal
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

    let eid = ExecutionId::new();

    // 1. Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "suspend_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;
    assert_eq!(att_idx, "0");

    // 2. Suspend
    let resume_cond = build_resume_condition_json(&["approval_result"], "fail");
    let (suspension_id, waitpoint_id, _waitpoint_key, sub_status) =
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
        "approval_result", "approval", "",
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

    let eid = ExecutionId::new();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "timeout_fail", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Suspend with timeout in the past (triggers immediate expiry)
    let past_timeout = TimestampMs::now().0 - 1000; // 1s ago
    let resume_cond = build_resume_condition_json(&["approval"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub) = fcall_suspend_execution(
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

    let eid = ExecutionId::new();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "timeout_resume", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Suspend with auto_resume timeout behavior
    // Lua checks behavior == "auto_resume" (not the long form)
    let past_timeout = TimestampMs::now().0 - 1000;
    let resume_cond = build_resume_condition_json(&["approval"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub) = fcall_suspend_execution(
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

    let eid = ExecutionId::new();

    // Create + claim
    fcall_create_execution(&tc, &eid, NS, LANE, "pending_wp_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // 1. Create pending waitpoint while still active
    let (waitpoint_id, _waitpoint_key) = fcall_create_pending_waitpoint(
        &tc, &eid, &att_idx, 60_000,
    ).await;

    // 2. Buffer a signal against the pending waitpoint (before suspension)
    let (_sig_id, _effect) = fcall_buffer_signal(
        &tc, &eid, &waitpoint_id, "callback_result", "",
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

    let eid = ExecutionId::new();

    // Create + claim + suspend
    fcall_create_execution(&tc, &eid, NS, LANE, "idem_test", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    // Use 2 matchers so first signal doesn't trigger resume
    let resume_cond = build_resume_condition_json(&["sig_a", "sig_b"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub) = fcall_suspend_execution(
        &tc, &eid, LANE, WORKER_INST,
        &lease_id, &epoch, &att_idx, &attempt_id,
        "waiting_for_signal", &resume_cond, None, "fail",
    ).await;

    // First signal delivery
    let (sig_id_1, effect_1) = fcall_deliver_signal(
        &tc, &eid, LANE, &waitpoint_id,
        "sig_a", "callback", "dedup-key-1",
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

    let eid = ExecutionId::new();

    // Create + claim + suspend
    fcall_create_execution(&tc, &eid, NS, LANE, "cancel_susp", 0).await;
    fcall_issue_claim_grant(&tc, &eid, LANE, WORKER, WORKER_INST).await;
    let (lease_id, epoch, att_idx, attempt_id) =
        fcall_claim_execution(&tc, &eid, LANE, WORKER, WORKER_INST, 30_000).await;

    let resume_cond = build_resume_condition_json(&["approval"], "fail");
    let (_sid, waitpoint_id, _wpk, _sub) = fcall_suspend_execution(
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

    let eid = ExecutionId::new();
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

    let eid = ExecutionId::new();

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

    let eid = ExecutionId::new();
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

/// Helper: set up a budget directly in Valkey (no ff_create_budget function yet).
/// Uses a fixed {b:0} tag for test simplicity.
async fn setup_test_budget(
    tc: &TestCluster,
    budget_id: &str,
    hard_limits: &[(&str, u64)],
    soft_limits: &[(&str, u64)],
) {
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");

    // Budget definition
    tc.client()
        .cmd("HSET").arg(&def_key)
        .arg("budget_id").arg(budget_id)
        .arg("scope_type").arg("lane")
        .arg("scope_id").arg("test-lane")
        .arg("on_hard_limit").arg("fail")
        .arg("on_soft_limit").arg("warn")
        .arg("enforcement_mode").arg("strict")
        .arg("breach_count").arg("0")
        .arg("soft_breach_count").arg("0")
        .arg("last_updated_at").arg("0")
        .execute::<i64>()
        .await
        .expect("HSET budget_def failed");

    // Budget limits
    for (dim, limit) in hard_limits {
        tc.client()
            .cmd("HSET").arg(&limits_key)
            .arg(format!("hard:{dim}")).arg(limit.to_string())
            .execute::<i64>()
            .await
            .expect("HSET hard limit failed");
    }
    for (dim, limit) in soft_limits {
        tc.client()
            .cmd("HSET").arg(&limits_key)
            .arg(format!("soft:{dim}")).arg(limit.to_string())
            .execute::<i64>()
            .await
            .expect("HSET soft limit failed");
    }

    // Empty usage hash (ensure it exists)
    tc.client()
        .cmd("HSET").arg(&usage_key)
        .arg("_init").arg("0")
        .execute::<i64>()
        .await
        .expect("HSET usage init failed");
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

    // Parse result array — domain-specific format (not ok/err convention)
    match &raw {
        Value::Array(arr) => {
            arr.iter().filter_map(|v| match v {
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
    setup_test_budget(&tc, budget_id,
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
    setup_test_budget(&tc, budget_id2,
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
    setup_test_budget(&tc, budget_id,
        &[("total_tokens", 1000)],
        &[],
    ).await;

    // Set reset_interval_ms on the budget def
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    tc.client()
        .cmd("HSET").arg(&def_key)
        .arg("reset_interval_ms").arg("3600000") // 1 hour
        .arg("reset_count").arg("0")
        .execute::<i64>()
        .await
        .expect("HSET reset_interval_ms failed");

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

    let eid = ExecutionId::new();
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
    let eid2 = ExecutionId::new();
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

    // Set up quota def
    tc.client()
        .cmd("HSET").arg(&def_key)
        .arg("quota_policy_id").arg(policy_id)
        .arg("on_rate_breach").arg("delay_until_window")
        .execute::<i64>()
        .await.unwrap();

    // Admit execution 1 → ADMITTED
    let eid1 = ExecutionId::new();
    let guard1 = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{}", eid1);
    let now1 = TimestampMs::now();
    let keys1: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard1];
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
    let eid2 = ExecutionId::new();
    let guard2 = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{}", eid2);
    let now2 = TimestampMs::now();
    let keys2: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard2];
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
    let eid3 = ExecutionId::new();
    let guard3 = format!("ff:quota:{{q:0}}:{policy_id}:admitted:{}", eid3);
    let now3 = TimestampMs::now();
    let keys3: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard3];
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
    let keys4: Vec<String> = vec![window_key.clone(), concurrency_key.clone(), def_key.clone(), guard3b];
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

/// Set up a flow directly in Valkey.
async fn setup_test_flow(tc: &TestCluster, flow_id: &str, members: &[&ExecutionId]) {
    let flow_core = format!("ff:flow:{{fp:0}}:{flow_id}:core");
    let members_key = format!("ff:flow:{{fp:0}}:{flow_id}:members");
    let nc = members.len().to_string();
    tc.client().cmd("HSET").arg(&flow_core)
        .arg("flow_id").arg(flow_id).arg("flow_kind").arg("test")
        .arg("namespace").arg(NS).arg("graph_revision").arg("0")
        .arg("node_count").arg(nc.as_str()).arg("edge_count").arg("0")
        .arg("public_flow_state").arg("open")
        .execute::<i64>().await.expect("HSET flow_core");
    for eid in members {
        tc.client().cmd("SADD").arg(&members_key).arg(eid.to_string())
            .execute::<i64>().await.expect("SADD member");
    }
}

async fn set_flow_id_on_exec(tc: &TestCluster, eid: &ExecutionId, flow_id: &str) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    tc.client().cmd("HSET").arg(&ctx.core()).arg("flow_id").arg(flow_id)
        .execute::<i64>().await.expect("HSET flow_id");
}

async fn fcall_apply_dependency(
    tc: &TestCluster, downstream: &ExecutionId, flow_id: &str,
    edge_id: &str, upstream: &ExecutionId,
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
    ];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        flow_id.to_owned(), edge_id.to_owned(), upstream.to_string(),
        "1".to_owned(), "success_only".to_owned(), String::new(), now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let raw: Value = tc.client()
        .fcall("ff_apply_dependency_to_child", &kr, &ar)
        .await.expect("FCALL apply_dep");
    assert_ok(&raw, "apply_dep");
}

async fn fcall_resolve_dependency(
    tc: &TestCluster, downstream: &ExecutionId,
    edge_id: &str, outcome: &str,
) -> String {
    let config = test_config();
    let p = execution_partition(downstream, &config);
    let ctx = ExecKeyContext::new(&p, downstream);
    let idx = IndexKeys::new(&p);
    let lane_id = LaneId::new(LANE);
    let eid_p = ff_core::types::EdgeId::parse(edge_id).unwrap();
    let att = AttemptIndex::new(0);
    let keys: Vec<String> = vec![
        ctx.core(), ctx.deps_meta(), ctx.deps_unresolved(),
        ctx.dep_edge(&eid_p), idx.lane_eligible(&lane_id),
        idx.lane_terminal(&lane_id), idx.lane_blocked_dependencies(&lane_id),
        ctx.attempt_hash(att), ctx.stream_meta(att),
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
    let a = ExecutionId::new();
    let b = ExecutionId::new();
    let c = ExecutionId::new();
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_bc = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "chain_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "chain_b", 0).await;
    fcall_create_execution(&tc, &c, NS, LANE, "chain_c", 0).await;
    setup_test_flow(&tc, fid, &[&a, &b, &c]).await;
    set_flow_id_on_exec(&tc, &a, fid).await;
    set_flow_id_on_exec(&tc, &b, fid).await;
    set_flow_id_on_exec(&tc, &c, fid).await;
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
    assert_eq!(fcall_resolve_dependency(&tc, &b, &e_ab, "success").await, "satisfied");
    assert_in_eligible(&tc, &b, &LaneId::new(LANE)).await;
    // Complete B, resolve B->C
    fcall_issue_claim_grant(&tc, &b, LANE, WORKER, WORKER_INST).await;
    let (lb, eb, _, ab) = fcall_claim_execution(&tc, &b, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &b, LANE, WORKER_INST, &lb, &eb, &ab).await;
    assert_eq!(fcall_resolve_dependency(&tc, &c, &e_bc, "success").await, "satisfied");
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

/// Fan-out: A->B, A->C. Complete A, both B and C unblocked.
#[tokio::test]
#[serial_test::serial]
async fn test_flow_fan_out() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let fid = "flow-fanout";
    let a = ExecutionId::new();
    let b = ExecutionId::new();
    let c = ExecutionId::new();
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_ac = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "fo_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "fo_b", 0).await;
    fcall_create_execution(&tc, &c, NS, LANE, "fo_c", 0).await;
    setup_test_flow(&tc, fid, &[&a, &b, &c]).await;
    set_flow_id_on_exec(&tc, &a, fid).await;
    set_flow_id_on_exec(&tc, &b, fid).await;
    set_flow_id_on_exec(&tc, &c, fid).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    fcall_apply_dependency(&tc, &c, fid, &e_ac, &a).await;
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    fcall_resolve_dependency(&tc, &b, &e_ab, "success").await;
    fcall_resolve_dependency(&tc, &c, &e_ac, "success").await;
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
    let a = ExecutionId::new();
    let b = ExecutionId::new();
    let c = ExecutionId::new();
    let e_ab = uuid::Uuid::new_v4().to_string();
    let e_bc = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "fp_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "fp_b", 0).await;
    fcall_create_execution(&tc, &c, NS, LANE, "fp_c", 0).await;
    setup_test_flow(&tc, fid, &[&a, &b, &c]).await;
    set_flow_id_on_exec(&tc, &a, fid).await;
    set_flow_id_on_exec(&tc, &b, fid).await;
    set_flow_id_on_exec(&tc, &c, fid).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    fcall_apply_dependency(&tc, &c, fid, &e_bc, &b).await;
    // Complete A, resolve A->B
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_complete_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa).await;
    fcall_resolve_dependency(&tc, &b, &e_ab, "success").await;
    // Fail B
    fcall_issue_claim_grant(&tc, &b, LANE, WORKER, WORKER_INST).await;
    let (lb, eb, _, ab) = fcall_claim_execution(&tc, &b, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_fail_execution(&tc, &b, LANE, WORKER_INST, &lb, &eb, &ab, "err", "worker_error", "").await;
    // Resolve B->C as "failed" -> C becomes skipped
    assert_eq!(fcall_resolve_dependency(&tc, &c, &e_bc, "failed").await, "impossible");
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
    let a = ExecutionId::new();
    let b = ExecutionId::new();
    let e_ab = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "ri_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "ri_b", 0).await;
    setup_test_flow(&tc, fid, &[&a, &b]).await;
    set_flow_id_on_exec(&tc, &a, fid).await;
    set_flow_id_on_exec(&tc, &b, fid).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;
    assert_eq!(fcall_resolve_dependency(&tc, &b, &e_ab, "failed").await, "impossible");
    // Second resolve -> already_resolved
    assert_eq!(fcall_resolve_dependency(&tc, &b, &e_ab, "success").await, "already_resolved");
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
    let a = ExecutionId::new();
    let b = ExecutionId::new();
    let e_ab = uuid::Uuid::new_v4().to_string();
    fcall_create_execution(&tc, &a, NS, LANE, "fl_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "fl_b", 0).await;
    setup_test_flow(&tc, fid, &[&a, &b]).await;
    set_flow_id_on_exec(&tc, &a, fid).await;
    set_flow_id_on_exec(&tc, &b, fid).await;
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
    fcall_resolve_dependency(&tc, &b, &e_ab, "success").await;
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
    let a = ExecutionId::new();
    let b = ExecutionId::new();
    let e_ab = uuid::Uuid::new_v4().to_string();
    let config = test_config();

    fcall_create_execution(&tc, &a, NS, LANE, "rep_a", 0).await;
    fcall_create_execution(&tc, &b, NS, LANE, "rep_b", 0).await;
    setup_test_flow(&tc, fid, &[&a, &b]).await;
    set_flow_id_on_exec(&tc, &a, fid).await;
    set_flow_id_on_exec(&tc, &b, fid).await;
    fcall_apply_dependency(&tc, &b, fid, &e_ab, &a).await;

    // Fail A (terminal)
    fcall_issue_claim_grant(&tc, &a, LANE, WORKER, WORKER_INST).await;
    let (la, ea, _, aa) = fcall_claim_execution(&tc, &a, LANE, WORKER, WORKER_INST, 30_000).await;
    fcall_fail_execution(&tc, &a, LANE, WORKER_INST, &la, &ea, &aa, "err", "worker_error", "").await;

    // Resolve A->B as "failed" -> B becomes skipped
    fcall_resolve_dependency(&tc, &b, &e_ab, "failed").await;
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
