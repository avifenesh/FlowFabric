//! Postgres timeout-reconciler integration tests (RFC-v0.7 Wave 6c).
//!
//! Covers the three scanner ports landed in wave 6c:
//!
//!   * `attempt_timeout` — expired lease with retries remaining →
//!     attempt interrupted + execution re-eligible. Same with retries
//!     exhausted → terminal + completion event emitted.
//!   * `lease_expiry` — orphaned lease → reclaim becomes redeemable
//!     via `claim_from_resume_grant`.
//!   * `suspension_timeout` — `Fail` behavior → terminal; `Signal`
//!     behavior → synthetic timeout entry in member_map + timeout
//!     cleared so the scanner doesn't re-fire.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave6c_test \
//!   cargo test -p ff-test --test pg_reconciler_timeouts -- --ignored
//! ```

use std::collections::HashMap;

use ff_backend_postgres::PostgresBackend;
use ff_backend_postgres::reconcilers::{
    attempt_timeout as rt_attempt, lease_expiry as rt_lease, suspension_timeout as rt_susp,
};
use ff_core::backend::{CapabilitySet, ClaimPolicy, ScannerFilter};
use ff_core::contracts::CreateExecutionArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::policy::{ExecutionPolicy, RetryPolicy};
use ff_core::types::{ExecutionId, LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

// ── fixture helpers ─────────────────────────────────────────────────

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
    Some(pool)
}

fn now() -> TimestampMs {
    TimestampMs(1_700_000_000_000)
}

fn exec_uuid(eid: &ExecutionId) -> uuid::Uuid {
    uuid::Uuid::parse_str(eid.as_str().split_once("}:").unwrap().1).unwrap()
}

fn part_key(eid: &ExecutionId) -> i16 {
    eid.partition() as i16
}

fn policy_with_max_retries(n: u32) -> ExecutionPolicy {
    let rp = RetryPolicy {
        max_retries: n,
        ..RetryPolicy::default()
    };
    ExecutionPolicy {
        retry_policy: Some(rp),
        ..ExecutionPolicy::default()
    }
}

async fn seed_exec_with_policy(
    backend: &std::sync::Arc<PostgresBackend>,
    lane: &LaneId,
    policy: Option<ExecutionPolicy>,
) -> ExecutionId {
    let eid = ExecutionId::solo(lane, &PartitionConfig::default());
    backend
        .create_execution(CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new("wave6c-test"),
            lane_id: lane.clone(),
            execution_kind: "task".into(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("application/json".into()),
            priority: 0,
            creator_identity: "pg-wave6c".into(),
            idempotency_key: None,
            tags: HashMap::new(),
            policy,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: eid.partition(),
            now: now(),
        })
        .await
        .expect("create_execution OK");
    eid
}

/// Place the execution in `active` phase with a live lease on
/// `attempt_index = 0`, owned by `wid`/`wiid`, expiring at `expires_ms`.
async fn install_active_lease(
    pool: &PgPool,
    eid: &ExecutionId,
    wid: &WorkerId,
    wiid: &WorkerInstanceId,
    expires_ms: i64,
) {
    let part = part_key(eid);
    let uuid = exec_uuid(eid);
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='active', ownership_state='leased', \
         eligibility_state='not_applicable', attempt_state='running_attempt' \
         WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(uuid)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ff_attempt (partition_key, execution_id, attempt_index, worker_id, \
            worker_instance_id, lease_epoch, lease_expires_at_ms, started_at_ms) \
         VALUES ($1, $2, 0, $3, $4, 1, $5, $6) \
         ON CONFLICT (partition_key, execution_id, attempt_index) DO UPDATE SET \
             worker_id=EXCLUDED.worker_id, worker_instance_id=EXCLUDED.worker_instance_id, \
             lease_epoch=1, lease_expires_at_ms=EXCLUDED.lease_expires_at_ms, \
             outcome=NULL, terminal_at_ms=NULL",
    )
    .bind(part)
    .bind(uuid)
    .bind(wid.as_str())
    .bind(wiid.as_str())
    .bind(expires_ms)
    .bind(expires_ms - 10_000)
    .execute(pool)
    .await
    .unwrap();
}

// ── attempt_timeout ─────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn attempt_timeout_retries_remain_makes_execution_eligible_again() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(format!("wave6c-at-retry-{}", uuid::Uuid::new_v4()));
    let eid = seed_exec_with_policy(&backend, &lane, Some(policy_with_max_retries(3))).await;
    let part = part_key(&eid);
    let wid = WorkerId::new("w-wave6c-at");
    let wiid = WorkerInstanceId::new("wi-wave6c-at");

    // Expired 1h ago.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    install_active_lease(&pool, &eid, &wid, &wiid, now_ms - 3_600_000).await;

    let report = rt_attempt::scan_tick(&pool, part, &ScannerFilter::NOOP)
        .await
        .expect("scan_tick OK");
    assert_eq!(report.processed, 1, "exactly one timed-out attempt");
    assert_eq!(report.errors, 0);

    let row = sqlx::query(
        "SELECT lifecycle_phase, eligibility_state, attempt_state, attempt_index \
           FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_one(&pool)
    .await
    .unwrap();
    let phase: String = row.get("lifecycle_phase");
    let elig: String = row.get("eligibility_state");
    let astate: String = row.get("attempt_state");
    let attempt_index: i32 = row.get("attempt_index");
    assert_eq!(phase, "runnable", "retries remain → runnable");
    assert_eq!(elig, "eligible_now");
    assert_eq!(astate, "pending_retry_attempt");
    assert_eq!(attempt_index, 1, "attempt_index bumped");

    let outcome: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM ff_attempt WHERE partition_key=$1 AND execution_id=$2 AND attempt_index=0",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outcome.as_deref(), Some("interrupted"));
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn attempt_timeout_retries_exhausted_terminalizes_and_emits_event() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(format!("wave6c-at-exhaust-{}", uuid::Uuid::new_v4()));
    let eid = seed_exec_with_policy(&backend, &lane, Some(policy_with_max_retries(0))).await;
    let part = part_key(&eid);
    let wid = WorkerId::new("w-wave6c-at2");
    let wiid = WorkerInstanceId::new("wi-wave6c-at2");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    install_active_lease(&pool, &eid, &wid, &wiid, now_ms - 60_000).await;

    let report = rt_attempt::scan_tick(&pool, part, &ScannerFilter::NOOP)
        .await
        .expect("scan_tick OK");
    assert_eq!(report.processed, 1);

    let phase: String = sqlx::query_scalar(
        "SELECT lifecycle_phase FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phase, "terminal");

    let outcomes: Vec<String> = sqlx::query_scalar(
        "SELECT outcome FROM ff_completion_event WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        outcomes.contains(&"expired".to_string()),
        "expected completion event with outcome=expired, got {outcomes:?}"
    );
}

// ── lease_expiry ────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn lease_expiry_releases_orphaned_lease_and_claim_from_reclaim_succeeds() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(format!("wave6c-le-{}", uuid::Uuid::new_v4()));
    let eid = seed_exec_with_policy(&backend, &lane, None).await;
    let part = part_key(&eid);
    let wid_old = WorkerId::new("w-gone");
    let wiid_old = WorkerInstanceId::new("wi-gone");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    install_active_lease(&pool, &eid, &wid_old, &wiid_old, now_ms - 300_000).await;

    let report = rt_lease::scan_tick(&pool, part, &ScannerFilter::NOOP)
        .await
        .expect("lease scan_tick OK");
    assert_eq!(report.processed, 1, "one expired lease released");
    assert_eq!(report.errors, 0);

    // Lease column nulled + exec flipped back to runnable.
    let row = sqlx::query(
        "SELECT a.lease_expires_at_ms, a.outcome, c.lifecycle_phase, c.eligibility_state \
           FROM ff_attempt a JOIN ff_exec_core c \
             ON c.partition_key=a.partition_key AND c.execution_id=a.execution_id \
          WHERE a.partition_key=$1 AND a.execution_id=$2 AND a.attempt_index=0",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_one(&pool)
    .await
    .unwrap();
    let lease_at: Option<i64> = row.get("lease_expires_at_ms");
    let outcome: Option<String> = row.get("outcome");
    let phase: String = row.get("lifecycle_phase");
    let elig: String = row.get("eligibility_state");
    assert!(lease_at.is_none(), "lease column cleared");
    assert_eq!(outcome.as_deref(), Some("lease_expired_reclaimable"));
    assert_eq!(phase, "runnable");
    assert_eq!(elig, "eligible_now");

    // A fresh worker can now claim the lane — the released lease means
    // the standard claim path picks the exec back up.
    let caps: CapabilitySet = CapabilitySet::new(std::iter::empty::<&str>());
    let policy = ClaimPolicy::new(
        WorkerId::new("w-fresh"),
        WorkerInstanceId::new("wi-fresh"),
        30_000,
        None,
    );
    let h = backend.claim(&lane, &caps, policy).await.expect("claim OK");
    assert!(h.is_some(), "fresh worker claims the reclaimed exec");
}

// ── suspension_timeout ──────────────────────────────────────────────

async fn install_suspension(pool: &PgPool, eid: &ExecutionId, timeout_at_ms: i64, behavior: &str) {
    let part = part_key(eid);
    let uuid = exec_uuid(eid);
    // Flip exec_core into a `suspended` lifecycle phase so the
    // terminal branch has a visible source state to transition from.
    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase='suspended', ownership_state='unowned', \
         eligibility_state='blocked_by_dependencies', attempt_state='attempt_interrupted' \
         WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(uuid)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO ff_suspension_current \
           (partition_key, execution_id, suspension_id, suspended_at_ms, timeout_at_ms, \
            reason_code, condition, satisfied_set, member_map, timeout_behavior) \
         VALUES ($1, $2, $3, $4, $5, 'waiting_for_signal', '{}'::jsonb, '[]'::jsonb, \
                 '{}'::jsonb, $6)",
    )
    .bind(part)
    .bind(uuid)
    .bind(uuid::Uuid::new_v4())
    .bind(timeout_at_ms - 60_000)
    .bind(timeout_at_ms)
    .bind(behavior)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn suspension_timeout_signal_behavior_appends_timeout_member() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(format!("wave6c-st-sig-{}", uuid::Uuid::new_v4()));
    let eid = seed_exec_with_policy(&backend, &lane, None).await;
    let part = part_key(&eid);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    install_suspension(
        &pool,
        &eid,
        now_ms - 1_000,
        "auto_resume_with_timeout_signal",
    )
    .await;

    let report = rt_susp::scan_tick(&pool, part, &ScannerFilter::NOOP)
        .await
        .expect("susp scan_tick OK");
    assert_eq!(report.processed, 1);
    assert_eq!(report.errors, 0);

    let row = sqlx::query(
        "SELECT member_map, timeout_at_ms FROM ff_suspension_current \
         WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_one(&pool)
    .await
    .unwrap();
    let mm: serde_json::Value = row.get("member_map");
    let to_at: Option<i64> = row.get("timeout_at_ms");
    assert!(to_at.is_none(), "timeout_at cleared after signal fan-out");
    let timeout_bucket = mm
        .get("__timeout__")
        .and_then(|v| v.as_array())
        .expect("__timeout__ bucket present");
    assert_eq!(timeout_bucket.len(), 1);
    assert_eq!(
        timeout_bucket[0]
            .get("signal_name")
            .and_then(|v| v.as_str()),
        Some("timeout")
    );
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn suspension_timeout_fail_behavior_terminalizes_and_clears_suspension() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default());
    let lane = LaneId::new(format!("wave6c-st-fail-{}", uuid::Uuid::new_v4()));
    let eid = seed_exec_with_policy(&backend, &lane, None).await;
    let part = part_key(&eid);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    install_suspension(&pool, &eid, now_ms - 1_000, "fail").await;

    let report = rt_susp::scan_tick(&pool, part, &ScannerFilter::NOOP)
        .await
        .expect("susp scan_tick OK");
    assert_eq!(report.processed, 1);

    let phase: String = sqlx::query_scalar(
        "SELECT lifecycle_phase FROM ff_exec_core WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phase, "terminal");

    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_suspension_current \
         WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0, "suspension row cleared");

    let outcomes: Vec<String> = sqlx::query_scalar(
        "SELECT outcome FROM ff_completion_event WHERE partition_key=$1 AND execution_id=$2",
    )
    .bind(part)
    .bind(exec_uuid(&eid))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(outcomes.iter().any(|o| o == "expired"));
}
