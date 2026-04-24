//! Wave 4h — Postgres `CompletionBackend` integration test.
//!
//! Covers the Q1-adjudicated LISTEN/NOTIFY + outbox-replay transport:
//!
//! 1. Happy path — subscribe, INSERT into `ff_completion_event`,
//!    receive the payload.
//! 2. Filter — a namespace-scoped subscriber sees only matching rows.
//! 3. Replay — INSERTs that happen between two wakes are drained in
//!    order (validates the "NOTIFY coalesces, replay catches bursts"
//!    invariant).
//! 4. Subscriber drop — dropping the stream ends the background task
//!    cleanly.
//!
//! The reconnect-on-connection-drop scenario is covered by the
//! backend's internal reconnect loop (tested by fault injection in a
//! follow-up — it would require a second PgPool pointed at a
//! connection-terminating proxy, out of scope for Wave 4h).
//!
//! # Running
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave4h_test \
//!   cargo test -p ff-test --test pg_completion_backend -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` because the tests share the `ff_completion_event`
//! outbox table and observe absolute row counts.

use std::time::Duration;

use ff_backend_postgres::PostgresBackend;
use ff_core::backend::ScannerFilter;
use ff_core::completion_backend::CompletionBackend;
use ff_core::partition::PartitionConfig;
use futures::StreamExt;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(8) // subscribers each take a dedicated LISTEN conn
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    Some(pool)
}

/// Insert one row into `ff_completion_event`. Returns the
/// partition_key used so tests can reconstruct the expected
/// ExecutionId hash-tag.
async fn insert_completion(
    pool: &PgPool,
    execution_id: Uuid,
    flow_id: Option<Uuid>,
    outcome: &str,
    namespace: Option<&str>,
    instance_tag: Option<&str>,
) -> i16 {
    // Pick partition_key = 0 for tests; the trigger fires the same
    // way on any partition. Using a fixed key keeps the expected
    // ExecutionId shape predictable.
    let partition_key: i16 = 0;
    sqlx::query(
        "INSERT INTO ff_completion_event \
         (partition_key, execution_id, flow_id, outcome, namespace, instance_tag, occurred_at_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(partition_key)
    .bind(execution_id)
    .bind(flow_id)
    .bind(outcome)
    .bind(namespace)
    .bind(instance_tag)
    .bind(1_700_000_000_000_i64)
    .execute(pool)
    .await
    .expect("insert ff_completion_event");
    partition_key
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn subscribe_receives_inserted_completion() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let mut stream = backend
        .subscribe_completions()
        .await
        .expect("subscribe_completions");

    let eid = Uuid::new_v4();
    let fid = Uuid::new_v4();
    insert_completion(&pool, eid, Some(fid), "complete", None, None).await;

    let got = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("completion arrives within 3s")
        .expect("stream yields Some");

    assert_eq!(got.outcome, "complete");
    assert_eq!(got.flow_id.map(|f| f.0), Some(fid));
    assert!(got.execution_id.as_str().contains(&eid.to_string()));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn filter_drops_non_matching_namespace() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let filter = ScannerFilter::new().with_namespace("tenant-a");
    let mut stream = backend
        .subscribe_completions_filtered(&filter)
        .await
        .expect("subscribe_completions_filtered");

    // Mismatched namespace — must NOT be delivered.
    insert_completion(
        &pool,
        Uuid::new_v4(),
        Some(Uuid::new_v4()),
        "complete",
        Some("tenant-b"),
        None,
    )
    .await;
    // Matching namespace — must be delivered.
    let eid_match = Uuid::new_v4();
    insert_completion(
        &pool,
        eid_match,
        Some(Uuid::new_v4()),
        "complete",
        Some("tenant-a"),
        None,
    )
    .await;

    let got = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .expect("filtered completion arrives")
        .expect("stream yields Some");
    assert!(got.execution_id.as_str().contains(&eid_match.to_string()));

    // Drain briefly to confirm the filtered-out row never arrives.
    let drained = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
    assert!(drained.is_err(), "no further events should arrive");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn replay_drains_burst_after_single_notify() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let mut stream = backend
        .subscribe_completions()
        .await
        .expect("subscribe_completions");

    // Burst-insert 5 rows inside a single transaction. NOTIFY
    // delivers at COMMIT; pg may coalesce identical-payload NOTIFYs
    // and the first wake will see all 5 via the cursor-driven
    // replay.
    let mut tx = pool.begin().await.expect("begin");
    let mut fids = Vec::new();
    for _ in 0..5 {
        let fid = Uuid::new_v4();
        fids.push(fid);
        sqlx::query(
            "INSERT INTO ff_completion_event \
             (partition_key, execution_id, flow_id, outcome, occurred_at_ms) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(0_i16)
        .bind(Uuid::new_v4())
        .bind(fid)
        .bind("complete")
        .bind(1_700_000_000_000_i64)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.expect("commit");

    let mut received = Vec::new();
    for _ in 0..5 {
        let got = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("all 5 completions arrive")
            .expect("stream yields Some");
        received.push(got.flow_id.unwrap().0);
    }
    // Monotonic event_id ordering ⇒ same insertion order.
    assert_eq!(received, fids);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn dropping_stream_exits_subscriber_task() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let stream = backend
        .subscribe_completions()
        .await
        .expect("subscribe_completions");

    // Drop the stream. The subscriber task's `tx.closed()` should
    // fire; dropping the stream also aborts the JoinHandle.
    drop(stream);

    // Subsequent inserts must not block or panic anything.
    insert_completion(
        &pool,
        Uuid::new_v4(),
        Some(Uuid::new_v4()),
        "complete",
        None,
        None,
    )
    .await;

    // Give the reactor a tick to observe the shutdown cleanly.
    tokio::time::sleep(Duration::from_millis(100)).await;
}
