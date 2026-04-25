//! RFC-019 Stage B — `PostgresBackend::subscribe_signal_delivery`
//! integration test. Gated on `FF_PG_TEST_URL`.
//!
//! Mirrors `subscribe_lease_history.rs`: synthetic direct-INSERT into
//! `ff_signal_event` to exercise the LISTEN + catch-up paths without
//! dragging the full HMAC + suspension state machine into scope. The
//! producer-side INSERT at the end of `deliver_signal_impl` is covered
//! by the `suspend_signal` suite (which still passes — it does not
//! assert on `ff_signal_event` but fails loudly if the INSERT is
//! broken).

use ff_backend_postgres::{apply_migrations, PostgresBackend};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::stream_subscribe::{decode_postgres_event_cursor, StreamCursor, StreamFamily};
use futures::StreamExt;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

async fn setup_or_skip() -> Option<sqlx::PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let bootstrap = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    apply_migrations(&bootstrap)
        .await
        .expect("apply_migrations clean");
    Some(bootstrap)
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn subscribe_signal_delivery_yields_inserted_event() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };

    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    let mut stream = backend
        .subscribe_signal_delivery(StreamCursor::empty())
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_millis(150)).await;

    let exec_uuid = uuid::Uuid::new_v4();
    let sid = uuid::Uuid::new_v4().to_string();
    let wp = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO ff_signal_event \
         (execution_id, signal_id, waitpoint_id, source_identity, delivered_at_ms, partition_key) \
         VALUES ($1, $2, $3, 'unit-test', $4, 0)",
    )
    .bind(exec_uuid.to_string())
    .bind(&sid)
    .bind(&wp)
    .bind(1_700_000_000_000_i64)
    .execute(&pool)
    .await
    .expect("insert synthetic event");

    let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout waiting for event")
        .expect("stream ended")
        .expect("stream error");

    assert_eq!(event.family, StreamFamily::SignalDelivery);
    assert_eq!(event.timestamp.0, 1_700_000_000_000);

    let decoded = decode_postgres_event_cursor(&event.cursor)
        .expect("cursor decode")
        .expect("some event_id");
    assert!(decoded > 0, "event_id should be positive, got {decoded}");

    let payload_str = std::str::from_utf8(&event.payload).expect("utf8");
    assert!(payload_str.contains(&sid), "payload missing signal_id: {payload_str}");
    assert!(payload_str.contains(&wp), "payload missing waitpoint_id: {payload_str}");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn subscribe_signal_delivery_cursor_resume() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    let tail: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(event_id), 0) FROM ff_signal_event WHERE partition_key = 0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let exec_uuid = uuid::Uuid::new_v4();
    let sid_a = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO ff_signal_event \
         (execution_id, signal_id, waitpoint_id, source_identity, delivered_at_ms, partition_key) \
         VALUES ($1, $2, NULL, NULL, $3, 0)",
    )
    .bind(exec_uuid.to_string())
    .bind(&sid_a)
    .bind(1_700_000_000_001_i64)
    .execute(&pool)
    .await
    .unwrap();

    let sid_b = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO ff_signal_event \
         (execution_id, signal_id, waitpoint_id, source_identity, delivered_at_ms, partition_key) \
         VALUES ($1, $2, NULL, NULL, $3, 0)",
    )
    .bind(exec_uuid.to_string())
    .bind(&sid_b)
    .bind(1_700_000_000_002_i64)
    .execute(&pool)
    .await
    .unwrap();

    let start_cursor = ff_core::stream_subscribe::encode_postgres_event_cursor(tail);
    let mut stream = backend
        .subscribe_signal_delivery(start_cursor)
        .await
        .expect("subscribe resume");

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout #1")
        .expect("stream ended")
        .expect("err");
    let first_payload = std::str::from_utf8(&first.payload).unwrap();
    assert!(first_payload.contains(&sid_a), "first: {first_payload}");

    let resume_cursor = first.cursor.clone();
    drop(stream);

    let mut stream2 = backend
        .subscribe_signal_delivery(resume_cursor.clone())
        .await
        .expect("subscribe resume after drop");
    let second = tokio::time::timeout(Duration::from_secs(5), stream2.next())
        .await
        .expect("timeout #2")
        .expect("stream ended")
        .expect("err");

    assert_ne!(second.cursor, resume_cursor, "cursor must advance");
    let payload = std::str::from_utf8(&second.payload).unwrap();
    assert!(payload.contains(&sid_b), "second: {payload}");
}
