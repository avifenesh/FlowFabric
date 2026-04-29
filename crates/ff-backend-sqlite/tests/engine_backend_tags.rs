//! SQLite parity coverage for `EngineBackend` tag methods (issue #433).
//!
//! The shared `ff-test` backend matrix does not yet cover SQLite, so
//! this file mirrors the cross-backend tests against a
//! `SqliteBackend` spun up from an in-memory database. The assertions
//! match the semantics documented on the trait rustdoc:
//!
//! * `validate_tag_key` rejects malformed keys before the backend call;
//! * `set_*_tag` returns `NotFound` on a missing entity;
//! * `get_*_tag` collapses missing-tag + missing-entity to `Ok(None)`.

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{CreateExecutionArgs, CreateFlowArgs};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::types::{ExecutionId, FlowId, LaneId, Namespace, TimestampMs};
use uuid::Uuid;

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tid = std::thread::current().id();
    format!("{ns}-{tid:?}").replace([':', ' '], "-")
}

async fn fresh_backend() -> Arc<SqliteBackend> {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let uri = format!("file:engine-backend-tags-{}?mode=memory&cache=shared", uuid_like());
    SqliteBackend::new(&uri).await.expect("backend")
}

fn new_exec_id() -> ExecutionId {
    ExecutionId::parse(&format!("{{fp:0}}:{}", Uuid::new_v4())).expect("exec id")
}

async fn seed_execution(backend: &Arc<SqliteBackend>, lane: &str) -> ExecutionId {
    let eid = new_exec_id();
    let args = CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new("tags-ns"),
        lane_id: LaneId::new(lane),
        execution_kind: "tags_test".to_owned(),
        input_payload: b"{}".to_vec(),
        payload_encoding: Some("application/json".to_owned()),
        priority: 0,
        creator_identity: "tags-test".to_owned(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: 0,
        now: TimestampMs::now(),
    };
    backend.create_execution(args).await.expect("create_execution");
    eid
}

async fn seed_flow(backend: &Arc<SqliteBackend>) -> FlowId {
    let flow = FlowId::new();
    backend
        .create_flow(CreateFlowArgs {
            flow_id: flow.clone(),
            flow_kind: "tags_test_flow".to_owned(),
            namespace: Namespace::new("tags-ns"),
            now: TimestampMs::now(),
        })
        .await
        .expect("create_flow");
    flow
}

#[tokio::test]
async fn set_and_get_execution_tag_roundtrips() {
    let backend = fresh_backend().await;
    let eid = seed_execution(&backend, "tags-lane-exec").await;

    let before = backend
        .get_execution_tag(&eid, "cairn.session_id")
        .await
        .expect("pre-write get");
    assert!(before.is_none());

    backend
        .set_execution_tag(&eid, "cairn.session_id", "s-1")
        .await
        .expect("set");
    let got = backend
        .get_execution_tag(&eid, "cairn.session_id")
        .await
        .expect("post-write get");
    assert_eq!(got.as_deref(), Some("s-1"));

    // Overwrite + distinct key coexist.
    backend
        .set_execution_tag(&eid, "cairn.session_id", "s-2")
        .await
        .expect("overwrite");
    backend
        .set_execution_tag(&eid, "cairn.project", "p-1")
        .await
        .expect("second tag");
    assert_eq!(
        backend
            .get_execution_tag(&eid, "cairn.session_id")
            .await
            .unwrap()
            .as_deref(),
        Some("s-2")
    );
    assert_eq!(
        backend
            .get_execution_tag(&eid, "cairn.project")
            .await
            .unwrap()
            .as_deref(),
        Some("p-1")
    );
}

#[tokio::test]
async fn set_and_get_flow_tag_roundtrips() {
    let backend = fresh_backend().await;
    let flow = seed_flow(&backend).await;

    assert!(
        backend
            .get_flow_tag(&flow, "cairn.archived")
            .await
            .expect("pre-write get")
            .is_none()
    );
    backend
        .set_flow_tag(&flow, "cairn.archived", "true")
        .await
        .expect("set");
    assert_eq!(
        backend
            .get_flow_tag(&flow, "cairn.archived")
            .await
            .unwrap()
            .as_deref(),
        Some("true")
    );
}

#[tokio::test]
async fn invalid_tag_key_rejected() {
    let backend = fresh_backend().await;
    let eid = seed_execution(&backend, "tags-lane-invalid").await;

    for bad in ["", "Cairn.x", "cairn", "cairn.", "cairn..x", "1cairn.x"] {
        let err = backend
            .set_execution_tag(&eid, bad, "v")
            .await
            .expect_err(&format!("{bad:?} should reject"));
        assert!(
            matches!(
                err,
                EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    ..
                }
            ),
            "{bad:?}: unexpected err {err:?}"
        );
    }
}

#[tokio::test]
async fn missing_execution_returns_notfound_on_set_and_ok_none_on_get() {
    let backend = fresh_backend().await;
    let phantom = new_exec_id();

    let err = backend
        .set_execution_tag(&phantom, "cairn.x", "v")
        .await
        .expect_err("missing exec set");
    assert!(matches!(
        err,
        EngineError::NotFound {
            entity: "execution"
        }
    ));

    let got = backend
        .get_execution_tag(&phantom, "cairn.x")
        .await
        .expect("missing exec get collapses");
    assert!(got.is_none());
}

#[tokio::test]
async fn missing_flow_returns_notfound_on_set_and_ok_none_on_get() {
    let backend = fresh_backend().await;
    let phantom = FlowId::new();

    let err = backend
        .set_flow_tag(&phantom, "cairn.x", "v")
        .await
        .expect_err("missing flow set");
    assert!(matches!(err, EngineError::NotFound { entity: "flow" }));

    let got = backend
        .get_flow_tag(&phantom, "cairn.x")
        .await
        .expect("missing flow get collapses");
    assert!(got.is_none());
}
