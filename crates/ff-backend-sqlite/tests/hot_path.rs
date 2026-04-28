//! RFC-023 Phase 2a.2 — claim / complete / fail hot-path tests.
//!
//! The tests drive the [`SqliteBackend`] through the `EngineBackend`
//! trait surface: they seed `ff_exec_core` + `ff_execution_capabilities`
//! rows via raw SQL (the `create_execution` trait method lands in Phase
//! 2b), call `claim` to mint a real handle, then exercise
//! `complete`/`fail` against the handle and assert the post-state via
//! follow-up SELECTs.
//!
//! # Test-environment invariant
//!
//! Every test is `#[serial(ff_dev_mode)]` — `SqliteBackend::new` refuses
//! construction without `FF_DEV_MODE=1`, and the shared environment
//! variable must not race across test binaries.

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{
    BackendTag, CapabilitySet, ClaimPolicy, FailureClass, FailureReason, Frame, FrameKind, Handle,
    HandleKind, PatchKind, ReclaimToken, StreamMode,
};
use ff_core::contracts::ReclaimGrant;
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, ValidationKind};
use ff_core::handle_codec::{HandlePayload, encode as encode_opaque};
use ff_core::partition::PartitionConfig;
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, WorkerId, WorkerInstanceId,
};
use serial_test::serial;
use std::sync::Arc;
use uuid::Uuid;

// ── Setup helpers ──────────────────────────────────────────────────────

/// Spin up an isolated SQLite backend against a shared-cache `:memory:`
/// URI whose name embeds nanotime + thread-id so parallel tests never
/// collide on the registry key.
async fn fresh_backend() -> Arc<SqliteBackend> {
    // SAFETY: test-only env mutation; every caller is tagged
    // `#[serial(ff_dev_mode)]` which serializes all env readers +
    // writers across test binaries.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:rfc-023-hot-path-{}?mode=memory&cache=shared",
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

/// Seed one runnable, eligible execution on partition 0 / `lane_id`
/// with the given capability tokens. Returns `(execution_id, exec_uuid)`.
async fn seed_runnable_execution(
    backend: &SqliteBackend,
    lane_id: &str,
    capabilities: &[&str],
) -> (ExecutionId, Uuid) {
    let pool = backend.pool_for_test();
    let exec_uuid = Uuid::new_v4();
    // Partition 0 per RFC-023 §4.1 A3 — num_flow_partitions = 1.
    let exec_id = ExecutionId::parse(&format!("{{fp:0}}:{exec_uuid}")).expect("construct exec_id");

    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, lane_id, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state, priority, created_at_ms
        ) VALUES (0, ?1, ?2, 0,
                  'runnable', 'unowned', 'eligible_now',
                  'pending', 'initial', 0, 1)
        "#,
    )
    .bind(exec_uuid)
    .bind(lane_id)
    .execute(pool)
    .await
    .expect("seed ff_exec_core");

    for cap in capabilities {
        sqlx::query(
            r#"
            INSERT INTO ff_execution_capabilities (execution_id, capability)
            VALUES (?1, ?2)
            "#,
        )
        .bind(exec_uuid)
        .bind(*cap)
        .execute(pool)
        .await
        .expect("seed ff_execution_capabilities");
    }

    (exec_id, exec_uuid)
}

/// Build a non-blocking `ClaimPolicy` with a deterministic worker
/// identity + a 30s lease TTL.
fn claim_policy() -> ClaimPolicy {
    ClaimPolicy::new(
        WorkerId::new("test-worker"),
        WorkerInstanceId::new("test-worker-instance"),
        30_000,
        None,
    )
}

/// Read one attempt row's terminal state for post-condition assertions.
async fn read_attempt_outcome(
    backend: &SqliteBackend,
    exec_uuid: Uuid,
    attempt_index: i64,
) -> Option<String> {
    let pool = backend.pool_for_test();
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT outcome FROM ff_attempt \
          WHERE partition_key = 0 AND execution_id = ?1 AND attempt_index = ?2",
    )
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_one(pool)
    .await
    .expect("read attempt outcome")
}

/// Read one exec_core row's `lifecycle_phase`.
async fn read_exec_phase(backend: &SqliteBackend, exec_uuid: Uuid) -> String {
    let pool = backend.pool_for_test();
    sqlx::query_scalar::<_, String>(
        "SELECT lifecycle_phase FROM ff_exec_core \
          WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read lifecycle_phase")
}

/// Read one exec_core row's `public_state`.
async fn read_exec_public_state(backend: &SqliteBackend, exec_uuid: Uuid) -> String {
    let pool = backend.pool_for_test();
    sqlx::query_scalar::<_, String>(
        "SELECT public_state FROM ff_exec_core \
          WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read public_state")
}

// ── claim ──────────────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_happy_path_mints_handle_and_transitions_state() {
    let backend = fresh_backend().await;
    let (_exec_id, exec_uuid) = seed_runnable_execution(&backend, "default", &["capA"]).await;

    let caps = CapabilitySet::new(["capA"]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("Some(handle)");

    // Handle must be SQLite-tagged and Fresh.
    assert_eq!(h.backend, BackendTag::Sqlite);
    assert_eq!(h.kind, HandleKind::Fresh);

    // exec_core flipped to active.
    assert_eq!(read_exec_phase(&backend, exec_uuid).await, "active");

    // `public_state = 'running'` parity write with Postgres
    // (`ff-backend-postgres/src/suspend_ops.rs:958-960`). The
    // Spine-B normaliser maps the raw literal back to
    // `PublicState::Active`; before the fix the `public_state`
    // column remained at its create-time `'waiting'` literal
    // (`'pending'` is the sibling `attempt_state` literal, not
    // `public_state`) and direct SQL readers observed the wrong
    // public state even though the inferred enum was correct.
    assert_eq!(
        read_exec_public_state(&backend, exec_uuid).await,
        "running"
    );

    // Attempt row created with no terminal outcome yet.
    assert_eq!(read_attempt_outcome(&backend, exec_uuid, 0).await, None);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_returns_none_when_no_eligible_rows() {
    let backend = fresh_backend().await;
    // No seed.
    let caps = CapabilitySet::new::<_, &str>([]);
    let r = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim");
    assert!(r.is_none());
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_capability_mismatch_skips_and_returns_none() {
    let backend = fresh_backend().await;
    let (_exec_id, exec_uuid) =
        seed_runnable_execution(&backend, "default", &["capA", "capB"]).await;

    // Worker does not carry capB — expect Ok(None), exec_core unchanged.
    let caps = CapabilitySet::new(["capA"]);
    let r = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim");
    assert!(r.is_none());
    // exec_core still runnable — no attempt row written.
    assert_eq!(read_exec_phase(&backend, exec_uuid).await, "runnable");
}

// ── complete ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_writes_terminal_and_clears_lease() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    backend
        .complete(&h, Some(b"result".to_vec()))
        .await
        .expect("complete");

    // exec_core is terminal now.
    assert_eq!(read_exec_phase(&backend, exec_uuid).await, "terminal");
    // Attempt outcome is success.
    assert_eq!(
        read_attempt_outcome(&backend, exec_uuid, 0).await,
        Some("success".into())
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn complete_fence_mismatch_returns_contention() {
    let backend = fresh_backend().await;
    let (eid, _exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    // Mint a second handle with a wrong epoch pointing at the same
    // attempt — decoder must accept the shape but fence_check must reject.
    let bad_payload = HandlePayload::new(
        eid.clone(),
        AttemptIndex::new(0),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(999), // deliberately wrong
        30_000,
        LaneId::new("default"),
        WorkerInstanceId::new("test-worker-instance"),
    );
    let bad_opaque = encode_opaque(BackendTag::Sqlite, &bad_payload);
    let bad_handle = Handle::new(BackendTag::Sqlite, HandleKind::Fresh, bad_opaque);

    let err = backend
        .complete(&bad_handle, None)
        .await
        .expect_err("fence mismatch must surface as Contention");
    assert!(
        matches!(err, EngineError::Contention(ContentionKind::LeaseConflict)),
        "expected LeaseConflict, got {err:?}"
    );

    // Good handle still works — the txn rolled back cleanly.
    backend.complete(&h, None).await.expect("complete");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn handle_from_valkey_rejected_on_complete() {
    let backend = fresh_backend().await;
    // Forge a Valkey-tagged handle — SqliteBackend must reject with
    // `HandleFromOtherBackend`.
    let payload = HandlePayload::new(
        ExecutionId::solo(&LaneId::new("default"), &PartitionConfig::default()),
        AttemptIndex::new(0),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(1),
        30_000,
        LaneId::new("default"),
        WorkerInstanceId::new("test-worker-instance"),
    );
    let valkey_opaque = encode_opaque(BackendTag::Valkey, &payload);
    let valkey_handle = Handle::new(BackendTag::Valkey, HandleKind::Fresh, valkey_opaque);

    let err = backend
        .complete(&valkey_handle, None)
        .await
        .expect_err("valkey-tagged handle must be rejected");
    match err {
        EngineError::Validation { kind, detail } => {
            assert_eq!(kind, ValidationKind::HandleFromOtherBackend);
            assert!(
                detail.contains("Valkey"),
                "detail should name the foreign backend: {detail:?}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

// ── fail ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn fail_transient_schedules_retry_and_reruns_runnable() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    let out = backend
        .fail(
            &h,
            FailureReason::new("transient boom"),
            FailureClass::Transient,
        )
        .await
        .expect("fail");
    use ff_core::backend::FailOutcome;
    assert!(
        matches!(out, FailOutcome::RetryScheduled { .. }),
        "expected RetryScheduled, got {out:?}"
    );

    // Execution is back to runnable with attempt_index bumped.
    assert_eq!(read_exec_phase(&backend, exec_uuid).await, "runnable");
    let pool = backend.pool_for_test();
    let ai: i64 = sqlx::query_scalar(
        "SELECT attempt_index FROM ff_exec_core \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read attempt_index");
    assert_eq!(ai, 1);

    // The original attempt (attempt_index=0) is marked 'retry'.
    assert_eq!(
        read_attempt_outcome(&backend, exec_uuid, 0).await,
        Some("retry".into())
    );
}

/// Capability subset check walks past higher-priority rows that the
/// worker cannot serve rather than returning `None` immediately. This
/// is the fix for PR-375 review finding on single-partition starvation.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_walks_past_capability_mismatch_to_match_lower_priority() {
    let backend = fresh_backend().await;
    let pool = backend.pool_for_test();

    // Seed two rows on the same lane:
    // * high-priority requires capB (worker lacks it)
    // * low-priority requires capA (worker has it)
    let hi = Uuid::new_v4();
    let lo = Uuid::new_v4();
    for (uuid, priority, cap) in [(hi, 10, "capB"), (lo, 0, "capA")] {
        sqlx::query(
            r#"
            INSERT INTO ff_exec_core (
                partition_key, execution_id, lane_id, attempt_index,
                lifecycle_phase, ownership_state, eligibility_state,
                public_state, attempt_state, priority, created_at_ms
            ) VALUES (0, ?1, 'default', 0,
                      'runnable', 'unowned', 'eligible_now',
                      'pending', 'initial', ?2, 1)
            "#,
        )
        .bind(uuid)
        .bind(priority)
        .execute(pool)
        .await
        .expect("seed exec_core");
        sqlx::query(
            "INSERT INTO ff_execution_capabilities (execution_id, capability) VALUES (?1, ?2)",
        )
        .bind(uuid)
        .bind(cap)
        .execute(pool)
        .await
        .expect("seed caps");
    }

    let caps = CapabilitySet::new(["capA"]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("claims the matching lower-priority row");

    // The low-priority row got claimed; the high-priority row is still
    // runnable.
    assert_eq!(h.backend, BackendTag::Sqlite);
    assert_eq!(read_exec_phase(&backend, lo).await, "active");
    assert_eq!(read_exec_phase(&backend, hi).await, "runnable");
}

/// claim + complete each emit a row into `ff_lease_event` (acquired /
/// revoked) so `subscribe_lease_history` readers observe a coherent
/// lifecycle. Mirrors the PG path via `lease_event::emit`.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_and_complete_emit_lease_events() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");
    backend.complete(&h, None).await.expect("complete");

    let pool = backend.pool_for_test();
    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT event_type, execution_id FROM ff_lease_event \
          WHERE execution_id = ?1 ORDER BY event_id ASC",
    )
    .bind(exec_uuid.to_string())
    .fetch_all(pool)
    .await
    .expect("read lease events");
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(types, vec!["acquired", "revoked"]);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn fail_permanent_writes_terminal_failed() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    let out = backend
        .fail(
            &h,
            FailureReason::new("permanent boom"),
            FailureClass::Permanent,
        )
        .await
        .expect("fail");
    use ff_core::backend::FailOutcome;
    assert!(
        matches!(out, FailOutcome::TerminalFailed),
        "expected TerminalFailed, got {out:?}"
    );

    assert_eq!(read_exec_phase(&backend, exec_uuid).await, "terminal");
    assert_eq!(
        read_attempt_outcome(&backend, exec_uuid, 0).await,
        Some("failed".into())
    );
}

// ── Phase 2a.3 — progress ───────────────────────────────────────────────

/// Helper: read `progress_pct` + `progress_message` back from the
/// raw_fields JSON document.
async fn read_progress(backend: &SqliteBackend, exec_uuid: Uuid) -> (Option<i64>, Option<String>) {
    let pool = backend.pool_for_test();
    let row: (Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT json_extract(raw_fields, '$.progress_pct'), \
                json_extract(raw_fields, '$.progress_message') \
         FROM ff_exec_core WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read progress");
    row
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn progress_writes_pct_and_message_to_raw_fields() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    backend
        .progress(&h, Some(42), Some("halfway".into()))
        .await
        .expect("progress");

    let (pct, msg) = read_progress(&backend, exec_uuid).await;
    assert_eq!(pct, Some(42));
    assert_eq!(msg, Some("halfway".into()));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn progress_preserves_prior_values_on_partial_update() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    backend
        .progress(&h, Some(10), Some("starting".into()))
        .await
        .expect("progress-1");

    // Second call only sets pct — message must remain "starting".
    backend
        .progress(&h, Some(75), None)
        .await
        .expect("progress-2");
    let (pct, msg) = read_progress(&backend, exec_uuid).await;
    assert_eq!(pct, Some(75));
    assert_eq!(msg, Some("starting".into()));

    // Third call only sets message — pct stays at 75.
    backend
        .progress(&h, None, Some("winding down".into()))
        .await
        .expect("progress-3");
    let (pct, msg) = read_progress(&backend, exec_uuid).await;
    assert_eq!(pct, Some(75));
    assert_eq!(msg, Some("winding down".into()));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn progress_fence_mismatch_returns_contention() {
    let backend = fresh_backend().await;
    let (eid, _exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    let bad_payload = HandlePayload::new(
        eid,
        AttemptIndex::new(0),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(999),
        30_000,
        LaneId::new("default"),
        WorkerInstanceId::new("test-worker-instance"),
    );
    let bad_opaque = encode_opaque(BackendTag::Sqlite, &bad_payload);
    let bad_handle = Handle::new(BackendTag::Sqlite, HandleKind::Fresh, bad_opaque);

    let err = backend
        .progress(&bad_handle, Some(50), None)
        .await
        .expect_err("fence mismatch must surface");
    assert!(
        matches!(err, EngineError::Contention(ContentionKind::LeaseConflict)),
        "expected LeaseConflict, got {err:?}"
    );
}

// ── Phase 2a.3 — renew ─────────────────────────────────────────────────

async fn read_lease_expiry(
    backend: &SqliteBackend,
    exec_uuid: Uuid,
    attempt_index: i64,
) -> Option<i64> {
    let pool = backend.pool_for_test();
    sqlx::query_scalar(
        "SELECT lease_expires_at_ms FROM ff_attempt \
         WHERE partition_key = 0 AND execution_id = ?1 AND attempt_index = ?2",
    )
    .bind(exec_uuid)
    .bind(attempt_index)
    .fetch_one(pool)
    .await
    .expect("read lease expiry")
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_advances_lease_expiry_and_emits_event() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    let expiry_before = read_lease_expiry(&backend, exec_uuid, 0).await.unwrap();
    // Sleep 2ms so the re-computed `now + ttl` advances beyond the
    // claim's `now + ttl` by a strictly positive amount.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    let renewal = backend.renew(&h).await.expect("renew");
    assert!(
        renewal.expires_at_ms as i64 > expiry_before,
        "renew must advance lease expiry (before={expiry_before}, after={})",
        renewal.expires_at_ms,
    );

    let expiry_after = read_lease_expiry(&backend, exec_uuid, 0).await.unwrap();
    assert_eq!(expiry_after, renewal.expires_at_ms as i64);

    let pool = backend.pool_for_test();
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM ff_lease_event \
         WHERE execution_id = ?1 ORDER BY event_id ASC",
    )
    .bind(exec_uuid.to_string())
    .fetch_all(pool)
    .await
    .expect("read lease events");
    assert_eq!(events, vec!["acquired", "renewed"]);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_fence_mismatch_returns_contention() {
    let backend = fresh_backend().await;
    let (eid, _exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    let bad_payload = HandlePayload::new(
        eid,
        AttemptIndex::new(0),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(999),
        30_000,
        LaneId::new("default"),
        WorkerInstanceId::new("test-worker-instance"),
    );
    let bad_opaque = encode_opaque(BackendTag::Sqlite, &bad_payload);
    let bad_handle = Handle::new(BackendTag::Sqlite, HandleKind::Fresh, bad_opaque);

    let err = backend
        .renew(&bad_handle)
        .await
        .expect_err("fence mismatch");
    assert!(
        matches!(err, EngineError::Contention(ContentionKind::LeaseConflict)),
        "expected LeaseConflict, got {err:?}"
    );
}

// ── Phase 2a.3 — append_frame ──────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn append_frame_durable_writes_frame_row() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    let frame = Frame::new(b"hello".to_vec(), FrameKind::Stdout);
    let out = backend.append_frame(&h, frame).await.expect("append");
    assert_eq!(out.frame_count, 1);
    assert!(out.stream_id.contains('-'));
    assert_eq!(out.summary_version, None);

    let pool = backend.pool_for_test();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ff_stream_frame \
         WHERE partition_key = 0 AND execution_id = ?1 AND attempt_index = 0",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("count");
    assert_eq!(count, 1);

    // Append a second frame → count=2.
    let frame2 = Frame::new(b"world".to_vec(), FrameKind::Stdout);
    let out2 = backend.append_frame(&h, frame2).await.expect("append 2");
    assert_eq!(out2.frame_count, 2);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn append_frame_summary_merges_document_and_bumps_version() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    // First delta: {"a":1}
    let f1 = Frame::new(br#"{"a":1}"#.to_vec(), FrameKind::Event).with_mode(
        StreamMode::DurableSummary {
            patch_kind: PatchKind::JsonMergePatch,
        },
    );
    let out1 = backend.append_frame(&h, f1).await.expect("append 1");
    assert_eq!(out1.summary_version, Some(1));

    // Second delta: {"b":2} — merges with prior.
    let f2 = Frame::new(br#"{"b":2}"#.to_vec(), FrameKind::Event).with_mode(
        StreamMode::DurableSummary {
            patch_kind: PatchKind::JsonMergePatch,
        },
    );
    let out2 = backend.append_frame(&h, f2).await.expect("append 2");
    assert_eq!(out2.summary_version, Some(2));

    let pool = backend.pool_for_test();
    let (doc, version): (String, i64) = sqlx::query_as(
        "SELECT document_json, version FROM ff_stream_summary \
         WHERE partition_key = 0 AND execution_id = ?1 AND attempt_index = 0",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read summary");
    assert_eq!(version, 2);
    let doc_val: serde_json::Value = serde_json::from_str(&doc).expect("parse doc");
    assert_eq!(doc_val["a"], serde_json::Value::from(1));
    assert_eq!(doc_val["b"], serde_json::Value::from(2));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn append_frame_fence_mismatch_returns_contention() {
    let backend = fresh_backend().await;
    let (eid, _exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    let bad_payload = HandlePayload::new(
        eid,
        AttemptIndex::new(0),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch(999),
        30_000,
        LaneId::new("default"),
        WorkerInstanceId::new("test-worker-instance"),
    );
    let bad_opaque = encode_opaque(BackendTag::Sqlite, &bad_payload);
    let bad_handle = Handle::new(BackendTag::Sqlite, HandleKind::Fresh, bad_opaque);

    let frame = Frame::new(b"x".to_vec(), FrameKind::Stdout);
    let err = backend
        .append_frame(&bad_handle, frame)
        .await
        .expect_err("fence mismatch");
    assert!(
        matches!(err, EngineError::Contention(ContentionKind::LeaseConflict)),
        "expected LeaseConflict, got {err:?}"
    );
}

// ── Phase 2a.3 — claim_from_reclaim ────────────────────────────────────

/// Set the current attempt's lease to expired (now - 1s) to simulate a
/// stale lease eligible for reclaim.
async fn expire_current_lease(backend: &SqliteBackend, exec_uuid: Uuid, attempt_index: i64) {
    let pool = backend.pool_for_test();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    sqlx::query(
        "UPDATE ff_attempt SET lease_expires_at_ms = ?1 \
         WHERE partition_key = 0 AND execution_id = ?2 AND attempt_index = ?3",
    )
    .bind(now_ms - 1_000)
    .bind(exec_uuid)
    .bind(attempt_index)
    .execute(pool)
    .await
    .expect("expire lease");
}

fn make_reclaim_token(exec_id: ExecutionId) -> ReclaimToken {
    let partition = Partition {
        family: PartitionFamily::Execution,
        index: 0,
    };
    let grant = ReclaimGrant {
        execution_id: exec_id,
        partition_key: PartitionKey::from(&partition),
        grant_key: "sqlite:test-grant".into(),
        expires_at_ms: u64::MAX,
        lane_id: LaneId::new("default"),
    };
    ReclaimToken::new(
        grant,
        WorkerId::new("reclaim-worker"),
        WorkerInstanceId::new("reclaim-worker-instance"),
        30_000,
    )
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_from_reclaim_expired_lease_mints_resumed_handle() {
    let backend = fresh_backend().await;
    let (eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    // Fence forward: the lease is currently live. Expire it.
    expire_current_lease(&backend, exec_uuid, 0).await;

    let token = make_reclaim_token(eid);
    let h = backend
        .claim_from_reclaim(token)
        .await
        .expect("claim_from_reclaim")
        .expect("Some(handle)");
    assert_eq!(h.backend, BackendTag::Sqlite);
    assert_eq!(h.kind, HandleKind::Resumed);

    // The attempt row's epoch bumped to 2 (initial acquire was 1, the
    // reclaim step adds 1 more).
    let pool = backend.pool_for_test();
    let epoch: i64 = sqlx::query_scalar(
        "SELECT lease_epoch FROM ff_attempt \
         WHERE partition_key = 0 AND execution_id = ?1 AND attempt_index = 0",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read epoch");
    assert_eq!(epoch, 2);

    let events: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM ff_lease_event \
         WHERE execution_id = ?1 ORDER BY event_id ASC",
    )
    .bind(exec_uuid.to_string())
    .fetch_all(pool)
    .await
    .expect("read lease events");
    assert_eq!(events, vec!["acquired", "reclaimed"]);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn claim_from_reclaim_live_lease_returns_none() {
    let backend = fresh_backend().await;
    let (eid, _exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    // Do not expire — lease is still live. Reclaim must return None.
    let token = make_reclaim_token(eid);
    let res = backend
        .claim_from_reclaim(token)
        .await
        .expect("claim_from_reclaim");
    assert!(res.is_none(), "live lease must block reclaim");
}

/// PR #376 Copilot review — with the prior field absent + a NULL bind,
/// SQLite's `json_set(x, '$.k', coalesce(NULL, NULL))` could
/// materialize an explicit JSON `null`, diverging from the PG
/// `raw_fields ||` no-op semantics. Assert that a partial `progress`
/// on a fresh row leaves the un-set field ABSENT, not JSON null.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn progress_partial_update_leaves_absent_field_absent() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    // Set only pct; message has never been written.
    backend
        .progress(&h, Some(25), None)
        .await
        .expect("progress pct-only");

    // `json_type('$.progress_message')` on an absent key returns NULL.
    // If the write path materialized `"progress_message": null`,
    // `json_type` would return `'null'` (the string literal).
    let pool = backend.pool_for_test();
    let msg_type: Option<String> = sqlx::query_scalar(
        "SELECT json_type(raw_fields, '$.progress_message') \
         FROM ff_exec_core WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read json_type");
    assert_eq!(
        msg_type, None,
        "absent field must remain absent, got json_type = {msg_type:?}"
    );
}

/// PR #376 Copilot review — `progress(None, None)` must be a no-op.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn progress_both_none_is_no_op() {
    let backend = fresh_backend().await;
    let (_eid, exec_uuid) = seed_runnable_execution(&backend, "default", &[]).await;

    let caps = CapabilitySet::new::<_, &str>([]);
    let h = backend
        .claim(&LaneId::new("default"), &caps, claim_policy())
        .await
        .expect("claim")
        .expect("handle");

    backend.progress(&h, None, None).await.expect("progress noop");

    let pool = backend.pool_for_test();
    let pct_type: Option<String> = sqlx::query_scalar(
        "SELECT json_type(raw_fields, '$.progress_pct') \
         FROM ff_exec_core WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("read json_type");
    assert_eq!(pct_type, None);
}
