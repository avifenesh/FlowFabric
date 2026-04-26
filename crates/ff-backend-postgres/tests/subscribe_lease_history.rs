//! RFC-019 Stage C — `PostgresBackend::subscribe_lease_history`
//! integration test. Gated on `FF_PG_TEST_URL`.
//!
//! Covers:
//!   * subscribe from the empty cursor tails from "now"
//!   * synthetic `ff_lease_event` INSERT is observed within 5s as a
//!     typed `LeaseHistoryEvent::Acquired`
//!   * `LeaseHistoryEvent::cursor()` round-trips through
//!     `decode_postgres_event_cursor`
//!   * cursor resume across subscribe calls

use ff_backend_postgres::{apply_migrations, PostgresBackend};
use ff_core::backend::ScannerFilter;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::stream_events::LeaseHistoryEvent;
use ff_core::stream_subscribe::{decode_postgres_event_cursor, StreamCursor};
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
async fn subscribe_lease_history_yields_typed_acquired_event() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };

    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    let mut stream = backend
        .subscribe_lease_history(StreamCursor::empty(), &ScannerFilter::default())
        .await
        .expect("subscribe");

    tokio::time::sleep(Duration::from_millis(150)).await;

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

    let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout waiting for event")
        .expect("stream ended")
        .expect("stream error");

    match &event {
        LeaseHistoryEvent::Acquired {
            execution_id,
            lease_id,
            at,
            ..
        } => {
            assert_eq!(execution_id.to_string(), format!("{{fp:0}}:{test_uuid}"));
            assert!(lease_id.is_none(), "PG outbox does not persist lease_id");
            assert_eq!(at.0, 1_700_000_000_000);
        }
        other => panic!("expected LeaseHistoryEvent::Acquired, got {other:?}"),
    }

    let decoded = decode_postgres_event_cursor(event.cursor())
        .expect("cursor decode")
        .expect("some event_id");
    assert!(decoded > 0, "event_id should be positive, got {decoded}");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn subscribe_lease_history_cursor_resume() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());

    let tail: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(event_id), 0) FROM ff_lease_event WHERE partition_key = 0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

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

    let start_cursor = ff_core::stream_subscribe::encode_postgres_event_cursor(tail);
    let mut stream = backend
        .subscribe_lease_history(start_cursor, &ScannerFilter::default())
        .await
        .expect("subscribe resume");

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout #1")
        .expect("stream ended")
        .expect("err");
    match &first {
        LeaseHistoryEvent::Acquired { execution_id, .. } => {
            assert_eq!(execution_id.to_string(), format!("{{fp:0}}:{uid_a}"));
        }
        other => panic!("expected Acquired(uid_a), got {other:?}"),
    }

    let resume_cursor = first.cursor().clone();
    drop(stream);

    let mut stream2 = backend
        .subscribe_lease_history(resume_cursor.clone(), &ScannerFilter::default())
        .await
        .expect("subscribe resume after drop");
    let second = tokio::time::timeout(Duration::from_secs(5), stream2.next())
        .await
        .expect("timeout #2")
        .expect("stream ended")
        .expect("err");

    assert_ne!(second.cursor(), &resume_cursor, "cursor must advance");
    match &second {
        LeaseHistoryEvent::Renewed { execution_id, .. } => {
            assert_eq!(execution_id.to_string(), format!("{{fp:0}}:{uid_b}"));
        }
        other => panic!("expected Renewed(uid_b), got {other:?}"),
    }
}
