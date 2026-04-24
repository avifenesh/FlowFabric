//! Wave 6d — Postgres completion listener outbox drain.
//!
//! The listener subscribes to the Wave 4h `CompletionBackend` stream,
//! resolves each payload to its `event_id` via the
//! `(execution_id, occurred_at_ms)` index on `ff_completion_event`,
//! and invokes Wave 5a's `dispatch_completion` to run the per-hop-tx
//! cascade.
//!
//! Coverage:
//!
//! 1. Happy path — emit a completion event with a downstream edge,
//!    the listener picks it up, dispatches, downstream flips from
//!    `blocked` to `eligible_now`, and the outbox row is marked
//!    `dispatched_at_ms IS NOT NULL`.
//! 2. Subscriber drop — cancelling the watch fires a clean shutdown
//!    (JoinHandle resolves within 2s).
//! 3. Burst — emit 10 completions in quick succession, listener
//!    dispatches all 10 exactly once (each `dispatched_at_ms` is
//!    stamped; a follow-up call to `dispatch_completion` on the same
//!    event returns `NoOp`).
//!
//! # Running
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://postgres:postgres@localhost/ff_wave6d_test \
//!   cargo test -p ff-test --test pg_completion_listener -- --ignored --test-threads=1
//! ```

use std::sync::Arc;
use std::time::Duration;

use ff_backend_postgres::{apply_migrations, dispatch, PostgresBackend};
use ff_core::completion_backend::CompletionBackend;
use ff_core::partition::PartitionConfig;
use ff_engine::completion_listener::run_completion_listener_postgres;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tokio::sync::watch;
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

/// Seed a minimal AllOf(1) cascade: one flow, one downstream blocked
/// on one upstream edge. Returns (flow_id, downstream_eid, upstream_eid).
async fn seed_simple_flow(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let flow_id = Uuid::new_v4();
    let downstream = Uuid::new_v4();
    let upstream = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_flow_core (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms) \
         VALUES ($1, $2, 0, 'running', 0)",
    )
    .bind(P).bind(flow_id).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, \
         lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, \
         created_at_ms) VALUES ($1, $2, $3, 'default', 'blocked', 'unowned', 'blocked', 'pending', 'pending', 0)",
    )
    .bind(P).bind(downstream).bind(flow_id).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, \
         lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, \
         created_at_ms) VALUES ($1, $2, $3, 'default', 'active', 'leased', 'not_applicable', 'running', 'running', 0)",
    )
    .bind(P).bind(upstream).bind(flow_id).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ff_edge (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb)",
    )
    .bind(P).bind(flow_id).bind(Uuid::new_v4()).bind(upstream).bind(downstream)
    .execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ff_edge_group (partition_key, flow_id, downstream_eid, policy, k_target, \
         success_count, fail_count, skip_count, running_count) \
         VALUES ($1, $2, $3, $4, 1, 0, 0, 0, 1)",
    )
    .bind(P).bind(flow_id).bind(downstream).bind(serde_json::json!({"kind": "all_of"}))
    .execute(pool).await.unwrap();
    (flow_id, downstream, upstream)
}

/// Emit a completion event; the trigger fires NOTIFY on commit.
async fn emit(pool: &PgPool, exec: Uuid, flow: Uuid, outcome: &str, occurred_at_ms: i64) -> i64 {
    let row = sqlx::query(
        "INSERT INTO ff_completion_event (partition_key, execution_id, flow_id, outcome, occurred_at_ms) \
         VALUES ($1, $2, $3, $4, $5) RETURNING event_id",
    )
    .bind(P).bind(exec).bind(flow).bind(outcome).bind(occurred_at_ms)
    .fetch_one(pool).await.expect("emit");
    row.get::<i64, _>("event_id")
}

async fn eligibility(pool: &PgPool, exec: Uuid) -> String {
    let row = sqlx::query("SELECT eligibility_state FROM ff_exec_core WHERE execution_id = $1")
        .bind(exec).fetch_one(pool).await.expect("read");
    row.get("eligibility_state")
}

async fn dispatched_count(pool: &PgPool) -> i64 {
    let row = sqlx::query("SELECT COUNT(*)::bigint AS c FROM ff_completion_event WHERE dispatched_at_ms IS NOT NULL")
        .fetch_one(pool).await.expect("count dispatched");
    row.get("c")
}

// ── Test 1: Happy path ───────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn listener_dispatches_single_completion() {
    let Some(pool) = setup_or_skip().await else { return };
    let (flow, downstream, upstream) = seed_simple_flow(&pool).await;

    let backend: Arc<dyn CompletionBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let pool_clone = pool.clone();
    let handle = tokio::spawn(async move {
        run_completion_listener_postgres(backend, pool_clone, shutdown_rx).await
    });

    // Give the subscriber task time to LISTEN before we emit.
    tokio::time::sleep(Duration::from_millis(150)).await;

    emit(&pool, upstream, flow, "success", 1_700_000_000_000).await;

    // Poll for the downstream flip — cascade should run within a second
    // over loopback Postgres.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if eligibility(&pool, downstream).await == "eligible_now" {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("downstream did not flip within 5s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(dispatched_count(&pool).await, 1);

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await
        .expect("listener shuts down within 2s");
}

// ── Test 2: Subscriber drop / clean shutdown ─────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn listener_shuts_down_on_watch_signal() {
    let Some(pool) = setup_or_skip().await else { return };
    let backend: Arc<dyn CompletionBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let pool_clone = pool.clone();
    let handle = tokio::spawn(async move {
        run_completion_listener_postgres(backend, pool_clone, shutdown_rx).await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = shutdown_tx.send(true);

    let res = tokio::time::timeout(Duration::from_secs(2), handle).await
        .expect("listener joins within 2s")
        .expect("task not cancelled");
    assert!(res.is_ok(), "listener returned Ok on clean shutdown");
}

// ── Test 3: Burst — 10 completions dispatched exactly once ───────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn listener_dispatches_burst_exactly_once() {
    let Some(pool) = setup_or_skip().await else { return };

    // Seed 10 independent no-flow completions. `dispatch_completion`
    // on a flow_id=None row short-circuits to Advanced(0) after flipping
    // `dispatched_at_ms` — perfect for asserting "dispatched exactly
    // once" via the outbox row count without needing 10 flow-graphs.
    let mut execs = Vec::new();
    for _ in 0..10 {
        execs.push(Uuid::new_v4());
    }

    let backend: Arc<dyn CompletionBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let pool_clone = pool.clone();
    let handle = tokio::spawn(async move {
        run_completion_listener_postgres(backend, pool_clone, shutdown_rx).await
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    // Burst-insert 10 completions in a single tx; NOTIFY fires once on
    // commit and the stream drains all 10 via cursor replay.
    let mut tx = pool.begin().await.unwrap();
    let mut event_ids = Vec::new();
    for (i, exec) in execs.iter().enumerate() {
        let row = sqlx::query(
            "INSERT INTO ff_completion_event (partition_key, execution_id, outcome, occurred_at_ms) \
             VALUES ($1, $2, 'success', $3) RETURNING event_id",
        )
        .bind(P).bind(exec).bind(1_700_000_000_000_i64 + i as i64)
        .fetch_one(&mut *tx).await.unwrap();
        event_ids.push(row.get::<i64, _>("event_id"));
    }
    tx.commit().await.unwrap();

    // Wait for all 10 to be dispatched.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if dispatched_count(&pool).await >= 10 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("only {} / 10 dispatched within 10s", dispatched_count(&pool).await);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Exactly-once: re-dispatching any of the claimed event_ids yields
    // NoOp (the `dispatched_at_ms IS NULL` guard holds).
    for ev in &event_ids {
        let out = dispatch::dispatch_completion(&pool, *ev).await.expect("replay");
        assert!(
            matches!(out, dispatch::DispatchOutcome::NoOp),
            "event {ev} should be NoOp on replay, got {:?}", out
        );
    }

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await
        .expect("listener shuts down within 2s");
}
