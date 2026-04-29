//! RFC-023 Phase 3.3 — Wave 9 `list_pending_waitpoints` integration
//! tests. Covers the happy path, cursor pagination, empty result,
//! closed-state filtering, and the missing-execution NotFound gate.

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy};
use ff_core::contracts::{
    CreateExecutionArgs, CreateExecutionResult, ListPendingWaitpointsArgs, ResumeCondition,
    ResumePolicy, SeedWaitpointHmacSecretArgs, SignalMatcher, SuspendArgs, SuspensionReasonCode,
    WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::types::{
    ExecutionId, LaneId, Namespace, SuspensionId, TimestampMs, WaitpointId, WorkerId,
    WorkerInstanceId,
};
use serial_test::serial;
use uuid::Uuid;

// ── Setup ──────────────────────────────────────────────────────────

const KID: &str = "kid-test";
fn secret_hex() -> String {
    "cafebabe".repeat(8)
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

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn fresh_backend() -> Arc<SqliteBackend> {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:rfc-023-wave9-wp-{}?mode=memory&cache=shared",
        uuid_like()
    );
    let b = SqliteBackend::new(&uri).await.expect("backend");
    b.seed_waitpoint_hmac_secret(SeedWaitpointHmacSecretArgs::new(KID, secret_hex()))
        .await
        .expect("seed kid");
    b
}

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

async fn create_and_claim(
    b: &Arc<SqliteBackend>,
) -> (ExecutionId, ff_core::backend::Handle) {
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
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::from_millis(now_ms()),
    };
    let r = b.create_execution(args).await.expect("create");
    assert!(matches!(r, CreateExecutionResult::Created { .. }));
    let exec_uuid = Uuid::parse_str(exec_id.as_str().split_once("}:").unwrap().1).unwrap();
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='runnable', public_state='pending', \
         attempt_state='initial' WHERE partition_key=0 AND execution_id=?1",
    )
    .bind(exec_uuid)
    .execute(b.pool_for_test())
    .await
    .unwrap();
    let policy = ClaimPolicy::new(
        WorkerId::new("w1"),
        WorkerInstanceId::new("w1-i1"),
        30_000,
        None,
    );
    let handle = b
        .claim(&LaneId::new("default"), &CapabilitySet::default(), policy)
        .await
        .expect("claim")
        .expect("handle");
    (exec_id, handle)
}

/// Suspend on a bundle of N waitpoints so each one lands a
/// `ff_waitpoint_pending` row.
async fn suspend_n(
    b: &Arc<SqliteBackend>,
    handle: &ff_core::backend::Handle,
    n: usize,
) -> Vec<(WaitpointId, String)> {
    let mut bindings = Vec::with_capacity(n);
    for i in 0..n {
        bindings.push(WaitpointBinding::Fresh {
            waitpoint_id: WaitpointId::new(),
            waitpoint_key: format!("wpk:{i}"),
        });
    }
    let primary_key = match &bindings[0] {
        WaitpointBinding::Fresh { waitpoint_key, .. } => waitpoint_key.clone(),
        _ => unreachable!(),
    };
    let mut args = SuspendArgs::new(
        SuspensionId::new(),
        bindings[0].clone(),
        ResumeCondition::Single {
            waitpoint_key: primary_key,
            matcher: SignalMatcher::Wildcard,
        },
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::from_millis(now_ms()),
    );
    // Additional waitpoints go on via `waitpoints` vec (SuspendArgs
    // carries them directly).
    args.waitpoints = bindings.clone();

    let _ = b.suspend(handle, args).await.expect("suspend");
    bindings
        .into_iter()
        .map(|b| match b {
            WaitpointBinding::Fresh {
                waitpoint_id,
                waitpoint_key,
            } => (waitpoint_id, waitpoint_key),
            _ => unreachable!(),
        })
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────

#[tokio::test]
#[serial(ff_dev_mode)]
async fn list_pending_waitpoints_empty_for_no_suspension() {
    let b = fresh_backend().await;
    let (exec_id, _h) = create_and_claim(&b).await;
    let r = b
        .list_pending_waitpoints(ListPendingWaitpointsArgs::new(exec_id.clone()))
        .await
        .expect("ok");
    assert!(r.entries.is_empty());
    assert!(r.next_cursor.is_none());
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn list_pending_waitpoints_happy_path_returns_all_fields() {
    let b = fresh_backend().await;
    let (exec_id, handle) = create_and_claim(&b).await;
    let seeded = suspend_n(&b, &handle, 3).await;

    let r = b
        .list_pending_waitpoints(ListPendingWaitpointsArgs::new(exec_id.clone()))
        .await
        .expect("ok");
    assert_eq!(r.entries.len(), 3);
    assert!(r.next_cursor.is_none());

    for entry in &r.entries {
        assert_eq!(entry.execution_id, exec_id);
        assert_eq!(entry.state, "active", "suspend lands state='active'");
        assert!(!entry.waitpoint_key.is_empty());
        assert!(entry.activated_at.is_some());
        assert!(!entry.token_kid.is_empty(), "token_kid populated");
        assert!(!entry.token_fingerprint.is_empty(), "fingerprint populated");
        assert_eq!(
            entry.token_fingerprint.len(),
            16,
            "fingerprint is 16 hex chars = 8 bytes"
        );
    }

    // Every seeded waitpoint surfaces in the result.
    let returned: std::collections::HashSet<_> =
        r.entries.iter().map(|e| e.waitpoint_id.clone()).collect();
    for (wp_id, _) in &seeded {
        assert!(returned.contains(wp_id), "wp {wp_id:?} not in result");
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn list_pending_waitpoints_cursor_pagination() {
    let b = fresh_backend().await;
    let (exec_id, handle) = create_and_claim(&b).await;
    let _seeded = suspend_n(&b, &handle, 5).await;

    let page1 = b
        .list_pending_waitpoints(
            ListPendingWaitpointsArgs::new(exec_id.clone()).with_limit(2),
        )
        .await
        .expect("page 1");
    assert_eq!(page1.entries.len(), 2);
    let cursor = page1.next_cursor.expect("more rows remain");

    let page2 = b
        .list_pending_waitpoints(
            ListPendingWaitpointsArgs::new(exec_id.clone())
                .with_after(cursor.clone())
                .with_limit(2),
        )
        .await
        .expect("page 2");
    assert_eq!(page2.entries.len(), 2);
    let cursor2 = page2.next_cursor.expect("one more row remains");

    let page3 = b
        .list_pending_waitpoints(
            ListPendingWaitpointsArgs::new(exec_id.clone())
                .with_after(cursor2)
                .with_limit(2),
        )
        .await
        .expect("page 3");
    assert_eq!(page3.entries.len(), 1);
    assert!(page3.next_cursor.is_none());

    // Pages are distinct.
    let all_ids: Vec<_> = page1
        .entries
        .iter()
        .chain(page2.entries.iter())
        .chain(page3.entries.iter())
        .map(|e| e.waitpoint_id.clone())
        .collect();
    let set: std::collections::HashSet<_> = all_ids.iter().cloned().collect();
    assert_eq!(set.len(), 5, "5 distinct waitpoints across 3 pages");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn list_pending_waitpoints_missing_execution_returns_not_found() {
    let b = fresh_backend().await;
    let missing = new_exec_id();
    let err = b
        .list_pending_waitpoints(ListPendingWaitpointsArgs::new(missing))
        .await
        .expect_err("missing exec is NotFound");
    assert!(matches!(err, EngineError::NotFound { entity: "execution" }));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn list_pending_waitpoints_filters_closed_state() {
    let b = fresh_backend().await;
    let (exec_id, handle) = create_and_claim(&b).await;
    let seeded = suspend_n(&b, &handle, 2).await;

    // Flip one waitpoint to state='closed' directly — simulates a
    // satisfied/expired row the impl should hide.
    let target: Uuid = seeded[0].0.0;
    sqlx::query("UPDATE ff_waitpoint_pending SET state='closed' WHERE waitpoint_id=?1")
        .bind(target)
        .execute(b.pool_for_test())
        .await
        .unwrap();

    let r = b
        .list_pending_waitpoints(ListPendingWaitpointsArgs::new(exec_id.clone()))
        .await
        .expect("ok");
    assert_eq!(r.entries.len(), 1, "closed row filtered");
    assert_ne!(
        r.entries[0].waitpoint_id.0, target,
        "remaining entry is the non-closed one"
    );
}

/// Issue #434 — `EngineBackend::read_waitpoint_token` happy path on
/// SQLite. Suspend creates a waitpoint row with a HMAC token;
/// reading it back through the trait returns `Ok(Some(token))`.
/// A fresh `WaitpointId` that was never written returns `Ok(None)`.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn read_waitpoint_token_returns_some_for_existing_and_none_for_missing() {
    let b = fresh_backend().await;
    let (_exec_id, handle) = create_and_claim(&b).await;
    let seeded = suspend_n(&b, &handle, 1).await;
    let wp_id = seeded[0].0.clone();

    let partition = ff_core::partition::PartitionKey::from(
        &ff_core::partition::Partition {
            family: ff_core::partition::PartitionFamily::Flow,
            index: 0,
        },
    );

    let token = b
        .read_waitpoint_token(partition.clone(), &wp_id)
        .await
        .expect("read_waitpoint_token ok");
    let token = token.expect("waitpoint row exists → token populated");
    assert!(
        !token.is_empty() && token.contains(':'),
        "stored token should be kid-prefixed hex, got {token:?}"
    );

    // Missing waitpoint → Ok(None).
    let missing = b
        .read_waitpoint_token(partition, &WaitpointId::new())
        .await
        .expect("read_waitpoint_token ok for missing");
    assert!(missing.is_none(), "missing waitpoint should read back None");
}
