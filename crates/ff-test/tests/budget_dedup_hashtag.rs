//! Regression test for #108: the typed FCALL wrapper
//! `ff_script::functions::budget::ff_report_usage_and_check` must wrap the
//! caller-supplied `dedup_key` into `ff:usagededup:{b:M}:<id>` — matching
//! the format the REST layer (`ff-server`) and `ff-sdk` produce — so the
//! dedup state lands on the same cluster slot as the budget keys.
//!
//! Pre-fix:
//!   The wrapper forwarded `args.dedup_key.clone().unwrap_or_default()`
//!   verbatim. Lua then wrote the dedup flag at the bare key (e.g.
//!   `"retry-42"`). The test's post-condition GETs prove the bug:
//!     GET "ff:usagededup:{b:0}:retry-42"  → nil   (not populated)
//!     GET "retry-42"                      → "1"   (leaked at wrong key)
//!
//! Post-fix:
//!   The wrapper builds the dedup key via
//!   `usage_dedup_key(k.hash_tag, id)`. Lua writes at the wrapped key:
//!     GET "ff:usagededup:{b:0}:retry-42"  → "1"   (correct)
//!     GET "retry-42"                      → nil
//!
//! Cluster-mode manifestation: pre-fix, calling this against a cluster
//! returns CROSSSLOT because the bare dedup key and the `{b:0}`-tagged
//! budget keys hash to different slots. Post-fix, both share the `{b:0}`
//! tag and hash to the same slot. The `TestCluster` harness uses
//! `FF_CLUSTER` to switch between standalone and cluster — the shape
//! assertion below holds in both modes; cluster additionally proves the
//! CROSSSLOT path is closed because the FCALL does not error.

use ferriskey::Value;
use ff_core::contracts::ReportUsageArgs;
use ff_core::types::TimestampMs;
use ff_script::functions::budget::{ff_report_usage_and_check, BudgetOpKeys};
use ff_test::fixtures::TestCluster;

/// Inline `ff_create_budget` on the fixed `{b:0}` partition. Separate copy
/// from `e2e_lifecycle.rs` to keep this test self-contained — budget
/// helpers are test-private and not exported from `ff-test`.
async fn fcall_create_budget_b0(
    tc: &TestCluster,
    budget_id: &str,
    dim: &str,
    hard: u64,
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
        "0".to_owned(), // reset_interval_ms
        now.to_string(),
        "1".to_owned(), // dim_count
        dim.to_owned(),
        hard.to_string(),
        "0".to_owned(), // soft
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

/// The typed wrapper must wrap `dedup_key` with the budget hash-tag so the
/// Lua-side `SET <dedup_key> 1 EX <ttl>` lands on the wrapped key, not on
/// the bare caller-supplied id.
#[tokio::test]
#[serial_test::serial]
async fn ff_report_usage_and_check_wraps_dedup_key_with_budget_hashtag() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let budget_id = "pr108-dedup-wrap";
    fcall_create_budget_b0(&tc, budget_id, "tokens", 1000).await;

    let usage_key = format!("ff:budget:{{b:0}}:{budget_id}:usage");
    let limits_key = format!("ff:budget:{{b:0}}:{budget_id}:limits");
    let def_key = format!("ff:budget:{{b:0}}:{budget_id}");
    let hash_tag = "{b:0}";

    let keys = BudgetOpKeys {
        usage_key: &usage_key,
        limits_key: &limits_key,
        def_key: &def_key,
        hash_tag,
    };

    let raw_dedup_id = "retry-42";
    let args = ReportUsageArgs {
        dimensions: vec!["tokens".to_owned()],
        deltas: vec![10],
        now: TimestampMs::now(),
        dedup_key: Some(raw_dedup_id.to_owned()),
    };

    // The call itself must succeed — in cluster mode pre-fix this would
    // have returned CROSSSLOT because `"retry-42"` is untagged.
    let result = ff_report_usage_and_check(tc.client(), &keys, &args)
        .await
        .expect("typed wrapper must succeed (pre-fix: CROSSSLOT in cluster)");
    // Sanity: first call against a fresh budget is Ok (or AlreadyApplied
    // if the cleanup didn't wipe — cleanup is FLUSHDB-based so it did).
    use ff_core::contracts::ReportUsageResult;
    assert!(
        matches!(result, ReportUsageResult::Ok),
        "first dedup'd report must be Ok, got {result:?}"
    );

    // Key-shape post-condition: the wrapped key is populated, the bare key
    // is NOT. Pre-fix these two assertions invert.
    let wrapped_key = format!("ff:usagededup:{{b:0}}:{raw_dedup_id}");
    let wrapped_val: Option<String> = tc
        .client()
        .cmd("GET")
        .arg(wrapped_key.as_str())
        .execute()
        .await
        .expect("GET wrapped dedup key");
    assert_eq!(
        wrapped_val.as_deref(),
        Some("1"),
        "post-fix: dedup flag must be written at `ff:usagededup:{{b:0}}:<id>` \
         (co-located with budget keys on the `{{b:0}}` slot). Pre-fix this is nil.",
    );

    let bare_val: Option<String> = tc
        .client()
        .cmd("GET")
        .arg(raw_dedup_id)
        .execute()
        .await
        .expect("GET bare dedup key");
    assert_eq!(
        bare_val, None,
        "post-fix: dedup flag must NOT leak at the bare caller-supplied id. \
         Pre-fix this is Some(\"1\") — the symptom of the unwrapped forward.",
    );

    // Idempotency: repeating with the same dedup id must hit AlreadyApplied
    // (proves the wrapped key is actually the one Lua reads for dedup, not
    // just an orphan write).
    let args2 = ReportUsageArgs {
        dimensions: vec!["tokens".to_owned()],
        deltas: vec![10],
        now: TimestampMs::now(),
        dedup_key: Some(raw_dedup_id.to_owned()),
    };
    let result2 = ff_report_usage_and_check(tc.client(), &keys, &args2)
        .await
        .expect("second call must succeed");
    assert!(
        matches!(result2, ReportUsageResult::AlreadyApplied),
        "repeat with same dedup id must be AlreadyApplied, got {result2:?}",
    );
}
