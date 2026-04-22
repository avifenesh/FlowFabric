//! RFC-012 §R7.2.3 integration test.
//!
//! Exercises `EngineBackend::report_usage` against a live Valkey,
//! asserting:
//!
//! 1. The trait dispatches through `ValkeyBackend::report_usage_impl`
//!    to the `ff_report_usage_and_check` Lua function with the ARGV
//!    shape the Lua expects (dim_count, dims, deltas, now, dedup_key).
//! 2. The new `UsageDimensions.dedup_key: Option<String>` field
//!    threads to the wire — a second call with the same key returns
//!    `ReportUsageResult::AlreadyApplied` (idempotency round-trip).
//! 3. The round-7 return-type swap from `AdmissionDecision` to
//!    `ReportUsageResult` carries the structured `SoftBreach` /
//!    `HardBreach` variants end-to-end (wire → trait → caller).
//!
//! Run with: `cargo test -p ff-test --test r7_backend_report_usage -- --test-threads=1`

use std::collections::BTreeMap;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::{HandleKind, UsageDimensions};
use ff_core::contracts::ReportUsageResult;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{
    AttemptId, AttemptIndex, BudgetId, LaneId, LeaseEpoch, LeaseId, TimestampMs,
    WorkerInstanceId,
};
use ff_test::fixtures::TestCluster;

/// Inline `ff_create_budget` on the `{b:0}` partition. Mirrors
/// `budget_dedup_hashtag.rs` — budget fixture helpers are test-private
/// and not exported from `ff-test`.
async fn fcall_create_budget_b0(
    tc: &TestCluster,
    budget_id: &str,
    dim: &str,
    hard: u64,
    soft: u64,
) {
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let resets_key = "ff:idx:{b:0}:budget_resets".to_string();
    let policies_index = "ff:idx:{b:0}:budget_policies".to_string();

    let now = TimestampMs::now();
    let args: Vec<String> = vec![
        budget_id.to_owned(),
        "lane".to_owned(),
        "test-lane".to_owned(),
        "strict".to_owned(),
        "fail".to_owned(),
        "warn".to_owned(),
        "0".to_owned(),
        now.to_string(),
        "1".to_owned(),
        dim.to_owned(),
        hard.to_string(),
        soft.to_string(),
    ];
    let keys: Vec<String> = vec![def_key, limits_key, usage_key, resets_key, policies_index];
    let key_refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let _: Value = tc
        .client()
        .fcall("ff_create_budget", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_budget");
}

/// A `num_budget_partitions = 1` config so the fixture's hard-coded
/// `{b:0}` partition matches what `budget_partition(bid, &config)`
/// hashes to. Production `ff_report_usage_and_check` is
/// lease-independent, so the synthesised handle does not need a real
/// lease — the Lua never reads it.
fn single_budget_partition_config() -> PartitionConfig {
    PartitionConfig {
        num_flow_partitions: 1,
        num_budget_partitions: 1,
        num_quota_partitions: 1,
    }
}

async fn backend() -> std::sync::Arc<ValkeyBackend> {
    let tc = TestCluster::connect().await;
    // `connect` to Valkey through the backend so the dialled client +
    // a pinned partition config land in the Arc the trait uses.
    ValkeyBackend::from_client_and_partitions(
        tc.client().clone(),
        single_budget_partition_config(),
    )
}

/// Round-7 end-to-end: a succession of calls through
/// `EngineBackend::report_usage` produces each of the four
/// `ReportUsageResult` variants, including the dedup-key-driven
/// `AlreadyApplied` path on a repeat invocation with the same
/// `UsageDimensions.dedup_key`.
#[tokio::test]
#[serial_test::serial]
async fn r7_report_usage_trait_round_trip() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Use a deterministic uuid so the fixture key (`{b:0}:<uuid>`)
    // matches what `BudgetKeyContext::new(partition, &bid)` builds
    // inside the trait impl.
    let bid = BudgetId::new();
    let budget_id_str = bid.to_string();
    // hard=100, soft=50 — exercises SoftBreach + HardBreach variants.
    fcall_create_budget_b0(&tc, &budget_id_str, "tokens", 100, 50).await;

    let backend = backend().await;

    // Synthesise a handle. `report_usage` is lease-independent, so
    // none of these fields need to match a live lease.
    let exec_id = ff_core::types::ExecutionId::solo(
        &LaneId::new("test-lane"),
        &single_budget_partition_config(),
    );
    let handle = ValkeyBackend::encode_handle(
        exec_id,
        AttemptIndex::new(0),
        AttemptId::new(),
        LeaseId::new(),
        LeaseEpoch::new(0),
        30_000,
        LaneId::new("test-lane"),
        WorkerInstanceId::new("test-worker"),
        HandleKind::Fresh,
    );

    // ── 1. Ok path ────────────────────────────────────────────────
    let mut dims = UsageDimensions::default();
    dims.custom = BTreeMap::from([("tokens".to_owned(), 10)]);
    dims.dedup_key = Some("round-trip-1".to_owned());
    let r = backend
        .report_usage(&handle, &bid, dims.clone())
        .await
        .expect("report_usage Ok");
    assert!(matches!(r, ReportUsageResult::Ok), "first call Ok, got {r:?}");

    // ── 2. AlreadyApplied (same dedup_key) ────────────────────────
    let r = backend
        .report_usage(&handle, &bid, dims.clone())
        .await
        .expect("report_usage AlreadyApplied");
    assert!(
        matches!(r, ReportUsageResult::AlreadyApplied),
        "repeat with same dedup_key must be AlreadyApplied, got {r:?}"
    );

    // ── 3. SoftBreach: current=10, +45 = 55 > soft(50) ───────────
    dims.custom.insert("tokens".to_owned(), 45);
    dims.dedup_key = Some("round-trip-2".to_owned());
    let r = backend
        .report_usage(&handle, &bid, dims.clone())
        .await
        .expect("report_usage SoftBreach");
    match r {
        ReportUsageResult::SoftBreach {
            dimension,
            current_usage,
            soft_limit,
        } => {
            assert_eq!(dimension, "tokens");
            // new current: 55 (after increment applied — SoftBreach
            // is advisory, not rejecting)
            assert_eq!(current_usage, 55);
            assert_eq!(soft_limit, 50);
        }
        other => panic!("expected SoftBreach, got {other:?}"),
    }

    // ── 4. HardBreach: current=55, +50 = 105 > hard(100) ─────────
    dims.custom.insert("tokens".to_owned(), 50);
    dims.dedup_key = Some("round-trip-3".to_owned());
    let r = backend
        .report_usage(&handle, &bid, dims)
        .await
        .expect("report_usage HardBreach");
    match r {
        ReportUsageResult::HardBreach {
            dimension,
            current_usage,
            hard_limit,
        } => {
            assert_eq!(dimension, "tokens");
            // HardBreach is NOT applied — current stays at 55.
            assert_eq!(current_usage, 55);
            assert_eq!(hard_limit, 100);
        }
        other => panic!("expected HardBreach, got {other:?}"),
    }
}
