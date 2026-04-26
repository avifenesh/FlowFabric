//! RFC-020 Wave 9 Spine-A pt.2 — integration tests for
//! `change_priority` + `replay_execution` + `cancel_flow_header` +
//! `ack_cancel_member`.
//!
//! Follows the `FF_PG_TEST_URL` / `#[ignore]` convention from
//! `spine_a_pt1_mutations.rs` + `spine_b_reads.rs`.
//!
//! Coverage per RFC §4.2.3 / §4.2.4 / §4.2.5 / §4.2.7 / §4.3 + Rev 7
//! fork decisions:
//!
//! * `change_priority` — happy path (outbox emit), terminal ineligible,
//!   wrong eligibility_state.
//! * `replay_execution` — normal branch (in-place attempt mutate, no
//!   attempt_index bump, replay_count bump), skipped_flow_member
//!   branch (edge_group counter reset: skip/fail/running → 0,
//!   success preserved), non-terminal gate.
//! * `cancel_flow_header` — bulk member enumeration + backlog rows,
//!   AlreadyTerminal idempotent replay, outbox emit.
//! * `ack_cancel_member` — happy path drain, parent-row deletion when
//!   last member acks, full end-to-end cancel_flow_header → ack
//!   sequence.

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{
    CancelFlowArgs, CancelFlowHeader, ChangePriorityArgs, ChangePriorityResult,
    ReplayExecutionArgs, ReplayExecutionResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, StateKind};
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::state::PublicState;
use ff_core::types::{ExecutionId, FlowId, LaneId, TimestampMs};
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
    flow_id: Option<Uuid>,
    lifecycle_phase: &str,
    eligibility_state: &str,
    ownership_state: &str,
    public_state: &str,
    attempt_state: &str,
    attempt_index: i32,
    priority: i32,
    now: i64,
) {
    sqlx::query(
        "INSERT INTO ff_exec_core \
           (partition_key, execution_id, flow_id, lane_id, attempt_index, \
            lifecycle_phase, ownership_state, eligibility_state, \
            public_state, attempt_state, \
            priority, created_at_ms, raw_fields) \
         VALUES ($1, $2, $3, 'default', $4, $5, $6, $7, \
                 $8, $9, $10, $11, '{}'::jsonb)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(flow_id)
    .bind(attempt_index)
    .bind(lifecycle_phase)
    .bind(ownership_state)
    .bind(eligibility_state)
    .bind(public_state)
    .bind(attempt_state)
    .bind(priority)
    .bind(now)
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
    lease_epoch: i64,
) {
    sqlx::query(
        "INSERT INTO ff_attempt \
           (partition_key, execution_id, attempt_index, lease_epoch, outcome) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(attempt_index)
    .bind(lease_epoch)
    .bind(outcome)
    .execute(pool)
    .await
    .expect("insert ff_attempt");
}

async fn count_operator_events(
    pool: &PgPool,
    part: i16,
    exec_uuid: Uuid,
    event_type: &str,
) -> i64 {
    sqlx::query(
        "SELECT COUNT(*)::bigint AS n FROM ff_operator_event \
         WHERE partition_key = $1 AND execution_id = $2 AND event_type = $3",
    )
    .bind(i32::from(part))
    .bind(exec_uuid.to_string())
    .bind(event_type)
    .fetch_one(pool)
    .await
    .expect("count ff_operator_event")
    .try_get::<i64, _>("n")
    .expect("n column")
}

// ── change_priority ──────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn change_priority_happy_path_emits_operator_event() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        None,
        "runnable",
        "eligible_now",
        "unowned",
        "waiting",
        "pending",
        0,
        /*priority=*/ 100,
        now,
    )
    .await;

    let r = fx
        .backend
        .change_priority(ChangePriorityArgs {
            execution_id: fx.exec_id.clone(),
            new_priority: 5000,
            lane_id: LaneId::new("default"),
            now: TimestampMs::now(),
        })
        .await
        .expect("change_priority ok");
    assert!(matches!(r, ChangePriorityResult::Changed { .. }));

    let new_pri: i32 = sqlx::query_scalar(
        "SELECT priority FROM ff_exec_core WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read priority");
    assert_eq!(new_pri, 5000);

    let n = count_operator_events(&fx.pool, fx.part, fx.exec_uuid, "priority_changed").await;
    assert_eq!(n, 1, "one priority_changed row emitted");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn change_priority_terminal_is_not_eligible() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        None,
        /*lifecycle_phase=*/ "terminal",
        /*eligibility_state=*/ "not_applicable",
        "unowned",
        "success",
        "terminated",
        0,
        100,
        now,
    )
    .await;
    let err = fx
        .backend
        .change_priority(ChangePriorityArgs {
            execution_id: fx.exec_id.clone(),
            new_priority: 5000,
            lane_id: LaneId::new("default"),
            now: TimestampMs::now(),
        })
        .await
        .expect_err("expected execution_not_eligible");
    assert!(matches!(
        err,
        EngineError::Contention(ContentionKind::ExecutionNotEligible)
    ));
    let n = count_operator_events(&fx.pool, fx.part, fx.exec_uuid, "priority_changed").await;
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn change_priority_wrong_eligibility_state_is_not_eligible() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        None,
        "runnable",
        /*eligibility_state=*/ "blocked_by_dependencies",
        "unowned",
        "waiting_children",
        "pending",
        0,
        100,
        now,
    )
    .await;
    let err = fx
        .backend
        .change_priority(ChangePriorityArgs {
            execution_id: fx.exec_id.clone(),
            new_priority: 7000,
            lane_id: LaneId::new("default"),
            now: TimestampMs::now(),
        })
        .await
        .expect_err("expected execution_not_eligible (wrong eligibility)");
    assert!(matches!(
        err,
        EngineError::Contention(ContentionKind::ExecutionNotEligible)
    ));
}

// ── replay_execution ─────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn replay_execution_normal_branch_in_place_mutate() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        /*flow_id=*/ None,
        "terminal",
        "not_applicable",
        "unowned",
        "success",
        "terminated",
        /*attempt_index=*/ 0,
        500,
        now,
    )
    .await;
    insert_attempt(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        0,
        Some("success"),
        /*lease_epoch=*/ 3,
    )
    .await;

    let r = fx
        .backend
        .replay_execution(ReplayExecutionArgs {
            execution_id: fx.exec_id.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("replay ok");
    assert_eq!(r, ReplayExecutionResult::Replayed {
        public_state: PublicState::Waiting,
    });

    // exec_core: runnable + eligible_now + public_state=waiting.
    let (phase, elig, pub_s, attempt_idx): (String, String, String, i32) = sqlx::query_as(
        "SELECT lifecycle_phase, eligibility_state, public_state, attempt_index \
         FROM ff_exec_core WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read exec_core");
    assert_eq!(phase, "runnable");
    assert_eq!(elig, "eligible_now");
    assert_eq!(pub_s, "waiting");
    // attempt_index NOT bumped (Rev 7 Fork 2 Option A).
    assert_eq!(attempt_idx, 0, "attempt_index preserved");

    // replay_count bumped in raw_fields.
    let raw_fields: serde_json::Value = sqlx::query_scalar(
        "SELECT raw_fields FROM ff_exec_core WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read raw_fields");
    assert_eq!(raw_fields.get("replay_count").and_then(|v| v.as_i64()), Some(1));

    // ff_attempt: in-place mutation of the existing row, outcome cleared,
    // lease_epoch bumped. No new row inserted.
    let attempts: Vec<(i32, Option<String>, i64)> = sqlx::query_as(
        "SELECT attempt_index, outcome, lease_epoch FROM ff_attempt \
         WHERE partition_key = $1 AND execution_id = $2 ORDER BY attempt_index",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_all(&fx.pool)
    .await
    .expect("read attempts");
    assert_eq!(attempts.len(), 1, "no new attempt row inserted");
    assert_eq!(attempts[0].0, 0);
    assert!(attempts[0].1.is_none(), "outcome cleared");
    assert_eq!(attempts[0].2, 4, "lease_epoch bumped from 3 → 4");

    // Outbox emit: one replayed event with branch=normal.
    let row: (serde_json::Value,) = sqlx::query_as(
        "SELECT details FROM ff_operator_event \
         WHERE partition_key = $1 AND execution_id = $2 AND event_type = 'replayed'",
    )
    .bind(i32::from(fx.part))
    .bind(fx.exec_uuid.to_string())
    .fetch_one(&fx.pool)
    .await
    .expect("read operator_event");
    assert_eq!(
        row.0.get("branch").and_then(|v| v.as_str()),
        Some("normal")
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn replay_execution_skipped_flow_member_resets_edge_group_counters() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    let flow_uuid = Uuid::new_v4();

    // Seed a skipped flow member.
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        Some(flow_uuid),
        "terminal",
        "not_applicable",
        "unowned",
        "failed",
        "terminated",
        0,
        500,
        now,
    )
    .await;
    insert_attempt(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        0,
        Some("skipped"),
        5,
    )
    .await;

    // Seed an upstream edge + edge_group row referencing our exec as
    // downstream, with non-zero counters to verify the reset.
    let upstream_uuid = Uuid::new_v4();
    let edge_uuid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ff_edge (partition_key, flow_id, edge_id, upstream_eid, \
                              downstream_eid, policy, policy_version) \
         VALUES ($1, $2, $3, $4, $5, '{}'::jsonb, 0)",
    )
    .bind(fx.part)
    .bind(flow_uuid)
    .bind(edge_uuid)
    .bind(upstream_uuid)
    .bind(fx.exec_uuid)
    .execute(&fx.pool)
    .await
    .expect("insert ff_edge");

    sqlx::query(
        "INSERT INTO ff_edge_group (partition_key, flow_id, downstream_eid, policy, \
                                    k_target, success_count, fail_count, skip_count, running_count) \
         VALUES ($1, $2, $3, '{}'::jsonb, 3, 2, 1, 1, 1)",
    )
    .bind(fx.part)
    .bind(flow_uuid)
    .bind(fx.exec_uuid)
    .execute(&fx.pool)
    .await
    .expect("insert ff_edge_group");

    let r = fx
        .backend
        .replay_execution(ReplayExecutionArgs {
            execution_id: fx.exec_id.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect("replay ok");
    assert_eq!(
        r,
        ReplayExecutionResult::Replayed {
            public_state: PublicState::WaitingChildren,
        }
    );

    // edge_group: success_count preserved, skip/fail/running zeroed.
    let (succ, fail, skip, running): (i32, i32, i32, i32) = sqlx::query_as(
        "SELECT success_count, fail_count, skip_count, running_count \
         FROM ff_edge_group \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(fx.part)
    .bind(flow_uuid)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read edge_group");
    assert_eq!(succ, 2, "success_count preserved (Valkey parity)");
    assert_eq!(fail, 0);
    assert_eq!(skip, 0);
    assert_eq!(running, 0);

    // exec_core: runnable + blocked_by_dependencies + waiting_children.
    let (phase, elig, pub_s): (String, String, String) = sqlx::query_as(
        "SELECT lifecycle_phase, eligibility_state, public_state \
         FROM ff_exec_core WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(fx.part)
    .bind(fx.exec_uuid)
    .fetch_one(&fx.pool)
    .await
    .expect("read exec_core");
    assert_eq!(phase, "runnable");
    assert_eq!(elig, "blocked_by_dependencies");
    assert_eq!(pub_s, "waiting_children");

    // Outbox emit: branch=skipped_flow_member + groups_reset=1.
    let row: (serde_json::Value,) = sqlx::query_as(
        "SELECT details FROM ff_operator_event \
         WHERE partition_key = $1 AND execution_id = $2 AND event_type = 'replayed'",
    )
    .bind(i32::from(fx.part))
    .bind(fx.exec_uuid.to_string())
    .fetch_one(&fx.pool)
    .await
    .expect("read operator_event");
    assert_eq!(
        row.0.get("branch").and_then(|v| v.as_str()),
        Some("skipped_flow_member")
    );
    assert_eq!(row.0.get("groups_reset").and_then(|v| v.as_i64()), Some(1));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn replay_execution_non_terminal_is_not_terminal() {
    let Some(fx) = seed_backend().await else {
        return;
    };
    let now = TimestampMs::now().0;
    insert_exec_core(
        &fx.pool,
        fx.part,
        fx.exec_uuid,
        None,
        "active",
        "eligible_now",
        "leased",
        "running",
        "running_attempt",
        0,
        500,
        now,
    )
    .await;
    let err = fx
        .backend
        .replay_execution(ReplayExecutionArgs {
            execution_id: fx.exec_id.clone(),
            now: TimestampMs::now(),
        })
        .await
        .expect_err("expected execution_not_terminal");
    assert!(matches!(
        err,
        EngineError::State(StateKind::ExecutionNotTerminal)
    ));
}

// ── cancel_flow_header + ack_cancel_member ───────────────────────

async fn insert_flow_core(pool: &PgPool, part: i16, flow_uuid: Uuid, state: &str, now: i64) {
    sqlx::query(
        "INSERT INTO ff_flow_core \
           (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms) \
         VALUES ($1, $2, 0, $3, $4)",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(state)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert ff_flow_core");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_flow_header_creates_backlog_rows() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let cfg = PartitionConfig::default();
    let flow_id = FlowId::new();
    let part = flow_partition(&flow_id, &cfg).index as i16;
    let flow_uuid = flow_id.0;
    let now = TimestampMs::now().0;

    insert_flow_core(&pool, part, flow_uuid, "active", now).await;

    // Two in-flight members under this flow (partition key matches
    // because execution_ids share the flow's partition_key via RFC-011).
    let mem_a = Uuid::new_v4();
    let mem_b = Uuid::new_v4();
    for mem in [mem_a, mem_b] {
        insert_exec_core(
            &pool, part, mem, Some(flow_uuid), "active", "eligible_now", "leased",
            "running", "running_attempt", 0, 100, now,
        )
        .await;
    }

    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), cfg);
    let r = backend
        .cancel_flow_header(CancelFlowArgs {
            flow_id: flow_id.clone(),
            reason: "op-shutdown".into(),
            cancellation_policy: "cancel_all".into(),
            now: TimestampMs::now(),
        })
        .await
        .expect("cancel_flow_header ok");
    match r {
        CancelFlowHeader::Cancelled {
            cancellation_policy,
            member_execution_ids,
        } => {
            assert_eq!(cancellation_policy, "cancel_all");
            assert_eq!(member_execution_ids.len(), 2);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }

    // Backlog header row exists.
    let header_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_cancel_backlog \
         WHERE partition_key = $1 AND flow_id = $2",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_one(&pool)
    .await
    .expect("count backlog");
    assert_eq!(header_count, 1);

    // Member rows: 2 pending.
    let member_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_cancel_backlog_member \
         WHERE partition_key = $1 AND flow_id = $2 AND ack_state = 'pending'",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_one(&pool)
    .await
    .expect("count backlog members");
    assert_eq!(member_count, 2);

    // Outbox emit: flow_cancel_requested (partition_key + flow_uuid).
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_operator_event \
         WHERE partition_key = $1 AND execution_id = $2 AND event_type = 'flow_cancel_requested'",
    )
    .bind(i32::from(part))
    .bind(flow_uuid.to_string())
    .fetch_one(&pool)
    .await
    .expect("count flow_cancel_requested");
    assert_eq!(n, 1);

    // Members flipped to cancelled on ff_exec_core.
    let cancelled_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_exec_core \
         WHERE partition_key = $1 AND flow_id = $2 AND lifecycle_phase = 'cancelled'",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_one(&pool)
    .await
    .expect("count cancelled members");
    assert_eq!(cancelled_count, 2);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_flow_header_already_terminal_is_idempotent() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let cfg = PartitionConfig::default();
    let flow_id = FlowId::new();
    let part = flow_partition(&flow_id, &cfg).index as i16;
    let flow_uuid = flow_id.0;
    let now = TimestampMs::now().0;

    // Pre-cancelled flow with stored cancellation_policy in raw_fields.
    sqlx::query(
        "INSERT INTO ff_flow_core \
           (partition_key, flow_id, graph_revision, public_flow_state, created_at_ms, raw_fields) \
         VALUES ($1, $2, 0, 'cancelled', $3, \
                 jsonb_build_object('cancellation_policy', 'flow_only', \
                                    'cancel_reason', 'prior'))",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert ff_flow_core");

    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), cfg);
    let r = backend
        .cancel_flow_header(CancelFlowArgs {
            flow_id: flow_id.clone(),
            reason: "retry".into(),
            cancellation_policy: "cancel_all".into(),
            now: TimestampMs::now(),
        })
        .await
        .expect("idempotent ok");
    match r {
        CancelFlowHeader::AlreadyTerminal {
            stored_cancellation_policy,
            stored_cancel_reason,
            member_execution_ids,
        } => {
            assert_eq!(stored_cancellation_policy.as_deref(), Some("flow_only"));
            assert_eq!(stored_cancel_reason.as_deref(), Some("prior"));
            assert!(member_execution_ids.is_empty());
        }
        other => panic!("expected AlreadyTerminal, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn ack_cancel_member_drains_and_deletes_parent_when_last() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let cfg = PartitionConfig::default();
    let flow_id = FlowId::new();
    let part = flow_partition(&flow_id, &cfg).index as i16;
    let flow_uuid = flow_id.0;
    let now = TimestampMs::now().0;

    insert_flow_core(&pool, part, flow_uuid, "active", now).await;

    // Two members under the flow.
    let mem_a = Uuid::new_v4();
    let mem_b = Uuid::new_v4();
    for mem in [mem_a, mem_b] {
        insert_exec_core(
            &pool, part, mem, Some(flow_uuid), "active", "eligible_now", "leased",
            "running", "running_attempt", 0, 100, now,
        )
        .await;
    }

    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), cfg);
    let cancel_r = backend
        .cancel_flow_header(CancelFlowArgs {
            flow_id: flow_id.clone(),
            reason: "test".into(),
            cancellation_policy: "cancel_all".into(),
            now: TimestampMs::now(),
        })
        .await
        .expect("cancel_flow_header ok");
    let members = match cancel_r {
        CancelFlowHeader::Cancelled {
            member_execution_ids,
            ..
        } => member_execution_ids,
        other => panic!("expected Cancelled, got {other:?}"),
    };
    assert_eq!(members.len(), 2);

    // Ack the first member.
    let eid_a = ExecutionId::parse(&members[0]).expect("parse exec id");
    backend
        .ack_cancel_member(&flow_id, &eid_a)
        .await
        .expect("ack member 1");

    // After first ack: 1 member row remains, header row still present.
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_cancel_backlog_member \
         WHERE partition_key = $1 AND flow_id = $2",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_one(&pool)
    .await
    .expect("count members");
    assert_eq!(remaining, 1);
    let header: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_cancel_backlog \
         WHERE partition_key = $1 AND flow_id = $2",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_one(&pool)
    .await
    .expect("count header");
    assert_eq!(header, 1, "header row persists while members remain");

    // Ack the second (last) member.
    let eid_b = ExecutionId::parse(&members[1]).expect("parse exec id");
    backend
        .ack_cancel_member(&flow_id, &eid_b)
        .await
        .expect("ack member 2");

    // After last ack: both member rows AND header row gone (CTE
    // conditional parent-DELETE).
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_cancel_backlog_member \
         WHERE partition_key = $1 AND flow_id = $2",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_one(&pool)
    .await
    .expect("count members");
    assert_eq!(remaining, 0);
    let header: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_cancel_backlog \
         WHERE partition_key = $1 AND flow_id = $2",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_one(&pool)
    .await
    .expect("count header");
    assert_eq!(header, 0, "header row drained with last member");

    // ack_cancel_member emits no outbox row (§4.2.7 — quiet parity).
    let ack_outbox: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM ff_operator_event \
         WHERE partition_key = $1 AND execution_id IN ($2, $3)",
    )
    .bind(i32::from(part))
    .bind(mem_a.to_string())
    .bind(mem_b.to_string())
    .fetch_one(&pool)
    .await
    .expect("count ack outbox");
    assert_eq!(ack_outbox, 0, "ack_cancel_member emits no outbox row");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn ack_cancel_member_idempotent_on_missing() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let cfg = PartitionConfig::default();
    let flow_id = FlowId::new();
    let exec_id = ExecutionId::solo(&LaneId::new("default"), &cfg);
    let backend: Arc<dyn EngineBackend> =
        PostgresBackend::from_pool(pool.clone(), cfg);
    // No backlog seeded — ack should succeed silently (idempotent).
    backend
        .ack_cancel_member(&flow_id, &exec_id)
        .await
        .expect("ack on missing rows is ok");
}
