//! v0.12 PR-5.5 retry-path fix: pin the pointer-vs-counter distinction
//! between `read_current_attempt_index` and `read_total_attempt_count`.
//!
//! The SDK worker's `claim_from_grant` / `claim_execution` path needs
//! the **total attempt counter** (`total_attempt_count`) to compute
//! the next fresh attempt index. Pre-fix it was pulling the
//! **current-attempt pointer** (`current_attempt_index`) instead; on a
//! retry-of-a-retry the two fields legitimately diverge (the pointer
//! still names the previous terminal-failed attempt until the new
//! claim fires, while the counter has already been bumped by Lua 5920
//! when the prior attempt failed).
//!
//! This test stages a post-terminal-fail-pre-retry state by writing
//! the two fields to divergent values via raw HSET, then asserts the
//! two read paths surface the values the SDK expects — without this
//! divergence-check the fix is invisible (both methods read back the
//! same `0` on a pristine exec).
//!
//! Ignore-gated (live Valkey at 127.0.0.1:6379 required), matching
//! the sibling live tests.

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;
use ff_core::keys::ExecKeyContext;
use ff_core::partition::{PartitionConfig, execution_partition};
use ff_core::types::{ExecutionId, FlowId};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 6379;

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn read_total_attempt_count_diverges_from_pointer_on_retry_path() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey(HOST, PORT))
        .await
        .expect("connect ValkeyBackend");

    let raw = ferriskey::ClientBuilder::new()
        .host(HOST, PORT)
        .build()
        .await
        .expect("build raw ferriskey client");

    let config = PartitionConfig::default();
    let eid = ExecutionId::for_flow(&FlowId::new(), &config);
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);

    // Stage: write divergent fields. This simulates the Lua 5920 state
    // AFTER a first attempt (idx 0) has terminal-failed and BEFORE the
    // new retry-claim fires:
    //   * `current_attempt_index` = 0 (pointer still names the prior
    //     terminal-failed attempt until the retry claim seats a new one)
    //   * `total_attempt_count`   = 1 (counter already bumped by the
    //     fail transition; the next claim uses this value)
    let core_key = ctx.core();
    let _: Value = raw
        .cmd("HSET")
        .arg(&core_key)
        .arg("current_attempt_index")
        .arg("0")
        .arg("total_attempt_count")
        .arg("1")
        .execute()
        .await
        .expect("HSET divergent fields");

    // The pre-PR-5.5-fix SDK code path: `read_current_attempt_index`
    // returns the STALE pointer. On a retry this collided with the
    // terminal-failed attempt row — the bug.
    let pointer = backend
        .read_current_attempt_index(&eid)
        .await
        .expect("read_current_attempt_index");
    assert_eq!(
        pointer.0, 0,
        "pointer names the currently-leased (or most-recent-terminal) attempt"
    );

    // The post-fix SDK code path: `read_total_attempt_count` returns
    // the COUNTER — the fresh attempt index for the new claim.
    let counter = backend
        .read_total_attempt_count(&eid)
        .await
        .expect("read_total_attempt_count");
    assert_eq!(
        counter.0, 1,
        "counter gives the next fresh attempt index — distinct from the pointer on the \
         retry-of-terminal-failed path; this divergence is exactly what the SDK fix depends on"
    );

    // Clean up.
    let _: Value = raw
        .cmd("DEL")
        .arg(&core_key)
        .execute()
        .await
        .expect("DEL cleanup");
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn read_total_attempt_count_missing_row_surfaces_pre_claim_zero() {
    // Distinct from PG/SQLite: on Valkey the pre-claim semantic matches
    // the historic SDK inline HGET (nil/empty → 0) so a resume-grant
    // consumed against a not-yet-claimed exec still reaches the FCALL
    // rather than blowing up in the pre-read. Pin that shape here.
    let backend = ValkeyBackend::connect(BackendConfig::valkey(HOST, PORT))
        .await
        .expect("connect ValkeyBackend");

    let config = PartitionConfig::default();
    let eid = ExecutionId::for_flow(&FlowId::new(), &config);

    let counter = backend
        .read_total_attempt_count(&eid)
        .await
        .expect("read_total_attempt_count on missing exec must not error (matches HGET nil)");
    assert_eq!(counter.0, 0, "missing hash reads as 0 on Valkey");
}
