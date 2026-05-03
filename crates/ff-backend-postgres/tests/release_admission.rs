//! FF #511 Phase 1 — PG `release_admission` trait-method regression test.
//!
//! Mirrors the Valkey Lua body: idempotent DEL + DECR-if-positive.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://postgres:postgres@localhost:5432/ff_511 \
//!   cargo test -p ff-backend-postgres --test release_admission -- --ignored
//! ```

use sqlx::postgres::PgPoolOptions;

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{ReleaseAdmissionArgs, ReleaseAdmissionResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::{quota_partition, PartitionConfig};
use ff_core::types::{ExecutionId, FlowId, QuotaPolicyId};

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn release_admission_idempotent() {
    let url = std::env::var("FF_PG_TEST_URL").expect("FF_PG_TEST_URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations");

    let config = PartitionConfig::default();
    let backend = PostgresBackend::from_pool(pool.clone(), config.clone());
    let qid = QuotaPolicyId::new();
    let flow = FlowId::new();
    let eid = ExecutionId::for_flow(&flow, &config);
    let part = quota_partition(&qid, &config).index as i16;

    // Seed: insert an admitted row + bump active_concurrency to 1.
    sqlx::query(
        "INSERT INTO ff_quota_admitted \
             (partition_key, quota_policy_id, execution_id, admitted_at_ms, expires_at_ms) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(part)
    .bind(qid.to_string())
    .bind(eid.to_string())
    .bind(1_000_000i64)
    .bind(1_000_000i64 + 60_000)
    .execute(&pool)
    .await
    .expect("seed admitted");

    sqlx::query(
        "INSERT INTO ff_quota_policy \
             (partition_key, quota_policy_id, \
              requests_per_window_seconds, max_requests_per_window, \
              active_concurrency_cap, active_concurrency, \
              created_at_ms, updated_at_ms) \
         VALUES ($1, $2, 0, 0, 0, 1, 1_000_000, 1_000_000)",
    )
    .bind(part)
    .bind(qid.to_string())
    .execute(&pool)
    .await
    .expect("seed policy");

    // Release: first call deletes the row + decrements.
    let outcome = backend
        .release_admission(ReleaseAdmissionArgs::new(qid.clone(), eid.clone()))
        .await
        .expect("first release_admission");
    assert_eq!(outcome, ReleaseAdmissionResult::Released);

    let conc: i64 = sqlx::query_scalar(
        "SELECT active_concurrency FROM ff_quota_policy \
          WHERE partition_key = $1 AND quota_policy_id = $2",
    )
    .bind(part)
    .bind(qid.to_string())
    .fetch_one(&pool)
    .await
    .expect("read concurrency");
    assert_eq!(conc, 0, "concurrency should decrement to 0 after release");

    let present: Option<i64> = sqlx::query_scalar(
        "SELECT 1::bigint FROM ff_quota_admitted \
          WHERE partition_key = $1 AND quota_policy_id = $2 AND execution_id = $3",
    )
    .bind(part)
    .bind(qid.to_string())
    .bind(eid.to_string())
    .fetch_optional(&pool)
    .await
    .expect("read admitted");
    assert!(present.is_none(), "admitted row should be gone");

    // Idempotent replay: second call is a no-op, concurrency stays at 0
    // (not -1 — the floor clamp holds).
    let outcome2 = backend
        .release_admission(ReleaseAdmissionArgs::new(qid.clone(), eid.clone()))
        .await
        .expect("second release_admission");
    assert_eq!(outcome2, ReleaseAdmissionResult::Released);

    let conc2: i64 = sqlx::query_scalar(
        "SELECT active_concurrency FROM ff_quota_policy \
          WHERE partition_key = $1 AND quota_policy_id = $2",
    )
    .bind(part)
    .bind(qid.to_string())
    .fetch_one(&pool)
    .await
    .expect("read concurrency 2");
    assert_eq!(conc2, 0, "idempotent replay must not underflow concurrency");

    // Cleanup.
    sqlx::query("DELETE FROM ff_quota_policy WHERE partition_key = $1 AND quota_policy_id = $2")
        .bind(part)
        .bind(qid.to_string())
        .execute(&pool)
        .await
        .ok();
}
