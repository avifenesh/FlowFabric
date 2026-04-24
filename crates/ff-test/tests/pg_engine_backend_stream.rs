//! Wave 4e — Postgres stream-family integration tests (RFC-015).
//!
//! Tests are `#[ignore]` gated behind `FF_PG_TEST_URL` for the same
//! reason as the Wave-3 schema smoke test: they require a live
//! Postgres. Run with:
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave4_test \
//!   cargo test -p ff-test --test pg_engine_backend_stream -- --ignored
//! ```

use std::time::Duration;

use ff_backend_postgres::PostgresBackend;
use ff_core::backend::{
    BackendTag, Frame, FrameKind, Handle, HandleKind, PatchKind, StreamMode, TailVisibility,
};
use ff_core::contracts::StreamCursor;
use ff_core::engine_backend::EngineBackend;
use ff_core::handle_codec::{encode, HandlePayload};
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, WorkerInstanceId,
};
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

fn mint_handle() -> (Handle, ExecutionId, AttemptIndex) {
    let cfg = PartitionConfig::default();
    let lane = LaneId::new("default");
    let eid = ExecutionId::solo(&lane, &cfg);
    let aidx = AttemptIndex::new(1);
    let payload = HandlePayload::new(
        eid.clone(),
        aidx,
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(1),
        30_000,
        lane,
        WorkerInstanceId::new("pg-test"),
    );
    let opaque = encode(BackendTag::Postgres, &payload);
    (
        Handle::new(BackendTag::Postgres, HandleKind::Fresh, opaque),
        eid,
        aidx,
    )
}

fn durable_frame(body: &[u8]) -> Frame {
    Frame::new(body.to_vec(), FrameKind::Event)
}

fn summary_frame(patch: &str) -> Frame {
    Frame::new(patch.as_bytes().to_vec(), FrameKind::Event).with_mode(StreamMode::DurableSummary {
        patch_kind: PatchKind::JsonMergePatch,
    })
}

fn best_effort_frame(body: &[u8], ttl_ms: u32) -> Frame {
    Frame::new(body.to_vec(), FrameKind::Event).with_mode(StreamMode::best_effort_live(ttl_ms))
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn durable_append_and_read_roundtrip() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (handle, eid, aidx) = mint_handle();

    let out1 = backend
        .append_frame(&handle, durable_frame(b"one"))
        .await
        .expect("append 1");
    let out2 = backend
        .append_frame(&handle, durable_frame(b"two"))
        .await
        .expect("append 2");

    assert!(out1.stream_id.contains('-'));
    assert_eq!(out1.frame_count, 1);
    assert_eq!(out2.frame_count, 2);

    let frames = backend
        .read_stream(
            &eid,
            aidx,
            StreamCursor::Start,
            StreamCursor::End,
            100,
        )
        .await
        .expect("read");
    assert_eq!(frames.frames.len(), 2);
    assert_eq!(frames.frames[0].fields.get("payload").unwrap(), "one");
    assert_eq!(frames.frames[1].fields.get("payload").unwrap(), "two");
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn summary_three_deltas_merge() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (handle, eid, aidx) = mint_handle();

    backend
        .append_frame(&handle, summary_frame(r#"{"a":1}"#))
        .await
        .unwrap();
    backend
        .append_frame(&handle, summary_frame(r#"{"b":2}"#))
        .await
        .unwrap();
    let third = backend
        .append_frame(&handle, summary_frame(r#"{"a":42,"c":{"x":"__ff_null__"}}"#))
        .await
        .unwrap();

    assert_eq!(third.summary_version, Some(3));
    let summary = backend
        .read_summary(&eid, aidx)
        .await
        .unwrap()
        .expect("summary row");
    assert_eq!(summary.version, 3);
    let doc: serde_json::Value = serde_json::from_slice(&summary.document_json).unwrap();
    assert_eq!(doc["a"], 42);
    assert_eq!(doc["b"], 2);
    assert!(doc["c"]["x"].is_null());
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn best_effort_dynamic_maxlen() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (handle, eid, aidx) = mint_handle();

    for i in 0..100u32 {
        backend
            .append_frame(&handle, best_effort_frame(format!("n={i}").as_bytes(), 5_000))
            .await
            .unwrap();
    }

    let frames = backend
        .read_stream(&eid, aidx, StreamCursor::Start, StreamCursor::End, 10_000)
        .await
        .unwrap();
    // Floor is 64, ceiling 16384. With 100 appends at high rate, the
    // EMA-derived K stays above the count written, so all 100 persist.
    assert!(frames.frames.len() <= 100);
    assert!(frames.frames.len() >= 64);

    // EMA must be non-zero after burst.
    let ema: f64 = sqlx::query_scalar(
        "SELECT ema_rate_hz FROM ff_stream_meta WHERE execution_id=$1 AND attempt_index=$2",
    )
    .bind(ff_test_exec_uuid(&eid))
    .bind(aidx.0 as i32)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(ema > 0.0, "EMA should have advanced after 100 appends");
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn tail_blocks_without_holding_connection() {
    // Use a tight pool (max=4) so that if each parked tailer held a
    // pg conn, 5+ parked waiters would deadlock the append below.
    let Some(_) = std::env::var("FF_PG_TEST_URL").ok() else {
        return;
    };
    let url = std::env::var("FF_PG_TEST_URL").unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("migrate");

    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (handle, eid, aidx) = mint_handle();

    // Spawn 8 parked tailers — more than max_connections. If tailers
    // held a pg conn while parked, this would exhaust the pool and
    // the append below would time out. The fact that it succeeds is
    // the acceptance test (Q2).
    let mut tails = Vec::new();
    for _ in 0..8 {
        let backend_clone = backend.clone();
        let eid_clone = eid.clone();
        tails.push(tokio::spawn(async move {
            backend_clone
                .tail_stream(
                    &eid_clone,
                    aidx,
                    StreamCursor::from_beginning(),
                    5_000,
                    100,
                    TailVisibility::All,
                )
                .await
        }));
    }

    // Let them park.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Pool idle-size after park: must leave room for the append.
    let idle = pool.num_idle();
    let size = pool.size();
    eprintln!("parked: pool.size={size} idle={idle} (max=4, 8 tailers parked)");

    // Append through the SAME pool — must succeed quickly.
    tokio::time::timeout(
        Duration::from_secs(2),
        backend.append_frame(&handle, durable_frame(b"wake")),
    )
    .await
    .expect("append must not block on parked tailers")
    .expect("append ok");

    for t in tails {
        let res = t.await.unwrap().expect("tail");
        assert_eq!(res.frames.len(), 1);
    }
}

#[tokio::test]
#[ignore = "requires FF_PG_TEST_URL"]
async fn mixed_durable_and_best_effort_coexist() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let (handle, eid, aidx) = mint_handle();

    // Durable marker at the head.
    backend
        .append_frame(&handle, durable_frame(b"D0"))
        .await
        .unwrap();
    // 50 best-effort frames.
    for i in 0..50u32 {
        backend
            .append_frame(&handle, best_effort_frame(format!("b{i}").as_bytes(), 2_000))
            .await
            .unwrap();
    }
    // Durable tail marker.
    backend
        .append_frame(&handle, durable_frame(b"D1"))
        .await
        .unwrap();

    let all = backend
        .read_stream(&eid, aidx, StreamCursor::Start, StreamCursor::End, 10_000)
        .await
        .unwrap();
    // The trim is mode-agnostic — best_effort trim removes oldest
    // rows regardless of mode. Floor 64 > 52 total frames written,
    // so nothing should have been trimmed; both durable markers must
    // still be present.
    let payloads: Vec<String> = all
        .frames
        .iter()
        .filter_map(|f| f.fields.get("payload").cloned())
        .collect();
    assert!(payloads.contains(&"D0".to_owned()));
    assert!(payloads.contains(&"D1".to_owned()));

    // Excluding best-effort should leave only the two durable frames.
    let only_durable = backend
        .tail_stream(
            &eid,
            aidx,
            StreamCursor::from_beginning(),
            0,
            10_000,
            TailVisibility::ExcludeBestEffort,
        )
        .await
        .unwrap();
    assert_eq!(only_durable.frames.len(), 2);
}

/// Local helper: extract the uuid tail from an `ExecutionId`
/// ({fp:N}:<uuid>) for direct SQL probes.
fn ff_test_exec_uuid(eid: &ExecutionId) -> uuid::Uuid {
    let s = eid.as_str();
    let after = s.split_once("}:").map(|(_, r)| r).unwrap_or(s);
    uuid::Uuid::parse_str(after).expect("valid exec uuid")
}
