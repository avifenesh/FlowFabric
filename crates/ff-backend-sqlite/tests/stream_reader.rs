//! RFC-023 Phase 2b.2.2 — stream reader integration tests.
//!
//! Exercises the three Group C trait methods now populated on the
//! SQLite backend: `read_stream`, `tail_stream`, `read_summary`. Seeds
//! state via the Phase 2a producer path (`claim` + `append_frame`)
//! rather than raw SQL so the tests double as end-to-end coverage
//! from the producer outbox emit through to the reader surface.

#![cfg(feature = "streaming")]

use std::sync::Arc;
use std::time::Duration;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{
    BestEffortLiveConfig, CapabilitySet, ClaimPolicy, Frame, FrameKind, PatchKind, StreamMode,
    TailVisibility,
};
use ff_core::contracts::StreamCursor;
use ff_core::engine_backend::EngineBackend;
use ff_core::types::{AttemptIndex, ExecutionId, LaneId, WorkerId, WorkerInstanceId};
use serial_test::serial;
use uuid::Uuid;

// ── shared fixtures ────────────────────────────────────────────────────

async fn fresh_backend() -> Arc<SqliteBackend> {
    // SAFETY: test-only env mutation; every caller is tagged
    // `#[serial(ff_dev_mode)]`.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:rfc-023-stream-reader-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri).await.expect("construct backend")
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn seed_runnable_execution(backend: &SqliteBackend) -> (ExecutionId, Uuid) {
    let pool = backend.pool_for_test();
    let exec_uuid = Uuid::new_v4();
    let exec_id = ExecutionId::parse(&format!("{{fp:0}}:{exec_uuid}")).expect("construct exec_id");
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, lane_id, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state, priority, created_at_ms
        ) VALUES (0, ?1, 'default', 0,
                  'runnable', 'unowned', 'eligible_now',
                  'pending', 'initial', 0, 1)
        "#,
    )
    .bind(exec_uuid)
    .execute(pool)
    .await
    .expect("seed ff_exec_core");
    (exec_id, exec_uuid)
}

fn claim_policy() -> ClaimPolicy {
    ClaimPolicy::new(
        WorkerId::new("test-worker"),
        WorkerInstanceId::new("test-worker-instance"),
        30_000,
        None,
    )
}

async fn seed_and_claim(backend: &SqliteBackend) -> (ExecutionId, ff_core::backend::Handle) {
    let (eid, _uuid) = seed_runnable_execution(backend).await;
    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");
    (eid, h)
}

// ── read_stream ────────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_stream_happy_path_returns_frames_in_order() {
    let backend = fresh_backend().await;
    let (eid, h) = seed_and_claim(&backend).await;

    for i in 0..3 {
        let f = Frame::new(format!("payload-{i}").into_bytes(), FrameKind::Stdout);
        backend.append_frame(&h, f).await.expect("append");
    }

    let frames = backend
        .read_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::Start,
            StreamCursor::End,
            100,
        )
        .await
        .expect("read_stream");
    assert_eq!(frames.frames.len(), 3);
    // Verify ordering by embedded payload.
    let payloads: Vec<&str> = frames
        .frames
        .iter()
        .map(|f| {
            f.fields
                .get("payload")
                .map(String::as_str)
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(payloads, vec!["payload-0", "payload-1", "payload-2"]);
    assert!(frames.closed_at.is_none());
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_stream_cursor_pagination_resumes() {
    let backend = fresh_backend().await;
    let (eid, h) = seed_and_claim(&backend).await;

    for i in 0..5 {
        let f = Frame::new(format!("p{i}").into_bytes(), FrameKind::Stdout);
        backend.append_frame(&h, f).await.expect("append");
    }

    // Page 1: limit=2.
    let page1 = backend
        .read_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::Start,
            StreamCursor::End,
            2,
        )
        .await
        .expect("page 1");
    assert_eq!(page1.frames.len(), 2);

    // Resume from the last id of page 1 (inclusive bound means we'll
    // re-see that last frame; caller skips it, producing 3 new frames).
    let last_id = page1.frames.last().unwrap().id.clone();
    let page2 = backend
        .read_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::At(last_id.clone()),
            StreamCursor::End,
            100,
        )
        .await
        .expect("page 2");
    // Page 2 includes the cursor frame + the rest (4 remaining beyond
    // first 2 → page2 len is 4 with the re-observed last frame of page1).
    // The contract is "from >= cursor": caller dedups.
    assert!(page2.frames.len() >= 3);
    assert_eq!(page2.frames.first().unwrap().id, last_id);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_stream_isolates_per_attempt_index() {
    let backend = fresh_backend().await;
    let (eid, h) = seed_and_claim(&backend).await;

    // Write 2 frames against attempt_index = 0.
    for i in 0..2 {
        let f = Frame::new(format!("a0-{i}").into_bytes(), FrameKind::Stdout);
        backend.append_frame(&h, f).await.expect("append");
    }

    // Inject one raw frame at attempt_index = 1 (no replay path wired
    // on SQLite yet; the test only validates the read-side attempt
    // partitioning).
    let pool = backend.pool_for_test();
    let (_, exec_uuid) = split_eid(&eid);
    sqlx::query(
        "INSERT INTO ff_stream_frame \
         (partition_key, execution_id, attempt_index, ts_ms, seq, fields, mode, created_at_ms) \
         VALUES (0, ?1, 1, 100, 0, ?2, 'durable', 100)",
    )
    .bind(exec_uuid)
    .bind(r#"{"frame_type":"stdout","payload":"a1-only","encoding":"utf8","source":"worker"}"#)
    .execute(pool)
    .await
    .expect("raw insert");

    let a0 = backend
        .read_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::Start,
            StreamCursor::End,
            100,
        )
        .await
        .expect("read a0");
    let a1 = backend
        .read_stream(
            &eid,
            AttemptIndex::new(1),
            StreamCursor::Start,
            StreamCursor::End,
            100,
        )
        .await
        .expect("read a1");
    assert_eq!(a0.frames.len(), 2);
    assert_eq!(a1.frames.len(), 1);
    assert_eq!(
        a1.frames[0].fields.get("payload").map(String::as_str),
        Some("a1-only")
    );
}

fn split_eid(eid: &ExecutionId) -> (i64, Uuid) {
    let s = eid.as_str();
    let rest = s.strip_prefix("{fp:").unwrap();
    let close = rest.find("}:").unwrap();
    let part: i64 = rest[..close].parse().unwrap();
    let uuid = Uuid::parse_str(&rest[close + 2..]).unwrap();
    (part, uuid)
}

// ── read_summary ───────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_summary_returns_merged_document() {
    let backend = fresh_backend().await;
    let (eid, h) = seed_and_claim(&backend).await;

    // Two JSON-merge-patch deltas.
    let f1 = Frame::new(br#"{"a":1}"#.to_vec(), FrameKind::Event).with_mode(
        StreamMode::DurableSummary {
            patch_kind: PatchKind::JsonMergePatch,
        },
    );
    backend.append_frame(&h, f1).await.expect("append 1");
    let f2 = Frame::new(br#"{"b":2}"#.to_vec(), FrameKind::Event).with_mode(
        StreamMode::DurableSummary {
            patch_kind: PatchKind::JsonMergePatch,
        },
    );
    backend.append_frame(&h, f2).await.expect("append 2");

    let summary = backend
        .read_summary(&eid, AttemptIndex::new(0))
        .await
        .expect("read_summary")
        .expect("summary present");
    assert_eq!(summary.version, 2);
    let doc: serde_json::Value = serde_json::from_slice(&summary.document_json).expect("parse doc");
    assert_eq!(doc["a"], 1);
    assert_eq!(doc["b"], 2);
    assert!(summary.last_updated_ms > 0);
    assert!(summary.first_applied_ms > 0);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_summary_missing_returns_none() {
    let backend = fresh_backend().await;
    let (eid, _h) = seed_and_claim(&backend).await;

    let summary = backend
        .read_summary(&eid, AttemptIndex::new(0))
        .await
        .expect("read_summary");
    assert!(summary.is_none());
}

// ── tail_stream ────────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn tail_stream_fast_path_returns_existing_frames() {
    let backend = fresh_backend().await;
    let (eid, h) = seed_and_claim(&backend).await;

    backend
        .append_frame(&h, Frame::new(b"first".to_vec(), FrameKind::Stdout))
        .await
        .expect("append");

    let frames = backend
        .tail_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::At("0-0".into()),
            0, // block_ms = 0 → non-blocking peek
            100,
            TailVisibility::All,
        )
        .await
        .expect("tail");
    assert_eq!(frames.frames.len(), 1);
    assert_eq!(
        frames.frames[0].fields.get("payload").map(String::as_str),
        Some("first")
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn tail_stream_blocks_and_wakes_on_append() {
    let backend = fresh_backend().await;
    let (eid, h) = seed_and_claim(&backend).await;

    let backend2 = backend.clone();
    let h2 = h.clone();
    let produce = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        backend2
            .append_frame(&h2, Frame::new(b"late".to_vec(), FrameKind::Stdout))
            .await
            .expect("append from task");
    });

    let start = std::time::Instant::now();
    let frames = backend
        .tail_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::At("0-0".into()),
            2_000, // block up to 2s
            100,
            TailVisibility::All,
        )
        .await
        .expect("tail");
    let elapsed = start.elapsed();

    produce.await.expect("join producer");
    assert_eq!(frames.frames.len(), 1);
    assert_eq!(
        frames.frames[0].fields.get("payload").map(String::as_str),
        Some("late")
    );
    // Fast-path short-circuit should NOT trigger — verify we actually
    // parked (elapsed should be at least ~some fraction of the 80ms
    // sleep, generally well under the 2s block ceiling).
    assert!(elapsed >= Duration::from_millis(50));
    assert!(elapsed < Duration::from_millis(1_500));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn tail_stream_timeout_returns_empty_open() {
    let backend = fresh_backend().await;
    let (eid, _h) = seed_and_claim(&backend).await;

    let start = std::time::Instant::now();
    let frames = backend
        .tail_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::At("0-0".into()),
            150, // tight block window; no producer
            100,
            TailVisibility::All,
        )
        .await
        .expect("tail timeout");
    let elapsed = start.elapsed();
    assert!(frames.frames.is_empty());
    assert!(elapsed >= Duration::from_millis(100));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn tail_stream_rejects_open_cursors() {
    let backend = fresh_backend().await;
    let (eid, _h) = seed_and_claim(&backend).await;

    let err = backend
        .tail_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::Start,
            0,
            100,
            TailVisibility::All,
        )
        .await
        .expect_err("must reject Start");
    assert!(matches!(
        err,
        ff_core::engine_error::EngineError::Validation { .. }
    ));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn tail_stream_exclude_best_effort_filters_frames() {
    let backend = fresh_backend().await;
    let (eid, h) = seed_and_claim(&backend).await;

    // One durable + one best-effort frame.
    backend
        .append_frame(&h, Frame::new(b"dur".to_vec(), FrameKind::Stdout))
        .await
        .expect("append durable");
    let be_cfg = BestEffortLiveConfig::with_ttl(60_000);
    let be = Frame::new(b"be".to_vec(), FrameKind::Stdout)
        .with_mode(StreamMode::BestEffortLive { config: be_cfg });
    backend.append_frame(&h, be).await.expect("append BE");

    // Full view → both.
    let full = backend
        .tail_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::At("0-0".into()),
            0,
            100,
            TailVisibility::All,
        )
        .await
        .expect("tail all");
    assert_eq!(full.frames.len(), 2);

    // Exclude-BE → only the durable frame.
    let filtered = backend
        .tail_stream(
            &eid,
            AttemptIndex::new(0),
            StreamCursor::At("0-0".into()),
            0,
            100,
            TailVisibility::ExcludeBestEffort,
        )
        .await
        .expect("tail exclude-BE");
    assert_eq!(filtered.frames.len(), 1);
    assert_eq!(
        filtered.frames[0].fields.get("payload").map(String::as_str),
        Some("dur")
    );
}
