//! PR-C1 regression (#67): the budget reconciler must NOT remove a budget
//! from its partition policies-index when reading the budget definition
//! fails with a transport-layer error.
//!
//! Pre-fix:
//!   `reconcile_one_budget` reads the budget definition via
//!   `HGETALL def_key` with `.unwrap_or_default()`. A WRONGTYPE (or any
//!   other transient Valkey error) returns an empty Vec, which the code
//!   reads as "definition missing → prune the index entry". A momentary
//!   Valkey fault therefore becomes durable metadata loss — the budget
//!   is no longer discoverable by subsequent reconciler passes.
//!
//! Post-fix:
//!   `HGETALL` failures propagate as `ferriskey::Error`. The caller logs
//!   a warning and counts it as a scan error; the budget stays in the
//!   index so the next cycle re-attempts.
//!
//! Failure injection:
//!   Plant the budget definition key as a *string* (SET instead of HSET).
//!   HGETALL on a non-hash returns WRONGTYPE.
//!
//! Run with: cargo test -p ff-test --test pr_c1_budget_reconciler_transient \
//!           -- --test-threads=1

use ff_core::keys::budget_policies_index;
use ff_core::partition::{budget_partition, Partition, PartitionFamily};
use ff_core::types::*;
use ff_engine::scanner::Scanner;
use ff_test::fixtures::{TestCluster, TEST_PARTITION_CONFIG};

#[tokio::test]
#[serial_test::serial]
async fn budget_reconciler_preserves_index_on_transient_def_read_failure() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    let config = TEST_PARTITION_CONFIG;

    // 1. Pick a budget id and compute its partition tag.
    let budget_id = BudgetId::new();
    let bpart = budget_partition(&budget_id, &config);
    let tag = bpart.hash_tag();
    let policies_key = budget_policies_index(&tag);
    let def_key = format!("ff:budget:{}:{}", tag, budget_id);

    // 2. Register the budget in the partition index.
    let _: i64 = tc
        .client()
        .cmd("SADD")
        .arg(policies_key.as_str())
        .arg(budget_id.to_string().as_str())
        .execute()
        .await
        .expect("SADD policies_index");

    // 3. Plant WRONGTYPE on the def key — HGETALL will return an error
    //    rather than an empty-hash reply.
    let _: () = tc
        .client()
        .cmd("SET")
        .arg(def_key.as_str())
        .arg("not-a-hash")
        .execute()
        .await
        .expect("SET def_key WRONGTYPE");

    // 4. Run the reconciler on this budget's partition.
    let reconciler = ff_engine::scanner::budget_reconciler::BudgetReconciler::new(
        std::time::Duration::from_secs(60),
    );
    let _scan = reconciler
        .scan_partition(tc.client(), bpart.index)
        .await;

    // 5. The budget MUST still be in the policies index. Pre-fix it was
    //    SREM'd because the corrupted def_key looked empty.
    let still_indexed: bool = tc
        .client()
        .cmd("SISMEMBER")
        .arg(policies_key.as_str())
        .arg(budget_id.to_string().as_str())
        .execute()
        .await
        .expect("SISMEMBER policies_index");
    assert!(
        still_indexed,
        "budget MUST remain in policies_index on transient def-read failure; \
         pre-fix the reconciler SREMs on WRONGTYPE (data loss)"
    );

    // Quiet the unused-import warning on Partition/PartitionFamily — used
    // below only for documentation of which partition we scanned.
    let _ = Partition { family: PartitionFamily::Budget, index: bpart.index };
}
