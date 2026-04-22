//! PR-D1c / Test 5a — DAG-edge fanout + fanin, happy path.
//!
//! Pre-stages a small diamond DAG (A → {B, C} → D) through the typed
//! Server API, runs it to terminal via the SDK worker, and asserts the
//! engine's push-promotion path (completion_listener →
//! partition_router → ff_resolve_dependency) unblocks downstream
//! members in the expected shape.
//!
//! FF has no dynamic runtime `spawn_child`; fanout/fanin is an
//! edge-on-a-pre-staged-graph concept. That is exactly what this test
//! targets — the D-eligibility-between-B-and-C question is a claim
//! about the graph's `all_required` satisfaction semantics, not
//! dynamic spawn. (Confirmed with manager; RFC-007 §Flow DAG.)
//!
//! Split from the fanin-skip path into its own file to avoid
//! cross-test engine-pollution: `InProcessServer::drop` does not
//! shut the spawned engine down (engine tasks keep running on their
//! own JoinSet), so two tests in the same binary end up with two
//! concurrent completion_listeners racing on the same Valkey. The
//! manager's PR-D1c plan explicitly flagged this as "split if we see
//! it locally" — we saw it, so we split.
//!
//! Graph:
//!
//!                 ┌── B ──┐
//!              A ─┤       ├─ D
//!                 └── C ──┘
//!
//! Assertions:
//!   * After A completes: B and C become Waiting in parallel
//!     (push-promotion via completion_listener).
//!   * While only ONE of {B, C} has completed, D stays
//!     WaitingChildren (bounded-poll invariant — no single-shot race).
//!   * After BOTH B and C complete, D becomes Waiting, claims, and
//!     completes cleanly.
//!
//! Correctness only — no push-promotion-latency ceiling (manager
//! decision; add a timing assertion later if push-promo regresses).
//!
//! RED-proof: `tests/red-proofs/fanout_fanin_flow.log` (shared with
//! the fanin-skip subtest). The break for this file swaps the
//! post-A-complete `await_public_state(B, Waiting)` for the
//! single-shot `assert_public_state` — the async completion_listener
//! hasn't promoted yet, so the assertion fires on WaitingChildren.
//!
//! Run with:
//!   cargo test -p ff-readiness-tests --features readiness \
//!       --test fanout_fanin_flow_happy -- --ignored --test-threads=1
#![cfg(feature = "readiness")]

mod common;
use common::*;

use std::time::Instant;

use ff_core::state::PublicState;
use ff_readiness_tests::evidence;
use ff_readiness_tests::server::InProcessServer;
use ff_readiness_tests::valkey::{probe_server_version, TestCluster};
use serde_json::json;

const LANE: &str = "readiness-fanout-happy";
const NS: &str = "readiness-fanout-happy-ns";
const TEST_NAME: &str = "fanout_fanin_flow_happy";

#[tokio::test]
#[ignore]
#[serial_test::serial]
async fn fanout_fanin_flow_happy() {
    let t_start = Instant::now();

    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let valkey_version = probe_server_version(tc.client()).await;
    let server = InProcessServer::start(LANE).await;
    let worker = spawn_worker(LANE, NS, "happy").await;

    // ── Build the DAG: A fans out to B + C, both fan in to D ──
    let dag = stage_diamond_dag(&server, NS, LANE).await;
    let Dag { a, b, c, d, .. } = &dag;

    // Baseline: A Waiting (no deps); B, C, D all WaitingChildren.
    assert_public_state(&worker, a, PublicState::Waiting).await;
    assert_public_state(&worker, b, PublicState::WaitingChildren).await;
    assert_public_state(&worker, c, PublicState::WaitingChildren).await;
    assert_public_state(&worker, d, PublicState::WaitingChildren).await;

    // ── Step 1: claim + complete A ──
    let task_a = claim_specific(&worker, a).await;
    task_a.complete(Some(b"A-ok".to_vec())).await.expect("complete A");
    await_public_state(&worker, a, PublicState::Completed).await;

    // ── Step 2: push-promotion should have unblocked B AND C ──
    //    Completion listener is async, so bounded-poll both to Waiting.
    await_public_state(&worker, b, PublicState::Waiting).await;
    await_public_state(&worker, c, PublicState::Waiting).await;

    // D must STILL be blocked — both upstream legs are not yet terminal.
    assert_public_state(&worker, d, PublicState::WaitingChildren).await;

    // ── Step 3: complete the first of {B, C} the worker returns,
    //    then verify D is NOT eligible yet ──
    //    B and C are concurrently eligible after A completes; the
    //    worker's claim_next picks whichever lane_eligible ZRANGEBYSCORE
    //    returns first. Claiming "specifically B" and dropping C (or
    //    vice versa) would hold the wrongly-claimed task's lease for
    //    30s, wedging the next claim. Use claim_any_of to consume them
    //    in whatever order the scheduler serves.
    let siblings = [b, c];
    let (first_idx, first_task) = claim_any_of(&worker, &siblings).await;
    let first_label = ["B", "C"][first_idx];
    first_task
        .complete(Some(format!("{first_label}-ok").into_bytes()))
        .await
        .unwrap_or_else(|e| panic!("complete first sibling ({first_label}): {e}"));
    await_public_state(&worker, siblings[first_idx], PublicState::Completed).await;

    // Invariant: only ONE of {B, C} is terminal → D must stay
    // WaitingChildren. Bounded poll rather than single-shot — the
    // engine's completion_listener is asynchronous and could flip D
    // briefly if fanin semantics regress; if the invariant fails we
    // want deterministic failure, not a race-dependent flake.
    assert_not_waiting_for(&worker, d, 1_000).await;

    // ── Step 4: complete the remaining sibling → D must become Waiting ──
    let second_idx = 1 - first_idx;
    let second_label = ["B", "C"][second_idx];
    let (got_idx, second_task) = claim_any_of(&worker, &[siblings[second_idx]]).await;
    assert_eq!(got_idx, 0, "single-sibling claim must match slot 0");
    second_task
        .complete(Some(format!("{second_label}-ok").into_bytes()))
        .await
        .unwrap_or_else(|e| panic!("complete second sibling ({second_label}): {e}"));
    await_public_state(&worker, siblings[second_idx], PublicState::Completed).await;

    await_public_state(&worker, d, PublicState::Waiting).await;

    // ── Step 5: claim + complete D ──
    let task_d = claim_specific(&worker, d).await;
    task_d.complete(Some(b"D-ok".to_vec())).await.expect("complete D");
    await_public_state(&worker, d, PublicState::Completed).await;

    // Explicit teardown — drains engine + background tasks so state
    // does not leak into the next serial readiness test.
    drop(worker);
    server.shutdown().await;

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
                "fanout_b_waiting_after_a": true,
                "fanout_c_waiting_after_a": true,
                "d_blocked_while_only_b_complete": true,
                "d_waiting_after_both_parents": true,
                "terminal_state_all": "Completed",
            },
        }),
    );
}
