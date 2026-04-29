//! SQLite parity coverage for `EngineBackend` tag methods (issue #433).
//!
//! The shared `ff-test` backend matrix does not yet cover SQLite, so
//! this file mirrors the cross-backend tests against a
//! `SqliteBackend` spun up from an in-memory database. The assertions
//! match the semantics documented on the trait rustdoc:
//!
//! * `validate_tag_key` rejects malformed keys before the backend call;
//! * `set_*_tag` returns `NotFound` on a missing entity;
//! * `get_*_tag` collapses missing-tag + missing-entity to `Ok(None)`;
//! * stored tag shape is flat (`raw_fields.tags["cairn.session_id"]`
//!   on executions, `raw_fields["cairn.archived"]` on flows) — not a
//!   nested path `raw_fields.tags.cairn.session_id` / `raw_fields.cairn.archived`
//!   that a dot-split JSON path would produce.
//!
//! Every test mutates the process-global `FF_DEV_MODE` env var under
//! `#[serial(ff_dev_mode)]`. Unguarded parallel env mutation is
//! unsound under Rust 2024 and races with other SQLite integration
//! tests that read the same var at `SqliteBackend::new` construction.

#![cfg(feature = "core")]

use std::sync::Arc;

use ff_backend_sqlite::SqliteBackend;
use ff_core::contracts::{CreateExecutionArgs, CreateFlowArgs};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::types::{ExecutionId, FlowId, LaneId, Namespace, TimestampMs};
use serial_test::serial;
use sqlx::Row;
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
    // SAFETY: test-only env mutation; all callers are
    // `#[serial(ff_dev_mode)]`-gated so no parallel test observes a
    // torn value. See module docs.
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
#[serial(ff_dev_mode)]
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
#[serial(ff_dev_mode)]
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
#[serial(ff_dev_mode)]
async fn invalid_tag_key_rejected() {
    let backend = fresh_backend().await;
    let eid = seed_execution(&backend, "tags-lane-invalid").await;

    for bad in [
        "",
        "Cairn.x",
        "cairn",
        "cairn.",
        "cairn..x",
        "1cairn.x",
        // Tightened by Finding 2: the full key must be
        // lowercase-alphanum-dot-underscore. Suffix-level rejections
        // below would have silently passed the prefix-only check.
        "cairn.foo bar",
        "cairn.foo\"bar",
        "cairn.Foo",
        "cairn.foo-bar",
    ] {
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
#[serial(ff_dev_mode)]
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
#[serial(ff_dev_mode)]
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

// ── Stored-shape assertions (Finding 1 regression guard) ────────
//
// These tests peek at `raw_fields` directly via the test pool and
// verify the JSON shape is flat — i.e. the dotted key is a single
// literal member name, not a nested path segment. A regression to
// the pre-Finding-1 `$.tags.<key>` / `$.<key>` unquoted-path form
// would silently fail these assertions.

#[tokio::test]
#[serial(ff_dev_mode)]
async fn execution_tag_stored_as_flat_key_not_nested() {
    let backend = fresh_backend().await;
    let eid = seed_execution(&backend, "tags-lane-shape-exec").await;

    backend
        .set_execution_tag(&eid, "cairn.session_id", "s-flat")
        .await
        .expect("set");

    let exec_uuid: Uuid = eid
        .as_str()
        .rsplit_once(':')
        .map(|(_, u)| Uuid::parse_str(u).expect("parse exec uuid"))
        .expect("split exec id");
    let row = sqlx::query("SELECT raw_fields FROM ff_exec_core WHERE execution_id = ?1")
        .bind(exec_uuid)
        .fetch_one(backend.pool_for_test())
        .await
        .expect("fetch raw_fields");
    let raw: String = row.try_get("raw_fields").expect("raw_fields col");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");

    assert!(
        parsed["tags"]["cairn.session_id"].is_string(),
        "tag must be stored as flat key; got raw_fields: {raw}"
    );
    assert!(
        parsed["tags"].get("cairn").is_none(),
        "tag must not be nested under a `cairn` object; got raw_fields: {raw}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn flow_tag_stored_as_flat_key_not_nested() {
    let backend = fresh_backend().await;
    let flow = seed_flow(&backend).await;

    backend
        .set_flow_tag(&flow, "cairn.archived", "true")
        .await
        .expect("set");

    let flow_uuid: Uuid = flow.0;
    let row = sqlx::query("SELECT raw_fields FROM ff_flow_core WHERE flow_id = ?1")
        .bind(flow_uuid)
        .fetch_one(backend.pool_for_test())
        .await
        .expect("fetch raw_fields");
    let raw: String = row.try_get("raw_fields").expect("raw_fields col");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");

    assert!(
        parsed["cairn.archived"].is_string(),
        "flow tag must be stored as flat top-level key; got raw_fields: {raw}"
    );
    assert!(
        parsed.get("cairn").is_none(),
        "flow tag must not be nested under a `cairn` object; got raw_fields: {raw}"
    );
}
