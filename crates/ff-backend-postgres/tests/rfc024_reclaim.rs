//! RFC-024 PR-D — issue_reclaim_grant + reclaim_execution integration
//! tests against a live Postgres with migration 0017 applied.
//!
//! Running:
//!   FF_PG_TEST_URL=postgres://... cargo test -p ff-backend-postgres
//!     --test rfc024_reclaim -- --ignored

use ff_backend_postgres::PostgresBackend;
use ff_core::backend::HandleKind;
use ff_core::contracts::{
    IssueReclaimGrantArgs, IssueReclaimGrantOutcome, ReclaimExecutionArgs,
    ReclaimExecutionOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, ExecutionId, LaneId, LeaseId, TimestampMs, WorkerId, WorkerInstanceId,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::BTreeSet;
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

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    sqlx::query(
        r#"
        INSERT INTO ff_waitpoint_hmac (kid, secret, rotated_at_ms, active)
        VALUES ('rfc024-test', decode('0102030405060708', 'hex'), $1, true)
        ON CONFLICT (kid) DO NOTHING
        "#,
    )
    .bind(now_ms())
    .execute(&pool)
    .await
    .expect("seed hmac");
    Some(pool)
}

async fn seed_exec(
    pool: &PgPool,
    lane: &str,
    ownership_state: &str,
    lifecycle_phase: &str,
    lease_reclaim_count: i32,
) -> (i16, Uuid, LaneId) {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms, lease_reclaim_count
        ) VALUES (
            $1, $2, NULL, $3,
            '{}', 0,
            $5, $4, 'not_applicable',
            'running', 'running_attempt',
            0, $6, $7
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lane)
    .bind(ownership_state)
    .bind(lifecycle_phase)
    .bind(now_ms())
    .bind(lease_reclaim_count)
    .execute(pool)
    .await
    .expect("seed exec");
    sqlx::query(
        r#"
        INSERT INTO ff_attempt (
            partition_key, execution_id, attempt_index,
            worker_id, worker_instance_id,
            lease_epoch, lease_expires_at_ms, started_at_ms
        ) VALUES ($1, $2, 0, 'w-orig', 'w-orig-1', 1, 0, $3)
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(now_ms())
    .execute(pool)
    .await
    .expect("seed attempt");
    (part, exec_uuid, LaneId::new(lane))
}

fn backend_from_pool(pool: PgPool) -> Arc<dyn EngineBackend> {
    PostgresBackend::from_pool(pool, PartitionConfig::default())
}

fn issue_args(eid: ExecutionId, lane: LaneId, worker: &str) -> IssueReclaimGrantArgs {
    IssueReclaimGrantArgs::new(
        eid,
        WorkerId::new(worker),
        WorkerInstanceId::new(format!("{worker}-1")),
        lane,
        None,
        60_000,
        None,
        None,
        BTreeSet::new(),
        TimestampMs::from_millis(now_ms()),
    )
}

fn reclaim_args(
    eid: ExecutionId,
    lane: LaneId,
    worker: &str,
    max: Option<u32>,
) -> ReclaimExecutionArgs {
    ReclaimExecutionArgs::new(
        eid,
        WorkerId::new(worker),
        WorkerInstanceId::new(format!("{worker}-1")),
        lane,
        None,
        LeaseId::new(),
        30_000,
        AttemptId::new(),
        "{}".into(),
        max,
        WorkerInstanceId::new("w-orig-1"),
        ff_core::types::AttemptIndex::new(0),
    )
}

fn exec_id(part: i16, uuid: Uuid) -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:{part}}}:{uuid}")).unwrap()
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn issue_reclaim_grant_happy_path() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-iss-happy-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let args = issue_args(exec_id(part, exec_uuid), lane_id, "w-rec");
    let out = backend.issue_reclaim_grant(args).await.expect("ok");
    match out {
        IssueReclaimGrantOutcome::Granted(g) => {
            assert_eq!(g.execution_id, exec_id(part, exec_uuid));
            assert!(g.expires_at_ms as i64 > now_ms());
        }
        other => panic!("expected Granted, got {other:?}"),
    }
    let row = sqlx::query(
        "SELECT kind, worker_id FROM ff_claim_grant WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .expect("row");
    assert_eq!(row.get::<String, _>("kind"), "reclaim");
    assert_eq!(row.get::<String, _>("worker_id"), "w-rec");
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn issue_reclaim_grant_wrong_phase_not_reclaimable() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-iss-phase-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) = seed_exec(&pool, &lane, "leased", "runnable", 0).await;
    sqlx::query("UPDATE ff_attempt SET lease_expires_at_ms = $1 WHERE partition_key = $2 AND execution_id = $3")
        .bind(now_ms() + 60_000)
        .bind(part)
        .bind(exec_uuid)
        .execute(&pool)
        .await
        .unwrap();
    let backend = backend_from_pool(pool.clone());
    let args = issue_args(exec_id(part, exec_uuid), lane_id, "w-rec");
    let out = backend.issue_reclaim_grant(args).await.expect("ok");
    assert!(
        matches!(out, IssueReclaimGrantOutcome::NotReclaimable { .. }),
        "got {out:?}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn issue_reclaim_grant_cap_exceeded() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-iss-cap-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 1000).await;
    let backend = backend_from_pool(pool.clone());
    let args = issue_args(exec_id(part, exec_uuid), lane_id, "w-rec");
    let out = backend.issue_reclaim_grant(args).await.expect("ok");
    assert!(
        matches!(
            out,
            IssueReclaimGrantOutcome::ReclaimCapExceeded { reclaim_count: 1000, .. }
        ),
        "got {out:?}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_happy_path_new_attempt_minted() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-happy-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);
    let _ = match backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap()
    {
        IssueReclaimGrantOutcome::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    };
    let outcome = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", None))
        .await
        .expect("reclaim ok");
    let handle = match outcome {
        ReclaimExecutionOutcome::Claimed(h) => h,
        other => panic!("expected Claimed, got {other:?}"),
    };
    assert_eq!(handle.kind, HandleKind::Reclaimed);

    let row = sqlx::query(
        "SELECT attempt_index, lease_reclaim_count, lifecycle_phase FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i32, _>("attempt_index"), 1);
    assert_eq!(row.get::<i32, _>("lease_reclaim_count"), 1);
    assert_eq!(row.get::<String, _>("lifecycle_phase"), "active");

    let prior: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM ff_attempt WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=0",
    )
    .bind(part).bind(exec_uuid).fetch_one(&pool).await.unwrap();
    assert_eq!(prior.as_deref(), Some("interrupted_reclaimed"));

    // Bug B parity: new attempt's lease_epoch = prior + 1. `seed_exec`
    // writes the prior attempt with lease_epoch=1, so reclaim mints 2.
    let new_epoch: i64 = sqlx::query_scalar(
        "SELECT lease_epoch FROM ff_attempt WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=1",
    )
    .bind(part).bind(exec_uuid).fetch_one(&pool).await.unwrap();
    assert_eq!(new_epoch, 2);

    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_claim_grant WHERE partition_key=$1 AND execution_id=$2 AND kind='reclaim'",
    )
    .bind(part).bind(exec_uuid).fetch_one(&pool).await.unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_worker_id_mismatch_errors() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-wid-{}", Uuid::new_v4());
    let (_part, _exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(_part, _exec_uuid);
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    let err = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-other", None))
        .await
        .expect_err("must error");
    assert!(
        matches!(err, ff_core::engine_error::EngineError::Validation { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_ttl_expired_grant_not_found() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-ttl-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE ff_claim_grant SET expires_at_ms = 1 WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let outcome = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", None))
        .await
        .expect("ok");
    assert!(
        matches!(outcome, ReclaimExecutionOutcome::GrantNotFound { .. }),
        "got {outcome:?}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_cap_exceeded_transitions_terminal() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-cap-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE ff_exec_core SET lease_reclaim_count = 5 WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let outcome = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", Some(5)))
        .await
        .expect("ok");
    assert!(
        matches!(
            outcome,
            ReclaimExecutionOutcome::ReclaimCapExceeded { reclaim_count: 6, .. }
        ),
        "got {outcome:?}"
    );
    let phase: String = sqlx::query_scalar(
        "SELECT lifecycle_phase FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phase, "terminal");
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_cap_exceeded_at_argv9_exact_boundary() {
    // Investigation: per memory project_reclaim_cap_exceeded_investigation.md,
    // the PR-F (#402) agent observed `lease_reclaim_count=4, max=Some(4)`
    // returning `Claimed` instead of `ReclaimCapExceeded` on Valkey. This
    // PG parallel asserts the same exact-boundary scenario enforces the
    // cap on the SQL backend too. PG reports the post-increment count
    // (see sibling cap_exceeded_transitions_terminal test at
    // `lease_reclaim_count=5, max=5 -> reclaim_count=6`), so boundary
    // `count=4, max=4` -> `reclaim_count=5`.
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-cap-exact-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 4).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    let outcome = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", Some(4)))
        .await
        .expect("ok");
    assert!(
        matches!(
            outcome,
            ReclaimExecutionOutcome::ReclaimCapExceeded { reclaim_count: 5, .. }
        ),
        "expected ReclaimCapExceeded at lease_reclaim_count=max=4 boundary, got {outcome:?}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn scheduler_claim_grant_writes_new_table_kind_claim() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-sched-tbl-{}", Uuid::new_v4());
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms
        ) VALUES (
            $1, $2, NULL, $3,
            '{}', 0,
            'runnable', 'unowned', 'eligible_now',
            'waiting', 'pending_first_attempt',
            0, $4
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(&lane)
    .bind(now_ms())
    .execute(&pool)
    .await
    .unwrap();
    let sched = ff_backend_postgres::scheduler::PostgresScheduler::new(pool.clone());
    let grant = sched
        .claim_for_worker(
            &LaneId::new(&lane),
            &WorkerId::new("w-sched"),
            &WorkerInstanceId::new("w-sched-1"),
            &BTreeSet::new(),
            30_000,
        )
        .await
        .unwrap();
    assert!(grant.is_some(), "scheduler should issue a grant");
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT kind, worker_id FROM ff_claim_grant WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let row = row.expect("grant row in ff_claim_grant");
    assert_eq!(row.0, "claim");
    assert_eq!(row.1, "w-sched");

    // RFC-024 PR-D rollout dual-write: the legacy JSON blob at
    // `ff_exec_core.raw_fields.claim_grant` must ALSO be populated
    // so an older-version reader in a rolling-deploy window can
    // still verify grants issued by the new code.
    let legacy_json: sqlx::types::JsonValue = sqlx::query_scalar(
        "SELECT raw_fields->'claim_grant' FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        legacy_json.is_object(),
        "legacy raw_fields.claim_grant must be populated by dual-write, got {legacy_json}"
    );
    assert_eq!(
        legacy_json.get("worker_id").and_then(|v| v.as_str()),
        Some("w-sched")
    );
    assert_eq!(
        legacy_json.get("worker_instance_id").and_then(|v| v.as_str()),
        Some("w-sched-1")
    );
    assert!(
        legacy_json.get("grant_key").and_then(|v| v.as_str()).is_some(),
        "legacy blob missing grant_key: {legacy_json}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_handle_carries_caller_supplied_ids() {
    // F6 parity: HandlePayload inside the minted handle must embed
    // the caller-supplied attempt_id + lease_id, not fresh IDs.
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-ids-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    let caller_attempt = AttemptId::new();
    let caller_lease = LeaseId::new();
    let args = ReclaimExecutionArgs::new(
        eid.clone(),
        WorkerId::new("w-rec"),
        WorkerInstanceId::new("w-rec-1"),
        lane_id.clone(),
        None,
        caller_lease.clone(),
        30_000,
        caller_attempt.clone(),
        "{}".into(),
        None,
        WorkerInstanceId::new("w-orig-1"),
        ff_core::types::AttemptIndex::new(0),
    );
    let outcome = backend.reclaim_execution(args).await.expect("ok");
    let handle = match outcome {
        ReclaimExecutionOutcome::Claimed(h) => h,
        other => panic!("expected Claimed, got {other:?}"),
    };
    let decoded = ff_core::handle_codec::decode(&handle.opaque).expect("decode handle");
    assert_eq!(
        decoded.payload.attempt_id, caller_attempt,
        "HandlePayload.attempt_id must echo caller-supplied value"
    );
    assert_eq!(
        decoded.payload.lease_id, caller_lease,
        "HandlePayload.lease_id must echo caller-supplied value"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_negative_reclaim_count_does_not_wrap() {
    // F5 defensive clamp: a (hypothetical) negative
    // `lease_reclaim_count` row must not wrap under `as u32`. The
    // schema disallows negatives via `DEFAULT 0`, but the code
    // clamps before cast anyway. Force a negative via direct UPDATE
    // (bypassing the schema's intent) and confirm the clamped path
    // still increments to 1 rather than saturating.
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-neg-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    // Bypass: overwrite the count to -1 post-seed.
    sqlx::query(
        "UPDATE ff_exec_core SET lease_reclaim_count = -1 WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    let outcome = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", None))
        .await
        .expect("ok");
    assert!(
        matches!(outcome, ReclaimExecutionOutcome::Claimed(_)),
        "got {outcome:?}"
    );
    let new_count: i32 = sqlx::query_scalar(
        "SELECT lease_reclaim_count FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(new_count, 1, "clamped (-1).max(0)+1 must be 1, got {new_count}");
}

/// Bug A parity: cap-exceeded emits BOTH a completion_event
/// (outcome='failed') and a lease_event (event_type='revoked'). Per
/// RFC-024 §4.2.7 / RFC-019 outbox matrix, every terminal transition
/// fires both. Mirrors SQLite
/// `reclaim_cap_exceeded_clears_prior_attempt_and_emits_events`.
#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_cap_exceeded_emits_completion_and_lease_revoked() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-cap-outbox-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE ff_exec_core SET lease_reclaim_count = 5 WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let outcome = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", Some(5)))
        .await
        .expect("ok");
    assert!(
        matches!(outcome, ReclaimExecutionOutcome::ReclaimCapExceeded { .. }),
        "expected cap-exceeded, got {outcome:?}"
    );

    let comp_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_completion_event \
         WHERE partition_key=$1 AND execution_id=$2 AND outcome='failed'",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        comp_count, 1,
        "terminal_failed must emit completion_event(outcome='failed')"
    );

    let lease_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_lease_event \
         WHERE partition_key=$1 AND execution_id=$2 AND event_type='revoked'",
    )
    .bind(i32::from(part))
    .bind(exec_uuid.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        lease_count, 1,
        "terminal_failed must emit lease_event(event_type='revoked')"
    );
}

/// Bug B parity: lease_epoch is derived from prior attempt's epoch
/// (prior+1) and monotonically increases across successive reclaims.
/// Mirrors SQLite `reclaim_execution_lease_epoch_monotonic_across_reclaims`
/// + Valkey Lua `flowfabric.lua:3106`.
#[tokio::test]
#[ignore = "requires live Postgres (FF_PG_TEST_URL)"]
async fn reclaim_execution_lease_epoch_monotonic_across_reclaims() {
    let Some(pool) = setup_or_skip().await else { return; };
    let lane = format!("lane-rec-epoch-mono-{}", Uuid::new_v4());
    let (part, exec_uuid, lane_id) =
        seed_exec(&pool, &lane, "lease_expired_reclaimable", "active", 0).await;
    let backend = backend_from_pool(pool.clone());
    let eid = exec_id(part, exec_uuid);

    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    let _ = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", None))
        .await
        .expect("reclaim-1");

    let epoch_1: i64 = sqlx::query_scalar(
        "SELECT lease_epoch FROM ff_attempt WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=1",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(epoch_1 >= 1, "first reclaim epoch must be >=1, got {epoch_1}");

    sqlx::query(
        "UPDATE ff_exec_core SET ownership_state='lease_expired_reclaimable' \
         WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid)
    .execute(&pool)
    .await
    .unwrap();
    let _ = backend
        .issue_reclaim_grant(issue_args(eid.clone(), lane_id.clone(), "w-rec"))
        .await
        .unwrap();
    let _ = backend
        .reclaim_execution(reclaim_args(eid.clone(), lane_id.clone(), "w-rec", None))
        .await
        .expect("reclaim-2");

    let epoch_2: i64 = sqlx::query_scalar(
        "SELECT lease_epoch FROM ff_attempt WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=2",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        epoch_2 > epoch_1,
        "second-reclaim epoch ({epoch_2}) must exceed first ({epoch_1})"
    );
}

#[test]
fn execution_id_parse_rejects_negative_partition() {
    // F4 upstream invariant: `ExecutionId::parse` already enforces
    // a `u16` partition index, so a negative literal never reaches
    // `split_exec_id` in practice. The i16→u16 parse fix in
    // `split_exec_id` is defense-in-depth + matches the Wave-9
    // `partition.index as i16` convention; verify the upstream
    // invariant that makes that layering sound.
    assert!(
        ExecutionId::parse("{fp:-1}:00000000-0000-0000-0000-000000000000").is_err(),
        "ExecutionId::parse must reject negative partition index"
    );
    assert!(
        ExecutionId::parse("{fp:0}:00000000-0000-0000-0000-000000000000").is_ok()
    );
}
