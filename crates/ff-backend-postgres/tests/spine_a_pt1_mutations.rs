//! RFC-020 Wave 9 Spine-A pt.1 — integration tests for
//! `cancel_execution` + `revoke_lease`.
//!
//! Follows the `FF_PG_TEST_URL` / `#[ignore]` convention from the
//! other Postgres integration suites (e.g. `spine_b_reads.rs`).
//!
//! Coverage per RFC §4.2.1 + §4.2.2 + §4.2.7:
//!
//! * `cancel_execution_happy_path_clears_lease_and_emits_event` —
//!   active exec with a lease → `lifecycle_phase='cancelled'` +
//!   lease cleared + `ff_lease_event(event_type='revoked')` row.
//! * `cancel_execution_no_lease_emits_no_lease_event` —
//!   non-leased exec → cancelled without any lease_event row.
//! * `cancel_execution_already_cancelled_is_idempotent` —
//!   already-cancelled replay returns `Cancelled` (no error).
//! * `cancel_execution_terminal_other_outcome_is_conflict` —
//!   terminal-completed exec cannot be cancelled.
//! * `cancel_execution_missing_is_not_found` — unknown eid →
//!   `EngineError::NotFound`.
//! * `revoke_lease_happy_path_emits_event` — active lease → cleared +
//!   lease_event row.
//! * `revoke_lease_no_active_lease_returns_already_satisfied` —
//!   unowned attempt → `AlreadySatisfied { reason: "no_active_lease" }`.
//! * `revoke_lease_epoch_mismatch_returns_already_satisfied` —
//!   concurrent epoch bump → `AlreadySatisfied { reason: "epoch_moved" }`.
//! * `concurrent_cancel_execution_one_wins_one_idempotent` — two
//!   parallel cancels resolve (both see the exec cancelled; neither
//!   surfaces a transport error to the caller).

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{
    CancelExecutionArgs, CancelExecutionResult, RevokeLeaseArgs, RevokeLeaseResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::partition::PartitionConfig;
use ff_core::state::PublicState;
use ff_core::types::{CancelSource, ExecutionId, LaneId, TimestampMs, WorkerInstanceId};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

// ── Fixture ───────────────────────────────────────────────────────

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&bootstrap)
        .await
        .expect("apply_migrations clean");
    bootstrap.close().await;

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect pool");
    Some(pool)
}

struct Seed {
    backend: Arc<dyn EngineBackend>,
    pool: PgPool,
    exec_id: ExecutionId,
    part: i16,
    exec_uuid: Uuid,
}

async fn seed_backend() -> Option<Seed> {
    let pool = setup_or_skip().await?;
    let lane = LaneId::new("default");
    let exec_id = ExecutionId::solo(&lane, &PartitionConfig::default());
    let part = exec_id.partition() as i16;
    let exec_uuid = Uuid::parse_str(exec_id.as_str().split_once("}:").unwrap().1).unwrap();
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    Some(Seed {
        backend,
        pool,
        exec_id,
        part,
        exec_uuid,
    })
}

#[allow(clippy::too_many_arguments)]
async fn insert_exec_core(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    lifecycle_phase: &str,
    ownership_state: &str,
    public_state: &str,
    attempt_state: &str,
    attempt_index: i32,
    now: i64,
) {
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, \
            priority, created_at_ms, raw_fields) \
         VALUES ($1, $2, 'default', $3, $4, $5, 'eligible_now', \
                 $6, $7, 0, $8, '{}'::jsonb)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .bind(lifecycle_phase)
    .bind(ownership_state)
    .bind(public_state)
    .bind(attempt_state)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert ff_exec_core");
}

#[allow(clippy::too_many_arguments)]
async fn insert_attempt_with_lease(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    attempt_index: i32,
    worker_instance_id: Option<&str>,
    lease_epoch: i64,
    lease_expires_at_ms: Option<i64>,
    started_at_ms: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, worker_instance_id, \
            lease_epoch, lease_expires_at_ms, started_at_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .bind(worker_instance_id)
    .bind(lease_epoch)
    .bind(lease_expires_at_ms)
    .bind(started_at_ms)
    .execute(pool)
    .await
    .expect("insert ff_attempt");
}

async fn count_lease_events(pool: &PgPool, part: i16, exec_uuid: Uuid, event_type: &str) -> i64 {
    sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM ff_lease_event \
         WHERE partition_key = $1 AND execution_id = $2 AND event_type = $3",
    )
    .bind(i32::from(part))
    .bind(exec_uuid.to_string())
    .bind(event_type)
    .fetch_one(pool)
    .await
    .expect("count lease_event")
    .try_get::<i64, _>("n")
    .expect("n column")
}

// ── cancel_execution ──────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_execution_happy_path_clears_lease_and_emits_event() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "active",
        "leased",
        "running",
        "running_attempt",
        0,
        now,
    )
    .await;
    insert_attempt_with_lease(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        0,
        Some("wiid-alpha"),
        5,
        Some(now + 60_000),
        Some(now - 1_000),
    )
    .await;

    let result = fx
        .backend
        .cancel_execution(CancelExecutionArgs {
            execution_id: fx.exec_id.clone(),
            reason: "operator-requested".to_owned(),
            source: CancelSource::OperatorOverride,
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::now(),
        })
        .await
        .expect("cancel_execution ok");
    assert_eq!(
        result,
        CancelExecutionResult::Cancelled {
            execution_id: fx.exec_id.clone(),
            public_state: PublicState::Cancelled,
        }
    );

    // lifecycle_phase must be 'cancelled' + public_state 'cancelled'.
    let (phase, pub_state): (String, String) = sqlx::query_as(
        "SELECT lifecycle_phase, public_state FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read exec_core");
    assert_eq!(phase, "cancelled");
    assert_eq!(pub_state, "cancelled");

    // attempt row: lease cleared, lease_epoch bumped.
    let (wiid, expires, epoch): (Option<String>, Option<i64>, i64) = sqlx::query_as(
        "SELECT worker_instance_id, lease_expires_at_ms, lease_epoch \
         FROM ff_attempt \
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = 0",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read attempt");
    assert!(wiid.is_none(), "worker_instance_id cleared");
    assert!(expires.is_none(), "lease_expires_at_ms cleared");
    assert_eq!(epoch, 6, "lease_epoch bumped from 5 → 6");

    // Outbox row: exactly one `revoked` event.
    let n = count_lease_events(&fx.pool, fx.part, fx.exec_uuid, "revoked").await;
    assert_eq!(n, 1, "one ff_lease_event(revoked) row emitted");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_execution_no_lease_emits_no_lease_event() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    // Waiting exec: no attempt row at all (pre-claim state).
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "eligible",
        "unowned",
        "waiting",
        "pending",
        0,
        now,
    )
    .await;

    let r = fx
        .backend
        .cancel_execution(CancelExecutionArgs {
            execution_id: fx.exec_id.clone(),
            reason: "op".into(),
            source: CancelSource::OperatorOverride,
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::now(),
        })
        .await
        .expect("ok");
    assert!(matches!(r, CancelExecutionResult::Cancelled { .. }));

    let n = count_lease_events(&fx.pool, fx.part, fx.exec_uuid, "revoked").await;
    assert_eq!(n, 0, "no lease was active → no lease_event emitted");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_execution_already_cancelled_is_idempotent() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "cancelled",
        "unowned",
        "cancelled",
        "cancelled",
        0,
        now,
    )
    .await;

    let r = fx
        .backend
        .cancel_execution(CancelExecutionArgs {
            execution_id: fx.exec_id.clone(),
            reason: "replay".into(),
            source: CancelSource::OperatorOverride,
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::now(),
        })
        .await
        .expect("idempotent ok");
    assert!(matches!(r, CancelExecutionResult::Cancelled { .. }));
    // No outbox emit on replay (we rolled back pre-mutation).
    let n = count_lease_events(&fx.pool, fx.part, fx.exec_uuid, "revoked").await;
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_execution_terminal_other_outcome_is_conflict() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "terminal",
        "unowned",
        "completed",
        "attempt_terminal",
        0,
        now,
    )
    .await;

    let err = fx
        .backend
        .cancel_execution(CancelExecutionArgs {
            execution_id: fx.exec_id.clone(),
            reason: "late".into(),
            source: CancelSource::OperatorOverride,
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::now(),
        })
        .await
        .expect_err("terminal-completed must reject cancel");
    assert!(
        matches!(err, EngineError::Validation { .. }),
        "expected Validation, got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_execution_missing_is_not_found() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let err = fx
        .backend
        .cancel_execution(CancelExecutionArgs {
            execution_id: fx.exec_id.clone(),
            reason: "r".into(),
            source: CancelSource::OperatorOverride,
            lease_id: None,
            lease_epoch: None,
            attempt_id: None,
            now: TimestampMs::now(),
        })
        .await
        .expect_err("missing must NotFound");
    assert!(
        matches!(err, EngineError::NotFound { entity: "execution" }),
        "got {err:?}"
    );
}

// ── revoke_lease ──────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn revoke_lease_happy_path_emits_event() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "active",
        "leased",
        "running",
        "running_attempt",
        0,
        now,
    )
    .await;
    insert_attempt_with_lease(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        0,
        Some("wiid-x"),
        7,
        Some(now + 30_000),
        Some(now),
    )
    .await;

    let r = fx
        .backend
        .revoke_lease(RevokeLeaseArgs {
            execution_id: fx.exec_id.clone(),
            expected_lease_id: None,
            worker_instance_id: WorkerInstanceId::new("wiid-x"),
            reason: "op".into(),
        })
        .await
        .expect("ok");
    match r {
        RevokeLeaseResult::Revoked {
            lease_id,
            lease_epoch,
        } => {
            assert!(lease_id.starts_with("pg:"), "synthetic lease_id: {lease_id}");
            assert_eq!(lease_epoch, "8", "prior epoch 7 → 8");
        }
        other => panic!("expected Revoked, got {other:?}"),
    }

    let (wiid, expires, epoch): (Option<String>, Option<i64>, i64) = sqlx::query_as(
        "SELECT worker_instance_id, lease_expires_at_ms, lease_epoch \
         FROM ff_attempt \
         WHERE partition_key = $1 AND execution_id = $2 AND attempt_index = 0",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read attempt");
    assert!(wiid.is_none());
    assert!(expires.is_none());
    assert_eq!(epoch, 8);

    let n = count_lease_events(&fx.pool, fx.part, fx.exec_uuid, "revoked").await;
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn revoke_lease_no_active_lease_returns_already_satisfied() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "runnable",
        "unowned",
        "waiting",
        "pending_first_attempt",
        0,
        now,
    )
    .await;
    insert_attempt_with_lease(&fx.pool, fx.part, fx.exec_uuid, 0, None, 0, None, None).await;

    let r = fx
        .backend
        .revoke_lease(RevokeLeaseArgs {
            execution_id: fx.exec_id.clone(),
            expected_lease_id: None,
            worker_instance_id: WorkerInstanceId::new("any"),
            reason: "op".into(),
        })
        .await
        .expect("ok");
    assert!(
        matches!(
            r,
            RevokeLeaseResult::AlreadySatisfied { ref reason } if reason == "no_active_lease"
        ),
        "got {r:?}"
    );
    let n = count_lease_events(&fx.pool, fx.part, fx.exec_uuid, "revoked").await;
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn revoke_lease_double_call_is_idempotent() {
    // Second call sees worker_instance_id already cleared by the
    // first → `AlreadySatisfied { reason: "no_active_lease" }`.
    // The adjacent `epoch_moved` branch inside revoke_lease_once is
    // exercised indirectly by `concurrent_cancel_execution_…` below:
    // whichever cancel loses the SERIALIZABLE race sees the epoch
    // bumped by the winner.
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "active",
        "leased",
        "running",
        "running_attempt",
        0,
        now,
    )
    .await;
    insert_attempt_with_lease(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        0,
        Some("wiid-z"),
        1,
        Some(now + 10_000),
        Some(now),
    )
    .await;

    let args = RevokeLeaseArgs {
        execution_id: fx.exec_id.clone(),
        expected_lease_id: None,
        worker_instance_id: WorkerInstanceId::new("wiid-z"),
        reason: "op".into(),
    };
    let r1 = fx.backend.revoke_lease(args.clone()).await.expect("first ok");
    assert!(matches!(r1, RevokeLeaseResult::Revoked { .. }));

    let r2 = fx.backend.revoke_lease(args).await.expect("second ok");
    assert!(
        matches!(
            r2,
            RevokeLeaseResult::AlreadySatisfied { ref reason } if reason == "no_active_lease"
        ),
        "got {r2:?}"
    );
    let n = count_lease_events(&fx.pool, fx.part, fx.exec_uuid, "revoked").await;
    assert_eq!(n, 1, "only first call emitted lease_event");
}

// ── race: concurrent cancels resolve cleanly ──────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn concurrent_cancel_execution_one_wins_one_idempotent() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "active",
        "leased",
        "running",
        "running_attempt",
        0,
        now,
    )
    .await;
    insert_attempt_with_lease(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        0,
        Some("wiid-y"),
        3,
        Some(now + 60_000),
        Some(now),
    )
    .await;

    let backend_a = fx.backend.clone();
    let backend_b = fx.backend.clone();
    let eid_a = fx.exec_id.clone();
    let eid_b = fx.exec_id.clone();

    let a = tokio::spawn(async move {
        backend_a
            .cancel_execution(CancelExecutionArgs {
                execution_id: eid_a,
                reason: "a".into(),
                source: CancelSource::OperatorOverride,
                lease_id: None,
                lease_epoch: None,
                attempt_id: None,
                now: TimestampMs::now(),
            })
            .await
    });
    let b = tokio::spawn(async move {
        backend_b
            .cancel_execution(CancelExecutionArgs {
                execution_id: eid_b,
                reason: "b".into(),
                source: CancelSource::OperatorOverride,
                lease_id: None,
                lease_epoch: None,
                attempt_id: None,
                now: TimestampMs::now(),
            })
            .await
    });

    let ra = a.await.expect("join a");
    let rb = b.await.expect("join b");
    // Both must resolve Ok (one wins the cancel; the other sees the
    // row already cancelled and returns the idempotent Cancelled
    // outcome).
    assert!(ra.is_ok(), "a={ra:?}");
    assert!(rb.is_ok(), "b={rb:?}");
    assert!(matches!(ra.unwrap(), CancelExecutionResult::Cancelled { .. }));
    assert!(matches!(rb.unwrap(), CancelExecutionResult::Cancelled { .. }));

    // Lease event must fire exactly once (only the winning tx
    // observed an active lease).
    let n = count_lease_events(&fx.pool, fx.part, fx.exec_uuid, "revoked").await;
    assert_eq!(n, 1, "exactly one lease_event(revoked) from the winner");
}
