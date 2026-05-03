//! Regression test for FF #508 — PG 16 rejects `RETURNING (xmax = 0)`
//! on a virtual-tuple slot with `SQLSTATE 0A000 "cannot retrieve a
//! system column in this context"`. The fix projects `xmax::text`
//! instead and compares client-side.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://postgres:postgres@localhost:5432/ff_508 \
//!   cargo test -p ff-backend-postgres --test worker_registry_pg16 -- --ignored
//! ```

use sqlx::postgres::PgPoolOptions;
use std::collections::BTreeSet;

use ff_backend_postgres::worker_registry;
use ff_core::contracts::{RegisterWorkerArgs, RegisterWorkerOutcome};
use ff_core::types::{LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId};

#[tokio::test]
#[ignore = "requires a live Postgres 16+; set FF_PG_TEST_URL"]
async fn register_worker_roundtrip_pg16() {
    let url = std::env::var("FF_PG_TEST_URL").expect("FF_PG_TEST_URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations");

    let ns = Namespace::new("ff-508-repro");
    let worker_id = WorkerId::new("test-pool");
    let instance = WorkerInstanceId::new(format!(
        "regression-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let lanes: BTreeSet<LaneId> = BTreeSet::from([LaneId::new("default")]);
    let caps: BTreeSet<String> = BTreeSet::from(["cuda".to_string()]);
    let now = TimestampMs::from_millis(1_000_000);

    let args = RegisterWorkerArgs::new(
        worker_id.clone(),
        instance.clone(),
        lanes.clone(),
        caps.clone(),
        30_000,
        ns.clone(),
        now,
    );

    let outcome = worker_registry::register_worker(&pool, args.clone())
        .await
        .expect("first register_worker");
    assert!(
        matches!(outcome, RegisterWorkerOutcome::Registered),
        "expected Registered on first call, got {:?}",
        outcome
    );

    let outcome2 = worker_registry::register_worker(&pool, args)
        .await
        .expect("second register_worker");
    assert!(
        matches!(outcome2, RegisterWorkerOutcome::Refreshed),
        "expected Refreshed on idempotent replay, got {:?}",
        outcome2
    );
}
