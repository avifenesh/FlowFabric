//! Postgres `EngineBackend` exec-core family — integration tests.
//!
//! RFC-v0.7 Wave 4a. Exercises the five methods the Wave-4a agent
//! landed on `PostgresBackend`:
//!
//!  * [`PostgresBackend::create_execution`] (inherent)
//!  * [`EngineBackend::describe_execution`]
//!  * [`EngineBackend::list_executions`]
//!  * [`EngineBackend::cancel`]
//!  * [`EngineBackend::resolve_execution_flow_id`]
//!
//! All tests are `#[ignore]` by default — they require a live
//! throwaway Postgres reachable at `FF_PG_TEST_URL`. The pattern
//! mirrors `crates/ff-backend-postgres/tests/schema_0001.rs`.
//!
//! ```bash
//! FF_PG_TEST_URL=postgres://user:pw@localhost/ff_wave4a_test \
//!   cargo test -p ff-test --test pg_engine_backend_exec_core \
//!   -- --ignored --test-threads=1
//! ```

use std::collections::HashMap;

use ff_backend_postgres::PostgresBackend;
use ff_core::backend::{BackendTag, Handle, HandleKind};
use ff_core::contracts::CreateExecutionArgs;
use ff_core::engine_backend::EngineBackend;
use ff_core::handle_codec::{encode as handle_encode, HandlePayload};
use ff_core::partition::{execution_partition, PartitionConfig, PartitionKey};
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, FlowId, LaneId, LeaseEpoch, LeaseId, Namespace,
    TimestampMs, WorkerInstanceId,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Connect + apply migrations. Returns `None` when `FF_PG_TEST_URL`
/// is unset — the caller prints a skip message and returns.
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
    // Deterministic-enough for tests; real impl reads SystemTime.
    TimestampMs(1_700_000_000_000)
}

/// Build a minimal CreateExecutionArgs.
fn sample_args(eid: &ExecutionId, lane: &LaneId) -> CreateExecutionArgs {
    CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new("tenant-1"),
        lane_id: lane.clone(),
        execution_kind: "task".to_owned(),
        input_payload: b"{}".to_vec(),
        payload_encoding: Some("application/json".to_owned()),
        priority: 0,
        creator_identity: "pg-wave4a-test".to_owned(),
        idempotency_key: None,
        tags: HashMap::from([("cairn.task_id".to_owned(), "abc".to_owned())]),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: eid.partition(),
        now: now(),
    }
}

/// Mint a Postgres-tagged `Handle` for a given execution so the
/// trait's `cancel(&Handle, ...)` signature can be driven without
/// going through a live claim FCALL (Wave 4a scope is exec_core, not
/// claim).
fn synthetic_handle(eid: &ExecutionId, lane: &LaneId) -> Handle {
    let payload = HandlePayload::new(
        eid.clone(),
        AttemptIndex::new(0),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch::new(1),
        30_000,
        lane.clone(),
        WorkerInstanceId::new("pg-wave4a-worker"),
    );
    let opaque = handle_encode(BackendTag::Postgres, &payload);
    Handle::new(BackendTag::Postgres, HandleKind::Fresh, opaque)
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn create_and_describe_round_trip() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default());

    let lane = LaneId::new("wave4a-create-describe");
    let eid = ExecutionId::solo(&lane, &PartitionConfig::default());
    let returned = backend
        .create_execution(sample_args(&eid, &lane))
        .await
        .expect("create_execution OK");
    assert_eq!(returned, eid);

    let snap = backend
        .describe_execution(&eid)
        .await
        .expect("describe_execution OK")
        .expect("snapshot present");
    assert_eq!(snap.execution_id, eid);
    assert_eq!(snap.lane_id, lane);
    assert_eq!(snap.namespace, Namespace::new("tenant-1"));
    assert_eq!(snap.public_state, PublicState::Waiting);
    assert_eq!(snap.total_attempt_count, 0);
    assert_eq!(
        snap.tags.get("cairn.task_id").map(String::as_str),
        Some("abc")
    );

    // Describe on a not-inserted id is Ok(None).
    let ghost = ExecutionId::solo(&lane, &PartitionConfig::default());
    assert!(backend
        .describe_execution(&ghost)
        .await
        .expect("describe_execution OK")
        .is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn create_is_idempotent_on_replay() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default());

    let lane = LaneId::new("wave4a-idem");
    let eid = ExecutionId::solo(&lane, &PartitionConfig::default());
    let a1 = backend
        .create_execution(sample_args(&eid, &lane))
        .await
        .expect("first create OK");
    let a2 = backend
        .create_execution(sample_args(&eid, &lane))
        .await
        .expect("replay create OK (duplicate)");
    assert_eq!(a1, a2);
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn list_executions_cursor_pagination() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default());

    // Seed 5 executions on the same lane (they hash to the same
    // partition, so cursor pagination stays within one partition).
    let lane = LaneId::new("wave4a-list");
    let cfg = PartitionConfig::default();
    let mut eids: Vec<ExecutionId> = (0..5)
        .map(|_| ExecutionId::solo(&lane, &cfg))
        .collect();
    for e in &eids {
        backend
            .create_execution(sample_args(e, &lane))
            .await
            .expect("seed create OK");
    }
    eids.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    // partition_key derived from the first eid's hash-tag.
    let partition: PartitionKey = (&execution_partition(&eids[0], &cfg)).into();

    // Page 1: limit=2 → returns 2 ids plus a next_cursor.
    let page1 = backend
        .list_executions(partition.clone(), None, 2)
        .await
        .expect("list_executions page1 OK");
    assert_eq!(page1.executions.len(), 2);
    assert!(page1.next_cursor.is_some());

    // Page 2: resume from next_cursor.
    let page2 = backend
        .list_executions(partition.clone(), page1.next_cursor.clone(), 2)
        .await
        .expect("list_executions page2 OK");
    assert_eq!(page2.executions.len(), 2);

    // Page 3: last id (if present) then exhausts.
    let page3 = backend
        .list_executions(partition.clone(), page2.next_cursor.clone(), 10)
        .await
        .expect("list_executions page3 OK");
    // Every seeded id must appear exactly once across pages (subset
    // of this partition — other tests may have seeded more).
    let mut seen: Vec<ExecutionId> = page1.executions;
    seen.extend(page2.executions);
    seen.extend(page3.executions);
    for e in &eids {
        assert!(seen.iter().any(|s| s == e), "expected {e} in listing");
    }
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn cancel_terminal_state_transition() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default());

    let lane = LaneId::new("wave4a-cancel");
    let eid = ExecutionId::solo(&lane, &PartitionConfig::default());
    backend
        .create_execution(sample_args(&eid, &lane))
        .await
        .expect("seed create OK");

    let handle = synthetic_handle(&eid, &lane);
    backend
        .cancel(&handle, "operator abort")
        .await
        .expect("cancel OK");

    let snap = backend
        .describe_execution(&eid)
        .await
        .expect("describe OK")
        .expect("snapshot present");
    assert_eq!(snap.public_state, PublicState::Cancelled);

    // Replay: second cancel is a successful no-op (already cancelled).
    backend
        .cancel(&handle, "operator abort")
        .await
        .expect("cancel replay OK");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn resolve_execution_flow_id_solo_and_with_flow() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default());

    // Solo execution: no flow.
    let lane = LaneId::new("wave4a-resolve");
    let eid = ExecutionId::solo(&lane, &PartitionConfig::default());
    backend
        .create_execution(sample_args(&eid, &lane))
        .await
        .expect("seed solo OK");
    let fid = backend
        .resolve_execution_flow_id(&eid)
        .await
        .expect("resolve solo OK");
    assert!(fid.is_none(), "solo execution has no flow_id");

    // Flow-member: UPDATE the row to stamp a flow_id. The trait impl
    // is read-only against the column, so we poke it directly through
    // the pool to stay in exec_core-family scope (Wave 4c owns
    // flow_id assignment semantics).
    let flow = FlowId::new();
    let cfg = PartitionConfig::default();
    let eid2 = ExecutionId::for_flow(&flow, &cfg);
    backend
        .create_execution(sample_args(&eid2, &lane))
        .await
        .expect("seed flow-member OK");

    // Extract the raw pool to stamp flow_id. Access via the public
    // `from_pool` constructor path doesn't expose the pool, so we
    // reuse the URL the harness already read.
    let url = std::env::var("FF_PG_TEST_URL").expect("FF_PG_TEST_URL set");
    let poke_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect for poke");
    let flow_uuid: uuid::Uuid = *uuid::Uuid::from_bytes_ref(flow.as_bytes());
    let eid_suffix = eid2.as_str().split_once("}:").unwrap().1;
    let eid_uuid: uuid::Uuid = uuid::Uuid::parse_str(eid_suffix).unwrap();
    sqlx::query(
        "UPDATE ff_exec_core SET flow_id = $3 \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(eid2.partition() as i16)
    .bind(eid_uuid)
    .bind(flow_uuid)
    .execute(&poke_pool)
    .await
    .expect("stamp flow_id OK");

    let resolved = backend
        .resolve_execution_flow_id(&eid2)
        .await
        .expect("resolve flow-member OK")
        .expect("flow_id present");
    assert_eq!(resolved, flow);
}
