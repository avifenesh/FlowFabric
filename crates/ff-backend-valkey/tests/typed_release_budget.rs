//! cairn #454 Phase 3b - live-Valkey coverage of the typed
//! `EngineBackend::release_budget` body.
//!
//! Validates the per-execution ledger approach (option A) decided on
//! #454 comment 4356633770:
//!
//! 1. A matching record_spend followed by release returns the aggregate to zero.
//! 2. Multiple record_spend calls accumulate; a single release reverses them.
//! 3. Release on an execution that never recorded is an idempotent no-op.
//! 4. Release itself is idempotent: calling twice is safe.
//! 5. Releasing one execution does not touch another's attribution.
//!
//! Ignore-gated (live Valkey at 127.0.0.1:6379). Run via:
//!
//! ```text
//! cargo test -p ff-backend-valkey --test typed_release_budget -- \
//!     --ignored --test-threads=1
//! ```

use std::collections::BTreeMap;

use ferriskey::Value;
use ff_backend_valkey::{ValkeyBackend, build_client};
use ff_core::backend::BackendConfig;
use ff_core::contracts::{RecordSpendArgs, ReleaseBudgetArgs, ReportUsageResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, LaneId, TimestampMs};

const LANE: &str = "release-budget-test-lane";

fn single_budget_partition_config() -> PartitionConfig {
    PartitionConfig {
        num_flow_partitions: 1,
        num_budget_partitions: 1,
        num_quota_partitions: 1,
    }
}

async fn create_two_dim_budget(
    client: &ferriskey::Client,
    budget_id: &str,
    dims: &[(&str, u64, u64)],
) {
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let resets_key = "ff:idx:{b:0}:budget_resets".to_string();
    let policies_index = "ff:idx:{b:0}:budget_policies".to_string();

    let now = TimestampMs::now();
    let mut argv: Vec<String> = vec![
        budget_id.to_owned(),
        "lane".to_owned(),
        LANE.to_owned(),
        "strict".to_owned(),
        "fail".to_owned(),
        "warn".to_owned(),
        "0".to_owned(),
        now.to_string(),
        dims.len().to_string(),
    ];
    for (d, _, _) in dims {
        argv.push((*d).to_owned());
    }
    for (_, h, _) in dims {
        argv.push(h.to_string());
    }
    for (_, _, s) in dims {
        argv.push(s.to_string());
    }
    let keys: Vec<String> = vec![def_key, limits_key, usage_key, resets_key, policies_index];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();

    let _: Value = client
        .fcall("ff_create_budget", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_budget");
}

async fn backend() -> std::sync::Arc<ValkeyBackend> {
    let client = build_client(&BackendConfig::valkey("127.0.0.1", 6379))
        .await
        .expect("build_client");
    ff_script::loader::ensure_library(&client)
        .await
        .expect("ensure_library");
    ValkeyBackend::from_client_and_partitions(client, single_budget_partition_config())
}

fn spend_args(
    bid: &BudgetId,
    eid: &ExecutionId,
    deltas: &[(&str, u64)],
    idem: &str,
) -> RecordSpendArgs {
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    for (k, v) in deltas {
        m.insert((*k).to_owned(), *v);
    }
    RecordSpendArgs::new(bid.clone(), eid.clone(), m, idem.to_owned())
}

async fn usage_tokens(client: &ferriskey::Client, bid: &BudgetId) -> u64 {
    let key = format!("ff:budget:{{b:0}}:{bid}:usage");
    let v: Option<String> = client
        .hget(&key, "tokens")
        .await
        .expect("hget usage tokens");
    v.as_deref().unwrap_or("0").parse().unwrap_or(0)
}

async fn ledger_exists(client: &ferriskey::Client, bid: &BudgetId, eid: &ExecutionId) -> bool {
    let key = format!("ff:budget:{{b:0}}:{bid}:by_exec:{eid}");
    let v: bool = client
        .cmd("EXISTS")
        .arg(&key)
        .execute()
        .await
        .expect("EXISTS ledger");
    v
}

/// 1 - record 5 tokens, release, aggregate back to 0, ledger gone.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn release_after_record_zeros_aggregate() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(be.client(), &bid.to_string(), &[("tokens", 100, 50)]).await;

    let eid = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    let r = be
        .record_spend(spend_args(&bid, &eid, &[("tokens", 5)], "rel-1"))
        .await
        .expect("record_spend Ok");
    assert!(matches!(r, ReportUsageResult::Ok));
    assert_eq!(usage_tokens(be.client(), &bid).await, 5);

    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), eid.clone()))
        .await
        .expect("release_budget Ok");
    assert_eq!(
        usage_tokens(be.client(), &bid).await,
        0,
        "aggregate tokens must drop back to 0 after release"
    );
    assert!(
        !ledger_exists(be.client(), &bid, &eid).await,
        "ledger hash must be deleted after release"
    );
}

/// 2 - three record_spend calls accumulating; release zeroes all.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn release_after_multiple_records_zeros_aggregate() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(be.client(), &bid.to_string(), &[("tokens", 100, 90)]).await;

    let eid = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    be.record_spend(spend_args(&bid, &eid, &[("tokens", 3)], "m-1"))
        .await
        .expect("spend 1");
    be.record_spend(spend_args(&bid, &eid, &[("tokens", 3)], "m-2"))
        .await
        .expect("spend 2");
    be.record_spend(spend_args(&bid, &eid, &[("tokens", 4)], "m-3"))
        .await
        .expect("spend 3");
    assert_eq!(usage_tokens(be.client(), &bid).await, 10);

    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), eid.clone()))
        .await
        .expect("release_budget Ok");
    assert_eq!(usage_tokens(be.client(), &bid).await, 0);
    assert!(!ledger_exists(be.client(), &bid, &eid).await);
}

/// 3 - release without prior record is an idempotent no-op.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn release_without_prior_record_is_noop() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(be.client(), &bid.to_string(), &[("tokens", 100, 50)]).await;

    let eid = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), eid.clone()))
        .await
        .expect("release_budget idempotent no-op");
    assert_eq!(usage_tokens(be.client(), &bid).await, 0);
}

/// 4 - release twice is idempotent; aggregate stays at 0.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn release_is_idempotent() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(be.client(), &bid.to_string(), &[("tokens", 100, 50)]).await;

    let eid = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    be.record_spend(spend_args(&bid, &eid, &[("tokens", 7)], "i-1"))
        .await
        .expect("spend");
    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), eid.clone()))
        .await
        .expect("first release");
    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), eid.clone()))
        .await
        .expect("second release (no-op)");
    assert_eq!(usage_tokens(be.client(), &bid).await, 0);
}

/// 5 - releasing one execution does not affect another's attribution.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn release_one_exec_does_not_affect_another_exec() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(be.client(), &bid.to_string(), &[("tokens", 100, 90)]).await;

    let eid_a = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    let eid_b = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    be.record_spend(spend_args(&bid, &eid_a, &[("tokens", 5)], "a-1"))
        .await
        .expect("spend a");
    be.record_spend(spend_args(&bid, &eid_b, &[("tokens", 7)], "b-1"))
        .await
        .expect("spend b");
    assert_eq!(usage_tokens(be.client(), &bid).await, 12);

    be.release_budget(ReleaseBudgetArgs::new(bid.clone(), eid_a.clone()))
        .await
        .expect("release a");
    assert_eq!(
        usage_tokens(be.client(), &bid).await,
        7,
        "aggregate must reflect only exec_b's attribution"
    );
    assert!(!ledger_exists(be.client(), &bid, &eid_a).await);
    assert!(ledger_exists(be.client(), &bid, &eid_b).await);
}
