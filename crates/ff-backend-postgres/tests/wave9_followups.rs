//! RFC-020 Wave 9 follow-ups (#354, #355, #356) — regression coverage.
//!
//! * #354 is doc-only (RFC-020 §4.1 + exec_core.rs module comment).
//! * #355 — `ff_attempt.outcome` must be cleared on `cancel_flow` member
//!   loop.
//! * #356 — `ff_exec_core.started_at_ms` is set-once on first claim.
//!   Reclaim + retry must not overwrite.

use ff_backend_postgres::PostgresBackend;
use ff_core::backend::{CancelFlowPolicy, CancelFlowWait, CapabilitySet, ClaimPolicy};
use ff_core::contracts::CancelFlowResult;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::types::{ExecutionId, FlowId, LaneId, TimestampMs, WorkerId, WorkerInstanceId};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&bootstrap)
        .await
        .expect("apply_migrations clean");
    bootstrap.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect pool");
    Some(pool)
}

fn split_exec(eid: &ExecutionId) -> (i16, Uuid) {
    let (_, u) = eid.as_str().split_once("}:").unwrap();
    (eid.partition() as i16, Uuid::parse_str(u).unwrap())
}

#[allow(clippy::too_many_arguments)]
async fn insert_exec_core(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    flow_id: Option<Uuid>,
    lane_id: &str,
    lifecycle_phase: &str,
    public_state: &str,
    attempt_index: i32,
    now: i64,
) {
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, flow_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, \
            priority, created_at_ms, raw_fields) \
         VALUES ($1, $2, $3, $4, $5, $6, 'unowned', 'eligible_now', \
                 $7, 'running_attempt', 0, $8, '{}'::jsonb)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(flow_id)
    .bind(lane_id)
    .bind(attempt_index)
    .bind(lifecycle_phase)
    .bind(public_state)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert ff_exec_core");
}

async fn insert_attempt_with_outcome(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    attempt_index: i32,
    outcome: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, lease_epoch, outcome) \
         VALUES ($1, $2, $3, 1, $4)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .bind(outcome)
    .execute(pool)
    .await
    .expect("insert ff_attempt");
}

async fn read_attempt_outcome(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    attempt_index: i32,
) -> Option<String> {
    let row = sqlx::query(
        "SELECT outcome FROM ff_attempt \
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = $3",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_one(pool)
    .await
    .expect("select outcome");
    row.try_get::<Option<String>, _>("outcome").expect("outcome")
}

async fn read_exec_started_at(pool: &PgPool, part: i16, exec_uuid: Uuid) -> Option<i64> {
    let row = sqlx::query(
        "SELECT started_at_ms FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("select started_at_ms");
    row.try_get::<Option<i64>, _>("started_at_ms").expect("col")
}

fn claim_policy_for(worker: &str, ttl_ms: u32) -> ClaimPolicy {
    ClaimPolicy::new(
        WorkerId::new(worker),
        WorkerInstanceId::new(format!("{worker}-1")),
        ttl_ms,
        Some(Duration::from_millis(100)),
    )
}

// ── #355 — cancel_flow clears member attempt outcome ─────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_flow_clears_member_attempt_outcome() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    // Seed a flow + member execution. `flow_partition` and
    // `ExecutionId::solo` hash independently; the cancel_flow member
    // scan is partition-local against the flow's partition, so seed
    // the member onto the flow's partition_key directly.
    let flow_id = FlowId::new();
    let flow_uuid = flow_id.0;
    let part = flow_partition(&flow_id, &PartitionConfig::default()).index as i16;
    let now = TimestampMs::now().0;

    sqlx::query(
        "INSERT INTO ff_flow_core \
           (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms) \
         VALUES ($1, $2, 0, 'open', $3)",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert ff_flow_core");

    // Mint a fresh UUID for the member, co-partitioned with the flow.
    let m_uuid = Uuid::new_v4();
    insert_exec_core(
        &pool,
        part,
        m_uuid,
        Some(flow_uuid),
        "default",
        "runnable",
        "waiting",
        0,
        now,
    )
    .await;
    insert_attempt_with_outcome(&pool, part, m_uuid, 0, Some("retry")).await;

    // Pre: stale outcome observable.
    assert_eq!(
        read_attempt_outcome(&pool, part, m_uuid, 0).await.as_deref(),
        Some("retry"),
        "pre-cancel: stale retry outcome seeded"
    );

    let r = backend
        .cancel_flow(&flow_id, CancelFlowPolicy::CancelAll, CancelFlowWait::NoWait)
        .await
        .expect("cancel_flow ok");
    assert!(
        matches!(r, CancelFlowResult::Cancelled { .. }),
        "got {r:?}"
    );

    // Post: outcome nulled (#355).
    assert_eq!(
        read_attempt_outcome(&pool, part, m_uuid, 0).await,
        None,
        "post-cancel: attempt.outcome must be NULL (#355 regression)"
    );
}

// ── #356 — started_at_ms set-once on ff_exec_core ───────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn first_claim_populates_started_at_ms_set_once() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    let lane = LaneId::new(format!("wave9-followup-356-{}", Uuid::new_v4()).as_str());
    let eid = ExecutionId::solo(&lane, &PartitionConfig::default());
    let (part, exec_uuid) = split_exec(&eid);
    let now = TimestampMs::now().0;

    insert_exec_core(
        &pool,
        part,
        exec_uuid,
        None,
        lane.as_str(),
        "runnable",
        "waiting",
        0,
        now,
    )
    .await;

    // Pre: column NULL.
    assert_eq!(read_exec_started_at(&pool, part, exec_uuid).await, None);

    let _h1 = backend
        .claim(
            &lane,
            &CapabilitySet::default(),
            claim_policy_for("w356", 60_000),
        )
        .await
        .expect("first claim ok")
        .expect("first claim Some");

    let first_val = read_exec_started_at(&pool, part, exec_uuid).await;
    assert!(
        first_val.is_some(),
        "first claim must populate ff_exec_core.started_at_ms"
    );
    let first_ts = first_val.unwrap();

    // Force exec back to runnable/eligible + bump attempt_index so the
    // scanner reselects it. Second claim must NOT overwrite started_at_ms.
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', ownership_state='unowned', \
            eligibility_state='eligible_now', attempt_index = attempt_index + 1 \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .expect("reset to runnable for retry-claim");

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let _h2 = backend
        .claim(
            &lane,
            &CapabilitySet::default(),
            claim_policy_for("w356", 60_000),
        )
        .await
        .expect("second claim ok")
        .expect("second claim Some");

    let second_val = read_exec_started_at(&pool, part, exec_uuid)
        .await
        .expect("second claim leaves column populated");
    assert_eq!(
        second_val, first_ts,
        "set-once: reclaim/retry must not overwrite started_at_ms (#356)"
    );
}
