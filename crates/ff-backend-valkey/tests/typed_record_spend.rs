//! cairn #454 Phase 3a — live-Valkey coverage of the typed
//! `EngineBackend::record_spend` body.
//!
//! Asserts the round-trip through `ValkeyBackend::record_spend` →
//! `ff_record_spend` FCALL → `ReportUsageResult` for the four outcome
//! variants:
//!
//! 1. `Ok` on first application under the hard limit.
//! 2. `SoftBreach` when the increment crosses the soft limit (still
//!    applied — soft is advisory).
//! 3. `HardBreach` when the increment would cross the hard limit
//!    (rejected — counter unchanged).
//! 4. `AlreadyApplied` on a replay with the same `idempotency_key`.
//!
//! Ignore-gated (live Valkey at 127.0.0.1:6379) following
//! `read_waitpoint_token_missing.rs` / `capabilities.rs`. Run via:
//!
//! ```text
//! cargo test -p ff-backend-valkey --test typed_record_spend -- \
//!     --ignored --test-threads=1
//! ```

use std::collections::BTreeMap;

use ferriskey::Value;
use ff_backend_valkey::{ValkeyBackend, build_client};
use ff_core::backend::BackendConfig;
use ff_core::contracts::{RecordSpendArgs, ReportUsageResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, LaneId, TimestampMs};

const LANE: &str = "record-spend-test-lane";

/// One partition so `budget_partition(bid, cfg)` unambiguously hashes
/// to `{b:0}` — matches the inline `ff_create_budget` below.
fn single_budget_partition_config() -> PartitionConfig {
    PartitionConfig {
        num_flow_partitions: 1,
        num_budget_partitions: 1,
        num_quota_partitions: 1,
    }
}

/// Inline `ff_create_budget` on `{b:0}` with two open-set dimensions
/// (`tokens` and `cost_cents`). Mirrors the fixture in
/// `ff-test/tests/r7_backend_report_usage.rs` — the `ff-test` crate is
/// not a dev-dep on this crate so the fixture is inlined.
async fn create_two_dim_budget(
    client: &ferriskey::Client,
    budget_id: &str,
    dims: &[(&str, u64, u64)], // (dim, hard, soft)
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
    // Run the flowfabric library loader so a freshly-bumped
    // `flowfabric_lua_version` reloads the module before the first FCALL.
    ff_script::loader::ensure_library(&client)
        .await
        .expect("ensure_library");
    ValkeyBackend::from_client_and_partitions(client, single_budget_partition_config())
}

fn spend_args(
    bid: &BudgetId,
    exec: &ExecutionId,
    deltas: &[(&str, u64)],
    idem: &str,
) -> RecordSpendArgs {
    let mut m: BTreeMap<String, u64> = BTreeMap::new();
    for (k, v) in deltas {
        m.insert((*k).to_owned(), *v);
    }
    RecordSpendArgs::new(bid.clone(), exec.clone(), m, idem.to_owned())
}

/// Path 1 — happy-path `Ok` under the hard limit.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn record_spend_ok_path() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(
        be.client(),
        &bid.to_string(),
        &[("tokens", 100, 50), ("cost_cents", 1000, 500)],
    )
    .await;

    let exec = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    let r = be
        .record_spend(spend_args(
            &bid,
            &exec,
            &[("tokens", 10), ("cost_cents", 20)],
            "ok-1",
        ))
        .await
        .expect("record_spend Ok call");
    assert!(matches!(r, ReportUsageResult::Ok), "expected Ok, got {r:?}");

    // cairn #454 Phase 3b — ledger was mirrored.
    let by_exec_key = format!("ff:budget:{{b:0}}:{bid}:by_exec:{exec}");
    let tokens: Option<String> = be
        .client()
        .hget(&by_exec_key, "tokens")
        .await
        .expect("hget ledger tokens");
    assert_eq!(tokens.as_deref(), Some("10"), "ledger tokens must equal recorded delta");
    let cost: Option<String> = be
        .client()
        .hget(&by_exec_key, "cost_cents")
        .await
        .expect("hget ledger cost");
    assert_eq!(cost.as_deref(), Some("20"), "ledger cost_cents must equal recorded delta");

    // Index tracks this execution.
    let index_key = format!("ff:budget:{{b:0}}:{bid}:by_exec:index");
    let is_member: bool = be
        .client()
        .cmd("SISMEMBER")
        .arg(&index_key)
        .arg(exec.to_string())
        .execute()
        .await
        .expect("SISMEMBER index");
    assert!(is_member, "execution must be in the by_exec index");
}

/// Path 2 — idempotent replay on the same `idempotency_key` returns
/// `AlreadyApplied` without double-incrementing.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn record_spend_idempotent_replay() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(
        be.client(),
        &bid.to_string(),
        &[("tokens", 100, 50)],
    )
    .await;

    let exec = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    let args = spend_args(&bid, &exec, &[("tokens", 7)], "replay-me");
    let r = be
        .record_spend(args.clone())
        .await
        .expect("first record_spend");
    assert!(matches!(r, ReportUsageResult::Ok), "first call Ok, got {r:?}");

    let r2 = be.record_spend(args).await.expect("replay record_spend");
    assert!(
        matches!(r2, ReportUsageResult::AlreadyApplied),
        "replay with same idempotency_key must be AlreadyApplied, got {r2:?}"
    );
}

/// Path 3 — crossing the soft limit returns `SoftBreach` AND applies
/// the increment (soft limit is advisory).
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn record_spend_soft_breach() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(
        be.client(),
        &bid.to_string(),
        &[("tokens", 100, 50)],
    )
    .await;

    let exec = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    // current=0, +60 = 60 > soft(50), still < hard(100)
    let r = be
        .record_spend(spend_args(
            &bid,
            &exec,
            &[("tokens", 60)],
            "soft-1",
        ))
        .await
        .expect("record_spend soft path");
    match r {
        ReportUsageResult::SoftBreach {
            dimension,
            current_usage,
            soft_limit,
        } => {
            assert_eq!(dimension, "tokens");
            assert_eq!(current_usage, 60, "soft breach applies the increment");
            assert_eq!(soft_limit, 50);
        }
        other => panic!("expected SoftBreach, got {other:?}"),
    }
}

/// Path 4 — crossing the hard limit returns `HardBreach` and DOES NOT
/// apply the increment.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn record_spend_hard_breach() {
    let be = backend().await;
    let bid = BudgetId::new();
    create_two_dim_budget(
        be.client(),
        &bid.to_string(),
        &[("tokens", 100, 50)],
    )
    .await;

    let exec = ExecutionId::solo(&LaneId::new(LANE), &single_budget_partition_config());
    // Seed usage to 40 with a first Ok call.
    let _ = be
        .record_spend(spend_args(&bid, &exec, &[("tokens", 40)], "hard-seed"))
        .await
        .expect("seed Ok");

    // +80 = 120 > hard(100) — reject; counter stays at 40.
    let r = be
        .record_spend(spend_args(&bid, &exec, &[("tokens", 80)], "hard-1"))
        .await
        .expect("record_spend hard path");
    match r {
        ReportUsageResult::HardBreach {
            dimension,
            current_usage,
            hard_limit,
        } => {
            assert_eq!(dimension, "tokens");
            assert_eq!(current_usage, 40, "hard breach must NOT apply the increment");
            assert_eq!(hard_limit, 100);
        }
        other => panic!("expected HardBreach, got {other:?}"),
    }
}
