//! Backend-matrix variant of the cursor-pagination half of
//! `engine_backend_list_executions.rs` — RFC-v0.7 Wave 7a.
//!
//! Exercises [`EngineBackend::list_executions`] with keyset cursors
//! on both Valkey + Postgres backends. The HTTP half (which boots
//! `ff-server`) stays in the Valkey-only file — ff-server's
//! Postgres path is a sibling wave's scope.

use std::collections::HashMap;

use ff_core::contracts::CreateExecutionArgs;
use ff_core::partition::{execution_partition, PartitionKey};
use ff_core::types::{ExecutionId, FlowId, LaneId, Namespace, TimestampMs};
use ff_test::backend_matrix::{run_matrix, BackendFixture};

const LANE: &str = "matrix-list-execs-lane";
const NS: &str = "matrix-list-execs-ns";

fn sample_args(
    eid: &ExecutionId,
    lane: &LaneId,
    partition_id: u16,
) -> CreateExecutionArgs {
    CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new(NS),
        lane_id: lane.clone(),
        execution_kind: "matrix_list_kind".to_owned(),
        input_payload: b"{}".to_vec(),
        payload_encoding: Some("application/json".to_owned()),
        priority: 0,
        creator_identity: "matrix-list-test".to_owned(),
        idempotency_key: None,
        tags: HashMap::new(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id,
        now: TimestampMs::now(),
    }
}

/// Seed `n` executions all co-located to the same flow partition so
/// they land in a single partition, and return the sorted id list.
async fn seed_n(fx: &BackendFixture, flow: &FlowId, n: usize) -> Vec<ExecutionId> {
    let cfg = *fx.partition_config();
    let lane = LaneId::new(LANE);
    let mut ids: Vec<ExecutionId> = Vec::with_capacity(n);
    for _ in 0..n {
        let eid = ExecutionId::for_flow(flow, &cfg);
        let partition = execution_partition(&eid, &cfg);
        let args = sample_args(&eid, &lane, partition.index);
        fx.seed_execution(args).await;
        ids.push(eid);
    }
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    ids
}

#[tokio::test(flavor = "multi_thread")]
async fn list_executions_threads_cursor_across_pages() {
    run_matrix(|fx| async move {
        let flow = FlowId::new();
        let seeded = seed_n(&fx, &flow, 15).await;
        let cfg = *fx.partition_config();
        let partition: PartitionKey =
            (&ff_core::partition::flow_partition(&flow, &cfg)).into();

        // Page 1 — limit 5 → 5 ids + next_cursor=Some.
        let p1 = fx
            .backend()
            .list_executions(partition.clone(), None, 5)
            .await
            .expect("list_executions p1");
        assert_eq!(
            p1.executions.len(),
            5,
            "[{}] p1 should hold 5 ids",
            fx.kind().label()
        );
        assert!(
            p1.next_cursor.is_some(),
            "[{}] p1 should carry a next_cursor",
            fx.kind().label()
        );

        // Page 2 — resume.
        let p2 = fx
            .backend()
            .list_executions(partition.clone(), p1.next_cursor.clone(), 5)
            .await
            .expect("list_executions p2");
        assert_eq!(p2.executions.len(), 5);
        assert!(p2.next_cursor.is_some());

        // Page 3 — tail.
        let p3 = fx
            .backend()
            .list_executions(partition.clone(), p2.next_cursor.clone(), 5)
            .await
            .expect("list_executions p3");
        assert_eq!(p3.executions.len(), 5);
        assert_eq!(
            p3.next_cursor,
            None,
            "[{}] p3 is tail page, next_cursor must be None",
            fx.kind().label()
        );

        // Union of all pages = seeded set (partition has nothing else:
        // fixture truncates / FLUSHDBs before each run).
        let mut seen: Vec<ExecutionId> = p1.executions;
        seen.extend(p2.executions);
        seen.extend(p3.executions);
        seen.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            seen, seeded,
            "[{}] page union must equal seeded set",
            fx.kind().label()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_executions_short_partition_has_no_cursor() {
    run_matrix(|fx| async move {
        let flow = FlowId::new();
        let seeded = seed_n(&fx, &flow, 3).await;
        let cfg = *fx.partition_config();
        let partition: PartitionKey =
            (&ff_core::partition::flow_partition(&flow, &cfg)).into();

        let page = fx
            .backend()
            .list_executions(partition, None, 10)
            .await
            .expect("list_executions");
        assert_eq!(page.executions.len(), 3);
        assert_eq!(
            page.next_cursor,
            None,
            "[{}] partition smaller than limit ⇒ next_cursor=None",
            fx.kind().label()
        );

        let mut got = page.executions;
        got.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(got, seeded);
    })
    .await;
}
