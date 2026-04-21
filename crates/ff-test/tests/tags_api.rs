//! Integration tests for the tags API (issue #58.4).
//!
//! Exercises `ff_set_execution_tags` + `ff_set_flow_tags` via the typed
//! `ff_script::functions::{execution,flow}` wrappers. Each test asserts:
//!
//!   * tags land on the separate `:tags` key (not on `exec_core` /
//!     `flow_core`);
//!   * `last_mutation_at` on the core hash bumps on every call;
//!   * namespace-reserved keys (`^[a-z][a-z0-9_]*%.`) are required —
//!     violations fail-closed without any writes;
//!   * flow_core lazy migration moves any pre-58.4 reserved-namespace
//!     fields that were stashed inline onto the new tags key and
//!     HDELs them from flow_core.
//!
//! Run with: `cargo test -p ff-test --test tags_api -- --test-threads=1`

use std::collections::BTreeMap;

use ff_core::contracts::{
    SetExecutionTagsArgs, SetExecutionTagsResult, SetFlowTagsArgs, SetFlowTagsResult,
};
use ff_core::keys::{ExecKeyContext, FlowKeyContext};
use ff_core::partition::{execution_partition, flow_partition};
use ff_core::types::{ExecutionId, FlowId, TimestampMs};
use ff_script::error::ScriptError;
use ff_script::functions::execution::ff_set_execution_tags;
use ff_script::functions::flow::ff_set_flow_tags;
use ff_test::fixtures::TestCluster;

// ─── Helpers ────────────────────────────────────────────────────────────

/// Seed an execution via `ff_create_execution` so tag writes have
/// something to land on. Uses the TestCluster's solo-lane routing.
async fn seed_execution(tc: &TestCluster) -> (ExecutionId, ExecKeyContext) {
    let eid = tc.new_execution_id();
    let partition = execution_partition(&eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = ff_core::keys::IndexKeys::new(&partition);
    let lane = tc.test_lane();

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&lane),
        ctx.noop(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        tc.test_namespace().to_string(),
        lane.to_string(),
        "tags-test".to_owned(),
        "0".to_owned(),
        "tags-test".to_owned(),
        "{}".to_owned(),
        "{}".to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _raw: ferriskey::Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution");
    (eid, ctx)
}

/// Seed a flow via `ff_create_flow` on its natural flow partition.
async fn seed_flow(tc: &TestCluster) -> (FlowId, FlowKeyContext) {
    let fid = FlowId::new();
    let partition = flow_partition(&fid, tc.partition_config());
    let fctx = FlowKeyContext::new(&partition, &fid);
    let fidx = ff_core::keys::FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![fctx.core(), fctx.members(), fidx.flow_index()];
    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        fid.to_string(),
        "tags-test".to_owned(),
        tc.test_namespace().to_string(),
        now.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _raw: ferriskey::Value = tc
        .client()
        .fcall("ff_create_flow", &kr, &ar)
        .await
        .expect("FCALL ff_create_flow");
    (fid, fctx)
}

// ─── ff_set_execution_tags ──────────────────────────────────────────────

#[tokio::test]
async fn set_execution_tags_happy_path_single() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (eid, ctx) = seed_execution(&tc).await;

    let mut tags = BTreeMap::new();
    tags.insert("cairn.task_id".to_owned(), "t-001".to_owned());

    let res = ff_set_execution_tags(
        tc.client(),
        &ctx,
        &SetExecutionTagsArgs {
            execution_id: eid,
            tags,
        },
    )
    .await
    .expect("single-tag write");
    assert!(matches!(res, SetExecutionTagsResult::Ok { count: 1 }));

    assert_eq!(
        tc.hget(&ctx.tags(), "cairn.task_id").await.as_deref(),
        Some("t-001")
    );
    assert!(
        tc.hget(&ctx.core(), "cairn.task_id").await.is_none(),
        "caller tag must not land on exec_core"
    );
}

#[tokio::test]
async fn set_execution_tags_happy_path_batch() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (eid, ctx) = seed_execution(&tc).await;

    let mut tags = BTreeMap::new();
    tags.insert("cairn.task_id".to_owned(), "t-001".to_owned());
    tags.insert("cairn.project".to_owned(), "alpha".to_owned());
    tags.insert("my_app.run_id".to_owned(), "r-42".to_owned());

    let res = ff_set_execution_tags(
        tc.client(),
        &ctx,
        &SetExecutionTagsArgs {
            execution_id: eid,
            tags,
        },
    )
    .await
    .expect("batch write");
    assert!(matches!(res, SetExecutionTagsResult::Ok { count: 3 }));

    assert_eq!(tc.hget(&ctx.tags(), "cairn.task_id").await.as_deref(), Some("t-001"));
    assert_eq!(tc.hget(&ctx.tags(), "cairn.project").await.as_deref(), Some("alpha"));
    assert_eq!(tc.hget(&ctx.tags(), "my_app.run_id").await.as_deref(), Some("r-42"));
}

#[tokio::test]
async fn set_execution_tags_bumps_last_mutation_at() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (eid, ctx) = seed_execution(&tc).await;

    let before: i64 = tc
        .hget(&ctx.core(), "last_mutation_at")
        .await
        .expect("created exec has last_mutation_at")
        .parse()
        .expect("numeric");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let mut tags = BTreeMap::new();
    tags.insert("cairn.task_id".to_owned(), "t-002".to_owned());
    ff_set_execution_tags(
        tc.client(),
        &ctx,
        &SetExecutionTagsArgs {
            execution_id: eid,
            tags,
        },
    )
    .await
    .expect("tag write");

    let after: i64 = tc
        .hget(&ctx.core(), "last_mutation_at")
        .await
        .expect("still present")
        .parse()
        .expect("numeric");
    assert!(
        after > before,
        "last_mutation_at must bump on tag write (before={before}, after={after})"
    );
}

#[tokio::test]
async fn set_execution_tags_rejects_key_without_dot() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (eid, ctx) = seed_execution(&tc).await;

    let mut tags = BTreeMap::new();
    tags.insert("no_dot_here".to_owned(), "value".to_owned());
    let err = ff_set_execution_tags(
        tc.client(),
        &ctx,
        &SetExecutionTagsArgs {
            execution_id: eid,
            tags,
        },
    )
    .await
    .expect_err("must reject without namespace dot");
    match err {
        ScriptError::InvalidTagKey(k) => assert_eq!(k, "no_dot_here"),
        other => panic!("expected InvalidTagKey, got {other:?}"),
    }
    assert!(
        tc.hget(&ctx.tags(), "no_dot_here").await.is_none(),
        "rejected key must not land on tags key"
    );
}

#[tokio::test]
async fn set_execution_tags_rejects_uppercase_prefix() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (eid, ctx) = seed_execution(&tc).await;

    let mut tags = BTreeMap::new();
    tags.insert("Cairn.task_id".to_owned(), "t-003".to_owned());
    let err = ff_set_execution_tags(
        tc.client(),
        &ctx,
        &SetExecutionTagsArgs {
            execution_id: eid,
            tags,
        },
    )
    .await
    .expect_err("uppercase first-char must be rejected");
    assert!(matches!(err, ScriptError::InvalidTagKey(_)));
}

#[tokio::test]
async fn set_execution_tags_rejects_empty_input() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (eid, ctx) = seed_execution(&tc).await;

    let err = ff_set_execution_tags(
        tc.client(),
        &ctx,
        &SetExecutionTagsArgs {
            execution_id: eid,
            tags: BTreeMap::new(),
        },
    )
    .await
    .expect_err("empty tag map must be rejected");
    // Lua side uses `invalid_input` for shape errors (distinct from
    // `invalid_tag_key`, which is per-key validation).
    assert!(matches!(err, ScriptError::InvalidInput(_)));
}

// ─── ff_set_flow_tags ──────────────────────────────────────────────────

#[tokio::test]
async fn set_flow_tags_happy_path_and_bumps_last_mutation_at() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (fid, fctx) = seed_flow(&tc).await;

    let before: i64 = tc
        .hget(&fctx.core(), "last_mutation_at")
        .await
        .expect("created flow has last_mutation_at")
        .parse()
        .expect("numeric");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let mut tags = BTreeMap::new();
    tags.insert("cairn.project".to_owned(), "alpha".to_owned());
    let res = ff_set_flow_tags(
        tc.client(),
        &fctx,
        &SetFlowTagsArgs {
            flow_id: fid,
            tags,
        },
    )
    .await
    .expect("flow tag write");
    assert!(matches!(res, SetFlowTagsResult::Ok { count: 1 }));

    assert_eq!(
        tc.hget(&fctx.tags(), "cairn.project").await.as_deref(),
        Some("alpha")
    );
    assert!(
        tc.hget(&fctx.core(), "cairn.project").await.is_none(),
        "caller tag must not stay inline on flow_core"
    );
    let after: i64 = tc
        .hget(&fctx.core(), "last_mutation_at")
        .await
        .unwrap()
        .parse()
        .unwrap();
    assert!(after > before, "flow last_mutation_at must bump");
}

#[tokio::test]
async fn set_flow_tags_lazy_migrates_inline_cairn_fields() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (fid, fctx) = seed_flow(&tc).await;

    // Simulate a pre-58.4 consumer (cairn-fabric today) that stashed
    // reserved-namespace fields directly on flow_core via raw HSETs.
    tc.client()
        .hset(fctx.core(), "cairn.task_id", "pre-existing-task")
        .await
        .expect("seed cairn.task_id");
    tc.client()
        .hset(fctx.core(), "cairn.session_id", "pre-existing-session")
        .await
        .expect("seed cairn.session_id");

    // First tags call must migrate them into :tags and HDEL from
    // flow_core, atomically with the new write.
    let mut tags = BTreeMap::new();
    tags.insert("cairn.project".to_owned(), "alpha".to_owned());
    ff_set_flow_tags(
        tc.client(),
        &fctx,
        &SetFlowTagsArgs {
            flow_id: fid,
            tags,
        },
    )
    .await
    .expect("flow tag write with migration");

    assert_eq!(
        tc.hget(&fctx.tags(), "cairn.task_id").await.as_deref(),
        Some("pre-existing-task")
    );
    assert_eq!(
        tc.hget(&fctx.tags(), "cairn.session_id").await.as_deref(),
        Some("pre-existing-session")
    );
    assert_eq!(
        tc.hget(&fctx.tags(), "cairn.project").await.as_deref(),
        Some("alpha")
    );
    assert!(tc.hget(&fctx.core(), "cairn.task_id").await.is_none());
    assert!(tc.hget(&fctx.core(), "cairn.session_id").await.is_none());
    // Non-reserved flow_core fields survive untouched.
    assert_eq!(
        tc.hget(&fctx.core(), "flow_kind").await.as_deref(),
        Some("tags-test")
    );
}

#[tokio::test]
async fn set_execution_tags_rejects_trailing_dot_at_lua_layer() {
    // Lua-side pattern must reject `cairn.` (empty field segment)
    // regardless of client-side validation.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (eid, ctx) = seed_execution(&tc).await;

    let mut tags = BTreeMap::new();
    tags.insert("cairn.".to_owned(), "value".to_owned());
    let err = ff_set_execution_tags(
        tc.client(),
        &ctx,
        &SetExecutionTagsArgs {
            execution_id: eid,
            tags,
        },
    )
    .await
    .expect_err("trailing-dot must fail server-side too");
    match err {
        ScriptError::InvalidTagKey(k) => assert_eq!(k, "cairn."),
        other => panic!("expected InvalidTagKey, got {other:?}"),
    }
}

#[tokio::test]
async fn set_flow_tags_migration_uses_sentinel_to_skip_second_hgetall() {
    // After the first call migrates legacy fields and sets
    // `tags_migrated=1`, a subsequent call on a flow that still has
    // dot-named residue on flow_core (inserted after the first call)
    // MUST leave that residue in place — the sentinel short-circuits
    // the migration pass. This keeps tag writes O(1) instead of
    // paying an O(n) HGETALL on every call.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (fid, fctx) = seed_flow(&tc).await;

    // First write → triggers migration pass + installs sentinel.
    let mut tags = BTreeMap::new();
    tags.insert("cairn.project".to_owned(), "alpha".to_owned());
    ff_set_flow_tags(
        tc.client(),
        &fctx,
        &SetFlowTagsArgs {
            flow_id: fid.clone(),
            tags,
        },
    )
    .await
    .expect("first tag write");

    // Sentinel is installed.
    assert_eq!(
        tc.hget(&fctx.core(), "tags_migrated").await.as_deref(),
        Some("1")
    );

    // Seed dot-named residue on flow_core (simulating a hypothetical
    // bypass).
    tc.client()
        .hset(fctx.core(), "rogue.field", "should-not-migrate")
        .await
        .expect("seed rogue field");

    // Second write — sentinel short-circuits the migration, so the
    // rogue field stays on flow_core.
    let mut tags = BTreeMap::new();
    tags.insert("cairn.task_id".to_owned(), "t-999".to_owned());
    ff_set_flow_tags(
        tc.client(),
        &fctx,
        &SetFlowTagsArgs {
            flow_id: fid,
            tags,
        },
    )
    .await
    .expect("second tag write");

    // Rogue field is still on flow_core (sentinel short-circuited).
    assert_eq!(
        tc.hget(&fctx.core(), "rogue.field").await.as_deref(),
        Some("should-not-migrate"),
        "sentinel must short-circuit migration on subsequent calls"
    );
    // The fresh tag landed on :tags as expected.
    assert_eq!(
        tc.hget(&fctx.tags(), "cairn.task_id").await.as_deref(),
        Some("t-999")
    );
}

#[tokio::test]
async fn set_flow_tags_rejects_invalid_key() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let (fid, fctx) = seed_flow(&tc).await;

    let mut tags = BTreeMap::new();
    tags.insert("bogus".to_owned(), "value".to_owned());
    let err = ff_set_flow_tags(
        tc.client(),
        &fctx,
        &SetFlowTagsArgs {
            flow_id: fid,
            tags,
        },
    )
    .await
    .expect_err("flow tag write must reject key without dot");
    match err {
        ScriptError::InvalidTagKey(k) => assert_eq!(k, "bogus"),
        other => panic!("expected InvalidTagKey, got {other:?}"),
    }
    assert!(tc.hget(&fctx.tags(), "bogus").await.is_none());
}
