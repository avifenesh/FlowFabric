//! Cairn #454 Phase 4c — integration test for
//! `EngineBackend::issue_grant_and_claim` on Postgres.
//!
//! Mirrors the Valkey test suite
//! (`crates/ff-backend-valkey/tests/typed_issue_grant_and_claim.rs`,
//! PR #466) with PG-flavored assertions (lease_epoch fence only, no
//! fence triple in `ff_exec_core`).

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::IssueGrantAndClaimArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::partition::PartitionConfig;
use ff_core::types::{ExecutionId, LaneId};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect");
    ff_backend_postgres::apply_migrations(&pool).await.expect("migrate");
    Some(pool)
}

async fn seed_runnable(pool: &PgPool) -> (i16, Uuid, ExecutionId) {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms
        ) VALUES (
            $1, $2, NULL, 'default', '{}', 0,
            'runnable', 'unowned', 'eligible_now',
            'waiting', 'pending_first_attempt',
            0, $3
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed exec");
    (part, exec_uuid, eid)
}

fn args_for(eid: ExecutionId) -> IssueGrantAndClaimArgs {
    IssueGrantAndClaimArgs::new(eid, LaneId::new("default"), 60_000)
}

async fn count_grants(pool: &PgPool, part: i16, exec_uuid: Uuid) -> i64 {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ff_claim_grant \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .unwrap();
    n
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn issue_grant_and_claim_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    let outcome = backend
        .issue_grant_and_claim(args_for(eid))
        .await
        .expect("issue_grant_and_claim ok");
    assert_eq!(outcome.lease_epoch.0, 1, "fresh claim starts at epoch 1");
    assert_eq!(outcome.attempt_index.0, 0, "fresh claim at attempt 0");
    assert!(!outcome.lease_id.to_string().is_empty());

    // exec_core flipped to leased/running.
    let row = sqlx::query(
        "SELECT lifecycle_phase, ownership_state, public_state, attempt_state, attempt_index \
         FROM ff_exec_core WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "active");
    assert_eq!(row.get::<String, _>("ownership_state"), "leased");
    assert_eq!(row.get::<String, _>("public_state"), "running");
    assert_eq!(row.get::<String, _>("attempt_state"), "running_attempt");
    assert_eq!(
        row.get::<i32, _>("attempt_index"),
        1,
        "pointer bumped past the claimed attempt"
    );

    // Grant row cleaned up (DELETE inside same tx).
    assert_eq!(
        count_grants(&pool, part, exec_uuid).await,
        0,
        "grant row must be removed by composition"
    );

    // Attempt row exists at index 0 with matching lease identity.
    let (worker_id, lease_epoch_i): (String, i64) = sqlx::query_as(
        "SELECT worker_id, lease_epoch FROM ff_attempt \
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = 0",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(worker_id, "operator");
    assert_eq!(lease_epoch_i, 1);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn issue_grant_and_claim_rejects_already_leased() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    // Flip the exec to leased externally.
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state = 'leased' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.issue_grant_and_claim(args_for(eid)).await {
        Err(EngineError::Contention(ContentionKind::LeaseConflict)) => {}
        other => panic!("expected LeaseConflict, got {other:?}"),
    }
    // No-dangling-grant invariant on the reject path — tx rollback.
    assert_eq!(
        count_grants(&pool, part, exec_uuid).await,
        0,
        "rejected composition must not leak a grant row"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn issue_grant_and_claim_rejects_missing_execution() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    // Construct a well-formed ExecutionId that points at no row.
    let part: i16 = 0;
    let fake_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{fake_uuid}")).unwrap();
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.issue_grant_and_claim(args_for(eid)).await {
        Err(EngineError::NotFound { entity: "execution" }) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
    assert_eq!(count_grants(&pool, part, fake_uuid).await, 0);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn issue_grant_and_claim_no_dangling_grant_on_reject() {
    // Explicit atomicity assertion: a rejected composition must leave
    // `ff_claim_grant` empty for the execution. Force rejection via a
    // terminal exec (ExecutionNotActive) and probe the grant table.
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let (part, exec_uuid, eid) = seed_runnable(&pool).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal' \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    match backend.issue_grant_and_claim(args_for(eid)).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive { .. })) => {}
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
    assert_eq!(
        count_grants(&pool, part, exec_uuid).await,
        0,
        "rejected composition must not leak a grant row"
    );
}
