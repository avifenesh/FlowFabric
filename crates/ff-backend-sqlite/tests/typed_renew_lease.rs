//! #33 / Phase 1: integration test for SQLite
//! `EngineBackend::renew_lease`.
//!
//! Mirrors `ff-backend-postgres/tests/typed_renew_lease.rs`:
//!
//! - happy path extends `ff_attempt.lease_expires_at_ms`
//! - missing fence → `Validation{InvalidInput, "fence_required"}`
//! - stale fence (wrong epoch) → `State(StaleLease)`
//! - expired lease → `State(LeaseExpired)`
//! - non-active lifecycle → `Contention(ExecutionNotActive{...})`
//! - revoked ownership → `State(LeaseRevoked)`
//!
//! Runs against an in-memory `:memory:` DB (no env var gate). RFC-023
//! §4.1 A3 single-writer: partition_key = 0 on every seed.

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{RenewLeaseArgs, RenewLeaseResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LeaseEpoch, LeaseFence, LeaseId, TimestampMs,
};
use serial_test::serial;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
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
    // SAFETY: serial_test::serial(ff_dev_mode) wraps all callers.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!(
        "file:typed-renew-{}?mode=memory&cache=shared",
        uuid_like()
    );
    SqliteBackend::new(&uri)
        .await
        .expect("construct with migrations")
}

/// Seed an `active` + leased execution ready to be renewed.
/// Partition 0 per RFC-023 §4.1 A3. Returns the fence triple.
async fn seed_active_lease(
    backend: &SqliteBackend,
    lease_ttl_ms: i64,
) -> (Uuid, ExecutionId, LeaseFence) {
    let pool = backend.pool_for_test();
    let part: i64 = 0;
    let exec_uuid = Uuid::new_v4();
    let eid = ExecutionId::parse(&format!("{{fp:{part}}}:{exec_uuid}")).unwrap();
    let lease_id = LeaseId::new();
    let attempt_id = AttemptId::new();
    let lease_epoch: i64 = 1;
    let expires_at = now_ms() + lease_ttl_ms;

    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms, raw_fields
        ) VALUES (
            ?1, ?2, NULL, 'default',
            0,
            'active', 'leased', 'not_applicable',
            'active', 'running_attempt',
            0, ?3,
            json_set('{}', '$.current_attempt_id', ?4)
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .bind(attempt_id.to_string())
    .execute(pool)
    .await
    .expect("insert exec_core");

    sqlx::query(
        r#"
        INSERT INTO ff_attempt (
            partition_key, execution_id, attempt_index,
            worker_id, worker_instance_id,
            lease_epoch, lease_expires_at_ms, started_at_ms
        ) VALUES (?1, ?2, 0, 'w', 'wi', ?3, ?4, ?5)
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lease_epoch)
    .bind(expires_at)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("insert ff_attempt");

    let fence = LeaseFence {
        lease_id,
        lease_epoch: LeaseEpoch(lease_epoch as u64),
        attempt_id,
    };
    (exec_uuid, eid, fence)
}

async fn read_expires(backend: &SqliteBackend, exec_uuid: Uuid) -> i64 {
    use sqlx::Row;
    let pool = backend.pool_for_test();
    let row = sqlx::query(
        "SELECT lease_expires_at_ms FROM ff_attempt \
         WHERE partition_key = 0 AND execution_id = ?1 AND attempt_index = 0",
    )
    .bind(exec_uuid)
    .fetch_one(pool)
    .await
    .expect("fetch");
    row.try_get::<i64, _>("lease_expires_at_ms").unwrap()
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_lease_extends_expires_at_on_happy_path() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active_lease(&backend, 5_000).await;
    let pre = read_expires(&backend, exec_uuid).await;

    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    let res = be.renew_lease(args).await.expect("renew ok");
    let RenewLeaseResult::Renewed { expires_at } = res;

    let post = read_expires(&backend, exec_uuid).await;
    assert!(post > pre, "expires_at should move forward pre={pre} post={post}");
    assert_eq!(expires_at, TimestampMs::from_millis(post));
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_lease_rejects_missing_fence() {
    let backend = fresh_backend().await;
    let (_uuid, eid, _fence) = seed_active_lease(&backend, 5_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: None,
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match be.renew_lease(args).await {
        Err(EngineError::Validation { kind, detail }) => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert_eq!(detail, "fence_required");
        }
        other => panic!("expected Validation{{fence_required}}, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_lease_stale_lease_on_wrong_epoch() {
    let backend = fresh_backend().await;
    let (_uuid, eid, fence) = seed_active_lease(&backend, 5_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let bad = LeaseFence {
        lease_epoch: LeaseEpoch(fence.lease_epoch.0 + 99),
        ..fence
    };
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(bad),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match be.renew_lease(args).await {
        Err(EngineError::State(StateKind::StaleLease)) => {}
        other => panic!("expected StaleLease, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_lease_expired_when_past_ttl() {
    let backend = fresh_backend().await;
    // Seed already-expired: negative TTL.
    let (_uuid, eid, fence) = seed_active_lease(&backend, -5_000).await;
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match be.renew_lease(args).await {
        Err(EngineError::State(StateKind::LeaseExpired)) => {}
        other => panic!("expected LeaseExpired, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_lease_execution_not_active_on_terminal() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active_lease(&backend, 5_000).await;
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .expect("flip terminal");
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match be.renew_lease(args).await {
        Err(EngineError::Contention(ContentionKind::ExecutionNotActive {
            lifecycle_phase,
            ..
        })) => {
            assert_eq!(lifecycle_phase, "terminal");
        }
        other => panic!("expected ExecutionNotActive, got {other:?}"),
    }
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn renew_lease_revoked_when_ownership_lease_revoked() {
    let backend = fresh_backend().await;
    let (exec_uuid, eid, fence) = seed_active_lease(&backend, 5_000).await;
    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state = 'lease_revoked' \
         WHERE partition_key = 0 AND execution_id = ?1",
    )
    .bind(exec_uuid)
    .execute(backend.pool_for_test())
    .await
    .expect("flip revoked");
    let be: Arc<dyn EngineBackend> = backend.clone();
    let args = RenewLeaseArgs {
        execution_id: eid,
        attempt_index: AttemptIndex::new(0),
        fence: Some(fence),
        lease_ttl_ms: 60_000,
        lease_history_grace_ms: 60_000,
    };
    match be.renew_lease(args).await {
        Err(EngineError::State(StateKind::LeaseRevoked)) => {}
        other => panic!("expected LeaseRevoked, got {other:?}"),
    }
}
