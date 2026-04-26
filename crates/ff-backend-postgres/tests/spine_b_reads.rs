//! RFC-020 Wave 9 Spine-B — integration tests for the 3 read methods
//! (`read_execution_state`, `read_execution_info`, `get_execution_result`).
//!
//! Follows the `FF_PG_TEST_URL` / `#[ignore]` convention used by the
//! other Postgres integration suites.
//!
//! Coverage:
//!
//! * `read_execution_state_missing_returns_none` — `Ok(None)` when the
//!   execution id is unknown.
//! * `read_execution_state_returns_public_state` — point read against a
//!   seeded `ff_exec_core` row returns the parsed [`PublicState`].
//! * `read_execution_info_missing_returns_none` — `Ok(None)` when the
//!   execution id is unknown.
//! * `read_execution_info_lateral_joins_current_attempt` — seeds two
//!   attempt rows (index 0 + 1) + bumps `ff_exec_core.attempt_index = 1`;
//!   asserts the LATERAL join pins to attempt_index=1 and its
//!   `started_at_ms` flows into `ExecutionInfo::started_at`.
//! * `read_execution_info_terminal_derives_outcome` — terminal exec
//!   with attempt outcome=`success` surfaces `TerminalOutcome::Success`.
//! * `get_execution_result_missing_returns_none` — `Ok(None)` when the
//!   execution id is unknown.
//! * `get_execution_result_active_returns_none` — `Ok(None)` when the
//!   execution is seeded non-terminal with NULL `result`.
//! * `get_execution_result_terminal_returns_payload` — seeds a terminal
//!   execution with non-empty `result`; returns the bytes.

use ff_backend_postgres::PostgresBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::state::{LifecyclePhase, PublicState, TerminalOutcome};
use ff_core::types::{ExecutionId, LaneId, TimestampMs};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

// ── Fixture: pool + `PostgresBackend` ────────────────────────────

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
    backend: std::sync::Arc<dyn EngineBackend>,
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
    let backend = PostgresBackend::from_pool(pool.clone(), PartitionConfig::default())
        as std::sync::Arc<dyn EngineBackend>;
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
    lane_id: &str,
    lifecycle_phase: &str,
    ownership_state: &str,
    eligibility_state: &str,
    public_state: &str,
    attempt_state: &str,
    attempt_index: i32,
    now_ms: i64,
    terminal_at_ms: Option<i64>,
    result_payload: Option<Vec<u8>>,
    raw_fields: serde_json::Value,
) {
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, \
            priority, created_at_ms, terminal_at_ms, result, raw_fields) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 0, $10, $11, $12, $13)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lane_id)
    .bind(attempt_index)
    .bind(lifecycle_phase)
    .bind(ownership_state)
    .bind(eligibility_state)
    .bind(public_state)
    .bind(attempt_state)
    .bind(now_ms)
    .bind(terminal_at_ms)
    .bind(result_payload)
    .bind(raw_fields)
    .execute(pool)
    .await
    .expect("insert ff_exec_core");
}

async fn insert_attempt(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    attempt_index: i32,
    outcome: Option<&str>,
    started_at_ms: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, lease_epoch, outcome, started_at_ms) \
         VALUES ($1, $2, $3, 0, $4, $5)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .bind(outcome)
    .bind(started_at_ms)
    .execute(pool)
    .await
    .expect("insert ff_attempt");
}

// ── read_execution_state ──────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_state_missing_returns_none() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let missing = ExecutionId::solo(&LaneId::new("default"), &PartitionConfig::default());
    let got = fx.backend.read_execution_state(&missing).await.expect("call ok");
    assert!(got.is_none(), "missing exec → Ok(None)");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_state_returns_public_state() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "active",
        "leased",
        "not_applicable",
        "running", // Postgres writes `running`; read layer normalises → Active
        "running_attempt",
        0,
        now,
        None,
        None,
        serde_json::json!({}),
    )
    .await;

    let got = fx
        .backend
        .read_execution_state(&fx.exec_id)
        .await
        .expect("call ok");
    assert_eq!(got, Some(PublicState::Active));
}

// ── read_execution_info ───────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_info_missing_returns_none() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let missing = ExecutionId::solo(&LaneId::new("default"), &PartitionConfig::default());
    let got = fx.backend.read_execution_info(&missing).await.expect("call ok");
    assert!(got.is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_info_lateral_joins_both_attempts() {
    // Two LATERAL joins in `read_execution_info`:
    //   * current-attempt (attempt_index = ec.attempt_index) drives
    //     `outcome` → `TerminalOutcome`;
    //   * first-attempt (earliest started_at_ms) drives
    //     `ExecutionInfo.started_at` (first-claim timestamp, preserved
    //     across retries — Valkey-parity contract).
    //
    // Seed two attempts:
    //   * attempt 0 started at `first_start` (old first-claim);
    //   * attempt 1 started at `later_start` (current attempt);
    // then bump `ec.attempt_index = 1`.
    // The read must surface
    //   * `started_at = first_start` (NOT later_start)
    //   * `outcome`-derived state from attempt_index=1 (NULL → None)
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    let first_start = now - 10_000;
    let later_start = now - 100;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "workers",
        "active",
        "leased",
        "not_applicable",
        "running", // Postgres writes `running`; read layer normalises → Active
        "running_attempt",
        1,
        now,
        None,
        None,
        serde_json::json!({
            "namespace": "ns-a",
            "execution_kind": "task",
            "blocking_detail": "",
        }),
    )
    .await;
    // Attempt 0: first-claim row; drives `started_at`.
    insert_attempt(&fx.pool, fx.part, fx.exec_uuid, 0, Some("failed"), Some(first_start))
        .await;
    // Attempt 1: current attempt; drives `outcome` (no terminal outcome yet).
    insert_attempt(&fx.pool, fx.part, fx.exec_uuid, 1, None, Some(later_start)).await;

    let info = fx
        .backend
        .read_execution_info(&fx.exec_id)
        .await
        .expect("call ok")
        .expect("exec present");

    assert_eq!(info.execution_id, fx.exec_id);
    assert_eq!(info.namespace, "ns-a");
    assert_eq!(info.lane_id, "workers");
    assert_eq!(info.execution_kind, "task");
    assert_eq!(info.public_state, PublicState::Active);
    assert_eq!(info.state_vector.lifecycle_phase, LifecyclePhase::Active);
    // Current-attempt (index=1) has no outcome → None (not the
    // attempt-0 `failed` outcome, which would be a LATERAL mispin).
    assert_eq!(info.state_vector.terminal_outcome, TerminalOutcome::None);
    assert_eq!(info.current_attempt_index, 1);
    // First-claim `started_at` (attempt_index=0 row); NOT the current
    // attempt's `later_start`.
    assert_eq!(
        info.started_at.as_deref(),
        Some(first_start.to_string().as_str()),
        "started_at must be first-claim timestamp (attempt 0), \
         not current-attempt (attempt 1)"
    );
    assert!(info.completed_at.is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_info_terminal_derives_outcome() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "terminal",
        "unowned",
        "not_applicable",
        "completed",
        "attempt_terminal",
        0,
        now,
        Some(now + 1_000),
        None,
        serde_json::json!({}),
    )
    .await;
    insert_attempt(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        0,
        Some("success"),
        Some(now - 500),
    )
    .await;

    let info = fx
        .backend
        .read_execution_info(&fx.exec_id)
        .await
        .expect("call ok")
        .expect("exec present");

    assert_eq!(info.public_state, PublicState::Completed);
    assert_eq!(info.state_vector.lifecycle_phase, LifecyclePhase::Terminal);
    assert_eq!(info.state_vector.terminal_outcome, TerminalOutcome::Success);
    assert_eq!(
        info.completed_at.as_deref(),
        Some((now + 1_000).to_string().as_str())
    );
}

// ── get_execution_result ──────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn get_execution_result_missing_returns_none() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let missing = ExecutionId::solo(&LaneId::new("default"), &PartitionConfig::default());
    let got = fx
        .backend
        .get_execution_result(&missing)
        .await
        .expect("call ok");
    assert!(got.is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn get_execution_result_active_returns_none() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "active",
        "leased",
        "not_applicable",
        "running",
        "running_attempt",
        0,
        now,
        None,
        None,
        serde_json::json!({}),
    )
    .await;

    let got = fx
        .backend
        .get_execution_result(&fx.exec_id)
        .await
        .expect("call ok");
    assert!(got.is_none(), "active exec with NULL result → Ok(None)");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn get_execution_result_terminal_returns_payload() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    let payload = b"hello-world-result".to_vec();
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "terminal",
        "unowned",
        "not_applicable",
        "completed",
        "attempt_terminal",
        0,
        now,
        Some(now + 1),
        Some(payload.clone()),
        serde_json::json!({}),
    )
    .await;

    let got = fx
        .backend
        .get_execution_result(&fx.exec_id)
        .await
        .expect("call ok");
    assert_eq!(got.as_deref(), Some(payload.as_slice()));
}

// ── Normalization coverage for write-site literals ───────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_info_fresh_create_literals() {
    // Mirrors exactly the literals `create_execution_impl` writes on
    // INSERT: submitted/unowned/eligible_now/waiting/pending. Exercises
    // `attempt_state = 'pending'` → `PendingFirstAttempt` normalisation.
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "submitted",
        "unowned",
        "eligible_now",
        "waiting",
        "pending", // Postgres create-time literal
        0,
        now,
        None,
        None,
        serde_json::json!({}),
    )
    .await;

    let info = fx
        .backend
        .read_execution_info(&fx.exec_id)
        .await
        .expect("call ok")
        .expect("exec present");
    assert_eq!(info.public_state, PublicState::Waiting);
    // `pending` normalises to `PendingFirstAttempt`.
    assert_eq!(
        info.state_vector.attempt_state,
        ff_core::state::AttemptState::PendingFirstAttempt
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_info_cancelled_row() {
    // Postgres cancel path writes lifecycle_phase='cancelled',
    // eligibility_state='cancelled', public_state='cancelled',
    // attempt_state='cancelled' (flow.rs:674 + attempt.rs cancel).
    // Exercises every normalisation branch on the terminal-cancel row.
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "cancelled",
        "unowned",
        "cancelled",
        "cancelled",
        "cancelled",
        0,
        now,
        Some(now + 1),
        None,
        serde_json::json!({}),
    )
    .await;

    let info = fx
        .backend
        .read_execution_info(&fx.exec_id)
        .await
        .expect("call ok")
        .expect("exec present");
    assert_eq!(info.public_state, PublicState::Cancelled);
    assert_eq!(info.state_vector.lifecycle_phase, LifecyclePhase::Terminal);
    assert_eq!(info.state_vector.terminal_outcome, TerminalOutcome::Cancelled);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_info_pending_claim_eligibility() {
    // Scheduler ClaimGrant transitional literal `pending_claim` on
    // `eligibility_state`; must normalise to `EligibleNow`.
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "runnable",
        "unowned",
        "pending_claim",
        "waiting",
        // Attempt_state must be a persisted literal; `pending_claim` is
        // only written on `eligibility_state`. Use the create-time
        // `pending` (covers the initial-INSERT path).
        "pending",
        0,
        now,
        None,
        None,
        serde_json::json!({}),
    )
    .await;
    let info = fx
        .backend
        .read_execution_info(&fx.exec_id)
        .await
        .expect("call ok")
        .expect("exec present");
    assert_eq!(
        info.state_vector.eligibility_state,
        ff_core::state::EligibilityState::EligibleNow
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn read_execution_info_flow_id_returns_bare_uuid() {
    // `ExecutionInfo.flow_id` is the bare UUID wire form of `FlowId`
    // (per `uuid_id!` macro, no hash-tag prefix).
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    // Seed a row and then UPDATE flow_id to a known uuid (the helper
    // doesn't take flow_id; a direct update keeps the fixture surface
    // small).
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        "default",
        "submitted",
        "unowned",
        "eligible_now",
        "waiting",
        "pending",
        0,
        now,
        None,
        None,
        serde_json::json!({}),
    )
    .await;
    let flow_uuid = Uuid::new_v4();
    sqlx::query("UPDATE ff_exec_core SET flow_id = $1 WHERE partition_key = $2 AND execution_id = $3")
        .bind(flow_uuid)
        .bind(fx.part)
        .bind(fx.exec_uuid)
        .execute(&fx.pool)
        .await
        .expect("update flow_id");

    let info = fx
        .backend
        .read_execution_info(&fx.exec_id)
        .await
        .expect("call ok")
        .expect("exec present");
    assert_eq!(
        info.flow_id.as_deref(),
        Some(flow_uuid.to_string().as_str()),
        "flow_id is bare UUID, not hash-tagged"
    );
}
