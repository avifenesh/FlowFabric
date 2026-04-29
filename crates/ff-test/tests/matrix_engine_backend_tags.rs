//! Cross-backend parity tests for the EngineBackend tag methods -
//! issue #433 (set_execution_tag, set_flow_tag, get_execution_tag,
//! get_flow_tag).
//!
//! Exercises Valkey + Postgres via the matrix harness. SQLite parity
//! lives alongside the SQLite backend's other trait tests because the
//! shared matrix does not currently cover SQLite.

use std::collections::HashMap;

use ff_core::contracts::{CreateExecutionArgs, CreateFlowArgs};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::types::{ExecutionId, FlowId, LaneId, Namespace, TimestampMs};
use ff_test::backend_matrix::{run_matrix, BackendFixture};

const LANE: &str = "tag-parity-lane";
const NS: &str = "tag-parity-ns";

fn sample_exec_args(eid: &ExecutionId, lane: &LaneId, partition_id: u16) -> CreateExecutionArgs {
    CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new(NS),
        lane_id: lane.clone(),
        execution_kind: "tag_parity_kind".to_owned(),
        input_payload: b"{}".to_vec(),
        payload_encoding: Some("application/json".to_owned()),
        priority: 0,
        creator_identity: "tag-parity-test".to_owned(),
        idempotency_key: None,
        tags: HashMap::new(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id,
        now: TimestampMs::now(),
    }
}

async fn seed_flow_via_backend(fx: &BackendFixture, flow_id: &FlowId) {
    let backend = fx.backend();
    backend
        .create_flow(CreateFlowArgs {
            flow_id: flow_id.clone(),
            flow_kind: "tag_parity_flow".to_owned(),
            namespace: Namespace::new(NS),
            now: TimestampMs::now(),
        })
        .await
        .expect("create_flow");
}

async fn seed_exec(fx: &BackendFixture) -> ExecutionId {
    let cfg = *fx.partition_config();
    let lane = LaneId::new(LANE);
    let flow = FlowId::new();
    let eid = ExecutionId::for_flow(&flow, &cfg);
    let partition = ff_core::partition::execution_partition(&eid, &cfg);
    let args = sample_exec_args(&eid, &lane, partition.index);
    fx.seed_execution(args).await;
    eid
}

#[tokio::test(flavor = "multi_thread")]
async fn set_and_get_execution_tag_roundtrips() {
    run_matrix(|fx| async move {
        let eid = seed_exec(&fx).await;
        let backend = fx.backend();

        let before = backend
            .get_execution_tag(&eid, "cairn.session_id")
            .await
            .expect("get_execution_tag pre-write");
        assert!(
            before.is_none(),
            "[{}] unseeded tag must read as None, got {:?}",
            fx.kind().label(),
            before
        );

        backend
            .set_execution_tag(&eid, "cairn.session_id", "session-42")
            .await
            .expect("set_execution_tag");

        let got = backend
            .get_execution_tag(&eid, "cairn.session_id")
            .await
            .expect("get_execution_tag post-write");
        assert_eq!(
            got.as_deref(),
            Some("session-42"),
            "[{}] tag must round-trip",
            fx.kind().label()
        );

        backend
            .set_execution_tag(&eid, "cairn.session_id", "session-99")
            .await
            .expect("overwrite tag");
        let got2 = backend
            .get_execution_tag(&eid, "cairn.session_id")
            .await
            .expect("get_execution_tag after overwrite");
        assert_eq!(got2.as_deref(), Some("session-99"));

        backend
            .set_execution_tag(&eid, "cairn.project", "proj-a")
            .await
            .expect("second tag");
        let proj = backend
            .get_execution_tag(&eid, "cairn.project")
            .await
            .expect("get second tag")
            .expect("second tag present");
        assert_eq!(proj, "proj-a");
        let sess = backend
            .get_execution_tag(&eid, "cairn.session_id")
            .await
            .expect("re-read first tag")
            .expect("first tag still present");
        assert_eq!(sess, "session-99");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_and_get_flow_tag_roundtrips() {
    run_matrix(|fx| async move {
        let flow = FlowId::new();
        seed_flow_via_backend(&fx, &flow).await;
        let backend = fx.backend();

        let before = backend
            .get_flow_tag(&flow, "cairn.archived")
            .await
            .expect("get_flow_tag pre-write");
        assert!(before.is_none(), "[{}] unseeded flow tag = None", fx.kind().label());

        backend
            .set_flow_tag(&flow, "cairn.archived", "true")
            .await
            .expect("set_flow_tag");
        let got = backend
            .get_flow_tag(&flow, "cairn.archived")
            .await
            .expect("get_flow_tag post-write");
        assert_eq!(got.as_deref(), Some("true"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_tag_key_rejected_before_backend_call() {
    run_matrix(|fx| async move {
        let eid = seed_exec(&fx).await;
        let backend = fx.backend();

        for bad in ["", "Cairn.x", "cairn", "cairn.", "cairn..x", "1cairn.x"] {
            let err = backend
                .set_execution_tag(&eid, bad, "v")
                .await
                .expect_err(&format!("[{}] {bad:?} should be rejected", fx.kind().label()));
            assert!(
                matches!(
                    err,
                    EngineError::Validation {
                        kind: ValidationKind::InvalidInput,
                        ..
                    }
                ),
                "[{}] {bad:?}: unexpected err {:?}",
                fx.kind().label(),
                err
            );
        }

        for bad in ["", "Cairn.x", "cairn"] {
            let err = backend
                .get_execution_tag(&eid, bad)
                .await
                .expect_err(&format!("[{}] get {bad:?} should be rejected", fx.kind().label()));
            assert!(matches!(
                err,
                EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    ..
                }
            ));
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_execution_tag_missing_returns_notfound() {
    run_matrix(|fx| async move {
        let cfg = *fx.partition_config();
        let phantom_flow = FlowId::new();
        let phantom = ExecutionId::for_flow(&phantom_flow, &cfg);

        let backend = fx.backend();
        let err = backend
            .set_execution_tag(&phantom, "cairn.x", "v")
            .await
            .expect_err("missing exec should err");
        assert!(
            matches!(err, EngineError::NotFound { entity: "execution" }),
            "[{}] missing-exec: unexpected err {:?}",
            fx.kind().label(),
            err
        );

        // Read-side collapses missing-row to Ok(None) (matches Valkey
        // HGET semantics — the trait documents this collapse).
        let got = backend
            .get_execution_tag(&phantom, "cairn.x")
            .await
            .expect("missing-exec get should collapse to Ok(None)");
        assert!(
            got.is_none(),
            "[{}] missing-exec get: expected None, got {:?}",
            fx.kind().label(),
            got
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_flow_tag_missing_returns_notfound() {
    run_matrix(|fx| async move {
        let phantom = FlowId::new();
        let backend = fx.backend();
        let err = backend
            .set_flow_tag(&phantom, "cairn.x", "v")
            .await
            .expect_err("missing flow should err");
        assert!(
            matches!(err, EngineError::NotFound { entity: "flow" }),
            "[{}] missing-flow: unexpected err {:?}",
            fx.kind().label(),
            err
        );

        let got = backend
            .get_flow_tag(&phantom, "cairn.x")
            .await
            .expect("missing-flow get should collapse to Ok(None)");
        assert!(
            got.is_none(),
            "[{}] missing-flow get: expected None, got {:?}",
            fx.kind().label(),
            got
        );
    })
    .await;
}
