//! Wave 4b attempt-family integration tests.
//!
//! Exercises the `claim`, `claim_from_reclaim`, `renew`, `progress`,
//! `complete`, `fail`, `delay`, `wait_children` trait methods against
//! a live Postgres with the Wave-3 schema applied.
//!
//! Spec intent placed this test under `crates/ff-test/tests/`; keeping
//! it in-crate to avoid adding a cross-crate dev-dep on
//! `ff-backend-postgres` to `ff-test` that would collide with parallel
//! Wave-4 agents editing the same Cargo.toml. The assertions are
//! identical; path differs only by coordination.
//!
//! # Running
//!
//! `FF_PG_TEST_URL=postgres://... cargo test -p ff-backend-postgres
//!  --test pg_engine_backend_attempt -- --ignored`

use ff_backend_postgres::PostgresBackend;
use ff_core::backend::ReclaimToken;
use ff_core::backend::{
    CapabilitySet, ClaimPolicy, FailOutcome, FailureClass, FailureReason,
};
use ff_core::contracts::ReclaimGrant;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::types::{LaneId, TimestampMs, WorkerId, WorkerInstanceId};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

async fn seed_exec(
    pool: &PgPool,
    lane: &str,
    caps: &[&str],
) -> (i16, Uuid, LaneId) {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    let caps: Vec<String> = caps.iter().map(|s| (*s).to_string()).collect();
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms
        ) VALUES (
            $1, $2, NULL, $3,
            $4, 0,
            'runnable', 'unowned', 'eligible_now',
            'waiting', 'pending_first_attempt',
            0, $5
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lane)
    .bind(&caps)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed exec");
    (part, exec_uuid, LaneId::new(lane))
}

fn claim_policy(worker: &str, ttl_ms: u32) -> ClaimPolicy {
    ClaimPolicy::new(
        WorkerId::new(worker),
        WorkerInstanceId::new(format!("{worker}-1")),
        ttl_ms,
        Some(Duration::from_millis(100)),
    )
}

async fn lease_row(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
) -> (i64, Option<i64>) {
    let row = sqlx::query(
        r#"
        SELECT lease_epoch, lease_expires_at_ms
          FROM ff_attempt
         WHERE partition_key = $1 AND execution_id = $2
         ORDER BY attempt_index DESC
         LIMIT 1
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("attempt row");
    (row.get("lease_epoch"), row.try_get("lease_expires_at_ms").ok())
}

async fn lifecycle_phase(pool: &PgPool, part: i16, exec_uuid: Uuid) -> String {
    let row = sqlx::query(
        "SELECT lifecycle_phase FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .unwrap();
    row.get::<String, _>("lifecycle_phase")
}

async fn completion_outbox_count(pool: &PgPool, exec_uuid: Uuid) -> i64 {
    let row = sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM ff_completion_event WHERE execution_id = $1",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .unwrap();
    row.get("n")
}

fn backend_from_pool(pool: PgPool) -> Arc<dyn EngineBackend> {
    PostgresBackend::from_pool(pool, PartitionConfig::default())
}

// Happy path
#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn happy_path_claim_progress_complete() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-happy-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-happy", 30_000))
        .await.expect("claim ok").expect("claim Some");
    assert_eq!(lifecycle_phase(&pool, part, exec_uuid).await, "active");
    let (epoch, expires) = lease_row(&pool, part, exec_uuid).await;
    assert_eq!(epoch, 1);
    assert!(expires.unwrap() > now_ms());
    backend.progress(&handle, Some(25), Some("step 1".into())).await.expect("p1");
    backend.progress(&handle, Some(75), Some("step 2".into())).await.expect("p2");
    backend.complete(&handle, Some(b"done".to_vec())).await.expect("complete");
    assert_eq!(lifecycle_phase(&pool, part, exec_uuid).await, "terminal");
    assert_eq!(completion_outbox_count(&pool, exec_uuid).await, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn claim_then_renew_bumps_expiry() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-renew-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-renew", 5_000))
        .await.unwrap().unwrap();
    let (_, before) = lease_row(&pool, part, exec_uuid).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let renewal = backend.renew(&handle).await.expect("renew");
    let (_, after) = lease_row(&pool, part, exec_uuid).await;
    assert!(after.unwrap() >= before.unwrap());
    assert_eq!(renewal.lease_epoch, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn fail_retryable_reeligible() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-failr-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-failr", 30_000))
        .await.unwrap().unwrap();
    let outcome = backend
        .fail(&handle, FailureReason::new("boom"), FailureClass::Transient)
        .await.expect("fail ok");
    assert!(matches!(outcome, FailOutcome::RetryScheduled { .. }));
    assert_eq!(lifecycle_phase(&pool, part, exec_uuid).await, "runnable");
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn fail_permanent_terminal() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-failp-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-failp", 30_000))
        .await.unwrap().unwrap();
    let outcome = backend
        .fail(&handle, FailureReason::new("nope"), FailureClass::Permanent)
        .await.expect("fail ok");
    assert_eq!(outcome, FailOutcome::TerminalFailed);
    assert_eq!(lifecycle_phase(&pool, part, exec_uuid).await, "terminal");
    assert_eq!(completion_outbox_count(&pool, exec_uuid).await, 1);
}

fn reclaim_grant(eid: ff_core::types::ExecutionId, lane: LaneId, part_idx: u16) -> ReclaimGrant {
    let pk = PartitionKey::from(&ff_core::partition::Partition {
        family: ff_core::partition::PartitionFamily::Flow,
        index: part_idx,
    });
    ReclaimGrant {
        execution_id: eid,
        partition_key: pk,
        grant_key: "gkey".into(),
        expires_at_ms: (now_ms() as u64) + 60_000,
        lane_id: lane,
    }
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn claim_from_reclaim_happy_path_bumps_epoch() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-reclaim-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let _orig = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-orig", 30_000))
        .await.unwrap().unwrap();
    sqlx::query("UPDATE ff_attempt SET lease_expires_at_ms = 0 WHERE partition_key = $1 AND execution_id = $2")
        .bind(part).bind(exec_uuid).execute(&pool).await.unwrap();
    let eid = ff_core::types::ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    let grant = reclaim_grant(eid, lane_id.clone(), part as u16);
    let token = ReclaimToken::new(grant, WorkerId::new("w-rec"), WorkerInstanceId::new("w-rec-1"), 30_000);
    let (epoch_before, _) = lease_row(&pool, part, exec_uuid).await;
    let handle = backend.claim_from_reclaim(token).await.expect("reclaim").expect("Some");
    let (epoch_after, _) = lease_row(&pool, part, exec_uuid).await;
    assert!(epoch_after > epoch_before, "epoch must bump: {epoch_before} -> {epoch_after}");
    backend.renew(&handle).await.expect("renew after reclaim");
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn claim_from_reclaim_live_lease_returns_none() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-reclaim-live-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let _orig = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-owner", 60_000))
        .await.unwrap().unwrap();
    let eid = ff_core::types::ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    let grant = reclaim_grant(eid, lane_id.clone(), part as u16);
    let token = ReclaimToken::new(grant, WorkerId::new("w-thief"), WorkerInstanceId::new("w-thief-1"), 30_000);
    let out = backend.claim_from_reclaim(token).await.expect("call ok");
    assert!(out.is_none(), "live lease must reject reclaim");
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn delay_releases_lease() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-delay-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-del", 30_000))
        .await.unwrap().unwrap();
    backend.delay(&handle, TimestampMs::from_millis(now_ms() + 10_000)).await.expect("delay");
    let (_, expires) = lease_row(&pool, part, exec_uuid).await;
    assert!(expires.is_none(), "delay releases lease");
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn wait_children_releases_lease() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-wc-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-wc", 30_000))
        .await.unwrap().unwrap();
    backend.wait_children(&handle).await.expect("wait_children");
    let (_, expires) = lease_row(&pool, part, exec_uuid).await;
    assert!(expires.is_none());
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn renew_after_epoch_bump_is_lease_conflict() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-fence-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, &[]).await;
    let backend = backend_from_pool(pool.clone());
    let handle = backend
        .claim(&lane_id, &CapabilitySet::default(), claim_policy("w-fence", 30_000))
        .await.unwrap().unwrap();
    sqlx::query("UPDATE ff_attempt SET lease_epoch = lease_epoch + 1 WHERE partition_key = $1 AND execution_id = $2")
        .bind(part).bind(exec_uuid).execute(&pool).await.unwrap();
    let err = backend.renew(&handle).await.expect_err("must fail");
    assert!(matches!(err, EngineError::Contention(ContentionKind::LeaseConflict)),
        "expected LeaseConflict, got {err:?}");
}
