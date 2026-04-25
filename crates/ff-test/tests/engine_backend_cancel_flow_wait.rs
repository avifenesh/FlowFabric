//! Issue #298 regression: `EngineBackend::cancel_flow` must honour
//! [`CancelFlowWait::WaitTimeout`] and [`CancelFlowWait::WaitIndefinite`]
//! on the Valkey backend.
//!
//! Prior to #298 both wait modes returned
//! [`EngineError::Unavailable`] at the trait boundary — Server had to
//! work around it with a client-side poll over `NoWait`. This test
//! covers the trait-level poll implementation:
//!
//! * `wait_timeout_returns_cancelled_when_cascade_completes_in_time` —
//!   `cancel_flow` with `WaitTimeout(500ms)` on a memberless flow
//!   returns `Ok(Cancelled)` and the flow's `public_flow_state` is
//!   observably `cancelled`.
//! * `wait_timeout_returns_timeout_when_deadline_exceeded` — the
//!   shared `wait_for_flow_cancellation` helper returns
//!   `EngineError::Timeout` when invoked with an impossibly tight
//!   deadline against a still-open flow.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_cancel_flow_wait -- \
//!       --test-threads=1

use ferriskey::Value;
use ff_core::backend::{CancelFlowPolicy, CancelFlowWait};
use ff_core::contracts::CancelFlowResult;
use ff_core::engine_backend::wait_for_flow_cancellation;
use ff_core::engine_error::EngineError;
use ff_core::keys::{FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn seed_partition_config(tc: &TestCluster) {
    let cfg = test_config();
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(cfg.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(cfg.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(cfg.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

async fn create_flow_raw(tc: &TestCluster, fid: &FlowId) {
    let cfg = test_config();
    let partition = flow_partition(fid, &cfg);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![ctx.core(), ctx.members(), fidx.flow_index()];
    let now_ms = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        fid.to_string(),
        "dag".to_owned(),
        "cancel-wait-ns".to_owned(),
        now_ms,
    ];
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_flow", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_flow failed");
}

#[tokio::test]
async fn wait_timeout_returns_cancelled_when_cascade_completes_in_time() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;

    let fid = FlowId::new();
    create_flow_raw(&tc, &fid).await;

    let backend = tc.backend();

    // WaitTimeout(500ms) against a memberless flow: the FCALL flips
    // `public_flow_state = "cancelled"` synchronously and the shared
    // poll observes it on the first iteration, well inside the
    // deadline.
    let result = backend
        .cancel_flow(
            &fid,
            CancelFlowPolicy::CancelAll,
            CancelFlowWait::WaitTimeout(std::time::Duration::from_millis(500)),
        )
        .await
        .expect("cancel_flow WaitTimeout ok");

    assert!(
        matches!(result, CancelFlowResult::Cancelled { .. }),
        "expected Cancelled, got {result:?}",
    );

    // Sanity: describe_flow confirms the terminal state.
    let snap = backend
        .describe_flow(&fid)
        .await
        .expect("describe_flow ok")
        .expect("flow must exist");
    assert_eq!(snap.public_flow_state, "cancelled");
}

#[tokio::test]
async fn wait_timeout_returns_timeout_when_deadline_exceeded() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;

    let fid = FlowId::new();
    create_flow_raw(&tc, &fid).await;

    let backend = tc.backend();

    // Do NOT cancel the flow. Invoke the shared wait helper with an
    // impossibly-tight deadline (1ms) against a still-`open` flow and
    // assert the Timeout error shape. The poll runs one iteration,
    // observes a non-cancelled state, then the deadline-check fires on
    // the next loop entry because `start.elapsed() >= 1ms` after the
    // first network round-trip.
    let err = wait_for_flow_cancellation(
        &*backend,
        &fid,
        std::time::Duration::from_millis(1),
    )
    .await
    .expect_err("wait_for_flow_cancellation must time out on an open flow");

    match err {
        EngineError::Timeout { op, elapsed } => {
            assert_eq!(op, "cancel_flow");
            assert_eq!(elapsed, std::time::Duration::from_millis(1));
        }
        other => panic!("expected EngineError::Timeout, got {other:?}"),
    }
}
