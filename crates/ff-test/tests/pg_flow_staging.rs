//! Wave 4i — Postgres flow-staging integration tests.
//!
//! Exercises the four inherent methods landed on [`PostgresBackend`]:
//!
//!   * `create_flow`
//!   * `add_execution_to_flow`
//!   * `stage_dependency_edge`  (including stale-graph-revision CAS)
//!   * `apply_dependency_to_child`
//!
//! Plus the end-to-end scenario Wave 7b's unskip agent hit a wall on:
//! create flow + 1 downstream + 3 upstream members + Stage-A policy
//! `AnyOf{CancelRemaining}` + stage 3 edges + apply 3 deps + drive one
//! upstream to completion + `dispatch::dispatch_completion` + assert
//! downstream eligible + `ff_pending_cancel_groups` enqueued + the
//! `cancel_siblings_pending_flag` raised on the edge_group.
//!
//! # Known Wave 5a gap surfaced by this test
//!
//! The dispatcher's `advance_edge_group` flips
//! `cancel_siblings_pending_flag=true` but does NOT populate
//! `cancel_siblings_pending_members` — the reconciler / dispatcher then
//! has no exec ids to cancel. Captured as a TODO on the end-to-end
//! test; the four methods covered by this PR are the prerequisite for
//! closing that gap.
//!
//! # Running
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave4i_test \
//!   cargo test -p ff-test --test pg_flow_staging -- --ignored --test-threads=1
//! ```

use std::collections::HashMap;

use ff_backend_postgres::PostgresBackend;
use ff_core::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, ApplyDependencyToChildArgs,
    ApplyDependencyToChildResult, CreateExecutionArgs, CreateFlowArgs, CreateFlowResult,
    EdgeDependencyPolicy, OnSatisfied, StageDependencyEdgeArgs, StageDependencyEdgeResult,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{ContentionKind, EngineError};
use ff_core::partition::PartitionConfig;
use ff_core::types::{EdgeId, ExecutionId, FlowId, LaneId, Namespace, TimestampMs};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations");
    Some(pool)
}

fn now() -> TimestampMs {
    TimestampMs(1_700_000_000_000)
}

fn sample_exec_args(eid: &ExecutionId, lane: &LaneId) -> CreateExecutionArgs {
    CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new("tenant-wave4i"),
        lane_id: lane.clone(),
        execution_kind: "task".to_owned(),
        input_payload: b"{}".to_vec(),
        payload_encoding: Some("application/json".to_owned()),
        priority: 0,
        creator_identity: "pg-wave4i-test".to_owned(),
        idempotency_key: None,
        tags: HashMap::new(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: eid.partition(),
        now: now(),
    }
}

fn new_flow() -> FlowId {
    FlowId::from_uuid(Uuid::new_v4())
}

async fn make_flow_and_exec(
    backend: &PostgresBackend,
    pc: &PartitionConfig,
    lane: &LaneId,
) -> (FlowId, ExecutionId) {
    let flow = new_flow();
    backend
        .create_flow(&CreateFlowArgs {
            flow_id: flow.clone(),
            flow_kind: "graph".to_owned(),
            namespace: Namespace::new("tenant-wave4i"),
            now: now(),
        })
        .await
        .expect("create_flow");

    let eid = ExecutionId::for_flow(&flow, pc);
    backend
        .create_execution(sample_exec_args(&eid, lane))
        .await
        .expect("create_execution");
    backend
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow.clone(),
            execution_id: eid.clone(),
            now: now(),
        })
        .await
        .expect("add_execution_to_flow");
    (flow, eid)
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn create_flow_happy_and_idempotent() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default());
    let flow = new_flow();

    let r1 = backend
        .create_flow(&CreateFlowArgs {
            flow_id: flow.clone(),
            flow_kind: "graph".to_owned(),
            namespace: Namespace::new("tenant-wave4i"),
            now: now(),
        })
        .await
        .expect("create_flow #1");
    assert!(matches!(r1, CreateFlowResult::Created { .. }));

    let r2 = backend
        .create_flow(&CreateFlowArgs {
            flow_id: flow,
            flow_kind: "graph".to_owned(),
            namespace: Namespace::new("tenant-wave4i"),
            now: now(),
        })
        .await
        .expect("create_flow #2");
    assert!(matches!(r2, CreateFlowResult::AlreadySatisfied { .. }));
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn add_execution_bumps_graph_revision_and_node_count() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let pc = PartitionConfig::default();
    let backend = PostgresBackend::from_pool(pool, pc.clone());
    let lane = LaneId::new("wave4i-add");

    let (flow, eid) = make_flow_and_exec(&backend, &pc, &lane).await;

    let snap = backend
        .describe_flow(&flow)
        .await
        .expect("describe_flow")
        .expect("flow present");
    assert_eq!(snap.graph_revision, 1);
    assert_eq!(snap.node_count, 1);

    let again = backend
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow.clone(),
            execution_id: eid,
            now: now(),
        })
        .await
        .expect("add #2");
    assert!(matches!(
        again,
        AddExecutionToFlowResult::AlreadyMember { .. }
    ));
    let snap2 = backend.describe_flow(&flow).await.unwrap().unwrap();
    assert_eq!(snap2.graph_revision, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn stage_and_apply_edge_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let pc = PartitionConfig::default();
    let backend = PostgresBackend::from_pool(pool, pc.clone());
    let lane = LaneId::new("wave4i-stage");

    let (flow, upstream) = make_flow_and_exec(&backend, &pc, &lane).await;
    let downstream = ExecutionId::for_flow(&flow, &pc);
    backend
        .create_execution(sample_exec_args(&downstream, &lane))
        .await
        .unwrap();
    backend
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow.clone(),
            execution_id: downstream.clone(),
            now: now(),
        })
        .await
        .unwrap();

    let edge_id = EdgeId::from_uuid(Uuid::new_v4());
    let staged = backend
        .stage_dependency_edge(&StageDependencyEdgeArgs {
            flow_id: flow.clone(),
            edge_id: edge_id.clone(),
            upstream_execution_id: upstream.clone(),
            downstream_execution_id: downstream.clone(),
            dependency_kind: "success_only".to_owned(),
            data_passing_ref: None,
            expected_graph_revision: 2,
            now: now(),
        })
        .await
        .expect("stage_dependency_edge");
    let StageDependencyEdgeResult::Staged { new_graph_revision, .. } = staged;
    assert_eq!(new_graph_revision, 3);

    let applied = backend
        .apply_dependency_to_child(&ApplyDependencyToChildArgs {
            flow_id: flow.clone(),
            edge_id: edge_id.clone(),
            downstream_execution_id: downstream.clone(),
            upstream_execution_id: upstream.clone(),
            graph_revision: 3,
            dependency_kind: "success_only".to_owned(),
            data_passing_ref: None,
            now: now(),
        })
        .await
        .expect("apply_dependency_to_child");
    assert!(matches!(
        applied,
        ApplyDependencyToChildResult::Applied { unsatisfied_count: 1 }
    ));

    let applied_again = backend
        .apply_dependency_to_child(&ApplyDependencyToChildArgs {
            flow_id: flow.clone(),
            edge_id: edge_id.clone(),
            downstream_execution_id: downstream.clone(),
            upstream_execution_id: upstream.clone(),
            graph_revision: 3,
            dependency_kind: "success_only".to_owned(),
            data_passing_ref: None,
            now: now(),
        })
        .await
        .expect("apply #2");
    assert!(matches!(
        applied_again,
        ApplyDependencyToChildResult::AlreadyApplied
    ));

    let edge = backend
        .describe_edge(&flow, &edge_id)
        .await
        .expect("describe_edge")
        .expect("edge present");
    assert_eq!(edge.edge_state, "applied");
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn stage_rejects_stale_graph_revision() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let pc = PartitionConfig::default();
    let backend = PostgresBackend::from_pool(pool, pc.clone());
    let lane = LaneId::new("wave4i-stale");

    let (flow, upstream) = make_flow_and_exec(&backend, &pc, &lane).await;
    let downstream = ExecutionId::for_flow(&flow, &pc);
    backend
        .create_execution(sample_exec_args(&downstream, &lane))
        .await
        .unwrap();
    backend
        .add_execution_to_flow(&AddExecutionToFlowArgs {
            flow_id: flow.clone(),
            execution_id: downstream.clone(),
            now: now(),
        })
        .await
        .unwrap();

    let err = backend
        .stage_dependency_edge(&StageDependencyEdgeArgs {
            flow_id: flow.clone(),
            edge_id: EdgeId::from_uuid(Uuid::new_v4()),
            upstream_execution_id: upstream,
            downstream_execution_id: downstream,
            dependency_kind: "success_only".to_owned(),
            data_passing_ref: None,
            expected_graph_revision: 99,
            now: now(),
        })
        .await
        .expect_err("should reject stale rev");
    assert!(matches!(
        err,
        EngineError::Contention(ContentionKind::StaleGraphRevision)
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres; set FF_PG_TEST_URL"]
async fn e2e_any_of_cancel_remaining_cascade() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let pc = PartitionConfig::default();
    let backend = PostgresBackend::from_pool(pool.clone(), pc.clone());
    let lane = LaneId::new("wave4i-e2e");

    let flow = new_flow();
    backend
        .create_flow(&CreateFlowArgs {
            flow_id: flow.clone(),
            flow_kind: "graph".to_owned(),
            namespace: Namespace::new("tenant-wave4i"),
            now: now(),
        })
        .await
        .expect("create_flow");

    let upstreams: Vec<ExecutionId> =
        (0..3).map(|_| ExecutionId::for_flow(&flow, &pc)).collect();
    let downstream = ExecutionId::for_flow(&flow, &pc);

    for eid in upstreams.iter().chain(std::iter::once(&downstream)) {
        backend
            .create_execution(sample_exec_args(eid, &lane))
            .await
            .expect("create_execution");
        backend
            .add_execution_to_flow(&AddExecutionToFlowArgs {
                flow_id: flow.clone(),
                execution_id: eid.clone(),
                now: now(),
            })
            .await
            .expect("add_execution_to_flow");
    }

    backend
        .set_edge_group_policy(
            &flow,
            &downstream,
            EdgeDependencyPolicy::AnyOf {
                on_satisfied: OnSatisfied::CancelRemaining,
            },
        )
        .await
        .expect("set_edge_group_policy");

    let mut rev: u64 = 4;
    let mut edges = Vec::new();
    for up in &upstreams {
        let edge_id = EdgeId::from_uuid(Uuid::new_v4());
        backend
            .stage_dependency_edge(&StageDependencyEdgeArgs {
                flow_id: flow.clone(),
                edge_id: edge_id.clone(),
                upstream_execution_id: up.clone(),
                downstream_execution_id: downstream.clone(),
                dependency_kind: "success_only".to_owned(),
                data_passing_ref: None,
                expected_graph_revision: rev,
                now: now(),
            })
            .await
            .expect("stage_dependency_edge");
        rev += 1;
        backend
            .apply_dependency_to_child(&ApplyDependencyToChildArgs {
                flow_id: flow.clone(),
                edge_id: edge_id.clone(),
                downstream_execution_id: downstream.clone(),
                upstream_execution_id: up.clone(),
                graph_revision: rev,
                dependency_kind: "success_only".to_owned(),
                data_passing_ref: None,
                now: now(),
            })
            .await
            .expect("apply_dependency_to_child");
        edges.push(edge_id);
    }

    let part_i16 = ff_core::partition::flow_partition(&flow, &pc).index as i16;
    let first_up_uuid = parse_exec_uuid(&upstreams[0]);

    sqlx::query(
        "UPDATE ff_exec_core SET lifecycle_phase = 'terminal', \
            public_state = 'completed', terminal_at_ms = $3 \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part_i16)
    .bind(first_up_uuid)
    .bind(now().0)
    .execute(&pool)
    .await
    .expect("UPDATE upstream");

    let event_id: i64 = sqlx::query_scalar(
        "INSERT INTO ff_completion_event \
            (partition_key, execution_id, flow_id, outcome, occurred_at_ms) \
         VALUES ($1, $2, $3, 'success', $4) RETURNING event_id",
    )
    .bind(part_i16)
    .bind(first_up_uuid)
    .bind(flow.0)
    .bind(now().0)
    .fetch_one(&pool)
    .await
    .expect("INSERT completion");

    let outcome = ff_backend_postgres::dispatch::dispatch_completion(&pool, event_id)
        .await
        .expect("dispatch_completion");
    assert!(matches!(
        outcome,
        ff_backend_postgres::dispatch::DispatchOutcome::Advanced(n) if n >= 1
    ));

    let down_uuid = parse_exec_uuid(&downstream);
    let elig: String = sqlx::query_scalar(
        "SELECT eligibility_state FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part_i16)
    .bind(down_uuid)
    .fetch_one(&pool)
    .await
    .expect("SELECT downstream");
    assert_eq!(elig, "eligible_now");

    let flag: bool = sqlx::query_scalar(
        "SELECT cancel_siblings_pending_flag FROM ff_edge_group \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(part_i16)
    .bind(flow.0)
    .bind(down_uuid)
    .fetch_one(&pool)
    .await
    .expect("SELECT edge_group flag");
    assert!(flag);

    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ff_pending_cancel_groups \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(part_i16)
    .bind(flow.0)
    .bind(down_uuid)
    .fetch_one(&pool)
    .await
    .expect("SELECT pending_cancel_groups");
    assert_eq!(pending_count, 1);
    // TODO(wave-5a): once `dispatch::advance_edge_group` populates
    // `cancel_siblings_pending_members`, extend this test to assert the
    // two remaining upstream siblings flip to `cancelled`.
}

fn parse_exec_uuid(eid: &ExecutionId) -> Uuid {
    let s = eid.as_str();
    let colon = s.rfind("}:").expect("hash-tag delimiter");
    Uuid::parse_str(&s[colon + 2..]).expect("uuid suffix")
}
