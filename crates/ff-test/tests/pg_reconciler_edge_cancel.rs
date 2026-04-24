//! Wave 6b — Postgres sibling-cancel dispatcher + reconciler
//! (RFC-016 Stages C+D).
//!
//! 1. Dispatcher drains flag=true + members=[a,b] → both siblings
//!    transition to terminal/cancelled, edge_group flag cleared,
//!    pending row deleted.
//! 2. Reconciler `sremmed_stale`: pending row + flag=false → pending
//!    row deleted, edge_group untouched.
//! 3. Reconciler `completed_drain`: flag=true, members=[a,b] already
//!    terminal → flag cleared + pending row deleted.
//! 4. Reconciler `no_op`: flag=true, at least one sibling still
//!    non-terminal → reconciler leaves everything untouched.
//! 5. Concurrent-drain coalescing: two `dispatcher_tick` calls race
//!    on the same group. `FOR UPDATE SKIP LOCKED` gives one winner;
//!    the other sees `skipped_locked`. Siblings cancelled exactly
//!    once.
//!
//! # Running
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://postgres:postgres@localhost/ff_wave6b_test \
//!   cargo test -p ff-test --test pg_reconciler_edge_cancel \
//!   -- --ignored --test-threads=1
//! ```

use ff_backend_postgres::{
    apply_migrations,
    reconcilers::{edge_cancel_dispatcher, edge_cancel_reconciler},
};
use ff_core::backend::ScannerFilter;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use uuid::Uuid;

const P: i16 = 0;

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    apply_migrations(&pool).await.expect("apply_migrations");
    for stmt in [
        "DELETE FROM ff_pending_cancel_groups",
        "DELETE FROM ff_edge_group",
        "DELETE FROM ff_exec_core",
        "DELETE FROM ff_flow_core",
    ] {
        sqlx::query(stmt).execute(&pool).await.expect("reset");
    }
    Some(pool)
}

/// Seed `ff_flow_core` + N sibling exec_core rows (in `active`) +
/// one edge_group with the provided flag/members set.
async fn seed_group(
    pool: &PgPool,
    sibling_states: &[&str], // lifecycle_phase per sibling
    flag: bool,
    enqueue_pending: bool,
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

    // Downstream exec (target of the group) — irrelevant for
    // dispatcher behaviour but seeded for FK-parity with production.
    sqlx::query(
        "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, \
         lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, \
         created_at_ms) \
         VALUES ($1, $2, $3, 'default', 'blocked', 'unowned', 'eligible_now', 'pending', 'pending', 0)",
    )
    .bind(P)
    .bind(downstream)
    .bind(flow_id)
    .execute(pool)
    .await
    .expect("seed downstream");

    let mut siblings = Vec::with_capacity(sibling_states.len());
    for state in sibling_states {
        let sid = Uuid::new_v4();
        siblings.push(sid);
        let (eligibility, public, attempt) = if *state == "terminal" {
            ("not_applicable", "completed", "completed")
        } else {
            ("not_applicable", "running", "running")
        };
        sqlx::query(
            "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, \
             lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, \
             created_at_ms) \
             VALUES ($1, $2, $3, 'default', $4, 'leased', $5, $6, $7, 0)",
        )
        .bind(P)
        .bind(sid)
        .bind(flow_id)
        .bind(state)
        .bind(eligibility)
        .bind(public)
        .bind(attempt)
        .execute(pool)
        .await
        .expect("seed sibling");
    }

    let member_strs: Vec<String> = siblings.iter().map(|u| u.to_string()).collect();
    sqlx::query(
        "INSERT INTO ff_edge_group (partition_key, flow_id, downstream_eid, policy, \
         k_target, success_count, fail_count, skip_count, running_count, \
         cancel_siblings_pending_flag, cancel_siblings_pending_members) \
         VALUES ($1, $2, $3, '{}'::jsonb, 1, 0, 0, 0, $4, $5, $6)",
    )
    .bind(P)
    .bind(flow_id)
    .bind(downstream)
    .bind(siblings.len() as i32)
    .bind(flag)
    .bind(&member_strs)
    .execute(pool)
    .await
    .expect("seed edge_group");

    if enqueue_pending {
        sqlx::query(
            "INSERT INTO ff_pending_cancel_groups \
             (partition_key, flow_id, downstream_eid, enqueued_at_ms) \
             VALUES ($1, $2, $3, 0)",
        )
        .bind(P)
        .bind(flow_id)
        .bind(downstream)
        .execute(pool)
        .await
        .expect("seed pending");
    }

    (flow_id, downstream, siblings)
}

async fn exec_phase(pool: &PgPool, eid: Uuid) -> String {
    let row = sqlx::query("SELECT lifecycle_phase FROM ff_exec_core WHERE execution_id = $1")
        .bind(eid)
        .fetch_one(pool)
        .await
        .expect("read exec");
    row.get("lifecycle_phase")
}

async fn exec_public(pool: &PgPool, eid: Uuid) -> String {
    let row = sqlx::query("SELECT public_state FROM ff_exec_core WHERE execution_id = $1")
        .bind(eid)
        .fetch_one(pool)
        .await
        .expect("read exec");
    row.get("public_state")
}

async fn group_flag(pool: &PgPool, flow: Uuid, downstream: Uuid) -> bool {
    let row = sqlx::query(
        "SELECT cancel_siblings_pending_flag FROM ff_edge_group \
         WHERE flow_id = $1 AND downstream_eid = $2",
    )
    .bind(flow)
    .bind(downstream)
    .fetch_one(pool)
    .await
    .expect("read group");
    row.get("cancel_siblings_pending_flag")
}

async fn pending_count(pool: &PgPool, flow: Uuid) -> i64 {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS c FROM ff_pending_cancel_groups WHERE flow_id = $1",
    )
    .bind(flow)
    .fetch_one(pool)
    .await
    .expect("count pending");
    row.get("c")
}

// ── Test 1: Dispatcher drains flag=true group ────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn dispatcher_drains_flag_true_group() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, siblings) =
        seed_group(&pool, &["active", "active"], true, true).await;

    let report = edge_cancel_dispatcher::dispatcher_tick(&pool, &ScannerFilter::default())
        .await
        .expect("dispatcher_tick");

    assert_eq!(report.dispatched, 1);
    assert_eq!(report.orphaned, 0);
    assert_eq!(report.errors, 0);

    for sib in &siblings {
        assert_eq!(exec_phase(&pool, *sib).await, "terminal");
        assert_eq!(exec_public(&pool, *sib).await, "cancelled");
    }
    assert!(!group_flag(&pool, flow, downstream).await);
    assert_eq!(pending_count(&pool, flow).await, 0);
}

// ── Test 2: Reconciler `sremmed_stale` ───────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn reconciler_sremmed_stale() {
    let Some(pool) = setup_or_skip().await else { return };
    // flag=false but pending row lingers.
    let (flow, downstream, siblings) =
        seed_group(&pool, &["active", "active"], false, true).await;

    let report = edge_cancel_reconciler::reconciler_tick(&pool, &ScannerFilter::default())
        .await
        .expect("reconciler_tick");

    assert_eq!(report.sremmed_stale, 1);
    assert_eq!(report.completed_drain, 0);
    assert_eq!(report.no_op, 0);

    assert_eq!(pending_count(&pool, flow).await, 0);
    // edge_group untouched, siblings untouched.
    for sib in &siblings {
        assert_eq!(exec_phase(&pool, *sib).await, "active");
    }
    assert!(!group_flag(&pool, flow, downstream).await);
}

// ── Test 3: Reconciler `completed_drain` ─────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn reconciler_completed_drain() {
    let Some(pool) = setup_or_skip().await else { return };
    // flag=true but siblings already terminal — mid-crash residue.
    let (flow, downstream, _siblings) =
        seed_group(&pool, &["terminal", "terminal"], true, true).await;

    let report = edge_cancel_reconciler::reconciler_tick(&pool, &ScannerFilter::default())
        .await
        .expect("reconciler_tick");

    assert_eq!(report.completed_drain, 1);
    assert_eq!(report.sremmed_stale, 0);
    assert_eq!(report.no_op, 0);

    assert!(!group_flag(&pool, flow, downstream).await);
    assert_eq!(pending_count(&pool, flow).await, 0);
}

// ── Test 4: Reconciler `no_op` ───────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn reconciler_no_op_dispatcher_owns() {
    let Some(pool) = setup_or_skip().await else { return };
    // flag=true, one sibling still active → dispatcher territory.
    let (flow, downstream, siblings) =
        seed_group(&pool, &["terminal", "active"], true, true).await;

    let report = edge_cancel_reconciler::reconciler_tick(&pool, &ScannerFilter::default())
        .await
        .expect("reconciler_tick");

    assert_eq!(report.no_op, 1);
    assert_eq!(report.completed_drain, 0);
    assert_eq!(report.sremmed_stale, 0);

    // Everything untouched — dispatcher will drain next tick.
    assert!(group_flag(&pool, flow, downstream).await);
    assert_eq!(pending_count(&pool, flow).await, 1);
    assert_eq!(exec_phase(&pool, siblings[1]).await, "active");
}

// ── Test 5: Concurrent-drain coalescing ──────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn concurrent_dispatcher_coalesces() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, siblings) =
        seed_group(&pool, &["active", "active"], true, true).await;

    // Hold a tx that acquires the edge_group row lock so the first
    // dispatcher_tick is forced onto the SKIP LOCKED path. Then
    // release the lock and run a second tick — it should drain.
    let mut blocker = pool.begin().await.expect("begin blocker");
    sqlx::query(
        "SELECT cancel_siblings_pending_flag FROM ff_edge_group \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3 FOR UPDATE",
    )
    .bind(P)
    .bind(flow)
    .bind(downstream)
    .fetch_one(&mut *blocker)
    .await
    .expect("acquire blocker lock");

    let first = edge_cancel_dispatcher::dispatcher_tick(&pool, &ScannerFilter::default())
        .await
        .expect("first tick");
    assert_eq!(first.skipped_locked, 1, "first tick hits SKIP LOCKED");
    assert_eq!(first.dispatched, 0);

    // Siblings must still be active — no cancels while locked.
    for sib in &siblings {
        assert_eq!(exec_phase(&pool, *sib).await, "active");
    }

    // Release the blocker.
    blocker.rollback().await.expect("release blocker");

    let second = edge_cancel_dispatcher::dispatcher_tick(&pool, &ScannerFilter::default())
        .await
        .expect("second tick");
    assert_eq!(second.dispatched, 1);
    assert_eq!(second.skipped_locked, 0);

    // Post-drain state.
    for sib in &siblings {
        assert_eq!(exec_phase(&pool, *sib).await, "terminal");
        assert_eq!(exec_public(&pool, *sib).await, "cancelled");
    }
    assert!(!group_flag(&pool, flow, downstream).await);
    assert_eq!(pending_count(&pool, flow).await, 0);
}
