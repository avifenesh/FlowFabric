//! FF #511 Phase 2a — PG `read_quota_policy_limits` trait-method
//! regression test.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://postgres:postgres@localhost:5432/ff_511a \
//!   cargo test -p ff-backend-postgres --test read_quota_policy_limits -- --ignored
//! ```

use sqlx::postgres::PgPoolOptions;

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::QuotaPolicyId;

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_quota_policy_limits_roundtrip() {
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
    let part = ff_core::partition::quota_partition(&qid, &config).index as i16;

    // Absent policy → None.
    let absent = backend
        .read_quota_policy_limits(&qid)
        .await
        .expect("read absent");
    assert!(absent.is_none(), "absent policy must read as None");

    // Seed a policy.
    sqlx::query(
        "INSERT INTO ff_quota_policy \
             (partition_key, quota_policy_id, \
              requests_per_window_seconds, max_requests_per_window, \
              active_concurrency_cap, active_concurrency, \
              created_at_ms, updated_at_ms) \
         VALUES ($1, $2, 60, 100, 5, 0, 1_000_000, 1_000_000)",
    )
    .bind(part)
    .bind(qid.to_string())
    .execute(&pool)
    .await
    .expect("seed policy");

    let limits = backend
        .read_quota_policy_limits(&qid)
        .await
        .expect("read present")
        .expect("present policy");

    assert_eq!(limits.max_requests_per_window, 100);
    assert_eq!(limits.requests_per_window_seconds, 60);
    assert_eq!(limits.active_concurrency_cap, 5);
    assert_eq!(limits.jitter_ms, 0, "PG schema has no jitter_ms; must default to 0");
    assert!(!limits.is_unlimited());

    // Cleanup.
    sqlx::query("DELETE FROM ff_quota_policy WHERE partition_key = $1 AND quota_policy_id = $2")
        .bind(part)
        .bind(qid.to_string())
        .execute(&pool)
        .await
        .ok();
}
