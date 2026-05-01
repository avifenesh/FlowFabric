//! Cairn #454 Phase 4b — integration tests for `EngineBackend::
//! deliver_approval_signal` on Postgres.
//!
//! Four `#[ignore]` + `FF_PG_TEST_URL`-gated scenarios that mirror
//! the Valkey coverage landed in PR #465:
//!
//! * approved path — operator delivers `"approved"`, execution
//!   resumes (matching waitpoint signal, resume-condition satisfied).
//! * rejected path — operator delivers `"rejected"` without a
//!   matching condition, signal is appended to the waitpoint but
//!   does not resume.
//! * missing waitpoint — unknown `waitpoint_id` → `NotFound`.
//! * lane mismatch — caller lane ≠ `ff_exec_core.lane_id` →
//!   `Validation(InvalidInput, "lane_mismatch: ...")`.
//!
//! The fixture pattern (per-test `ffpg_test_<uuid>` schema for the
//! global HMAC keystore, canonical partitioned tables in `public`)
//! matches `tests/suspend_signal.rs` — same `setup_or_skip` +
//! `setup_exec_or_skip` shape.

use ff_backend_postgres::signal::rotate_waitpoint_hmac_secret_all_impl;
use ff_backend_postgres::PostgresBackend;
use ff_core::backend::{BackendTag, Handle, HandleKind};
use ff_core::contracts::{
    DeliverApprovalSignalArgs, DeliverSignalResult, ResumeCondition, ResumePolicy,
    RotateWaitpointHmacSecretAllArgs, SignalMatcher, SuspendArgs, SuspendOutcome,
    SuspensionReasonCode, WaitpointBinding,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::handle_codec::{encode as encode_opaque, HandlePayload};
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, LaneId, LeaseEpoch, LeaseId, SuspensionId, TimestampMs,
    WaitpointId, WorkerInstanceId,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

// Per-test isolation: apply migrations to `public`, create a
// throwaway schema that shadows `ff_waitpoint_hmac` so parallel tests
// don't race on the global kid rotation table.
async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let bootstrap = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect");
    ff_backend_postgres::apply_migrations(&bootstrap)
        .await
        .expect("migrate");
    let schema = format!("ffpg_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&bootstrap)
        .await
        .expect("create schema");
    sqlx::query(&format!(
        "CREATE TABLE {schema}.ff_waitpoint_hmac \
         (LIKE public.ff_waitpoint_hmac INCLUDING ALL)"
    ))
    .execute(&bootstrap)
    .await
    .expect("shadow ff_waitpoint_hmac");
    bootstrap.close().await;

    let schema_for_hook = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |conn, _meta| {
            let schema = schema_for_hook.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO {schema}, public"))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect");
    Some(pool)
}

struct Fixture {
    backend: Arc<dyn EngineBackend>,
    #[allow(dead_code)]
    pool: PgPool,
    exec_id: ExecutionId,
    handle: Handle,
    lane: LaneId,
}

async fn setup_exec_or_skip() -> Option<Fixture> {
    let pool = setup_or_skip().await?;
    rotate_waitpoint_hmac_secret_all_impl(
        &pool,
        RotateWaitpointHmacSecretAllArgs::new("kid-approval-1", "ab".repeat(32), 0),
        1_000_000,
    )
    .await
    .unwrap();

    let lane = LaneId::new("default");
    let exec_id = ExecutionId::solo(&lane, &PartitionConfig::default());
    let part = exec_id.partition() as i16;
    let exec_uuid = Uuid::parse_str(exec_id.as_str().split_once("}:").unwrap().1).unwrap();

    let now = TimestampMs::now().0;
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, created_at_ms) \
         VALUES ($1, $2, $3, 0, 'active', 'leased', 'not_applicable', \
                 'running', 'running_attempt', $4)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lane.as_str())
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert exec_core");

    let attempt_index = AttemptIndex::new(0);
    let lease_epoch = LeaseEpoch(1);
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, worker_id, \
            worker_instance_id, lease_epoch, lease_expires_at_ms, started_at_ms) \
         VALUES ($1, $2, 0, 'w1', 'wi1', 1, $3, $4)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now + 30_000)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert attempt");

    let payload = HandlePayload::new(
        exec_id.clone(),
        attempt_index,
        AttemptId::new(),
        LeaseId::new(),
        lease_epoch,
        30_000,
        lane.clone(),
        WorkerInstanceId::new("wi1"),
    );
    let opaque = encode_opaque(BackendTag::Postgres, &payload);
    let handle = Handle::new(BackendTag::Postgres, HandleKind::Fresh, opaque);

    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default())
        as Arc<dyn EngineBackend>;

    Some(Fixture {
        backend,
        pool,
        exec_id,
        handle,
        lane,
    })
}

async fn suspend_on(fx: &Fixture, wp_key: &str, signal_name: &str) -> WaitpointId {
    let wp_id = WaitpointId::new();
    let binding = WaitpointBinding::Fresh {
        waitpoint_id: wp_id.clone(),
        waitpoint_key: wp_key.to_owned(),
    };
    let cond = ResumeCondition::Single {
        waitpoint_key: wp_key.to_owned(),
        matcher: SignalMatcher::ByName(signal_name.to_owned()),
    };
    let args = SuspendArgs::new(
        SuspensionId::new(),
        binding,
        cond,
        ResumePolicy::normal(),
        SuspensionReasonCode::WaitingForSignal,
        TimestampMs::now(),
    );
    let outcome = fx.backend.suspend(&fx.handle, args).await.expect("suspend");
    assert!(matches!(outcome, SuspendOutcome::Suspended { .. }));
    wp_id
}

fn approval_args(
    exec_id: &ExecutionId,
    lane: &LaneId,
    wp_id: &WaitpointId,
    signal_name: &str,
    suffix: &str,
) -> DeliverApprovalSignalArgs {
    DeliverApprovalSignalArgs::new(
        exec_id.clone(),
        lane.clone(),
        wp_id.clone(),
        signal_name,
        suffix,
        60_000,
        None,
        None,
    )
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn approved_path_resumes_execution() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };
    let wp_id = suspend_on(&fx, "wpk:approval:ok", "approved").await;
    let args = approval_args(&fx.exec_id, &fx.lane, &wp_id, "approved", "dec-1");
    let out = fx
        .backend
        .deliver_approval_signal(args)
        .await
        .expect("deliver_approval_signal ok");
    match out {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "resume_condition_satisfied");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn rejected_path_appends_without_resume() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };
    // Condition requires "approved"; delivering "rejected" should be
    // appended but does not satisfy the resume condition.
    let wp_id = suspend_on(&fx, "wpk:approval:reject", "approved").await;
    let args = approval_args(&fx.exec_id, &fx.lane, &wp_id, "rejected", "dec-1");
    let out = fx
        .backend
        .deliver_approval_signal(args)
        .await
        .expect("deliver_approval_signal ok");
    match out {
        DeliverSignalResult::Accepted { effect, .. } => {
            assert_eq!(effect, "appended_to_waitpoint");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn missing_waitpoint_returns_not_found() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };
    let bogus = WaitpointId::new();
    let args = approval_args(&fx.exec_id, &fx.lane, &bogus, "approved", "dec-1");
    match fx.backend.deliver_approval_signal(args).await.unwrap_err() {
        EngineError::NotFound { entity } => assert_eq!(entity, "waitpoint"),
        other => panic!("expected NotFound(waitpoint), got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn lane_mismatch_is_validation_error() {
    let Some(fx) = setup_exec_or_skip().await else {
        return;
    };
    let wp_id = suspend_on(&fx, "wpk:approval:lane", "approved").await;
    let wrong_lane = LaneId::new("not-default");
    let args = approval_args(&fx.exec_id, &wrong_lane, &wp_id, "approved", "dec-1");
    match fx.backend.deliver_approval_signal(args).await.unwrap_err() {
        EngineError::Validation { kind, detail } => {
            assert_eq!(kind, ValidationKind::InvalidInput);
            assert!(
                detail.starts_with("lane_mismatch:"),
                "unexpected detail: {detail}"
            );
        }
        other => panic!("expected Validation(InvalidInput), got {other:?}"),
    }
}
