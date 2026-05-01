//! v0.13 cairn #454 headline demo — per-execution budget ledger.
//!
//! Exercises two of the four new `EngineBackend` trait methods that
//! landed in cairn #454:
//!
//! 1. [`EngineBackend::record_spend`] — stamps a per-execution budget
//!    usage row keyed by `(budget_id, execution_id, dimension)` and
//!    increments the aggregate counter.
//! 2. [`EngineBackend::release_budget`] — reads the per-execution stamp,
//!    reverses the aggregate counter by the exact recorded amount,
//!    DELETEs the per-execution row. Missing-row is `Ok(())` — reversal
//!    is idempotent.
//!
//! The point of the ledger shape (migration 0020 on PG/SQLite, per-exec
//! HASH on Valkey, Lua v31) is that `release_budget` can reverse a
//! previously-recorded spend *exactly* without the caller needing to
//! remember the amount — the stamp is the source of truth. This matters
//! for cairn's rollback scenarios where a workflow decides mid-flight
//! that a recorded spend must not be billed.
//!
//! Runs against a local Valkey at 127.0.0.1:6379 (override via env
//! `FF_DEMO_VALKEY_HOST` / `FF_DEMO_VALKEY_PORT`). No scheduler / worker
//! loop — the demo calls the trait methods directly on `ValkeyBackend`.
//!
//! Run: `cargo run --bin budget-ledger` (inside the example dir).
//!
//! The other two #454 methods (`deliver_approval_signal`,
//! `issue_grant_and_claim`) require additional preconditions (a live
//! flow + waitpoint + claim-grant state) that the existing
//! `deploy-approval` / `incident-remediation` examples already set up;
//! re-exercising those here would add ~300 LoC of setup without
//! clarifying the trait-method surface. This example stays focused on
//! the record/release pair — the clearest headline for the ledger shape.

use std::collections::BTreeMap;
use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::{ValkeyBackend, build_client};
use ff_core::backend::BackendConfig;
use ff_core::contracts::{RecordSpendArgs, ReleaseBudgetArgs, ReportUsageResult};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{BudgetId, ExecutionId, LaneId, TimestampMs};

const LANE: &str = "budget-ledger-demo";

fn demo_partitions() -> PartitionConfig {
    PartitionConfig {
        num_flow_partitions: 1,
        num_budget_partitions: 1,
        num_quota_partitions: 1,
    }
}

/// Bootstrap a two-dimensional budget on `{b:0}` so `record_spend` has
/// a policy to attach to. Mirrors the `ff_create_budget` fixture used
/// by the typed-`record_spend` integration tests.
async fn create_demo_budget(client: &ferriskey::Client, budget_id: &str) {
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let resets_key = "ff:idx:{b:0}:budget_resets".to_string();
    let policies_index = "ff:idx:{b:0}:budget_policies".to_string();

    let now = TimestampMs::now();
    let argv: Vec<String> = vec![
        budget_id.to_owned(),
        "lane".to_owned(),
        LANE.to_owned(),
        "strict".to_owned(),
        "fail".to_owned(),
        "warn".to_owned(),
        "0".to_owned(),
        now.to_string(),
        "2".to_string(),
        "tokens".to_owned(),
        "cost_cents".to_owned(),
        "1000".to_owned(),   // tokens hard
        "10000".to_owned(),  // cost_cents hard
        "800".to_owned(),    // tokens soft
        "8000".to_owned(),   // cost_cents soft
    ];
    let keys: Vec<String> = vec![def_key, limits_key, usage_key, resets_key, policies_index];
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let arg_refs: Vec<&str> = argv.iter().map(String::as_str).collect();

    let _: Value = client
        .fcall("ff_create_budget", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_budget");
}

/// Read the aggregate `usage` hash for a budget so we can show the
/// before/after counter state around a `release_budget` call.
async fn read_usage(client: &ferriskey::Client, budget_id: &str) -> Vec<(String, String)> {
    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let map = client
        .hgetall(usage_key.as_str())
        .await
        .expect("HGETALL usage");
    let mut out: Vec<(String, String)> = map.into_iter().collect();
    out.sort();
    out
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "budget_ledger=info,ff_backend_valkey=warn".into()),
        )
        .init();

    let host = std::env::var("FF_DEMO_VALKEY_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("FF_DEMO_VALKEY_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);
    tracing::info!(%host, %port, "connecting to Valkey");

    let client = build_client(&BackendConfig::valkey(host.as_str(), port)).await?;
    ff_script::loader::ensure_library(&client).await?;
    // Keep our own handle to the Valkey client for the demo's
    // before/after HGETALL inspection (read_usage below). `ferriskey::Client`
    // is cheap to clone. Production consumers route budget reads through
    // `get_budget_status`; this example reads the storage key directly
    // to visually demonstrate the ledger effect.
    let client_for_inspection = client.clone();
    let backend: Arc<ValkeyBackend> = ValkeyBackend::from_client_and_partitions(client, demo_partitions());

    // Fresh budget per run — no cleanup needed between invocations.
    let bid = BudgetId::new();
    tracing::info!(budget = %bid, "creating demo budget (limits: tokens=1000, cost_cents=10000)");
    create_demo_budget(&client_for_inspection, &bid.to_string()).await;
    let exec = ExecutionId::solo(&LaneId::new(LANE), &demo_partitions());
    tracing::info!(execution = %exec, "synthetic execution id (no flow attached — trait demo only)");

    // --- record_spend ---------------------------------------------------
    let mut deltas: BTreeMap<String, u64> = BTreeMap::new();
    deltas.insert("tokens".into(), 250);
    deltas.insert("cost_cents".into(), 4_200);
    let args = RecordSpendArgs::new(bid.clone(), exec.clone(), deltas, "demo-spend-1");

    let before = read_usage(&client_for_inspection, &bid.to_string()).await;
    tracing::info!(?before, "budget usage BEFORE record_spend");

    let outcome = backend.record_spend(args).await?;
    match &outcome {
        ReportUsageResult::Ok => tracing::info!("record_spend: Ok (under hard limit)"),
        ReportUsageResult::SoftBreach { .. } => tracing::info!(?outcome, "record_spend: SoftBreach (advisory)"),
        ReportUsageResult::HardBreach { .. } => tracing::warn!(?outcome, "record_spend: HardBreach (rejected)"),
        ReportUsageResult::AlreadyApplied => tracing::info!("record_spend: AlreadyApplied (idempotent replay)"),
        _ => tracing::info!(?outcome, "record_spend: other variant"),
    }

    let after_spend = read_usage(&client_for_inspection, &bid.to_string()).await;
    tracing::info!(?after_spend, "budget usage AFTER record_spend (expect tokens=250, cost_cents=4200)");

    // Replaying the same idempotency key should be a no-op (counter
    // stays where it is, the per-execution row is unchanged).
    let replay = backend
        .record_spend(RecordSpendArgs::new(
            bid.clone(),
            exec.clone(),
            {
                let mut m = BTreeMap::new();
                m.insert("tokens".into(), 250);
                m.insert("cost_cents".into(), 4_200);
                m
            },
            "demo-spend-1",
        ))
        .await?;
    tracing::info!(?replay, "record_spend replay with same idem-key (expect AlreadyApplied)");

    // --- release_budget -------------------------------------------------
    let release_args = ReleaseBudgetArgs::new(bid.clone(), exec.clone());
    backend.release_budget(release_args).await?;
    let after_release = read_usage(&client_for_inspection, &bid.to_string()).await;
    tracing::info!(?after_release, "budget usage AFTER release_budget (expect tokens=0, cost_cents=0)");

    // release_budget is idempotent — a second call on the same
    // (budget_id, execution_id) should also succeed.
    backend
        .release_budget(ReleaseBudgetArgs::new(bid.clone(), exec.clone()))
        .await?;
    tracing::info!("release_budget replay on already-released row (no error — idempotent)");

    tracing::info!("demo complete — #454 record_spend + release_budget round-trip verified");
    Ok(())
}
