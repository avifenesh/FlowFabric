//! PR-D1b / Test 4 — Waitpoint HMAC token round-trip + rotation.
//!
//! Drives a full suspend → signal → resume round-trip through the SDK
//! HMAC token path (RFC-004 §Waitpoint Security), then exercises the
//! kid-rotation surface (`ff_rotate_waitpoint_hmac_secret`) to prove:
//!
//!   A. Happy path: worker suspends with a `review` matcher. The
//!      minted waitpoint_token authenticates `deliver_signal`. After
//!      re-claim, `resume_signals()` surfaces the `review` signal.
//!   B. Tamper: flipping one hex char of the token makes
//!      `deliver_signal` reject with `invalid_token`.
//!   C. Rotation-in-grace: after rotating k1→k2 with a large grace
//!      window, a k1-minted token still authenticates.
//!   D. New-kid mint: a fresh suspend post-rotation mints a k2-signed
//!      token (prefix `k2:`).
//!   E. Rotation-past-grace: forcing `expires_at:k1` to the deep past
//!      causes k1-signed tokens to reject with `token_expired`.
//!
//! Structurally mirrors `lifecycle_retry_backoff.rs` for the server +
//! worker + flow-membership scaffolding, and borrows the HMAC-fixture
//! idioms from `ff-test::waitpoint_tokens`.
//!
//! RED-proof: the tamper assertion (B) flipped to accept the tampered
//! token (or deliver the good token on the tampered branch) panics
//! the assertion. Captured transcript:
//! `tests/red-proofs/waitpoint_hmac_roundtrip.log`.
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test waitpoint_hmac_roundtrip -- --ignored --test-threads=1
#![cfg(feature = "readiness")]

use std::time::{Duration, Instant};

use ff_core::contracts::{AddExecutionToFlowArgs, CreateExecutionArgs, CreateFlowArgs};
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_readiness_tests::evidence;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{probe_server_version, TestCluster, TEST_PARTITION_CONFIG};
use ff_sdk::task::{
    ClaimedTask, ResumeCondition, ResumePolicy, Signal, SignalMatcher, SignalOutcome,
    SuspensionReasonCode, TimeoutBehavior,
};
use ff_sdk::SdkError;
use serde_json::json;

const TEST_NAME: &str = "waitpoint_hmac_roundtrip";
const LANE: &str = "readiness-hmac";
const NS: &str = "readiness-hmac-ns";
const SIGNAL_NAME: &str = "review";

/// Replacement kid installed by the rotation step. Picked as `k2`
/// to match the `waitpoint_tokens` fixture conventions — the
/// assertion on `token.starts_with("k2:")` pins the choice.
const NEW_KID: &str = "k2";
/// Distinct 32-byte hex secret for k2. Test fixture only — see
/// `admin_rotate_api::FIXTURE_HEX_B` for the CodeQL rationale.
// codeql[rust/cleartext-logging-sensitive-data]
const NEW_SECRET_HEX: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";
/// Grace window for the in-grace assertion. 10 minutes is comfortably
/// larger than any conceivable test runtime while letting a stuck test
/// still time out at the tokio deadline rather than dangle on a
/// wall-clock grace expiry.
const GRACE_MS: u64 = 600_000;

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn waitpoint_hmac_roundtrip() {
    let t_start = Instant::now();

    // 1. Valkey harness + in-process ff-server.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let valkey_version = probe_server_version(tc.client()).await;
    let server = InProcessServer::start(LANE).await;

    // 2. Spawn the SDK worker once — all four executions below share it.
    let worker_config = ff_sdk::WorkerConfig {
        backend: Some(ff_readiness_tests::valkey::backend_config_from_env()),
        worker_id: WorkerId::new("readiness-hmac-worker"),
        worker_instance_id: WorkerInstanceId::new("readiness-hmac-inst"),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 4,
    partition_config: None,
    };
    let worker = ff_sdk::FlowFabricWorker::connect(worker_config)
        .await
        .expect("FlowFabricWorker::connect");

    // ── Step A + B: happy round-trip + tampered-token rejection ──
    let (eid_a, task_a) = create_claim(&server, &worker, "hmac-A").await;
    let (wp_a, token_a) = suspend_with_review(task_a).await;
    assert!(
        token_a.as_str().starts_with("k1:"),
        "fresh mint must be k1-signed (cleanup installs k1): {token_a:?}"
    );

    // B. Tamper: flip the last hex char. deliver_signal must reject.
    let tampered = {
        let s = token_a.as_str();
        let mut chars: Vec<char> = s.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == '0' { '1' } else { '0' };
        WaitpointToken::new(chars.into_iter().collect::<String>())
    };
    let tamper_err = worker
        .deliver_signal(&eid_a, &wp_a, signal_with_token(tampered))
        .await
        .expect_err("tampered token must be rejected");
    assert!(
        matches!(&tamper_err, SdkError::Engine(_)),
        "tampered token should surface as engine error: {tamper_err:?}"
    );
    let msg = format!("{tamper_err}");
    assert!(
        msg.contains("invalid_token"),
        "tampered token error must carry 'invalid_token' code; got {msg}"
    );

    // A. Happy: good token → TriggeredResume → re-claim → resume_signals.
    match worker
        .deliver_signal(&eid_a, &wp_a, signal_with_token(token_a))
        .await
        .expect("good token deliver_signal")
    {
        SignalOutcome::TriggeredResume { .. } => {}
        other => panic!("expected TriggeredResume on good token, got {other:?}"),
    }

    let task_a2 = reclaim(&worker, &eid_a).await;
    let signals = task_a2.resume_signals().await.expect("resume_signals");
    assert_eq!(
        signals.len(),
        1,
        "exactly one matched signal expected, got {signals:?}"
    );
    assert_eq!(signals[0].signal_name, SIGNAL_NAME);
    task_a2
        .complete(Some(b"hmac-roundtrip-ok".to_vec()))
        .await
        .expect("complete A");

    // Sanity: terminal state reached.
    await_terminal(&worker, &eid_a).await;

    // ── Step C: rotate k1 → k2 with large grace; a separately-minted
    //    k1 token must still validate. ──
    let (eid_b, task_b) = create_claim(&server, &worker, "hmac-B").await;
    let (wp_b, token_b_k1) = suspend_with_review(task_b).await;
    assert!(token_b_k1.as_str().starts_with("k1:"), "B pre-rotation token is k1");

    tc.rotate_waitpoint_hmac_secret(NEW_KID, NEW_SECRET_HEX, GRACE_MS).await;

    match worker
        .deliver_signal(&eid_b, &wp_b, signal_with_token(token_b_k1))
        .await
        .expect("in-grace k1 token deliver_signal")
    {
        SignalOutcome::TriggeredResume { .. } => {}
        other => panic!("expected TriggeredResume on in-grace k1 token, got {other:?}"),
    }
    let task_b2 = reclaim(&worker, &eid_b).await;
    task_b2.complete(Some(b"in-grace-ok".to_vec())).await.expect("complete B");
    await_terminal(&worker, &eid_b).await;

    // ── Step D: new waitpoints post-rotation mint under k2. ──
    let (_eid_c, task_c) = create_claim(&server, &worker, "hmac-C").await;
    let (_wp_c, token_c) = suspend_with_review(task_c).await;
    let token_c_kid_prefix = format!("{NEW_KID}:");
    assert!(
        token_c.as_str().starts_with(&token_c_kid_prefix),
        "post-rotation mint must be {NEW_KID}-signed; got token with prefix {:?}",
        token_c.as_str().split(':').next()
    );

    // ── Step E: force k1's grace to the deep past → k1 tokens reject
    //    with `token_expired`. Create a FRESH execution for this arm;
    //    we need a k1-signed token that's unused, which only exists on
    //    executions suspended before the rotation and not yet signaled.
    //    Suspending a fourth exec pre-rotation isn't possible from here
    //    (rotation already ran), so re-install k1 as current, mint a
    //    token, rotate forward to k3 so k1 is no longer current, then
    //    stomp expires_at:k1 to force past-grace. ──
    tc.install_waitpoint_hmac_secret(
        "k1",
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .await;
    let (eid_d, task_d) = create_claim(&server, &worker, "hmac-D").await;
    let (wp_d, token_d_k1) = suspend_with_review(task_d).await;
    assert!(
        token_d_k1.as_str().starts_with("k1:"),
        "D token must be k1-signed after reinstalling k1 as current"
    );
    // Roll forward to a fresh kid so k1 is no longer current; the kid
    // remains in the hash (grace window), then stomp its expires_at.
    tc.rotate_waitpoint_hmac_secret(
        "k3",
        "2222222222222222222222222222222222222222222222222222222222222222",
        GRACE_MS,
    )
    .await;
    tc.hset_waitpoint_expires_at("k1", 1).await;

    let expired_err = worker
        .deliver_signal(&eid_d, &wp_d, signal_with_token(token_d_k1))
        .await
        .expect_err("k1 token past grace must be rejected");
    let expired_msg = format!("{expired_err}");
    assert!(
        expired_msg.contains("token_expired"),
        "past-grace error must be 'token_expired'; got {expired_msg}"
    );

    // Explicit teardown — drains engine + background tasks so state
    // does not leak into the next serial readiness test.
    drop(worker);
    server.shutdown().await;

    // Evidence JSON — mirrors lifecycle_retry_backoff's shape.
    evidence::write(
        TEST_NAME,
        &json!({
            "test": TEST_NAME,
            "total_ms": t_start.elapsed().as_millis() as u64,
            "valkey_version": valkey_version,
            "assertions": {
                "happy_roundtrip": "TriggeredResume + resume_signals matched",
                "tamper_rejected": "invalid_token",
                "rotation_in_grace_accepted": true,
                "new_mint_kid_prefix": NEW_KID,
                "past_grace_rejected": "token_expired",
            },
            "executions": {
                "happy": eid_a.to_string(),
                "in_grace": eid_b.to_string(),
                "past_grace": eid_d.to_string(),
            },
        }),
    );
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Create_flow + create_execution + add_to_flow + worker.claim_next.
/// Mirrors the opening block of `lifecycle_retry_backoff` but factored
/// out since this test drives four separate executions.
async fn create_claim(
    server: &InProcessServer,
    worker: &ff_sdk::FlowFabricWorker,
    tag: &str,
) -> (ExecutionId, ClaimedTask) {
    let flow_id = FlowId::new();
    let now = TimestampMs::now();
    server
        .server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: format!("readiness-hmac-{tag}"),
            namespace: Namespace::new(NS),
            now,
        })
        .await
        .expect("create_flow");

    let eid = ExecutionId::for_flow(&flow_id, &TEST_PARTITION_CONFIG);
    let deadline_at = TimestampMs::from_millis(now.0 + 60_000);
    server
        .server
        .create_execution(&CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE),
            execution_kind: format!("readiness-hmac-{tag}"),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".to_owned()),
            priority: 0,
            creator_identity: "readiness".to_owned(),
            idempotency_key: None,
            tags: std::collections::HashMap::new(),
            policy: None,
            delay_until: None,
            execution_deadline_at: Some(deadline_at),
            partition_id: execution_partition(&eid, &TEST_PARTITION_CONFIG).index,
            now,
        })
        .await
        .expect("create_execution");
    server
        .server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id,
            execution_id: eid.clone(),
            now,
        })
        .await
        .expect("add_execution_to_flow");

    let task = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next Result") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("claim_next timed out");
    (eid, task)
}

/// Suspend a claimed task on a single `review` matcher and unwrap the
/// suspended outcome. Consumes the task (suspend's API contract).
async fn suspend_with_review(task: ClaimedTask) -> (WaitpointId, WaitpointToken) {
    let wp_key = format!("wpk:{}", ff_core::types::WaitpointId::new());
    let outcome = task
        .suspend(
            SuspensionReasonCode::WaitingForOperatorReview,
            ResumeCondition::Single {
                waitpoint_key: wp_key,
                matcher: SignalMatcher::ByName(SIGNAL_NAME.into()),
            },
            Some((
                ff_core::types::TimestampMs::from_millis(
                    ff_core::types::TimestampMs::now().0 + 60_000,
                ),
                TimeoutBehavior::Fail,
            )),
            ResumePolicy::normal(),
        )
        .await
        .expect("suspend");
    (
        outcome.details.waitpoint_id.clone(),
        outcome.details.waitpoint_token.token().clone(),
    )
}

/// Build the canonical test `Signal` (review / decision / test) carrying
/// the supplied token. Centralises the boilerplate so the test body
/// stays focused on token identity, not payload shape.
fn signal_with_token(token: WaitpointToken) -> Signal {
    Signal {
        signal_name: SIGNAL_NAME.into(),
        signal_category: "decision".into(),
        payload: Some(b"{\"ok\":true}".to_vec()),
        source_type: "readiness".into(),
        source_identity: "hmac-test".into(),
        idempotency_key: None,
        waitpoint_token: token,
    }
}

/// Poll `worker.claim_next()` until it hands back the execution whose id
/// matches `eid`. Bounded at 10s so a regression in the resume path
/// fails fast rather than hanging.
async fn reclaim(worker: &ff_sdk::FlowFabricWorker, eid: &ExecutionId) -> ClaimedTask {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("reclaim claim_next") {
                if t.execution_id() == eid {
                    break t;
                }
                // Not our exec (belongs to a sibling run on the same
                // worker); relinquish by dropping. ClaimedTask::Drop
                // emits a tracing::warn! and aborts the renewal task —
                // it does not panic — so the background lease expiry
                // reclaims the task for the next poll.
                drop(t);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("reclaim timed out — resume never made execution claimable")
}

/// Poll describe_execution until the execution reaches a terminal
/// public state. Mirrors the tail of `lifecycle_retry_backoff`.
async fn await_terminal(worker: &ff_sdk::FlowFabricWorker, eid: &ExecutionId) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = worker
            .describe_execution(eid)
            .await
            .expect("describe_execution")
            .expect("snapshot must exist");
        if matches!(
            snapshot.public_state,
            PublicState::Completed
                | PublicState::Failed
                | PublicState::Cancelled
                | PublicState::Expired
                | PublicState::Skipped
        ) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {eid} to reach terminal (5s); last={:?}",
                snapshot.public_state
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
