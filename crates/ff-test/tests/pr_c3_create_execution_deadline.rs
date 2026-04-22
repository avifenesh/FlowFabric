//! PR-C3 regression (#68): the typed `ff_create_execution` wrapper in
//! `ff-script` must forward `CreateExecutionArgs::execution_deadline_at`
//! into ARGV slot 12 so direct consumers of the published crate can set
//! an execution deadline. The server-side code path in `ff-server`
//! already did this; only the public typed wrapper dropped it.
//!
//! Failure mode before the fix:
//!   Line `execution.rs:201` emitted `String::new()` regardless of
//!   `args.execution_deadline_at`, so the Lua contract observed an
//!   empty string and skipped the `ZADD` into `execution_deadline_zset`.
//!
//! Proof of fix:
//!   1. Create an execution through the typed wrapper with
//!      `execution_deadline_at = Some(t)`.
//!   2. `ZSCORE execution_deadline_zset eid == t`.
//!
//! Run with: `cargo test -p ff-test --test pr_c3_create_execution_deadline \
//!            -- --test-threads=1`

use ff_core::contracts::{CreateExecutionArgs, CreateExecutionResult};
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_script::functions::execution::{ff_create_execution, ExecOpKeys};
use ff_test::fixtures::TestCluster;

#[tokio::test]
#[serial_test::serial]
async fn typed_wrapper_threads_execution_deadline_at_into_argv12() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let partition = execution_partition(&eid, tc.partition_config());
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);
    let lane = tc.test_lane();
    let worker_instance = WorkerInstanceId::new("pr-c3-test-worker");

    // Deadline 10 minutes out — arbitrary, just needs to be a valid
    // ms timestamp we can read back from the deadline zset.
    let now = TimestampMs::now();
    let deadline = TimestampMs(now.0 + 600_000);

    let args = CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: tc.test_namespace(),
        lane_id: lane.clone(),
        execution_kind: "pr-c3-deadline".into(),
        input_payload: b"{}".to_vec(),
        payload_encoding: None,
        priority: 0,
        creator_identity: "pr-c3-test".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: Some(deadline),
        partition_id: partition.index,
        now,
    };

    let op_keys = ExecOpKeys {
        ctx: &ctx,
        idx: &idx,
        lane_id: &lane,
        worker_instance_id: &worker_instance,
    };

    let res = ff_create_execution(tc.client(), &op_keys, &args)
        .await
        .expect("ff_create_execution wrapper call must succeed");
    match res {
        CreateExecutionResult::Created { .. } => {}
        other => panic!("expected Created, got {other:?}"),
    }

    // Proof: the execution_deadline_zset carries this eid at the score
    // we passed in. If ARGV[12] were empty (the pre-fix bug) the Lua
    // contract would skip the ZADD entirely.
    let deadline_key = idx.execution_deadline();
    let score: Option<String> = tc
        .client()
        .cmd("ZSCORE")
        .arg(&deadline_key)
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .expect("ZSCORE must not error");
    let score = score.expect(
        "execution must be in execution_deadline zset — ARGV[12] was dropped",
    );
    let parsed: i64 = score.parse().expect("deadline score must parse as i64");
    assert_eq!(
        parsed, deadline.0,
        "deadline zset score must equal the value passed to the wrapper"
    );
}
