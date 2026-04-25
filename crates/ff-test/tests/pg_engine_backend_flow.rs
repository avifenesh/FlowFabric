//! Wave 4c — Postgres flow-family `EngineBackend` integration tests.
//!
//! Exercises the 6 methods landed in this wave against a live Postgres:
//!
//! * `describe_flow` — round-trip from seeded SQL rows.
//! * `list_flows` — cursor-paginated enumeration correctness.
//! * `list_edges` + `describe_edge` — happy-path reads.
//! * `cancel_flow` — three-member cascade.
//! * `cancel_flow` 40001 retry — exhausts → `ContentionKind::RetryExhausted`.
//! * `set_edge_group_policy` — ordering violation when edges are already
//!   staged (parity with the Valkey `invalid_input` error path).
//!
//! # Running
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave4c_test \
//!   cargo test -p ff-test --test pg_engine_backend_flow -- --ignored
//! ```
//!
//! All tests are `#[ignore]` by default so a workspace-wide
//! `cargo test` without a live Postgres stays green.

use std::sync::Arc;

use ff_backend_postgres::PostgresBackend;
use ff_core::backend::{CancelFlowPolicy, CancelFlowWait};
use ff_core::contracts::{
    CancelFlowResult, EdgeDependencyPolicy, EdgeDirection, SetEdgeGroupPolicyResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError, ValidationKind};
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::types::{ExecutionId, FlowId};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

/// Connect + migrate; skip the test if `FF_PG_TEST_URL` is unset.
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

fn flow_part(flow_id: &FlowId) -> i16 {
    ff_core::partition::flow_partition(flow_id, &PartitionConfig::default())
        .index as i16
}

async fn seed_flow(pool: &PgPool, flow_id: &FlowId, public_state: &str) {
    let part = flow_part(flow_id);
    sqlx::query(
        "INSERT INTO ff_flow_core \
         (partition_key, flow_id, graph_revision, public_flow_state, \
          created_at_ms, raw_fields) \
         VALUES ($1, $2, 0, $3, 1, $4)",
    )
    .bind(part)
    .bind(flow_id.0)
    .bind(public_state)
    .bind(serde_json::json!({
        "flow_kind": "dag",
        "namespace": "test",
        "node_count": 3,
        "edge_count": 2,
        "last_mutation_at_ms": 1_i64,
        "cairn.task_id": "abc",
    }))
    .execute(pool)
    .await
    .expect("insert ff_flow_core");
}

async fn seed_exec(pool: &PgPool, flow_id: &FlowId, exec_id: &ExecutionId) {
    let part = flow_part(flow_id);
    let uuid = exec_id
        .as_str()
        .rsplit_once("}:")
        .and_then(|(_, tail)| Uuid::parse_str(tail).ok())
        .expect("exec id uuid suffix");
    sqlx::query(
        "INSERT INTO ff_exec_core \
         (partition_key, execution_id, flow_id, lane_id, \
          lifecycle_phase, ownership_state, eligibility_state, \
          public_state, attempt_state, created_at_ms) \
         VALUES ($1, $2, $3, 'lane-a', 'eligible', 'unowned', \
                 'eligible', 'eligible', 'none', 1)",
    )
    .bind(part)
    .bind(uuid)
    .bind(flow_id.0)
    .execute(pool)
    .await
    .expect("insert ff_exec_core");
}

async fn seed_edge(
    pool: &PgPool,
    flow_id: &FlowId,
    edge_uuid: Uuid,
    upstream: &ExecutionId,
    downstream: &ExecutionId,
) {
    let part = flow_part(flow_id);
    let up_uuid = upstream
        .as_str()
        .rsplit_once("}:")
        .and_then(|(_, t)| Uuid::parse_str(t).ok())
        .expect("up uuid");
    let down_uuid = downstream
        .as_str()
        .rsplit_once("}:")
        .and_then(|(_, t)| Uuid::parse_str(t).ok())
        .expect("down uuid");
    sqlx::query(
        "INSERT INTO ff_edge \
         (partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(part)
    .bind(flow_id.0)
    .bind(edge_uuid)
    .bind(up_uuid)
    .bind(down_uuid)
    .bind(serde_json::json!({
        "dependency_kind": "success_only",
        "satisfaction_condition": "all_required",
        "edge_state": "pending",
        "created_at_ms": 1,
        "created_by": "engine",
    }))
    .execute(pool)
    .await
    .expect("insert ff_edge");
}

fn backend(pool: PgPool) -> Arc<dyn EngineBackend> {
    PostgresBackend::from_pool(pool, PartitionConfig::default())
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn describe_flow_round_trip() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let backend = backend(pool);
    let snap = backend
        .describe_flow(&flow_id)
        .await
        .expect("describe_flow ok")
        .expect("flow present");
    assert_eq!(snap.flow_id, flow_id);
    assert_eq!(snap.public_flow_state, "open");
    assert_eq!(snap.flow_kind, "dag");
    assert_eq!(snap.namespace.as_str(), "test");
    assert_eq!(snap.node_count, 3);
    assert_eq!(snap.edge_count, 2);
    assert_eq!(snap.tags.get("cairn.task_id").map(String::as_str), Some("abc"));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn describe_flow_absent_returns_none() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    let backend = backend(pool);
    assert!(backend.describe_flow(&flow_id).await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn list_flows_cursor_pagination() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    let mut by_part: std::collections::HashMap<i16, Vec<FlowId>> = Default::default();
    for _ in 0..200 {
        let id = FlowId::new();
        let p = flow_part(&id);
        let bucket = by_part.entry(p).or_default();
        if bucket.len() < 5 {
            bucket.push(id);
        }
        if by_part.values().any(|v| v.len() >= 5) {
            break;
        }
    }
    let (target_part, flows) = by_part
        .into_iter()
        .find(|(_, v)| v.len() >= 5)
        .expect("found 5 flows on one partition");

    for flow_id in &flows {
        seed_flow(&pool, flow_id, "open").await;
    }
    let partition_key = {
        let p = ff_core::partition::Partition {
            family: ff_core::partition::PartitionFamily::Flow,
            index: target_part as u16,
        };
        PartitionKey::from(&p)
    };

    let backend = backend(pool);

    let page1 = backend
        .list_flows(partition_key.clone(), None, 2)
        .await
        .expect("page1");
    assert_eq!(page1.flows.len(), 2);
    assert!(page1.next_cursor.is_some());

    let mut seen: std::collections::HashSet<FlowId> = Default::default();
    for f in &page1.flows {
        seen.insert(f.flow_id.clone());
    }
    let mut cursor = page1.next_cursor;
    while let Some(c) = cursor {
        let page = backend
            .list_flows(partition_key.clone(), Some(c), 10)
            .await
            .expect("page");
        for f in &page.flows {
            seen.insert(f.flow_id.clone());
        }
        cursor = page.next_cursor;
    }
    for f in &flows {
        assert!(seen.contains(f), "seeded flow {f} missing from listing");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn list_edges_and_describe_edge_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let cfg = PartitionConfig::default();
    let upstream = ExecutionId::for_flow(&flow_id, &cfg);
    let downstream = ExecutionId::for_flow(&flow_id, &cfg);
    seed_exec(&pool, &flow_id, &upstream).await;
    seed_exec(&pool, &flow_id, &downstream).await;
    let edge_uuid = Uuid::new_v4();
    seed_edge(&pool, &flow_id, edge_uuid, &upstream, &downstream).await;

    let backend = backend(pool);
    let out = backend
        .list_edges(
            &flow_id,
            EdgeDirection::Outgoing {
                from_node: upstream.clone(),
            },
        )
        .await
        .expect("list_edges out");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].upstream_execution_id, upstream);
    assert_eq!(out[0].downstream_execution_id, downstream);
    assert_eq!(out[0].dependency_kind, "success_only");

    let inc = backend
        .list_edges(
            &flow_id,
            EdgeDirection::Incoming {
                to_node: downstream.clone(),
            },
        )
        .await
        .expect("list_edges in");
    assert_eq!(inc.len(), 1);

    let edge_id = ff_core::types::EdgeId::from_uuid(edge_uuid);
    let single = backend
        .describe_edge(&flow_id, &edge_id)
        .await
        .expect("describe_edge")
        .expect("present");
    assert_eq!(single.edge_id, edge_id);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_flow_cascade_three_members() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let cfg = PartitionConfig::default();
    let members: Vec<ExecutionId> =
        (0..3).map(|_| ExecutionId::for_flow(&flow_id, &cfg)).collect();
    for m in &members {
        seed_exec(&pool, &flow_id, m).await;
    }

    let backend = backend(pool.clone());
    let res = backend
        .cancel_flow(&flow_id, CancelFlowPolicy::CancelAll, CancelFlowWait::NoWait)
        .await
        .expect("cancel_flow ok");
    match &res {
        CancelFlowResult::Cancelled {
            member_execution_ids,
            ..
        } => {
            assert_eq!(member_execution_ids.len(), 3);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }

    let part = flow_part(&flow_id);
    let cancelled_n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ff_exec_core \
         WHERE partition_key = $1 AND flow_id = $2 AND lifecycle_phase = 'cancelled'",
    )
    .bind(part)
    .bind(flow_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cancelled_n, 3);

    let events_n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ff_completion_event \
         WHERE partition_key = $1 AND flow_id = $2 AND outcome = 'cancelled'",
    )
    .bind(part)
    .bind(flow_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events_n, 3);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_flow_40001_retry_exhausts() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let blocker_pool = pool.clone();
    let blocker_flow = flow_id.clone();
    let part = flow_part(&flow_id);
    let blocker = tokio::spawn(async move {
        for _ in 0..200 {
            let mut tx = match blocker_pool.begin().await {
                Ok(t) => t,
                Err(_) => continue,
            };
            let _ = sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                .execute(&mut *tx)
                .await;
            let _ = sqlx::query(
                "SELECT raw_fields FROM ff_flow_core \
                 WHERE partition_key = $1 AND flow_id = $2",
            )
            .bind(part)
            .bind(blocker_flow.0)
            .fetch_optional(&mut *tx)
            .await;
            let _ = sqlx::query(
                "UPDATE ff_flow_core \
                 SET raw_fields = raw_fields || '{}'::jsonb \
                 WHERE partition_key = $1 AND flow_id = $2",
            )
            .bind(part)
            .bind(blocker_flow.0)
            .execute(&mut *tx)
            .await;
            let _ = tx.commit().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    });

    let backend = backend(pool.clone());
    let mut saw_retry_exhausted = false;
    for _ in 0..20 {
        match backend
            .cancel_flow(
                &flow_id,
                CancelFlowPolicy::CancelAll,
                CancelFlowWait::NoWait,
            )
            .await
        {
            Err(EngineError::Contention(ContentionKind::RetryExhausted)) => {
                saw_retry_exhausted = true;
                break;
            }
            Ok(_) => {
                let _ = sqlx::query(
                    "UPDATE ff_flow_core \
                     SET public_flow_state = 'open', terminal_at_ms = NULL \
                     WHERE partition_key = $1 AND flow_id = $2",
                )
                .bind(part)
                .bind(flow_id.0)
                .execute(&pool)
                .await;
            }
            Err(other) => {
                eprintln!("cancel_flow returned unexpected {other:?}; retrying");
            }
        }
    }
    blocker.abort();
    assert!(
        saw_retry_exhausted,
        "expected ContentionKind::RetryExhausted within 20 attempts"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn set_edge_group_policy_rejects_when_edges_already_staged() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let cfg = PartitionConfig::default();
    let upstream = ExecutionId::for_flow(&flow_id, &cfg);
    let downstream = ExecutionId::for_flow(&flow_id, &cfg);
    seed_exec(&pool, &flow_id, &upstream).await;
    seed_exec(&pool, &flow_id, &downstream).await;
    seed_edge(&pool, &flow_id, Uuid::new_v4(), &upstream, &downstream).await;

    let backend = backend(pool);
    let err = backend
        .set_edge_group_policy(&flow_id, &downstream, EdgeDependencyPolicy::AllOf)
        .await
        .expect_err("must reject after edges staged");
    match err {
        EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail,
        } => assert!(
            detail.contains("edge_group_policy_already_fixed"),
            "unexpected detail: {detail}"
        ),
        other => panic!("expected Validation(InvalidInput), got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn set_edge_group_policy_fresh_write_then_idempotent() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let cfg = PartitionConfig::default();
    let downstream = ExecutionId::for_flow(&flow_id, &cfg);

    let backend = backend(pool);
    let first = backend
        .set_edge_group_policy(&flow_id, &downstream, EdgeDependencyPolicy::AllOf)
        .await
        .expect("fresh set ok");
    assert_eq!(first, SetEdgeGroupPolicyResult::Set);

    let second = backend
        .set_edge_group_policy(&flow_id, &downstream, EdgeDependencyPolicy::AllOf)
        .await
        .expect("re-set ok");
    assert_eq!(second, SetEdgeGroupPolicyResult::AlreadySet);
}

/// Issue #298: `cancel_flow(WaitTimeout)` polls `describe_flow` until
/// `public_flow_state = "cancelled"`. On Postgres the cancel
/// transaction flips the state row synchronously, so a memberless flow
/// observes the terminal state on the first poll — well inside the
/// 500ms deadline.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_flow_wait_timeout_returns_cancelled_when_cascade_completes_in_time() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let backend = backend(pool);
    let res = backend
        .cancel_flow(
            &flow_id,
            CancelFlowPolicy::CancelAll,
            CancelFlowWait::WaitTimeout(std::time::Duration::from_millis(500)),
        )
        .await
        .expect("cancel_flow WaitTimeout ok");
    assert!(
        matches!(res, CancelFlowResult::Cancelled { .. }),
        "expected Cancelled, got {res:?}",
    );

    let snap = backend
        .describe_flow(&flow_id)
        .await
        .expect("describe_flow ok")
        .expect("flow must exist");
    assert_eq!(snap.public_flow_state, "cancelled");
}

/// Issue #298: the shared `wait_for_flow_cancellation` helper must
/// surface `EngineError::Timeout` when invoked with a deadline that
/// cannot be met against a still-open flow.
#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_flow_wait_timeout_returns_timeout_when_deadline_exceeded() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let flow_id = FlowId::new();
    seed_flow(&pool, &flow_id, "open").await;

    let backend = backend(pool);
    let err = ff_core::engine_backend::wait_for_flow_cancellation(
        &*backend,
        &flow_id,
        std::time::Duration::from_millis(1),
    )
    .await
    .expect_err("must time out on an open flow");
    match err {
        EngineError::Timeout { op, elapsed } => {
            assert_eq!(op, "cancel_flow");
            assert_eq!(elapsed, std::time::Duration::from_millis(1));
        }
        other => panic!("expected EngineError::Timeout, got {other:?}"),
    }
}
