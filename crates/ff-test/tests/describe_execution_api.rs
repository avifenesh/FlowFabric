//! Integration tests for `FlowFabricWorker::describe_execution` (issue #58.1).
//!
//! Exercises the typed snapshot read against a live Valkey: creates
//! executions via `ff_create_execution`, optionally drives them through
//! claim/lease, and asserts the resulting `ExecutionSnapshot` matches
//! the engine's on-disk exec_core + tags state.
//!
//! Run with:
//!   cargo test -p ff-test --test describe_execution_api -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::state::PublicState;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

const LANE: &str = "desc-exec-lane";
const NS: &str = "desc-exec-ns";

async fn seed_partition_config(tc: &TestCluster) {
    let cfg = test_config();
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(cfg.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(cfg.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(cfg.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

async fn build_worker(name_suffix: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        host: std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FF_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6379),
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        worker_id: WorkerId::new(format!("desc-exec-worker-{name_suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("desc-exec-inst-{name_suffix}")),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

/// Create an execution via FCALL, optionally with tags and/or a delay.
async fn create_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    tags_json: &str,
    delay_until: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

    let scheduling_key = if delay_until.is_empty() {
        idx.lane_eligible(&lane_id)
    } else {
        idx.lane_delayed(&lane_id)
    };

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        scheduling_key,
        ctx.noop(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];

    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        LANE.to_owned(),
        "test_kind".to_owned(),
        "0".to_owned(),
        "desc-exec-test".to_owned(),
        "{}".to_owned(),
        String::new(),
        delay_until.to_owned(),
        String::new(),
        tags_json.to_owned(),
        String::new(),
        partition.index.to_string(),
    ];

    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let _: Value = tc
        .client()
        .fcall("ff_create_execution", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_execution failed");
}

// ─── Tests ───

#[tokio::test]
async fn describe_missing_execution_returns_none() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("missing").await;

    // Fresh execution id — never written to Valkey.
    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    let snap = worker
        .describe_execution(&eid)
        .await
        .expect("describe_execution should not error on missing");
    assert!(snap.is_none(), "expected Ok(None) for missing execution");
}

#[tokio::test]
async fn describe_freshly_created_execution_is_waiting() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("waiting").await;

    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    create_execution(&tc, &eid, "{}", "").await;

    let snap = worker
        .describe_execution(&eid)
        .await
        .expect("describe_execution should succeed")
        .expect("snapshot should be Some for created execution");

    assert_eq!(snap.execution_id, eid);
    assert_eq!(snap.public_state, PublicState::Waiting);
    assert_eq!(snap.lane_id.as_str(), LANE);
    assert_eq!(snap.namespace.as_str(), NS);
    assert!(snap.current_attempt.is_none(), "no attempt pre-claim");
    assert!(snap.current_lease.is_none(), "no lease pre-claim");
    assert!(snap.current_waitpoint.is_none(), "no waitpoint pre-suspension");
    assert!(snap.flow_id.is_none(), "solo exec has no flow affinity");
    assert_eq!(snap.total_attempt_count, 0);
    assert!(snap.tags.is_empty(), "empty tags_json → empty tags map");
    assert!(snap.created_at.0 > 0, "created_at must be populated");
    assert_eq!(
        snap.last_mutation_at, snap.created_at,
        "on create, last_mutation_at == created_at"
    );
    assert_eq!(
        snap.blocking_reason.as_deref(),
        Some("waiting_for_worker"),
        "freshly-eligible exec is blocked on worker availability"
    );
    assert!(
        snap.blocking_detail.is_none(),
        "blocking_detail is empty for waiting_for_worker"
    );
}

#[tokio::test]
async fn describe_execution_with_tags_round_trips() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("tags").await;

    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    let tags_json = r#"{"cairn.task_id":"t-42","cairn.project":"proj-a"}"#;
    create_execution(&tc, &eid, tags_json, "").await;

    let snap = worker
        .describe_execution(&eid)
        .await
        .expect("describe_execution succeeds")
        .expect("snapshot present");

    assert_eq!(snap.tags.len(), 2);
    assert_eq!(
        snap.tags.get("cairn.task_id").map(String::as_str),
        Some("t-42")
    );
    assert_eq!(
        snap.tags.get("cairn.project").map(String::as_str),
        Some("proj-a")
    );
    // BTreeMap sort order keeps enumeration deterministic.
    let ordered_keys: Vec<&str> = snap.tags.keys().map(String::as_str).collect();
    assert_eq!(ordered_keys, vec!["cairn.project", "cairn.task_id"]);
}

#[tokio::test]
async fn describe_delayed_execution_is_delayed() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("delayed").await;

    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    // 5 minutes in the future — well beyond any test's wall-clock.
    let delay_until_ms = (TimestampMs::now().0 + 5 * 60 * 1000).to_string();
    create_execution(&tc, &eid, "{}", &delay_until_ms).await;

    let snap = worker
        .describe_execution(&eid)
        .await
        .expect("describe succeeds")
        .expect("snapshot present");

    assert_eq!(snap.public_state, PublicState::Delayed);
    assert_eq!(snap.blocking_reason.as_deref(), Some("waiting_for_delay"));
    // blocking_detail is `"delayed until <ms>"` — don't pin the exact
    // string; assert the prefix so a future format tweak doesn't break
    // the test.
    assert!(
        snap.blocking_detail
            .as_deref()
            .is_some_and(|d| d.starts_with("delayed until ")),
        "blocking_detail = {:?}",
        snap.blocking_detail
    );
}

#[tokio::test]
async fn describe_execution_after_claim_has_attempt_and_lease() {
    // After a worker claim, the snapshot must report the active attempt
    // (current_attempt_id / current_attempt_index) and the held lease
    // (current_worker_instance_id / lease_expires_at / lease_epoch).
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("claimed").await;

    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    create_execution(&tc, &eid, "{}", "").await;

    // Directly HSET claim-state fields on exec_core to simulate a live
    // claim without the full FCALL dance (mirroring how the other read
    // tests in this file avoid reimplementing the claim lifecycle).
    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let attempt_id = AttemptId::new();
    let wid = "desc-exec-inst-claimed";
    let expires_at = TimestampMs::now().0 + 30_000;
    let _: Value = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core())
        .arg("current_attempt_id")
        .arg(attempt_id.to_string().as_str())
        .arg("current_attempt_index")
        .arg("0")
        .arg("current_worker_instance_id")
        .arg(wid)
        .arg("lease_expires_at")
        .arg(expires_at.to_string().as_str())
        .arg("current_lease_epoch")
        .arg("1")
        .arg("total_attempt_count")
        .arg("1")
        .execute()
        .await
        .unwrap();

    let snap = worker
        .describe_execution(&eid)
        .await
        .expect("describe succeeds")
        .expect("snapshot present");

    let att = snap.current_attempt.expect("attempt summary present");
    assert_eq!(att.attempt_id, attempt_id);
    assert_eq!(att.attempt_index, AttemptIndex::new(0));

    let lease = snap.current_lease.expect("lease summary present");
    assert_eq!(lease.lease_epoch, LeaseEpoch::new(1));
    assert_eq!(lease.worker_instance_id.as_str(), wid);
    assert_eq!(lease.expires_at.0, expires_at);

    assert_eq!(snap.total_attempt_count, 1);
}

#[tokio::test]
async fn describe_execution_corrupt_public_state_surfaces_error() {
    // Defensive: if a future field rename leaves a stray literal on
    // exec_core, describe_execution must fail loudly rather than
    // silently coercing to a safe default. This test pins the
    // parse-error contract so a regression to "unwrap_or(Waiting)" is
    // caught in CI.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("corrupt").await;

    let eid = ExecutionId::for_flow(&FlowId::new(), &test_config());
    create_execution(&tc, &eid, "{}", "").await;

    let config = test_config();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let _: Value = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core())
        .arg("public_state")
        .arg("not_a_real_state")
        .execute()
        .await
        .unwrap();

    let err = worker
        .describe_execution(&eid)
        .await
        .expect_err("corrupt public_state must surface as error");
    match err {
        ff_sdk::SdkError::Config { message: msg, .. } => {
            assert!(
                msg.contains("public_state"),
                "error message must name the field: {msg}"
            );
        }
        other => panic!("expected SdkError::Config, got {other:?}"),
    }
}
