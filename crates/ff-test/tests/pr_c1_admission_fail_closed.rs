//! PR-C1 regression (#64): scheduler admission must fail CLOSED (deny) on
//! transport-layer faults reading budget/quota metadata.
//!
//! Pre-fix:
//!   `BudgetChecker::check_budget` catches `ferriskey::Error` from the
//!   budget HGETALL/HGET and caches `BudgetCheckResult::Ok` ("advisory"
//!   mode). Net effect: a flapping Valkey or corrupted budget key makes
//!   the scheduler ADMIT the execution, contradicting the documented
//!   "scheduler enforces admission control" contract.
//!
//! Post-fix:
//!   Transport errors from `Scheduler::claim_for_worker` propagate as
//!   scheduler errors; budget-read failures on this path surface as
//!   `SchedulerError::ValkeyContext { .. }` (context-preserving) rather
//!   than bare `SchedulerError::Valkey(…)`. The caller (server →
//!   worker) sees a 5xx and retries next cycle. No admission granted
//!   while the fault persists.
//!
//! Failure injection:
//!   Plant the budget `:limits` key as a *string* (SET instead of HSET).
//!   `HGETALL` on a non-hash returns WRONGTYPE — a deterministic
//!   transport error we can exercise without mocking Valkey.
//!
//! Run with: cargo test -p ff-test --test pr_c1_admission_fail_closed \
//!           -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{budget_partition, execution_partition};
use ff_core::types::*;
use ff_test::fixtures::{TestCluster, TEST_PARTITION_CONFIG};

const LANE: &str = "pr-c1-admit-lane";
const NS: &str = "pr-c1-admit-ns";
const WORKER: &str = "pr-c1-admit-worker";
const WORKER_INST: &str = "pr-c1-admit-worker-inst-1";

fn test_config() -> ff_core::partition::PartitionConfig {
    TEST_PARTITION_CONFIG
}

async fn fcall_create_execution(
    tc: &TestCluster,
    eid: &ExecutionId,
    namespace: &str,
    lane: &str,
) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(lane);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&lane_id),
        ctx.noop(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        namespace.to_owned(),
        lane.to_owned(),
        "pr-c1-admit".to_owned(),
        "0".to_owned(),
        "pr-c1".to_owned(),
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
    let _raw: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution");
}

#[tokio::test]
#[serial_test::serial]
async fn scheduler_fails_closed_on_corrupted_budget_limits() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let config = test_config();

    // 1. Create an eligible execution.
    let lane_id = LaneId::new(LANE);
    let eid = ExecutionId::solo(&lane_id, &config);
    fcall_create_execution(&tc, &eid, NS, LANE).await;

    // 2. Attach a fresh budget_id to the exec core.
    let budget_id = BudgetId::new();
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core().as_str())
        .arg("budget_ids")
        .arg(budget_id.to_string().as_str())
        .execute()
        .await
        .expect("HSET budget_ids");

    // 3. Plant WRONGTYPE on the budget `:limits` key.
    let bpart = budget_partition(&budget_id, &config);
    let tag = bpart.hash_tag();
    let limits_key = format!("ff:budget:{}:{}:limits", tag, budget_id);
    let _: () = tc
        .client()
        .cmd("SET")
        .arg(limits_key.as_str())
        .arg("not-a-hash")
        .execute()
        .await
        .expect("SET WRONGTYPE");

    // 4. Call the scheduler. Post-fix: returns Err(SchedulerError::Valkey)
    //    carrying the WRONGTYPE. Pre-fix: silently returns Ok(Some(grant))
    //    because the budget checker swallowed the error and cached Ok.
    let scheduler = ff_scheduler::claim::Scheduler::new(tc.client().clone(), config);
    let worker_id = WorkerId::new(WORKER);
    let wiid = WorkerInstanceId::new(WORKER_INST);
    let no_caps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    let result = scheduler
        .claim_for_worker(&lane_id, &worker_id, &wiid, &no_caps, 5_000)
        .await;

    match result {
        Err(e) => {
            // Expected: transport error surfaces. Any Valkey-kind error is
            // acceptable (ResponseError for WRONGTYPE is the usual shape).
            let kind = e.valkey_kind();
            assert!(
                kind.is_some(),
                "expected a Valkey-kind error, got {e:?} (kind={kind:?})"
            );
        }
        Ok(grant) => panic!(
            "expected Err on corrupted budget limits, got Ok({grant:?}) \
             — admission control is failing OPEN"
        ),
    }
}
