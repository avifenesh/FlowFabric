//! PR-D1c / Test 5b — DAG-edge fanin-skip on upstream failure.
//!
//! Same diamond DAG as `fanout_fanin_flow_happy`, but B FAILS. The
//! `success_only` edge B→D becomes Impossible, so D must reach
//! terminal Skipped even after C completes successfully.
//!
//! Split from the happy subtest — see that file's header for the
//! engine-pollution rationale.
//!
//! Graph:
//!
//!                 ┌── B (Fail) ──┐
//!              A ─┤              ├─ D (Skipped)
//!                 └── C ─────────┘
//!
//! Assertions:
//!   * B reaches terminal Failed.
//!   * C still reaches Completed (sibling-branch failure does not
//!     poison C).
//!   * D reaches terminal Skipped (impossibility propagates through
//!     the `success_only` edge via ff_resolve_dependency).
//!
//! RED-proof: `tests/red-proofs/fanout_fanin_flow.log`. The break
//! for this file flips the tail `await_public_state(D, Skipped)` to
//! `Completed`; the bounded poll times out against the actual
//! terminal (Skipped).
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test fanout_fanin_flow_fanin_skip -- --ignored --test-threads=1
#![cfg(feature = "readiness")]

mod common;
use common::*;

use std::time::Instant;

use ff_core::state::PublicState;
use ff_readiness_tests::evidence;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{probe_server_version, TestCluster};
use ff_sdk::task::ClaimedTask;
use serde_json::json;

/// Finalise a sibling task: slot 0 = B (fail), slot 1 = C (complete).
/// Kept alongside the test because the semantics (which sibling fails)
/// are specific to the fanin-skip shape — not worth hoisting into
/// `common/mod.rs`.
async fn dispose_sibling(slot: usize, task: ClaimedTask) {
    match slot {
        0 => {
            task.fail("synthetic-b-failure", "worker_error")
                .await
                .expect("fail B");
        }
        1 => {
            task.complete(Some(b"C-ok".to_vec()))
                .await
                .expect("complete C");
        }
        other => panic!("dispose_sibling: unexpected slot {other}"),
    }
}

const LANE: &str = "readiness-fanout-skip";
const NS: &str = "readiness-fanout-skip-ns";
const TEST_NAME: &str = "fanout_fanin_flow_fanin_skip";

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn fanout_fanin_flow_fanin_skip() {
    let t_start = Instant::now();

    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let valkey_version = probe_server_version(tc.client()).await;
    let server = InProcessServer::start(LANE).await;
    let worker = spawn_worker(LANE, NS, "skip").await;

    let dag = stage_diamond_dag(&server, NS, LANE).await;
    let Dag { a, b, c, d, .. } = &dag;

    // Complete A so B + C unblock.
    let task_a = claim_specific(&worker, a).await;
    task_a.complete(Some(b"A-ok".to_vec())).await.expect("complete A");
    await_public_state(&worker, a, PublicState::Completed).await;
    await_public_state(&worker, b, PublicState::Waiting).await;
    await_public_state(&worker, c, PublicState::Waiting).await;

    // Claim B and C in worker-claim order (post-A-complete fanout makes
    // both concurrently eligible; `claim_specific(b)` would drop C and
    // hold its 30s lease if C came back first, wedging the next claim).
    // Fail the one that corresponds to B, complete the one that
    // corresponds to C — order-independent.
    let siblings = [b, c];
    let (first_idx, first_task) = claim_any_of(&worker, &siblings).await;
    dispose_sibling(first_idx, first_task).await;
    let second_idx = 1 - first_idx;
    let (got_idx, second_task) = claim_any_of(&worker, &[siblings[second_idx]]).await;
    assert_eq!(got_idx, 0, "single-sibling claim must match slot 0");
    dispose_sibling(second_idx, second_task).await;

    await_public_state(&worker, b, PublicState::Failed).await;
    await_public_state(&worker, c, PublicState::Completed).await;

    // D must reach Skipped. Push-promotion of an impossible edge flips
    // D via ff_resolve_dependency→Impossible. Bounded.
    await_public_state(&worker, d, PublicState::Skipped).await;

    evidence::write(
        TEST_NAME,
        &json!({
            "test": TEST_NAME,
            "total_ms": t_start.elapsed().as_millis() as u64,
            "valkey_version": valkey_version,
            "flow_id": dag.flow_id.to_string(),
            "executions": {
                "A": a.to_string(),
                "B": b.to_string(),
                "C": c.to_string(),
                "D": d.to_string(),
            },
            "assertions": {
                "b_terminal_failed": true,
                "c_terminal_completed": true,
                "d_terminal_skipped": true,
            },
        }),
    );
}
