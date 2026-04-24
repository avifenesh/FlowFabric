//! Wave 5a — Postgres cascade dispatch (`dispatch_dependency_resolution`).
//!
//! Covers the per-hop-tx cascade adjudicated in K-2 of the RFC-v0.7
//! migration-master round-2 debate:
//!
//! 1. Cascade happy-path — 3-member AllOf flow, each completes,
//!    downstream transitions to eligible after all 3.
//! 2. AnyOf + CancelRemaining — first completes, downstream
//!    eligible + `ff_pending_cancel_groups` gets bookkeeping rows.
//! 3. Quorum(2) + LetRun — 3 upstream, 2 complete, downstream
//!    eligible and pending_cancel_groups stays empty.
//! 4. Short-circuit skip — Quorum(3) of 5 with 3 failures,
//!    downstream transitions to skipped + emits its own completion
//!    event for cascade.
//! 5. Replay idempotency — dispatch same event_id twice, second is
//!    NoOp.
//! 6. Retry exhaustion — synthetic poison makes
//!    advance_edge_group_with_retry return `RetryExhausted`.
//! 7. Large-fanout n=50 — cascade across 50 edges, `pg_locks`
//!    snapshot at mid-cascade shows no cross-hop row lock hold.
//!
//! # Running
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://postgres:postgres@localhost/ff_wave5a_test \
//!   cargo test -p ff-test --test pg_dispatch_dependency -- --ignored --test-threads=1
//! ```

use ff_backend_postgres::{apply_migrations, dispatch};
use ff_core::engine_error::{ContentionKind, EngineError};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Partition byte shared by every fixture — lets us reason about a
/// single partition_key for the whole test.
const P: i16 = 0;

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    apply_migrations(&pool).await.expect("apply_migrations");
    // Clean slate between runs.
    for stmt in [
        "DELETE FROM ff_pending_cancel_groups",
        "DELETE FROM ff_completion_event",
        "DELETE FROM ff_edge_group",
        "DELETE FROM ff_edge",
        "DELETE FROM ff_exec_core",
        "DELETE FROM ff_flow_core",
    ] {
        sqlx::query(stmt).execute(&pool).await.expect("reset");
    }
    Some(pool)
}

/// Seed one downstream exec in `blocked` + one `ff_flow_core` + N
/// upstream execs + N `ff_edge` rows + one `ff_edge_group` with the
/// supplied policy json.
///
/// Returns `(flow_id, downstream_eid, upstream_eids)`.
async fn seed_flow(
    pool: &PgPool,
    fanout: usize,
    policy_json: serde_json::Value,
    k_target: i32,
) -> (Uuid, Uuid, Vec<Uuid>) {
    let flow_id = Uuid::new_v4();
    let downstream = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_flow_core (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms) \
         VALUES ($1, $2, 0, 'running', 0)",
    )
    .bind(P)
    .bind(flow_id)
    .execute(pool)
    .await
    .expect("seed flow");

    // Downstream exec in blocked.
    sqlx::query(
        "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, \
         lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, \
         created_at_ms) \
         VALUES ($1, $2, $3, 'default', 'blocked', 'unowned', 'blocked', 'pending', 'pending', 0)",
    )
    .bind(P)
    .bind(downstream)
    .bind(flow_id)
    .execute(pool)
    .await
    .expect("seed downstream");

    let mut upstreams = Vec::with_capacity(fanout);
    for _ in 0..fanout {
        let u = Uuid::new_v4();
        upstreams.push(u);
        // Upstream exec in active-ish — we don't transition it; we
        // just emit completion events.
        sqlx::query(
            "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, \
             lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, \
             created_at_ms) \
             VALUES ($1, $2, $3, 'default', 'active', 'leased', 'not_applicable', 'running', 'running', 0)",
        )
        .bind(P)
        .bind(u)
        .bind(flow_id)
        .execute(pool)
        .await
        .expect("seed upstream");

        // Edge upstream -> downstream.
        sqlx::query(
            "INSERT INTO ff_edge (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
             VALUES ($1, $2, $3, $4, $5, '{}'::jsonb)",
        )
        .bind(P)
        .bind(flow_id)
        .bind(Uuid::new_v4())
        .bind(u)
        .bind(downstream)
        .execute(pool)
        .await
        .expect("seed edge");
    }

    // Edge-group row for the downstream.
    sqlx::query(
        "INSERT INTO ff_edge_group (partition_key, flow_id, downstream_eid, policy, k_target, \
         success_count, fail_count, skip_count, running_count) \
         VALUES ($1, $2, $3, $4, $5, 0, 0, 0, $6)",
    )
    .bind(P)
    .bind(flow_id)
    .bind(downstream)
    .bind(&policy_json)
    .bind(k_target)
    .bind(fanout as i32)
    .execute(pool)
    .await
    .expect("seed edge_group");

    (flow_id, downstream, upstreams)
}

/// Emit one completion event for `upstream` with outcome, return the
/// new `event_id`.
async fn emit(pool: &PgPool, upstream: Uuid, flow_id: Uuid, outcome: &str) -> i64 {
    let row = sqlx::query(
        "INSERT INTO ff_completion_event (partition_key, execution_id, flow_id, outcome, occurred_at_ms) \
         VALUES ($1, $2, $3, $4, 0) RETURNING event_id",
    )
    .bind(P)
    .bind(upstream)
    .bind(flow_id)
    .bind(outcome)
    .fetch_one(pool)
    .await
    .expect("emit event");
    row.get::<i64, _>("event_id")
}

async fn exec_eligibility(pool: &PgPool, exec: Uuid) -> String {
    let row = sqlx::query("SELECT eligibility_state FROM ff_exec_core WHERE execution_id = $1")
        .bind(exec)
        .fetch_one(pool)
        .await
        .expect("read exec");
    row.get("eligibility_state")
}

async fn exec_public_state(pool: &PgPool, exec: Uuid) -> String {
    let row = sqlx::query("SELECT public_state FROM ff_exec_core WHERE execution_id = $1")
        .bind(exec)
        .fetch_one(pool)
        .await
        .expect("read exec");
    row.get("public_state")
}

async fn pending_cancel_rows(pool: &PgPool, flow_id: Uuid) -> i64 {
    let row = sqlx::query("SELECT COUNT(*)::bigint AS c FROM ff_pending_cancel_groups WHERE flow_id = $1")
        .bind(flow_id)
        .fetch_one(pool)
        .await
        .expect("count pending_cancel");
    row.get("c")
}

// ── Test 1: Happy path — AllOf cascade ───────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cascade_all_of_happy_path() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, upstreams) = seed_flow(
        &pool,
        3,
        serde_json::json!({"kind": "all_of"}),
        3,
    )
    .await;

    for (i, u) in upstreams.iter().enumerate() {
        let ev = emit(&pool, *u, flow, "success").await;
        let out = dispatch::dispatch_completion(&pool, ev).await.expect("dispatch ok");
        if i < 2 {
            // Not yet satisfied.
            assert_eq!(exec_eligibility(&pool, downstream).await, "blocked");
        }
        let _ = out;
    }
    // After the third success, downstream flips.
    assert_eq!(exec_eligibility(&pool, downstream).await, "eligible_now");
    assert_eq!(pending_cancel_rows(&pool, flow).await, 0);
}

// ── Test 2: AnyOf CancelRemaining ────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cascade_any_of_cancel_remaining() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, upstreams) = seed_flow(
        &pool,
        3,
        serde_json::json!({"kind": "any_of", "on_satisfied": "cancel_remaining"}),
        1,
    )
    .await;

    let ev = emit(&pool, upstreams[0], flow, "success").await;
    dispatch::dispatch_completion(&pool, ev).await.expect("dispatch ok");

    assert_eq!(exec_eligibility(&pool, downstream).await, "eligible_now");
    // Bookkeeping row inserted (one per group; Stage-C dispatcher
    // expands into per-sibling rows).
    assert_eq!(pending_cancel_rows(&pool, flow).await, 1);
}

// ── Test 3: Quorum(2) + LetRun ───────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cascade_quorum_let_run() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, upstreams) = seed_flow(
        &pool,
        3,
        serde_json::json!({"kind": "quorum", "k": 2, "on_satisfied": "let_run"}),
        2,
    )
    .await;

    let ev = emit(&pool, upstreams[0], flow, "success").await;
    dispatch::dispatch_completion(&pool, ev).await.expect("dispatch ok");
    // 1/2 — still pending.
    assert_eq!(exec_eligibility(&pool, downstream).await, "blocked");

    let ev = emit(&pool, upstreams[1], flow, "success").await;
    dispatch::dispatch_completion(&pool, ev).await.expect("dispatch ok");
    // Satisfied, LetRun → no pending_cancel rows.
    assert_eq!(exec_eligibility(&pool, downstream).await, "eligible_now");
    assert_eq!(pending_cancel_rows(&pool, flow).await, 0);
}

// ── Test 4: Short-circuit skip ───────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cascade_quorum_impossible_skips() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, upstreams) = seed_flow(
        &pool,
        5,
        serde_json::json!({"kind": "quorum", "k": 3, "on_satisfied": "cancel_remaining"}),
        3,
    )
    .await;

    // Three failures — headroom = 5 - 3 = 2 < k=3 → impossible.
    for u in upstreams.iter().take(3) {
        let ev = emit(&pool, *u, flow, "failed").await;
        dispatch::dispatch_completion(&pool, ev).await.expect("dispatch ok");
    }
    assert_eq!(exec_public_state(&pool, downstream).await, "skipped");

    // And a cascade completion event was emitted for the downstream.
    let count: i64 = sqlx::query("SELECT COUNT(*)::bigint AS c FROM ff_completion_event WHERE execution_id = $1 AND outcome = 'skipped'")
        .bind(downstream)
        .fetch_one(&pool)
        .await
        .expect("count")
        .get("c");
    assert_eq!(count, 1, "skip event must be emitted for cascade");
}

// ── Test 5: Replay idempotency ───────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn replay_is_noop() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, _downstream, upstreams) = seed_flow(
        &pool,
        1,
        serde_json::json!({"kind": "all_of"}),
        1,
    )
    .await;
    let ev = emit(&pool, upstreams[0], flow, "success").await;

    let first = dispatch::dispatch_completion(&pool, ev).await.expect("first dispatch");
    let second = dispatch::dispatch_completion(&pool, ev).await.expect("second dispatch");

    assert!(matches!(first, dispatch::DispatchOutcome::Advanced(1)));
    assert_eq!(second, dispatch::DispatchOutcome::NoOp);
}

// ── Test 6: Retry exhaustion ─────────────────────────────────────────
//
// We synthesize the contention by taking an exclusive row lock on
// the edge_group row from a separate transaction and holding it
// past the dispatcher's 3-attempt + 5ms/15ms/... backoff budget. The
// dispatcher's per-hop SERIALIZABLE tx cannot acquire `FOR UPDATE`
// and bubbles a timeout → `LeaseConflict` → retry → exhausted.

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn retry_exhaustion_returns_contention() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, upstreams) = seed_flow(
        &pool,
        1,
        serde_json::json!({"kind": "all_of"}),
        1,
    )
    .await;

    // Park a competing tx that holds the edge_group row FOR UPDATE
    // and sets a tight lock_timeout to force the dispatcher to error
    // on its own `FOR UPDATE` attempt. We use a second pool so we
    // can safely move it into a task.
    let blocker_pool = pool.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let (blocker_ready_tx, blocker_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let blocker = tokio::spawn(async move {
        let mut tx = blocker_pool.begin().await.expect("begin blocker");
        sqlx::query(
            "SELECT 1 FROM ff_edge_group \
             WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3 \
             FOR UPDATE",
        )
        .bind(P)
        .bind(flow)
        .bind(downstream)
        .execute(&mut *tx)
        .await
        .expect("blocker acquires FOR UPDATE");
        blocker_ready_tx.send(()).ok();
        // Hold until the dispatcher has exhausted its retries.
        done_rx.await.ok();
        tx.commit().await.ok();
    });

    blocker_ready_rx.await.expect("blocker ready");

    // Set a per-session lock_timeout so our dispatcher's FOR UPDATE
    // errors quickly instead of blocking forever.
    sqlx::query("ALTER DATABASE ff_wave5a_test SET lock_timeout = '50ms'")
        .execute(&pool)
        .await
        .ok(); // ignore error when user lacks privilege; the retry will still fire on serialization-conflict simulation below.

    // Force lock_timeout at the session level by opening a fresh
    // pool with the timeout set via runtime `SET`.
    let url = std::env::var("FF_PG_TEST_URL").expect("FF_PG_TEST_URL");
    let pool_timeout = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET lock_timeout = '50ms'")
                    .execute(&mut *conn)
                    .await
                    .map(|_| ())
            })
        })
        .connect(&url)
        .await
        .expect("connect timeout pool");

    let ev = emit(&pool, upstreams[0], flow, "success").await;
    let result = dispatch::dispatch_completion(&pool_timeout, ev).await;

    done_tx.send(()).ok();
    blocker.await.ok();

    match result {
        Err(EngineError::Contention(ContentionKind::RetryExhausted)) => {}
        other => panic!("expected Contention(RetryExhausted), got {other:?}"),
    }
}

// ── Test 7: Large fanout + pg_locks invariant ────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn large_fanout_per_hop_tx_releases_locks() {
    let Some(pool) = setup_or_skip().await else { return };

    // 50 independent (flow, downstream) pairs, one upstream each,
    // under AllOf(k=1). We cascade sequentially and sample pg_locks
    // mid-way to prove no cross-hop lock retention.
    let n = 50usize;
    let mut flows: Vec<(Uuid, Uuid, Vec<Uuid>)> = Vec::with_capacity(n);
    for _ in 0..n {
        flows.push(
            seed_flow(&pool, 1, serde_json::json!({"kind": "all_of"}), 1).await,
        );
    }

    let mut peak_holds: i64 = 0;
    for (i, (flow, _downstream, upstreams)) in flows.iter().enumerate() {
        let ev = emit(&pool, upstreams[0], *flow, "success").await;
        dispatch::dispatch_completion(&pool, ev).await.expect("dispatch");

        // Every 10 hops, sample active row-exclusive locks held by
        // any pid on `ff_edge_group_p*`. After a hop commits the
        // lock should be released; we assert the snapshot stays
        // small (≤ 1 — transient readers only).
        if i % 10 == 9 {
            let held: i64 = sqlx::query(
                "SELECT COUNT(*)::bigint AS c \
                 FROM pg_locks l \
                 JOIN pg_class c ON c.oid = l.relation \
                 WHERE c.relname LIKE 'ff_edge_group%' AND l.mode = 'RowExclusiveLock' \
                 AND l.granted",
            )
            .fetch_one(&pool)
            .await
            .expect("pg_locks")
            .get("c");
            peak_holds = peak_holds.max(held);
        }
    }

    // All 50 downstreams should be eligible.
    for (_, downstream, _) in &flows {
        assert_eq!(exec_eligibility(&pool, *downstream).await, "eligible_now");
    }
    // Per-hop-tx guarantee: no RowExclusiveLock on ff_edge_group
    // ever lingers past the single-hop tx boundary. Between hops we
    // sample and expect zero pinned row-exclusive locks held by our
    // dispatcher.
    assert!(
        peak_holds <= 1,
        "per-hop-tx invariant broken: peak_holds={peak_holds}"
    );
}

// ── Test 8: AnyOf + CancelRemaining populates pending_members ────────

/// Regression for the Wave 4i + Wave 6b finding: when the policy flips
/// `cancel_siblings_pending_flag = true`, `advance_edge_group` MUST
/// also write the list of sibling upstream execution_ids into
/// `cancel_siblings_pending_members` — the Wave 6b dispatcher reads
/// that column to drive the actual per-sibling cancel. Prior to the
/// fix the flag was set but members stayed `'{}'::text[]`.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn advance_edge_group_any_of_populates_cancel_members() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, upstreams) = seed_flow(
        &pool,
        3,
        serde_json::json!({"kind": "any_of", "on_satisfied": "cancel_remaining"}),
        1,
    )
    .await;

    // Winner = upstreams[0]; siblings[1..] should still be active
    // (we only emit the winner's completion event).
    let ev = emit(&pool, upstreams[0], flow, "success").await;
    dispatch::dispatch_completion(&pool, ev)
        .await
        .expect("dispatch ok");

    // Downstream flips eligible.
    assert_eq!(exec_eligibility(&pool, downstream).await, "eligible_now");
    // Pending-cancel bookkeeping row inserted.
    assert_eq!(pending_cancel_rows(&pool, flow).await, 1);

    // The flag is set + members list contains exactly the 2 non-winner
    // upstream execution_ids.
    let row = sqlx::query(
        "SELECT cancel_siblings_pending_flag, cancel_siblings_pending_members \
         FROM ff_edge_group \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(P)
    .bind(flow)
    .bind(downstream)
    .fetch_one(&pool)
    .await
    .expect("read edge_group");
    let flag: bool = row.get("cancel_siblings_pending_flag");
    let members: Vec<String> = row.get("cancel_siblings_pending_members");
    assert!(flag, "pending flag not set");
    assert_eq!(members.len(), 2, "expected 2 siblings; got {members:?}");
    let mut got: Vec<String> = members;
    got.sort();
    let mut want: Vec<String> = upstreams[1..].iter().map(|u| u.to_string()).collect();
    want.sort();
    assert_eq!(got, want, "sibling members mismatch");
}
