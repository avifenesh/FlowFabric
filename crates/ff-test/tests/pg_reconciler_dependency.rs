//! Wave 6a — Postgres dependency reconciler integration tests.
//!
//! The reconciler is the backstop for the per-hop-tx cascade in
//! `dispatch::dispatch_completion`. It scans `ff_completion_event`
//! for rows whose `dispatched_at_ms IS NULL` and whose
//! `committed_at_ms` is older than the stale threshold, and
//! re-invokes the dispatcher (idempotent via the claim UPDATE).
//!
//! Tests:
//!
//! 1. `normal_sweep_reconciles_stale_event`
//! 2. `crash_mid_cascade_two_hops`
//! 3. `reconciler_idempotent_on_second_tick`
//! 4. `scanner_filter_excludes_nonmatching_namespace`
//! 5. `transitive_three_hop_sweep`
//!
//! # Running
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://postgres:postgres@localhost/ff_wave6a_test \
//!   cargo test -p ff-test --test pg_reconciler_dependency -- --ignored --test-threads=1
//! ```

use ff_backend_postgres::reconcilers::dependency::{reconcile_tick, ReconcileReport};
use ff_backend_postgres::{apply_migrations, dispatch};
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

async fn insert_flow(pool: &PgPool) -> Uuid {
    let flow_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_flow_core (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms) \
         VALUES ($1, $2, 0, 'running', 0)",
    )
    .bind(P)
    .bind(flow_id)
    .execute(pool)
    .await
    .expect("flow");
    flow_id
}

async fn insert_exec_with_id(
    pool: &PgPool,
    eid: Uuid,
    flow_id: Uuid,
    lifecycle: &str,
    eligibility: &str,
    public: &str,
    attempt: &str,
) {
    sqlx::query(
        "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, \
         lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, \
         created_at_ms) \
         VALUES ($1, $2, $3, 'default', $4, 'unowned', $5, $6, $7, 0)",
    )
    .bind(P)
    .bind(eid)
    .bind(flow_id)
    .bind(lifecycle)
    .bind(eligibility)
    .bind(public)
    .bind(attempt)
    .execute(pool)
    .await
    .expect("exec");
}

async fn insert_edge(pool: &PgPool, flow_id: Uuid, upstream: Uuid, downstream: Uuid) {
    sqlx::query(
        "INSERT INTO ff_edge (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb)",
    )
    .bind(P)
    .bind(flow_id)
    .bind(Uuid::new_v4())
    .bind(upstream)
    .bind(downstream)
    .execute(pool)
    .await
    .expect("edge");
}

async fn insert_edge_group(
    pool: &PgPool,
    flow_id: Uuid,
    downstream: Uuid,
    fanout: i32,
    policy: serde_json::Value,
    k_target: i32,
) {
    sqlx::query(
        "INSERT INTO ff_edge_group (partition_key, flow_id, downstream_eid, policy, k_target, \
         success_count, fail_count, skip_count, running_count) \
         VALUES ($1, $2, $3, $4, $5, 0, 0, 0, $6)",
    )
    .bind(P)
    .bind(flow_id)
    .bind(downstream)
    .bind(&policy)
    .bind(k_target)
    .bind(fanout)
    .execute(pool)
    .await
    .expect("edge_group");
}

async fn emit_stale(
    pool: &PgPool,
    upstream: Uuid,
    flow_id: Option<Uuid>,
    outcome: &str,
    committed_at_ms: i64,
    namespace: Option<&str>,
) -> i64 {
    let row = sqlx::query(
        "INSERT INTO ff_completion_event \
             (partition_key, execution_id, flow_id, outcome, namespace, \
              occurred_at_ms, committed_at_ms) \
         VALUES ($1, $2, $3, $4, $5, 0, $6) RETURNING event_id",
    )
    .bind(P)
    .bind(upstream)
    .bind(flow_id)
    .bind(outcome)
    .bind(namespace)
    .bind(committed_at_ms)
    .fetch_one(pool)
    .await
    .expect("emit");
    row.get("event_id")
}

async fn dispatched_at(pool: &PgPool, event_id: i64) -> Option<i64> {
    let row = sqlx::query("SELECT dispatched_at_ms FROM ff_completion_event WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(pool)
        .await
        .expect("read");
    row.get("dispatched_at_ms")
}

async fn exec_eligibility(pool: &PgPool, eid: Uuid) -> String {
    let row = sqlx::query("SELECT eligibility_state FROM ff_exec_core WHERE execution_id = $1")
        .bind(eid)
        .fetch_one(pool)
        .await
        .expect("read");
    row.get("eligibility_state")
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX)
}

// ── Test 1: Normal sweep ────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn normal_sweep_reconciles_stale_event() {
    let Some(pool) = setup_or_skip().await else { return };

    let flow = insert_flow(&pool).await;
    let upstream = Uuid::new_v4();
    let downstream = Uuid::new_v4();
    insert_exec_with_id(&pool, upstream, flow, "active", "not_applicable", "running", "running").await;
    insert_exec_with_id(&pool, downstream, flow, "blocked", "blocked", "pending", "pending").await;
    insert_edge(&pool, flow, upstream, downstream).await;
    insert_edge_group(&pool, flow, downstream, 1, serde_json::json!({"kind": "all_of"}), 1).await;

    let tstart = now_ms();
    let ev = emit_stale(&pool, upstream, Some(flow), "success", tstart - 5_000, None).await;
    assert!(dispatched_at(&pool, ev).await.is_none(), "event starts undispatched");

    let report: ReconcileReport = reconcile_tick(&pool, &ScannerFilter::NOOP, 1_000)
        .await
        .expect("reconcile");
    assert_eq!(report.scanned, 1);
    assert_eq!(report.reconciled, 1);
    assert_eq!(report.errors, 0);

    assert!(dispatched_at(&pool, ev).await.is_some(), "claim flipped");
    assert_eq!(exec_eligibility(&pool, downstream).await, "eligible_now");
}

// ── Test 2: Crash mid-cascade (2 hops) ──────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn crash_mid_cascade_two_hops() {
    let Some(pool) = setup_or_skip().await else { return };

    let flow = insert_flow(&pool).await;
    let upstream = Uuid::new_v4();
    let mid = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    insert_exec_with_id(&pool, upstream, flow, "active", "not_applicable", "running", "running").await;
    insert_exec_with_id(&pool, mid, flow, "blocked", "blocked", "pending", "pending").await;
    insert_exec_with_id(&pool, leaf, flow, "blocked", "blocked", "pending", "pending").await;
    insert_edge(&pool, flow, upstream, mid).await;
    insert_edge(&pool, flow, mid, leaf).await;
    insert_edge_group(&pool, flow, mid, 1, serde_json::json!({"kind": "all_of"}), 1).await;
    insert_edge_group(&pool, flow, leaf, 1, serde_json::json!({"kind": "all_of"}), 1).await;

    let tstart = now_ms();
    let ev1 = emit_stale(&pool, upstream, Some(flow), "success", tstart - 5_000, None).await;
    dispatch::dispatch_completion(&pool, ev1).await.expect("hop1");
    assert_eq!(exec_eligibility(&pool, mid).await, "eligible_now");

    // Simulated crash: `mid` completed, its event landed, but the
    // dispatcher never claimed it.
    let ev2 = emit_stale(&pool, mid, Some(flow), "success", tstart - 5_000, None).await;
    assert!(dispatched_at(&pool, ev2).await.is_none());

    let report = reconcile_tick(&pool, &ScannerFilter::NOOP, 1_000)
        .await
        .expect("reconcile");
    assert_eq!(report.reconciled, 1, "reconciler dispatched hop 2");
    assert_eq!(exec_eligibility(&pool, leaf).await, "eligible_now");
}

// ── Test 3: Idempotency ─────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn reconciler_idempotent_on_second_tick() {
    let Some(pool) = setup_or_skip().await else { return };

    let flow = insert_flow(&pool).await;
    let upstream = Uuid::new_v4();
    let downstream = Uuid::new_v4();
    insert_exec_with_id(&pool, upstream, flow, "active", "not_applicable", "running", "running").await;
    insert_exec_with_id(&pool, downstream, flow, "blocked", "blocked", "pending", "pending").await;
    insert_edge(&pool, flow, upstream, downstream).await;
    insert_edge_group(&pool, flow, downstream, 1, serde_json::json!({"kind": "all_of"}), 1).await;

    let tstart = now_ms();
    let _ev = emit_stale(&pool, upstream, Some(flow), "success", tstart - 5_000, None).await;

    let first = reconcile_tick(&pool, &ScannerFilter::NOOP, 1_000).await.expect("t1");
    let second = reconcile_tick(&pool, &ScannerFilter::NOOP, 1_000).await.expect("t2");

    assert_eq!(first.scanned, 1);
    assert_eq!(first.reconciled, 1);
    assert_eq!(second.scanned, 0, "second tick sees nothing to sweep");
    assert_eq!(second.reconciled, 0);
    assert_eq!(second.errors, 0);
}

// ── Test 4: Namespace filter ────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn scanner_filter_excludes_nonmatching_namespace() {
    let Some(pool) = setup_or_skip().await else { return };

    let flow = insert_flow(&pool).await;
    let up_a = Uuid::new_v4();
    let down_a = Uuid::new_v4();
    let up_b = Uuid::new_v4();
    let down_b = Uuid::new_v4();
    for (u, d) in [(up_a, down_a), (up_b, down_b)] {
        insert_exec_with_id(&pool, u, flow, "active", "not_applicable", "running", "running").await;
        insert_exec_with_id(&pool, d, flow, "blocked", "blocked", "pending", "pending").await;
        insert_edge(&pool, flow, u, d).await;
        insert_edge_group(&pool, flow, d, 1, serde_json::json!({"kind": "all_of"}), 1).await;
    }

    let tstart = now_ms();
    let ev_a = emit_stale(&pool, up_a, Some(flow), "success", tstart - 5_000, Some("tenant-a")).await;
    let ev_b = emit_stale(&pool, up_b, Some(flow), "success", tstart - 5_000, Some("tenant-b")).await;

    let filter = ScannerFilter::new().with_namespace("tenant-a");
    let report = reconcile_tick(&pool, &filter, 1_000).await.expect("reconcile");

    assert_eq!(report.scanned, 1);
    assert_eq!(report.reconciled, 1);
    assert!(dispatched_at(&pool, ev_a).await.is_some(), "tenant-a dispatched");
    assert!(dispatched_at(&pool, ev_b).await.is_none(), "tenant-b untouched");
    assert_eq!(exec_eligibility(&pool, down_a).await, "eligible_now");
    assert_eq!(exec_eligibility(&pool, down_b).await, "blocked");
}

// ── Test 5: Transitive 3-hop sweep ──────────────────────────────────────
//
// Linear 3-hop: root → a → b → c. We simulate the scheduler
// completing each middle hop between reconciler ticks; the test
// asserts the reconciler walks the entire transitive set (not just
// 1-hop children of root), consistent with the K-2 round-2 contract.

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn transitive_three_hop_sweep() {
    let Some(pool) = setup_or_skip().await else { return };

    let flow = insert_flow(&pool).await;
    let root = Uuid::new_v4();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    insert_exec_with_id(&pool, root, flow, "active", "not_applicable", "running", "running").await;
    for d in [a, b, c] {
        insert_exec_with_id(&pool, d, flow, "blocked", "blocked", "pending", "pending").await;
    }
    insert_edge(&pool, flow, root, a).await;
    insert_edge(&pool, flow, a, b).await;
    insert_edge(&pool, flow, b, c).await;
    for d in [a, b, c] {
        insert_edge_group(&pool, flow, d, 1, serde_json::json!({"kind": "all_of"}), 1).await;
    }

    let tstart = now_ms();
    let _ev_root = emit_stale(&pool, root, Some(flow), "success", tstart - 5_000, None).await;

    // Threshold=0: any past `committed_at_ms` qualifies. Lets us
    // deterministically test without waiting real wall-clock time.
    let r1 = reconcile_tick(&pool, &ScannerFilter::NOOP, 0).await.expect("t1");
    assert!(r1.reconciled >= 1, "tick 1 reconciled root");
    assert_eq!(exec_eligibility(&pool, a).await, "eligible_now");

    let _ev_a = emit_stale(&pool, a, Some(flow), "success", tstart - 4_000, None).await;
    let r2 = reconcile_tick(&pool, &ScannerFilter::NOOP, 0).await.expect("t2");
    assert!(r2.reconciled >= 1, "tick 2 reconciled a");
    assert_eq!(exec_eligibility(&pool, b).await, "eligible_now");

    let _ev_b = emit_stale(&pool, b, Some(flow), "success", tstart - 3_000, None).await;
    let r3 = reconcile_tick(&pool, &ScannerFilter::NOOP, 0).await.expect("t3");
    assert!(r3.reconciled >= 1, "tick 3 reconciled b");
    assert_eq!(exec_eligibility(&pool, c).await, "eligible_now");
}
