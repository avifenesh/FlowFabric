//! PR-D1a / Test 3 — Retry + backoff floor.
//!
//! Proves a retryable failure reschedules the execution, the backoff
//! floor is honoured, and a second claim produces attempt 1 with its
//! own stream. Mirrors `lifecycle_happy_path.rs` in structure:
//!
//!   create_flow → create_execution(retry_policy) → add_to_flow
//!   → claim 0 → fail(retryable) → wait → claim 1 → complete
//!   → poll-to-terminal.
//!
//! Assertions (per PR-D1a spec):
//!   - snapshot.total_attempt_count == 2
//!   - Both attempt streams exist (XLEN > 0 on attempt 0 + attempt 1)
//!   - Observed delay between fail(0) and successful re-claim(1) is
//!     >= backoff floor (retry_policy.backoff.delay_ms)
//!
//! RED-proof: setting `max_retries=0` in the policy makes fail return
//! `TerminalFailed`; no attempt 1 is ever scheduled, the re-claim loop
//! times out. Captured transcript: `tests/red-proofs/lifecycle_retry_backoff.log`.
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test lifecycle_retry_backoff -- --ignored --test-threads=1
#![cfg(feature = "readiness")]

use std::time::{Duration, Instant};

use ff_core::contracts::{AddExecutionToFlowArgs, CreateExecutionArgs, CreateFlowArgs};
use ff_core::keys::ExecKeyContext;
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_readiness_tests::evidence;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{probe_server_version, TestCluster, TEST_PARTITION_CONFIG};
use serde_json::json;

const TEST_NAME: &str = "lifecycle_retry_backoff";
const LANE: &str = "readiness-retry";
const NS: &str = "readiness-retry-ns";

/// Backoff floor the test pins. It must be larger than the default
/// 750ms engine delayed-promoter interval; otherwise an observed
/// re-claim delay could satisfy the assertion even if the configured
/// backoff floor were ignored. Keep this comfortably above 750ms so
/// the floor is the dominant term despite promoter-tick quantization.
const BACKOFF_FLOOR_MS: u64 = 1500;

/// Max retries — must be >= 1 for attempt 1 to be scheduled. The
/// RED-proof flips this to 0.
const MAX_RETRIES: u32 = 1;

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn lifecycle_retry_backoff() {
    let t_start = Instant::now();

    // 1. Valkey harness.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let valkey_version = probe_server_version(tc.client()).await;

    // 2. In-process ff-server.
    let server = InProcessServer::start(LANE).await;

    // 3. Flow + 4. execution (with retry policy) + 5. membership.
    let flow_id = FlowId::new();
    let now = TimestampMs::now();
    server
        .server
        .create_flow(&CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "readiness-retry".to_owned(),
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
            execution_kind: "readiness-retry".to_owned(),
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

    // Overwrite the policy key directly with the retry-policy JSON the
    // engine's fail path reads. The typed ExecutionPolicy → policy_json
    // round-trip has edge-case behaviours with empty arrays (cjson)
    // that ff-test works around by poking the raw key — mirror that.
    let partition = execution_partition(&eid, &TEST_PARTITION_CONFIG);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let policy_json = format!(
        r#"{{"retry_policy":{{"max_retries":{MAX_RETRIES},"backoff":{{"type":"fixed","delay_ms":{BACKOFF_FLOOR_MS}}}}}}}"#
    );
    let _: () = tc
        .client()
        .cmd("SET")
        .arg(ctx.policy())
        .arg(policy_json.as_str())
        .execute()
        .await
        .expect("SET policy retry_policy");

    server
        .server
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now,
        })
        .await
        .expect("add_execution_to_flow");

    // 6. Spawn in-process SDK worker.
    let worker_config = ff_sdk::WorkerConfig {
        backend: Some(ff_readiness_tests::valkey::backend_config_from_env()),
        worker_id: WorkerId::new("readiness-retry-worker"),
        worker_instance_id: WorkerInstanceId::new("readiness-retry-inst"),
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

    // 7. Claim attempt 0.
    let task0 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next Result") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("claim_next(0) timed out — execution never became eligible");
    assert_eq!(task0.attempt_index(), AttemptIndex::new(0));
    assert_eq!(task0.execution_id(), &eid);

    // 8. Emit one frame so attempt 0's stream is observable on the
    //    post-terminal read below. The fail path closes `stream_meta`
    //    but does not append a terminal stream frame, so this explicit
    //    append is what makes the stream non-empty.
    let _ = task0
        .append_frame(
            "progress",
            &serde_json::to_vec(&json!({"attempt": 0})).expect("encode frame"),
            None,
        )
        .await
        .expect("append_frame attempt 0");

    // 9. Fail with a retryable category → RetryScheduled. The timer
    //    starts AFTER `fail` returns so fail-RPC/FCALL latency doesn't
    //    get credited to the backoff budget — the assertion measures
    //    only the post-fail scheduled delay.
    let outcome = task0
        .fail("injected", "transient")
        .await
        .expect("task0.fail");
    let t_fail = Instant::now();
    assert!(
        matches!(outcome, ff_sdk::FailOutcome::RetryScheduled { .. }),
        "max_retries={MAX_RETRIES} + retryable category should schedule retry, got {outcome:?}"
    );

    // 10. Re-claim attempt 1. The delayed promoter runs every 750ms so
    //     the observed delay is max(backoff_floor, promoter_tick).
    //     Bounded at 10s so a regression fails fast rather than hangs.
    let task1 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(t) = worker.claim_next().await.expect("claim_next(1) Result") {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("claim_next(1) timed out — retry was never promoted/claimable");
    let observed_delay_ms = t_fail.elapsed().as_millis() as u64;
    assert_eq!(
        task1.attempt_index(),
        AttemptIndex::new(1),
        "second claim should be attempt 1"
    );
    assert_eq!(task1.execution_id(), &eid);
    assert!(
        observed_delay_ms >= BACKOFF_FLOOR_MS,
        "observed retry delay {observed_delay_ms}ms must be >= backoff floor {BACKOFF_FLOOR_MS}ms"
    );

    // 11. Emit a frame on attempt 1 and complete. Appending here is
    //     what makes attempt 1's stream observable via XLEN — the
    //     complete path itself does not write a frame.
    let _ = task1
        .append_frame(
            "progress",
            &serde_json::to_vec(&json!({"attempt": 1})).expect("encode frame"),
            None,
        )
        .await
        .expect("append_frame attempt 1");
    task1
        .complete(Some(b"readiness-retry-ok".to_vec()))
        .await
        .expect("task1.complete");

    // 12. Poll describe_execution until terminal.
    let deadline = Instant::now() + Duration::from_secs(5);
    let (public_state, total_attempts) = loop {
        let snapshot = worker
            .describe_execution(&eid)
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
            break (snapshot.public_state, snapshot.total_attempt_count);
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for execution {eid} to reach terminal (5s); \
                 last public_state={:?}",
                snapshot.public_state
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // 13. Assert terminal shape: Completed, 2 attempts.
    assert_eq!(
        public_state,
        PublicState::Completed,
        "derived public state should be Completed after retry success"
    );
    assert_eq!(
        total_attempts, 2,
        "failed attempt 0 + succeeded attempt 1 should give total_attempt_count=2"
    );

    // 14. Both attempt streams must be visible (XLEN > 0 on each).
    let stream_0_key = ctx.stream(AttemptIndex::new(0));
    let stream_1_key = ctx.stream(AttemptIndex::new(1));
    let stream_0_len: i64 = tc
        .client()
        .cmd("XLEN")
        .arg(stream_0_key.as_str())
        .execute()
        .await
        .expect("XLEN attempt 0");
    let stream_1_len: i64 = tc
        .client()
        .cmd("XLEN")
        .arg(stream_1_key.as_str())
        .execute()
        .await
        .expect("XLEN attempt 1");
    assert!(
        stream_0_len >= 1,
        "attempt 0 stream should carry the progress frame, got {stream_0_len}"
    );
    assert!(
        stream_1_len >= 1,
        "attempt 1 stream should contain at least the appended progress frame, got {stream_1_len}"
    );

    // Explicit teardown — drains engine + background tasks so state
    // does not leak into the next serial readiness test.
    drop(worker);
    server.shutdown().await;

    // 15. Evidence JSON.
    evidence::write(
        TEST_NAME,
        &json!({
            "test": TEST_NAME,
            "total_ms": t_start.elapsed().as_millis() as u64,
            "valkey_version": valkey_version,
            "execution_id": eid.to_string(),
            "flow_id": flow_id.to_string(),
            "backoff_floor_ms": BACKOFF_FLOOR_MS,
            "observed_delay_ms": observed_delay_ms,
            "assertions": {
                "public_state": format!("{:?}", public_state),
                "total_attempt_count": total_attempts,
                "stream_0_len": stream_0_len,
                "stream_1_len": stream_1_len,
            },
        }),
    );
}
