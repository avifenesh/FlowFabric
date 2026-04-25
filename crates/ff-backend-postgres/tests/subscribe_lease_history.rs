//! RFC-019 Stage B — `PostgresBackend::subscribe_lease_history`
//! integration test. Gated on `FF_PG_TEST_URL`.
//!
//! Covers:
//!   * subscribe from the empty cursor tails from "now"
//!   * synthetic `ff_lease_event` INSERT is observed within 5s
//!   * `StreamEvent::cursor` round-trips through
//!     `decode_postgres_event_cursor`
//!
//! Per-test isolation mirrors `suspend_signal.rs` — we apply the
//! workspace migrations to `public`, then lean on the global
//! `ff_lease_event` outbox directly (no per-test table shadow
//! needed since the partition_key filter already narrows reads to
//! the test's partition + synthetic event_id is unique per run).

use ff_backend_postgres::{apply_migrations, PostgresBackend};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::stream_subscribe::{decode_postgres_event_cursor, StreamCursor, StreamFamily};
use futures::StreamExt;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

async fn setup_or_skip() -> Option<sqlx::PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    // The subscriber loop holds a dedicated `PgListener` connection
    // for the duration of the subscription. The bootstrap pool
    // serves both the backend's catch-up queries AND the test's
    // direct INSERTs, so we need enough capacity for all three.
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
async fn subscribe_lease_history_yields_inserted_event() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };

    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    // Empty cursor → start from tail.
    let mut stream = backend
        .subscribe_lease_history(StreamCursor::empty())
        .await
        .expect("subscribe");

    // Give the LISTEN task a beat to register before we INSERT so
    // the NOTIFY is actually observed (catch-up replay will also
    // pick it up regardless).
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Insert a synthetic lease-event row at partition 0 (the
    // subscription is partition-scoped to 0 per the impl).
    let test_uuid = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_lease_event \
         (execution_id, lease_id, event_type, occurred_at_ms, partition_key) \
         VALUES ($1, NULL, 'acquired', $2, 0)",
    )
    .bind(test_uuid.to_string())
    .bind(1_700_000_000_000_i64)
    .execute(&pool)
    .await
    .expect("insert synthetic event");

    // Poll the stream for up to 5s for the event to arrive.
    let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout waiting for event")
        .expect("stream ended")
        .expect("stream error");

    assert_eq!(event.family, StreamFamily::LeaseHistory);
    assert_eq!(event.timestamp.0, 1_700_000_000_000);

    // Cursor decodes under the Postgres prefix and carries a
    // non-zero event_id.
    let decoded =
        decode_postgres_event_cursor(&event.cursor).expect("cursor decode").expect("some event_id");
    assert!(decoded > 0, "event_id should be positive, got {decoded}");

    // Payload is the JSON body the subscriber emits.
    let payload_str = std::str::from_utf8(&event.payload).expect("utf8");
    assert!(payload_str.contains("\"acquired\""), "payload: {payload_str}");
    assert!(
        payload_str.contains(&test_uuid.to_string()),
        "payload: {payload_str}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn subscribe_lease_history_cursor_resume() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    // Read the current tail so we skip any pre-existing rows from
    // earlier runs of the test DB — the outbox is global + the
    // bigserial event_id monotonically climbs, so a cursor at
    // `max(event_id)` before our INSERTs strictly isolates this run.
    let tail: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(event_id), 0) FROM ff_lease_event WHERE partition_key = 0")
            .fetch_one(&pool)
            .await
            .unwrap();

    // Insert two events.
    let uid_a = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_lease_event \
         (execution_id, lease_id, event_type, occurred_at_ms, partition_key) \
         VALUES ($1, NULL, 'acquired', $2, 0)",
    )
    .bind(uid_a.to_string())
    .bind(1_700_000_000_001_i64)
    .execute(&pool)
    .await
    .unwrap();

    let uid_b = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_lease_event \
         (execution_id, lease_id, event_type, occurred_at_ms, partition_key) \
         VALUES ($1, NULL, 'renewed', $2, 0)",
    )
    .bind(uid_b.to_string())
    .bind(1_700_000_000_002_i64)
    .execute(&pool)
    .await
    .unwrap();

    // Subscribe from the pre-test tail → the first drain of replay
    // yields the `acquired` event.
    let start_cursor = ff_core::stream_subscribe::encode_postgres_event_cursor(tail);
    let mut stream = backend
        .subscribe_lease_history(start_cursor)
        .await
        .expect("subscribe resume");

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout #1")
        .expect("stream ended")
        .expect("err");
    let first_payload = std::str::from_utf8(&first.payload).unwrap();
    assert!(
        first_payload.contains("\"acquired\"") && first_payload.contains(&uid_a.to_string()),
        "first payload: {first_payload}"
    );

    // Resume from the first cursor → only the `renewed` event
    // should land next.
    let resume_cursor = first.cursor.clone();
    drop(stream);

    let mut stream2 = backend
        .subscribe_lease_history(resume_cursor.clone())
        .await
        .expect("subscribe resume after drop");
    let second = tokio::time::timeout(Duration::from_secs(5), stream2.next())
        .await
        .expect("timeout #2")
        .expect("stream ended")
        .expect("err");

    assert_ne!(second.cursor, resume_cursor, "cursor must advance");
    let payload = std::str::from_utf8(&second.payload).unwrap();
    assert!(
        payload.contains("\"renewed\"") && payload.contains(&uid_b.to_string()),
        "second payload: {payload}"
    );
}
