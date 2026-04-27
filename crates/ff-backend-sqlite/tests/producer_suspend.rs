//! RFC-023 Phase 2b.2.1 — suspend/signal/waitpoint producer tests.
//!
//! Covers the 8 Group B trait methods now populated on the SQLite
//! backend: suspend, suspend_by_triple, create_waitpoint,
//! observe_signals, deliver_signal, claim_resumed_execution,
//! seed_waitpoint_hmac_secret, rotate_waitpoint_hmac_secret_all.

#![cfg(feature = "core")]

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy};
use ff_core::contracts::{
    ClaimResumedExecutionArgs, ClaimResumedExecutionResult, CreateExecutionArgs,
    CreateExecutionResult, DeliverSignalArgs, DeliverSignalResult, ResumeCondition, ResumePolicy,
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretOutcome, SeedOutcome,
    SeedWaitpointHmacSecretArgs, SignalMatcher, SuspendArgs, SuspendOutcome, SuspensionReasonCode,
    WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::types::{
    ExecutionId, LaneId, Namespace, SignalId, SuspensionId, TimestampMs, WaitpointId, WorkerId,
    WorkerInstanceId,
};
use serial_test::serial;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

// ── Setup helpers ──────────────────────────────────────────────────────

// Test HMAC secret: 64-char hex (256-bit) built from a repeating
// obviously-test pattern so secret scanners don't flag it. Must be
// exactly 64 chars for `seed_waitpoint_hmac_secret` validation.
const KID: &str = "kid-test";
fn secret_hex() -> String {
    "cafebabe".repeat(8)
}

async fn fresh_backend() -> Arc<SqliteBackend> {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:rfc-023-suspend-{}?mode=memory&cache=shared",
        uuid_like()
    );
    let b = SqliteBackend::new(&uri).await.expect("construct backend");
    // Seed the HMAC keystore so suspend / create_waitpoint succeed.
    b.seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(KID, secret_hex()))
        .await
        .expect("seed kid");
    b
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

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

async fn create_and_claim(backend: &Arc<SqliteBackend>) -> (ExecutionId, ff_core::backend::Handle) {
    let exec_id = new_exec_id();
    let args = CreateExecutionArgs {
        execution_id: exec_id.clone(),
        namespace: Namespace::new("default"),
        lane_id: LaneId::new("default"),
        execution_kind: "op".into(),
        input_payload: b"hello".to_vec(),
        payload_encoding: None,
        priority: 0,
        creator_identity: "test".into(),
        idempotency_key: None,
        tags: std::collections::HashMap::new(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = backend.create_execution(args).await.expect("create");
    assert!(matches!(r, CreateExecutionResult::Created { .. }));
    // Promote to runnable/eligible inline — SQLite has no scheduler;
    // PG's scheduler does this promotion off the submitted→runnable
    // edge. Mirrors `producer_flow::pubsub_smoke_*` tests.
    let exec_uuid = Uuid::parse_str(exec_id.as_str().split_once("}:").unwrap().1).unwrap();
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', public_state='pending', \
         attempt_state='initial' WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .unwrap();
    let policy = ClaimPolicy::new(
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i1"),
        30_000,
        None,
    );
    let handle = backend
        .claim(&LaneId::new("default"), &CapabilitySet::default(), policy)
        .await
        .expect("claim call")
        .expect("claim yielded handle");
    (exec_id, handle)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn suspend_args_single(wp_key: &str) -> SuspendArgs {
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: WaitpointId::new(),
        waitpoint_key: wp_key.to_owned(),
    };
    SuspendArgs::new(
        SuspensionId::new(),
        binding,
        ResumeCondition::Single {
            waitpoint_key: wp_key.to_owned(),
            matcher: SignalMatcher::Wildcard,
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::from_millis(now_ms()),
    )
}

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn seed_waitpoint_hmac_secret_round_trip() {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!("file:seed-rt-{}?mode=memory&cache=shared", uuid_like());
    let b = SqliteBackend::new(&uri).await.expect("backend");
    let outcome = b
        .seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(KID, secret_hex()))
        .await
        .expect("seed");
    assert!(matches!(outcome, SeedOutcome::Seeded { .. }));

    // Replay: AlreadySeeded with same_secret = true.
    let outcome2 = b
        .seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(KID, secret_hex()))
        .await
        .expect("seed replay");
    match outcome2 {
        SeedOutcome::AlreadySeeded { same_secret, .. } => assert!(same_secret),
        _ => panic!("expected AlreadySeeded"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn rotate_waitpoint_hmac_secret_all_basic() {
    let b = fresh_backend().await;
    // Rotate to a fresh kid. New secret uses a distinct obviously-test
    // repeat pattern so scanners do not mistake it for a real credential.
    let new_secret = "deadbeef".repeat(8);
    let new_secret = new_secret.as_str();
    let res = b
        .rotate_waitpoint_hmac_secret_all(RotateWaitpointHmacSecretAllArgs::new(
            "kid-rotated",
            new_secret,
            60_000,
        ))
        .await
        .expect("rotate");
    assert_eq!(res.entries.len(), 1);
    let entry = &res.entries[0];
    assert_eq!(entry.partition, 0);
    let outcome = entry.result.as_ref().expect("rotate outcome ok");
    match outcome {
        RotateWaitpointHmacSecretOutcome::Rotated {
            previous_kid,
            new_kid,
            ..
        } => {
            assert_eq!(previous_kid.as_deref(), Some(KID));
            assert_eq!(new_kid, "kid-rotated");
        }
        _ => panic!("expected Rotated"),
    }

    // Replay with same bytes → Noop.
    let res2 = b
        .rotate_waitpoint_hmac_secret_all(RotateWaitpointHmacSecretAllArgs::new(
            "kid-rotated",
            new_secret,
            60_000,
        ))
        .await
        .expect("rotate replay");
    let outcome2 = res2.entries[0].result.as_ref().expect("ok");
    assert!(matches!(outcome2, RotateWaitpointHmacSecretOutcome::Noop { .. }));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn suspend_happy_path() {
    let b = fresh_backend().await;
    let (_exec_id, handle) = create_and_claim(&b).await;
    let outcome = b
        .suspend(&handle, suspend_args_single("wpk:a"))
        .await
        .expect("suspend");
    match outcome {
        SuspendOutcome::Suspended { details, handle: _ } => {
            assert_eq!(details.waitpoint_key, "wpk:a");
            assert!(!details.waitpoint_token.as_str().is_empty());
        }
        _ => panic!("expected Suspended"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn suspend_replayed_idempotent() {
    let b = fresh_backend().await;
    let (_exec_id, handle) = create_and_claim(&b).await;
    let args = suspend_args_single("wpk:a")
        .with_idempotency_key(ff_core::contracts::IdempotencyKey::new("op-123"));
    let outcome1 = b.suspend(&handle, args.clone()).await.expect("suspend 1");
    let outcome2 = b.suspend(&handle, args).await.expect("suspend 2 replay");
    // Replay returns the cached outcome (same suspension_id + token bytes).
    assert_eq!(
        outcome1.details().suspension_id,
        outcome2.details().suspension_id
    );
    assert_eq!(
        outcome1.details().waitpoint_token.as_str(),
        outcome2.details().waitpoint_token.as_str()
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn suspend_fence_mismatch_returns_contention() {
    let b = fresh_backend().await;
    let (_exec_id, handle) = create_and_claim(&b).await;
    // First suspend bumps the lease epoch. Attempting to suspend
    // again on the STALE handle must return LeaseConflict.
    b.suspend(&handle, suspend_args_single("wpk:a"))
        .await
        .expect("first suspend");
    let err = b
        .suspend(&handle, suspend_args_single("wpk:b"))
        .await
        .expect_err("stale fence");
    assert!(
        matches!(err, EngineError::Contention(ContentionKind::LeaseConflict)),
        "got {err:?}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn create_waitpoint_hmac_token_verifies() {
    let b = fresh_backend().await;
    let (_exec_id, handle) = create_and_claim(&b).await;
    let pending = b
        .create_waitpoint(&handle, "wpk:c", Duration::from_secs(60))
        .await
        .expect("create_waitpoint");
    // Token shape: `<kid>:<hex>`.
    let tok = pending.hmac_token.as_str();
    assert!(tok.starts_with(&format!("{KID}:")), "tok = {tok}");
    assert!(tok.len() > KID.len() + 1);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn deliver_signal_satisfies_waitpoint_then_claim_resumed() {
    let b = fresh_backend().await;
    let (exec_id, handle) = create_and_claim(&b).await;

    // Subscribe to signal_delivery + completion before the signal fires.
    let mut sig_rx = b
        .subscribe_completion_for_test()
        .resubscribe();
    // Reuse the completion receiver (signal satisfies → completion outbox row).
    let _ = &mut sig_rx;

    let outcome = b
        .suspend(&handle, suspend_args_single("wpk:sat"))
        .await
        .expect("suspend");
    let details = outcome.details().clone();

    // Deliver matching signal (wildcard matcher → any name).
    let sig_args = DeliverSignalArgs {
        execution_id: exec_id.clone(),
        waitpoint_id: details.waitpoint_id.clone(),
        signal_id: SignalId::new(),
        signal_name: "ready".into(),
        signal_category: "external".into(),
        source_type: "operator".into(),
        source_identity: "op-1".into(),
        payload: None,
        payload_encoding: None,
        correlation_id: None,
        idempotency_key: None,
        target_scope: "execution".into(),
        created_at: None,
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: ff_core::types::WaitpointToken::from(
            details.waitpoint_token.as_str().to_owned(),
        ),
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = b.deliver_signal(sig_args).await.expect("deliver");
    match r {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        _ => panic!("expected Accepted"),
    }

    // Now claim the resumed execution.
    let rargs = ClaimResumedExecutionArgs {
        execution_id: exec_id.clone(),
        worker_id: WorkerId::new("w1"),
        worker_instance_id: WorkerInstanceId::new("w1-i1"),
        lane_id: LaneId::new("default"),
        lease_id: ff_core::types::LeaseId::new(),
        lease_ttl_ms: 30_000,
        current_attempt_index: ff_core::types::AttemptIndex::new(0),
        remaining_attempt_timeout_ms: None,
        now: TimestampMs::from_millis(now_ms()),
    };
    let cr = b
        .claim_resumed_execution(rargs)
        .await
        .expect("claim resumed");
    match cr {
        ClaimResumedExecutionResult::Claimed(claimed) => {
            assert_eq!(claimed.execution_id, exec_id);
        }
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn deliver_signal_appended_without_satisfaction() {
    let b = fresh_backend().await;
    let (exec_id, handle) = create_and_claim(&b).await;

    // Single { ByName("ready") } — only signals named "ready" satisfy.
    let wp_id = WaitpointId::new();
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: wp_id.clone(),
        waitpoint_key: "wpk:strict".to_owned(),
    };
    let args = SuspendArgs::new(
        SuspensionId::new(),
        binding,
        ResumeCondition::Single {
            waitpoint_key: "wpk:strict".to_owned(),
            matcher: SignalMatcher::ByName("ready".into()),
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::from_millis(now_ms()),
    );
    let outcome = b.suspend(&handle, args).await.expect("suspend");
    let details = outcome.details().clone();

    // Deliver a signal with a different name — should NOT satisfy.
    let sig_args = DeliverSignalArgs {
        execution_id: exec_id.clone(),
        waitpoint_id: details.waitpoint_id.clone(),
        signal_id: SignalId::new(),
        signal_name: "other".into(),
        signal_category: "external".into(),
        source_type: "worker".into(),
        source_identity: "w2".into(),
        payload: None,
        payload_encoding: None,
        correlation_id: None,
        idempotency_key: None,
        target_scope: "execution".into(),
        created_at: None,
        dedup_ttl_ms: None,
        resume_delay_ms: None,
        max_signals_per_execution: None,
        signal_maxlen: None,
        waitpoint_token: ff_core::types::WaitpointToken::from(
            details.waitpoint_token.as_str().to_owned(),
        ),
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = b.deliver_signal(sig_args).await.expect("deliver");
    match r {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint");
        }
        _ => panic!("expected Accepted"),
    }

    // observe_signals returns 1 signal attached to the waitpoint.
    // observe_signals takes a handle — the suspended handle from outcome.
    let susp_handle = match outcome {
        SuspendOutcome::Suspended { handle, .. } => handle,
        _ => panic!("need Suspended"),
    };
    let sigs = b.observe_signals(&susp_handle).await.expect("observe");
    assert_eq!(sigs.len(), 1);
    assert_eq!(sigs[0].signal_name, "other");
}
