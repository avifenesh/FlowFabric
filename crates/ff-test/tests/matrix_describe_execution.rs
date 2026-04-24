//! Backend-matrix variant of `engine_backend_describe.rs`
//! (execution-side only) — RFC-v0.7 Wave 7a.
//!
//! `describe_flow` stays Valkey-only for now: the Valkey path seeds
//! flows through `ff_create_flow` FCALL; the Postgres equivalent
//! (`ff_flow_core` row insertion) is still being shaped by
//! Waves 4c/4f and not yet exposed via a backend-agnostic fixture
//! surface.

use std::collections::HashMap;

use ff_core::contracts::CreateExecutionArgs;
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::{ExecutionId, FlowId, LaneId, Namespace, TimestampMs};
use ff_test::backend_matrix::run_matrix;

const LANE: &str = "matrix-desc-lane";
const NS: &str = "matrix-desc-ns";

#[tokio::test(flavor = "multi_thread")]
async fn describe_execution_returns_snapshot_for_fresh_row() {
    run_matrix(|fx| async move {
        let cfg = *fx.partition_config();
        let lane = LaneId::new(LANE);
        let eid = ExecutionId::for_flow(&FlowId::new(), &cfg);
        let partition = execution_partition(&eid, &cfg);

        let mut tags = HashMap::new();
        tags.insert("cairn.task_id".to_owned(), "t-www".to_owned());
        tags.insert("cairn.project".to_owned(), "ff".to_owned());

        let args = CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: lane.clone(),
            execution_kind: "matrix_desc_kind".to_owned(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("application/json".to_owned()),
            priority: 0,
            creator_identity: "matrix-desc-test".to_owned(),
            idempotency_key: None,
            tags: tags.clone(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: partition.index,
            now: TimestampMs::now(),
        };
        fx.seed_execution(args).await;

        let snap = fx
            .backend()
            .describe_execution(&eid)
            .await
            .expect("describe_execution OK")
            .expect("snapshot present");

        assert_eq!(snap.execution_id, eid, "[{}]", fx.kind().label());
        assert_eq!(snap.lane_id, lane);
        assert_eq!(snap.namespace.as_str(), NS);
        assert_eq!(snap.public_state, PublicState::Waiting);
        assert_eq!(
            snap.tags.get("cairn.task_id").map(String::as_str),
            Some("t-www")
        );
        assert_eq!(
            snap.tags.get("cairn.project").map(String::as_str),
            Some("ff")
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn describe_execution_on_unknown_returns_none() {
    run_matrix(|fx| async move {
        let cfg = *fx.partition_config();
        let eid = ExecutionId::for_flow(&FlowId::new(), &cfg);
        let snap = fx
            .backend()
            .describe_execution(&eid)
            .await
            .expect("describe_execution OK");
        assert!(
            snap.is_none(),
            "[{}] describe on unseeded id must be None",
            fx.kind().label()
        );
    })
    .await;
}
