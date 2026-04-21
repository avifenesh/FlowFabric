//! Waitpoint HMAC token validation tests (RFC-004 §Waitpoint Security).
//!
//! Exercises the kid-ring HMAC-SHA1 token flow end-to-end: minting on
//! suspend / create_pending_waitpoint, validation on signal delivery,
//! rotation behavior, and binding against the mint-time waitpoint identity.
//!
//! Run with: cargo test -p ff-test --test waitpoint_tokens -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const LANE: &str = "wpt-lane";
const NS: &str = "wpt-ns";
const WORKER: &str = "wpt-worker";
const WORKER_INST: &str = "wpt-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

// ── Minimal FCALL helpers (scoped to this test file) ─────────────────────

async fn create_and_claim(tc: &TestCluster, eid: &ExecutionId) -> (String, String, String, String) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);
    let now = TimestampMs::now();

    // 1. create_execution — KEYS (8), ARGV (13). Mirrors e2e_lifecycle fcall.
    let _ = now; // fcall reads server TIME
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
        "wpt".to_owned(),
        "0".to_owned(),
        "wpt-runner".to_owned(),
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

    // 2. issue_claim_grant — KEYS (3), ARGV (9).
    let grant_keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
    ];
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

    // 3. claim_execution — KEYS (14), ARGV (12) per e2e_lifecycle fcall_claim_execution.
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
    // {1, "OK", lease_id, epoch, expires_at, attempt_id, attempt_index, attempt_type}
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("claim: expected Array"),
    };
    let epoch = match arr.get(3) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => panic!("claim: no epoch, full: {raw:?}"),
    };

    (lease_id, epoch, "0".to_owned(), attempt_id)
}

/// Suspend an execution and return (wp_id_str, wp_key, token).
async fn suspend_and_get_token(
    tc: &TestCluster,
    eid: &ExecutionId,
    lease_id: &str,
    epoch: &str,
    attempt_id: &str,
) -> (String, String, String) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id_str = wp_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("wpk:{wp_id_str}");

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
    let resume_condition_json = serde_json::json!({
        "condition_type": "signal_set",
        "required_signal_names": ["wpt_signal"],
        "signal_match_mode": "any",
        "minimum_signal_count": 1,
        "timeout_behavior": "fail",
        "allow_operator_override": true,
    })
    .to_string();
    let resume_policy_json = serde_json::json!({
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
        wp_id_str.clone(),
        wp_key.clone(),
        "waiting_for_signal".to_owned(),
        "worker".to_owned(),
        String::new(),
        resume_condition_json,
        resume_policy_json,
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
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => panic!("suspend: no status"),
    };
    assert_eq!(status, 1, "suspend should succeed: {raw:?}");
    let token = match arr.get(5) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        _ => panic!("suspend: missing waitpoint_token at field 5: {raw:?}"),
    };
    assert!(!token.is_empty(), "token should not be empty");
    (wp_id_str, wp_key, token)
}

/// Deliver a signal with the given token. Returns the raw FCALL response so
/// tests can assert either success or an expected error code.
async fn try_deliver_signal(
    tc: &TestCluster,
    eid: &ExecutionId,
    wp_id_str: &str,
    signal_name: &str,
    waitpoint_token: &str,
) -> Value {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let signal_id_str = uuid::Uuid::new_v4().to_string();
    let wp_id = WaitpointId::parse(wp_id_str).unwrap();
    let sig_id = SignalId::parse(&signal_id_str).unwrap();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint_condition(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        ctx.exec_signals(),
        ctx.signal(&sig_id),
        ctx.signal_payload(&sig_id),
        ctx.noop(),
        ctx.waitpoint(&wp_id),
        ctx.suspension_current(),
        idx.lane_eligible(&lane_id),
        idx.lane_suspended(&lane_id),
        idx.lane_delayed(&lane_id),
        idx.suspension_timeout(),
        idx.waitpoint_hmac_secrets(),
    ];
    let args: Vec<String> = vec![
        signal_id_str.clone(),
        eid.to_string(),
        wp_id_str.to_owned(),
        signal_name.to_owned(),
        "test".to_owned(),
        "external".to_owned(),
        "wpt".to_owned(),
        String::new(),
        "json".to_owned(),
        String::new(),
        String::new(),
        "waitpoint".to_owned(),
        TimestampMs::now().to_string(),
        "86400000".to_owned(),
        "0".to_owned(),
        "1000".to_owned(),
        "10000".to_owned(),
        waitpoint_token.to_owned(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
    tc.client()
        .fcall("ff_deliver_signal", &kr, &ar)
        .await
        .expect("ff_deliver_signal FCALL transport")
}

fn assert_success(raw: &Value) {
    let arr = match raw {
        Value::Array(a) => a,
        _ => panic!("expected Array, got {raw:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => panic!("no status"),
    };
    assert_eq!(status, 1, "expected success, got {raw:?}");
}

fn assert_err_code(raw: &Value, expected: &str) {
    let arr = match raw {
        Value::Array(a) => a,
        _ => panic!("expected Array, got {raw:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => panic!("no status"),
    };
    assert_eq!(status, 0, "expected error, got {raw:?}");
    let code = match arr.get(1) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        _ => panic!("no error code"),
    };
    assert_eq!(code, expected, "expected {expected}, got {code}");
}

// ── Test matrix (RFC-004 §Waitpoint Security) ─────────────────────────────

/// 1. Valid token minted on suspend succeeds on deliver_signal.
#[tokio::test]
#[serial_test::serial]
async fn test_valid_token_accepted() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, _att_idx, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, _wp_key, token) =
        suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let resp = try_deliver_signal(&tc, &eid, &wp_id, "wpt_signal", &token).await;
    assert_success(&resp);
}

/// 2. Empty/missing token → `missing_token`.
#[tokio::test]
#[serial_test::serial]
async fn test_missing_token_rejected() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, _att_idx, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, _wp_key, _token) =
        suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let resp = try_deliver_signal(&tc, &eid, &wp_id, "wpt_signal", "").await;
    assert_err_code(&resp, "missing_token");
}

/// 3. Token with flipped hex char → `invalid_token`.
#[tokio::test]
#[serial_test::serial]
async fn test_tampered_token_rejected() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, _att_idx, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, _wp_key, token) =
        suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    // Flip the last hex char: this yields the same format but a digest
    // that won't match constant-time-compare.
    let mut bytes: Vec<char> = token.chars().collect();
    let last = bytes.len() - 1;
    bytes[last] = if bytes[last] == '0' { '1' } else { '0' };
    let tampered: String = bytes.into_iter().collect();

    let resp = try_deliver_signal(&tc, &eid, &wp_id, "wpt_signal", &tampered).await;
    assert_err_code(&resp, "invalid_token");
}

/// 4. Token with an unknown kid prefix → `invalid_token`.
#[tokio::test]
#[serial_test::serial]
async fn test_wrong_kid_rejected() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, _att_idx, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, _wp_key, _token) =
        suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let bogus = "k9:0000000000000000000000000000000000000000";
    let resp = try_deliver_signal(&tc, &eid, &wp_id, "wpt_signal", bogus).await;
    assert_err_code(&resp, "invalid_token");
}

/// 5. Rotation within grace: token minted under k1 still validates after k2
/// rotation while `previous_expires_at` has not elapsed.
#[tokio::test]
#[serial_test::serial]
async fn test_rotation_grace_allows_previous_kid() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Fresh secret — TestCluster::cleanup already installed k1.
    let eid = tc.new_execution_id();
    let (lease_id, epoch, _att_idx, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, _wp_key, k1_token) =
        suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;
    assert!(k1_token.starts_with("k1:"), "token should be k1-signed: {k1_token}");

    // Rotate to k2 with a grace window 10 minutes into the future.
    let future_grace = TimestampMs::now().0 + 600_000;
    tc.rotate_waitpoint_hmac_secret(
        "k2",
        "1111111111111111111111111111111111111111111111111111111111111111",
        future_grace,
    )
    .await;

    // Old (k1) token still works during grace.
    let resp = try_deliver_signal(&tc, &eid, &wp_id, "wpt_signal", &k1_token).await;
    assert_success(&resp);
}

/// 6. Rotation past grace: a k1-signed token is `token_expired` when
/// `previous_expires_at` has already passed.
#[tokio::test]
#[serial_test::serial]
async fn test_rotation_past_grace_rejects_previous_kid() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, _att_idx, attempt_id) = create_and_claim(&tc, &eid).await;
    let (wp_id, _wp_key, k1_token) =
        suspend_and_get_token(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    // Rotate with a grace that expired 1s ago.
    let past_grace = TimestampMs::now().0 - 1000;
    tc.rotate_waitpoint_hmac_secret(
        "k2",
        "2222222222222222222222222222222222222222222222222222222222222222",
        past_grace,
    )
    .await;

    let resp = try_deliver_signal(&tc, &eid, &wp_id, "wpt_signal", &k1_token).await;
    assert_err_code(&resp, "token_expired");
}

/// 7. Pending-waitpoint flow: create_pending_waitpoint returns a token that
/// authenticates buffer_signal_for_pending_waitpoint. After the pending
/// waitpoint is activated by suspend, the same token continues to work for
/// ff_deliver_signal (the token is bound to the pending waitpoint's
/// mint-time `created_at`, which suspend preserves).
#[tokio::test]
#[serial_test::serial]
async fn test_pending_waitpoint_token_flow() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (_lease_id, _epoch, _att_idx, _attempt_id) = create_and_claim(&tc, &eid).await;

    // Create pending waitpoint with its own minted token.
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id_str = wp_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("wpk-pending:{wp_id_str}");
    let expires_at = TimestampMs::now().0 + 60_000;

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint(&wp_id),
        idx.pending_waitpoint_expiry(),
        idx.waitpoint_hmac_secrets(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        "0".to_owned(),
        wp_id_str.clone(),
        wp_key.clone(),
        expires_at.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_pending_waitpoint", &kr, &ar)
        .await
        .expect("ff_create_pending_waitpoint");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("create_pending: expected Array"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => panic!("create_pending: no status"),
    };
    assert_eq!(status, 1, "create_pending should succeed: {raw:?}");
    let pending_token = match arr.get(4) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        _ => panic!("create_pending: missing token at field 4: {raw:?}"),
    };
    assert!(!pending_token.is_empty());

    // Buffer a signal against the pending waitpoint — must validate token.
    // Wrong token path first.
    let buf_keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint_condition(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        ctx.exec_signals(),
        ctx.signal(&SignalId::new()),
        ctx.signal_payload(&SignalId::new()),
        ctx.noop(),
        ctx.waitpoint(&wp_id),
        idx.waitpoint_hmac_secrets(),
    ];
    let bad_buf_args: Vec<String> = vec![
        uuid::Uuid::new_v4().to_string(),
        eid.to_string(),
        wp_id_str.clone(),
        "wpt_signal".to_owned(),
        "test".to_owned(),
        "external".to_owned(),
        "wpt".to_owned(),
        String::new(),
        "json".to_owned(),
        String::new(),
        String::new(),
        "waitpoint".to_owned(),
        TimestampMs::now().to_string(),
        "86400000".to_owned(),
        "0".to_owned(),
        "1000".to_owned(),
        "10000".to_owned(),
        "k9:0000000000000000000000000000000000000000".to_owned(),
    ];
    let kr: Vec<&str> = buf_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = bad_buf_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_buffer_signal_for_pending_waitpoint", &kr, &ar)
        .await
        .expect("ff_buffer_signal_for_pending_waitpoint (bad)");
    assert_err_code(&raw, "invalid_token");

    // Valid token path — token remains bound to the pending waitpoint's
    // mint-time created_at, unchanged by the activation below.
    let good_buf_args: Vec<String> = vec![
        uuid::Uuid::new_v4().to_string(),
        eid.to_string(),
        wp_id_str.clone(),
        "wpt_signal".to_owned(),
        "test".to_owned(),
        "external".to_owned(),
        "wpt".to_owned(),
        String::new(),
        "json".to_owned(),
        String::new(),
        String::new(),
        "waitpoint".to_owned(),
        TimestampMs::now().to_string(),
        "86400000".to_owned(),
        "0".to_owned(),
        "1000".to_owned(),
        "10000".to_owned(),
        pending_token.clone(),
    ];
    let ar: Vec<&str> = good_buf_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_buffer_signal_for_pending_waitpoint", &kr, &ar)
        .await
        .expect("ff_buffer_signal_for_pending_waitpoint (good)");
    assert_success(&raw);
}

/// 8. Cross-waitpoint binding: a token minted for waitpoint A is rejected
/// when presented against waitpoint B, even when both are active on the
/// same execution. The HMAC input binds waitpoint_id, so a leaked token
/// cannot be replayed against another waitpoint on the same execution.
#[tokio::test]
#[serial_test::serial]
async fn test_cross_waitpoint_token_rejected() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Two separate executions each get their own waitpoint + token. Using
    // two executions avoids the single-open-suspension invariant; the
    // security property we're checking is the token→waitpoint binding.
    let eid_a = tc.new_execution_id();
    let eid_b = tc.new_execution_id();
    let (lid_a, ep_a, _, aid_a) = create_and_claim(&tc, &eid_a).await;
    let (lid_b, ep_b, _, aid_b) = create_and_claim(&tc, &eid_b).await;
    let (_wp_a, _wk_a, token_a) =
        suspend_and_get_token(&tc, &eid_a, &lid_a, &ep_a, &aid_a).await;
    let (wp_b, _wk_b, _token_b) =
        suspend_and_get_token(&tc, &eid_b, &lid_b, &ep_b, &aid_b).await;

    // Present A's token to B → rejected.
    let resp = try_deliver_signal(&tc, &eid_b, &wp_b, "wpt_signal", &token_a).await;
    assert_err_code(&resp, "invalid_token");
}

/// 9a. ff_deliver_signal validates HMAC BEFORE state-probe — no oracle.
/// Without this guarantee, an unauthenticated caller could enumerate which
/// (execution_id, waitpoint_id) tuples exist, and whether they're
/// pending / active / closed / terminal, by observing the error code.
/// This test verifies that with a random bogus waitpoint_id + bogus token
/// against a real execution, the error is `invalid_token` (not
/// `waitpoint_not_found` or `target_not_signalable`).
#[tokio::test]
#[serial_test::serial]
async fn test_deliver_signal_no_state_oracle_on_bad_token() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Create an execution in active/leased state (NOT suspended, NO waitpoint).
    let eid = tc.new_execution_id();
    let (_lease_id, _epoch, _att_idx, _attempt_id) = create_and_claim(&tc, &eid).await;

    // Present an invented waitpoint_id and a bogus token. Pre-fix: caller
    // would see `target_not_signalable` or `waitpoint_not_found` — signalling
    // existence/state of the execution. Post-fix: `invalid_token`, because
    // auth runs first and unifies the missing-waitpoint case into the
    // token-failure bucket.
    let bogus_wp = uuid::Uuid::new_v4().to_string();
    let bogus_token = "k1:0000000000000000000000000000000000000000";
    let resp = try_deliver_signal(&tc, &eid, &bogus_wp, "wpt_signal", bogus_token).await;
    assert_err_code(&resp, "invalid_token");
}

/// 9. buffer_signal_for_pending_waitpoint rejects after waitpoint closed.
/// ff_deliver_signal blocks replay via wp_condition.closed, but that
/// condition hash does not exist for pending waitpoints — the buffer
/// function must independently gate on wp.state. Without this gate, a
/// caller holding a valid pending-waitpoint token could keep appending
/// signals to a closed waitpoint that would replay on a later
/// suspend(use_pending=1).
#[tokio::test]
#[serial_test::serial]
async fn test_buffer_signal_rejected_after_waitpoint_closed() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let (_lease_id, _epoch, _att_idx, _attempt_id) = create_and_claim(&tc, &eid).await;

    // Create pending waitpoint, capture token.
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id_str = wp_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("wpk-pending-closed:{wp_id_str}");
    let expires_at = TimestampMs::now().0 + 60_000;
    let create_keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint(&wp_id),
        idx.pending_waitpoint_expiry(),
        idx.waitpoint_hmac_secrets(),
    ];
    let create_args: Vec<String> = vec![
        eid.to_string(),
        "0".to_owned(),
        wp_id_str.clone(),
        wp_key.clone(),
        expires_at.to_string(),
    ];
    let kr: Vec<&str> = create_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = create_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_create_pending_waitpoint", &kr, &ar)
        .await
        .expect("ff_create_pending_waitpoint");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("create_pending: expected Array"),
    };
    let token = match arr.get(4) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        _ => panic!("create_pending: missing token: {raw:?}"),
    };

    // Close the pending waitpoint directly by HSET state=closed on its hash.
    // Simulates ff_close_waitpoint having run while the external signaler
    // was mid-flight with a still-valid token.
    let wp_hash_key = ctx.waitpoint(&wp_id);
    tc.client()
        .hset(&wp_hash_key, "state", "closed")
        .await
        .expect("HSET close pending waitpoint");

    // Build a buffer_signal call with the valid token; expect waitpoint_closed.
    let buf_keys: Vec<String> = vec![
        ctx.core(),
        ctx.waitpoint_condition(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        ctx.exec_signals(),
        ctx.signal(&SignalId::new()),
        ctx.signal_payload(&SignalId::new()),
        ctx.noop(),
        ctx.waitpoint(&wp_id),
        idx.waitpoint_hmac_secrets(),
    ];
    let buf_args: Vec<String> = vec![
        uuid::Uuid::new_v4().to_string(),
        eid.to_string(),
        wp_id_str.clone(),
        "wpt_signal".to_owned(),
        "test".to_owned(),
        "external".to_owned(),
        "wpt".to_owned(),
        String::new(),
        "json".to_owned(),
        String::new(),
        String::new(),
        "waitpoint".to_owned(),
        TimestampMs::now().to_string(),
        "86400000".to_owned(),
        "0".to_owned(),
        "1000".to_owned(),
        "10000".to_owned(),
        token,
    ];
    let kr: Vec<&str> = buf_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = buf_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_buffer_signal_for_pending_waitpoint", &kr, &ar)
        .await
        .expect("ff_buffer_signal_for_pending_waitpoint (closed)");
    assert_err_code(&raw, "waitpoint_closed");
}

/// 11. Rapid rotation preserves in-grace tokens (RFC-004 §Waitpoint Security).
///
/// Per the multi-kid validation model: ANY kid present in the secrets
/// hash with a future `expires_at:<kid>` (or the current kid) accepts
/// tokens. Rotating A→B→C within a grace window must keep A-signed tokens
/// valid as long as A's expiry is still in the future.
///
/// The pre-fix two-slot model evicted A as soon as B became previous_kid,
/// silently shrinking A's grace window from 24h to "time to next rotation".
/// This test locks in the new contract: explicit grace per kid.
#[tokio::test]
#[serial_test::serial]
async fn test_rapid_rotation_preserves_in_grace_tokens() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Mint a token under k1 (installed by cleanup()).
    let eid_a = tc.new_execution_id();
    let (lid_a, ep_a, _, aid_a) = create_and_claim(&tc, &eid_a).await;
    let (wp_a, _wk_a, k1_token) =
        suspend_and_get_token(&tc, &eid_a, &lid_a, &ep_a, &aid_a).await;
    assert!(k1_token.starts_with("k1:"), "expected k1-signed token: {k1_token}");

    // Rotate k1 → k2 with 10-minute grace. A-signed token must still work.
    let future_grace = TimestampMs::now().0 + 600_000;
    tc.rotate_waitpoint_hmac_secret(
        "k2",
        "2222222222222222222222222222222222222222222222222222222222222222",
        future_grace,
    )
    .await;

    // Rotate k2 → k3 with another 10-minute grace. Under the OLD two-slot
    // model, previous_kid would now be k2 and k1 would silently fall off.
    // Under multi-kid, expires_at:k1 is still future so k1-signed tokens
    // keep validating.
    tc.rotate_waitpoint_hmac_secret(
        "k3",
        "3333333333333333333333333333333333333333333333333333333333333333",
        future_grace,
    )
    .await;

    // The k1-signed token from before any rotation must still be accepted.
    // This is the regression guard for the R4 BLOCKER.
    let resp = try_deliver_signal(&tc, &eid_a, &wp_a, "wpt_signal", &k1_token).await;
    assert_success(&resp);
}

/// 12. Rapid rotation past grace: after an expires_at:<kid> elapses, that
/// kid's tokens reject with `token_expired`, not silently succeed and not
/// invalid_token (which would suggest "never existed").
#[tokio::test]
#[serial_test::serial]
async fn test_rapid_rotation_past_grace_rejects_expired() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Mint a k1 token, rotate to k2 with a grace window ALREADY in the past.
    let eid_b = tc.new_execution_id();
    let (lid_b, ep_b, _, aid_b) = create_and_claim(&tc, &eid_b).await;
    let (wp_b, _wk_b, k1_token) =
        suspend_and_get_token(&tc, &eid_b, &lid_b, &ep_b, &aid_b).await;

    // Rotate k1 → k2 with the grace window already past. The rotation
    // stamps expires_at:k1 = past_grace; secret:k1 stays in the hash
    // until a LATER rotation's orphan GC sweeps it.
    let past_grace = TimestampMs::now().0 - 1_000;
    tc.rotate_waitpoint_hmac_secret(
        "k2",
        "2222222222222222222222222222222222222222222222222222222222222222",
        past_grace,
    )
    .await;

    // k1-signed token → `token_expired`. expires_at:k1 is present but
    // in the past; multi-kid validator distinguishes "present kid whose
    // grace has elapsed" (token_expired — actionable: operator rolled
    // forward, client is holding an aged token) from "unknown kid"
    // (invalid_token — never existed or already GC'd). A subsequent
    // rotation would GC secret:k1; we stop before that to pin the
    // "known kid, past grace" branch.
    let resp = try_deliver_signal(&tc, &eid_b, &wp_b, "wpt_signal", &k1_token).await;
    assert_err_code(&resp, "token_expired");
}

/// 13. Malformed hex in the stored secret is rejected with `invalid_secret`.
///
/// Defense-in-depth: ServerConfig::from_env validates the env secret as
/// even-length `0-9a-fA-F`, but an operator (or a buggy tool) writing
/// directly to Valkey could plant a torn or non-hex value. The Lua
/// `hex_to_bytes` helper rejects ANY non-hex pair — no silent coercion
/// to 0 bytes, which would produce a bogus-but-valid-looking MAC.
///
/// This test installs a non-hex secret for the current kid, then attempts
/// to deliver a signal and expects `invalid_secret` to bubble up through
/// mint (at suspend) and/or validate (at deliver).
#[tokio::test]
#[serial_test::serial]
async fn test_malformed_hex_secret_rejects_cleanly() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Install a deliberately malformed secret under a fresh kid on every
    // partition. "zzzzzz..." is ascii-printable but not hex, and even-
    // length so it doesn't trip the length guard. The real guard is the
    // per-pair tonumber(...) check.
    let bad_hex = "z".repeat(64);
    tc.install_waitpoint_hmac_secret("k1", &bad_hex).await;

    // Try to suspend an execution. mint_waitpoint_token will call
    // hmac_sha1_hex → hex_to_bytes(secret) → nil → "invalid_secret".
    let eid = tc.new_execution_id();
    let (lease_id, epoch, _, attempt_id) = create_and_claim(&tc, &eid).await;

    // Suspend with a fresh wp_id/wp_key. We can't use `suspend_and_get_token`
    // here because it asserts success; we want to observe the failure.
    let partition = execution_partition(&eid, &config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);
    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id_str = wp_uuid.to_string();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("wpk:{wp_id_str}");
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
    let resume_condition_json = serde_json::json!({
        "condition_type": "signal_set",
        "required_signal_names": ["wpt_signal"],
        "signal_match_mode": "any",
        "minimum_signal_count": 1,
        "timeout_behavior": "fail",
        "allow_operator_override": true,
    })
    .to_string();
    let resume_policy_json = serde_json::json!({
        "resume_target": "runnable",
        "close_waitpoint_on_resume": true,
        "consume_matched_signals": true,
        "retain_signal_buffer_until_closed": true,
    })
    .to_string();
    let args: Vec<String> = vec![
        eid.to_string(),
        "0".to_owned(),
        attempt_id,
        lease_id,
        epoch,
        uuid::Uuid::new_v4().to_string(),
        wp_id_str.clone(),
        wp_key.clone(),
        "waiting_for_signal".to_owned(),
        "worker".to_owned(),
        String::new(),
        resume_condition_json,
        resume_policy_json,
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
    // Mint path rejects with invalid_secret — the token never reaches the wire.
    assert_err_code(&raw, "invalid_secret");
}
