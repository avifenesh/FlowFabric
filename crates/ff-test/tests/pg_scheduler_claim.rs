//! Postgres scheduler admission-pipeline integration tests
//! (RFC-v0.7 Wave 5b).
//!
//! Mirrors the Valkey-side scheduler's acceptance shape:
//!
//!  * happy-path claim — eligible row + matching caps → signed grant
//!  * capability mismatch — Ok(None), row stays eligible
//!  * budget block — hard-limit breach → Ok(None), row stays eligible
//!  * FIFO by priority — highest-priority row wins
//!  * skip-locked concurrency — two workers race, each gets a different row
//!  * grant expiry — verify_grant rejects grants past expires_at_ms
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave5b_test \
//!   cargo test -p ff-test --test pg_scheduler_claim -- --ignored
//! ```

use std::collections::{BTreeSet, HashMap};

use ff_backend_postgres::budget::upsert_policy_for_test;
use ff_backend_postgres::scheduler::{verify_grant, GrantVerifyError, PostgresScheduler};
use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{CreateExecutionArgs, RotateWaitpointHmacSecretAllArgs};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    BudgetId, ExecutionId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId,
};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

async fn ensure_hmac_secret(pool: &PgPool) {
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let args = RotateWaitpointHmacSecretAllArgs::new(
        "wave5b-test-kid",
        "deadbeefcafef00d1122334455667788",
        0,
    );
    let _ = backend.rotate_waitpoint_hmac_secret_all(args).await;
}

fn now() -> TimestampMs {
    TimestampMs(1_700_000_000_000)
}

fn args_with(eid: &ExecutionId, lane: &LaneId, priority: i32) -> CreateExecutionArgs {
    CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new("wave5b-test"),
        lane_id: lane.clone(),
        execution_kind: "task".to_owned(),
        input_payload: b"{}".to_vec(),
        payload_encoding: Some("application/json".to_owned()),
        priority,
        creator_identity: "pg-wave5b-test".to_owned(),
        idempotency_key: None,
        tags: HashMap::new(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: eid.partition(),
        now: now(),
    }
}

async fn patch_exec_row(
    pool: &PgPool,
    eid: &ExecutionId,
    required_caps: &[&str],
    budget_ids_csv: Option<&str>,
) {
    let part = eid.partition() as i16;
    let uuid = uuid::Uuid::parse_str(eid.as_str().split_once("}:").unwrap().1).unwrap();
    let caps_vec: Vec<String> = required_caps.iter().map(|s| (*s).to_owned()).collect();
    // Promote to runnable so the scheduler's eligibility filter
    // matches (create_execution seeds lifecycle_phase='submitted';
    // the submit→runnable promoter is out of Wave 5b scope, so the
    // fixture does that transition here).
    if let Some(csv) = budget_ids_csv {
        let patch = json!({ "budget_ids": csv });
        sqlx::query(
            "UPDATE ff_exec_core SET required_capabilities = $1, raw_fields = raw_fields || $2::jsonb, \
                                      lifecycle_phase = 'runnable' \
             WHERE partition_key = $3 AND execution_id = $4",
        )
        .bind(&caps_vec)
        .bind(patch)
        .bind(part)
        .bind(uuid)
        .execute(pool)
        .await
        .unwrap();
    } else {
        sqlx::query(
            "UPDATE ff_exec_core SET required_capabilities = $1, lifecycle_phase = 'runnable' \
             WHERE partition_key = $2 AND execution_id = $3",
        )
        .bind(&caps_vec)
        .bind(part)
        .bind(uuid)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn caps(tokens: &[&str]) -> BTreeSet<String> {
    tokens.iter().map(|s| (*s).to_owned()).collect()
}

fn wids() -> (WorkerId, WorkerInstanceId) {
    (WorkerId::new("w-test"), WorkerInstanceId::new("wi-test"))
}

async fn seed_exec(
    backend: &std::sync::Arc<PostgresBackend>,
    pool: &PgPool,
    lane: &LaneId,
    priority: i32,
    required_caps: &[&str],
    budget_ids_csv: Option<&str>,
) -> ExecutionId {
    let eid = ExecutionId::solo(lane, &PartitionConfig::default());
    backend
        .create_execution(args_with(&eid, lane, priority))
        .await
        .expect("create_execution OK");
    patch_exec_row(pool, &eid, required_caps, budget_ids_csv).await;
    eid
}

// ── tests ───────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn happy_path_claim_returns_signed_grant() {
    let Some(pool) = setup_or_skip().await else { return; };
    ensure_hmac_secret(&pool).await;
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(&format!("wave5b-happy-{}", uuid::Uuid::new_v4()));
    let eid = seed_exec(&backend, &pool, &lane, 5, &[], None).await;

    let sched = PostgresScheduler::new(pool.clone());
    let (wid, wiid) = wids();
    let grant = sched
        .claim_for_worker(&lane, &wid, &wiid, &caps(&[]), 30_000)
        .await
        .expect("claim_for_worker OK")
        .expect("a grant is issued");
    assert_eq!(grant.execution_id, eid);
    assert!(grant.grant_key.starts_with("pg:"));
    verify_grant(&pool, &grant).await.expect("grant verifies");
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn capability_mismatch_returns_none() {
    let Some(pool) = setup_or_skip().await else { return; };
    ensure_hmac_secret(&pool).await;
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(&format!("wave5b-capmiss-{}", uuid::Uuid::new_v4()));
    let _eid = seed_exec(&backend, &pool, &lane, 5, &["asr"], None).await;

    let sched = PostgresScheduler::new(pool.clone());
    let (wid, wiid) = wids();
    let out = sched
        .claim_for_worker(&lane, &wid, &wiid, &caps(&["llm"]), 30_000)
        .await
        .expect("claim_for_worker OK");
    assert!(out.is_none(), "worker without matching caps sees no grant");
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn budget_block_returns_none_and_row_stays_eligible() {
    let Some(pool) = setup_or_skip().await else { return; };
    ensure_hmac_secret(&pool).await;
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(&format!("wave5b-budget-{}", uuid::Uuid::new_v4()));

    let budget = BudgetId::new();
    let cfg = PartitionConfig::default();
    upsert_policy_for_test(
        &pool,
        &cfg,
        &budget,
        json!({ "hard_limit": 10u64, "dimension": "default" }),
    )
    .await
    .expect("upsert policy");
    let part = ff_core::partition::budget_partition(&budget, &cfg).index as i16;
    sqlx::query(
        "INSERT INTO ff_budget_usage (partition_key, budget_id, dimensions_key, current_value, updated_at_ms) \
         VALUES ($1, $2, 'default', 10, 0) \
         ON CONFLICT (partition_key, budget_id, dimensions_key) DO UPDATE SET current_value = 10",
    )
    .bind(part)
    .bind(budget.to_string())
    .execute(&pool)
    .await
    .unwrap();

    let _eid = seed_exec(&backend, &pool, &lane, 5, &[], Some(&budget.to_string())).await;

    let sched = PostgresScheduler::new(pool.clone());
    let (wid, wiid) = wids();
    let out = sched
        .claim_for_worker(&lane, &wid, &wiid, &caps(&[]), 30_000)
        .await
        .expect("claim_for_worker OK");
    assert!(out.is_none(), "budget-breached exec must not be granted");

    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_exec_core \
         WHERE lane_id = $1 AND eligibility_state = 'eligible_now'",
    )
    .bind(lane.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "budget-blocked row stays eligible");
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn fifo_by_priority_picks_highest_first() {
    let Some(pool) = setup_or_skip().await else { return; };
    ensure_hmac_secret(&pool).await;
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(&format!("wave5b-fifo-{}", uuid::Uuid::new_v4()));

    let _low = seed_exec(&backend, &pool, &lane, 1, &[], None).await;
    let hi = seed_exec(&backend, &pool, &lane, 10, &[], None).await;
    let _mid = seed_exec(&backend, &pool, &lane, 5, &[], None).await;

    let sched = PostgresScheduler::new(pool.clone());
    let (wid, wiid) = wids();
    let grant = sched
        .claim_for_worker(&lane, &wid, &wiid, &caps(&[]), 30_000)
        .await
        .expect("claim_for_worker OK")
        .expect("a grant is issued");
    assert_eq!(grant.execution_id, hi, "highest priority wins");
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn skip_locked_concurrent_claims_pick_distinct_rows() {
    let Some(pool) = setup_or_skip().await else { return; };
    ensure_hmac_secret(&pool).await;
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(&format!("wave5b-skip-{}", uuid::Uuid::new_v4()));

    let _a = seed_exec(&backend, &pool, &lane, 5, &[], None).await;
    let _b = seed_exec(&backend, &pool, &lane, 5, &[], None).await;

    let sched = std::sync::Arc::new(PostgresScheduler::new(pool.clone()));
    let lane_a = lane.clone();
    let lane_b = lane.clone();
    let s1 = sched.clone();
    let s2 = sched.clone();
    let h1 = tokio::spawn(async move {
        let (w, wi) = (WorkerId::new("w1"), WorkerInstanceId::new("wi1"));
        s1.claim_for_worker(&lane_a, &w, &wi, &caps(&[]), 30_000).await
    });
    let h2 = tokio::spawn(async move {
        let (w, wi) = (WorkerId::new("w2"), WorkerInstanceId::new("wi2"));
        s2.claim_for_worker(&lane_b, &w, &wi, &caps(&[]), 30_000).await
    });
    let r1 = h1.await.unwrap().unwrap();
    let r2 = h2.await.unwrap().unwrap();

    let g1 = r1.expect("worker1 got a grant");
    let g2 = r2.expect("worker2 got a grant");
    assert_ne!(
        g1.execution_id, g2.execution_id,
        "SKIP LOCKED must prevent double-claim"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn grant_expiry_is_enforced_by_verify() {
    let Some(pool) = setup_or_skip().await else { return; };
    ensure_hmac_secret(&pool).await;
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(&format!("wave5b-expiry-{}", uuid::Uuid::new_v4()));
    let _eid = seed_exec(&backend, &pool, &lane, 5, &[], None).await;

    let sched = PostgresScheduler::new(pool.clone());
    let (wid, wiid) = wids();
    let grant = sched
        .claim_for_worker(&lane, &wid, &wiid, &caps(&[]), 1)
        .await
        .expect("claim_for_worker OK")
        .expect("grant issued");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let err = verify_grant(&pool, &grant).await.expect_err("expired");
    assert!(matches!(err, GrantVerifyError::Expired));
}
