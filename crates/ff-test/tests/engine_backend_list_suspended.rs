//! Integration coverage for
//! [`ff_core::engine_backend::EngineBackend::list_suspended`] on the
//! Valkey-backed impl (issue #183).
//!
//! Seeds three suspended executions across two lanes in one
//! partition, then pages through via the trait method with
//! `limit=2` + cursor and asserts:
//!   - entries are ordered by ascending `suspended_at_ms` (score)
//!   - `reason` is populated from `suspension:current.reason_code`
//!   - `next_cursor` returns the remaining entry on a second call
//!   - a third call with the exhausted cursor returns an empty page
//!
//! The seeding path is direct `HSET` + `ZADD` rather than a full
//! `ff_suspend_execution` FCALL — list_suspended only reads
//! `lane:*:suspended` ZSETs + `suspension:current.reason_code`, so
//! the full suspend scaffold is out of scope.
//!
//! Run with: cargo test -p ff-test --test engine_backend_list_suspended -- --test-threads=1

use std::sync::Arc;

use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionKey};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> ff_core::partition::PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

async fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config())
}

/// Seed one suspended execution in `(partition, lane)` with the
/// given `suspended_at_ms` score on the lane-suspended ZSET and
/// `reason_code` on `suspension:current`.
async fn seed_suspended(
    tc: &TestCluster,
    eid: &ExecutionId,
    lane: &LaneId,
    suspended_at_ms: i64,
    reason_code: &str,
) {
    let partition = execution_partition(eid, &test_config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);

    // Seed exec_core minimally (not strictly required by list_suspended,
    // but keeps the fixture close to the real suspended state shape).
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(ctx.core().as_str())
        .arg("execution_id")
        .arg(eid.to_string().as_str())
        .arg("lifecycle_phase")
        .arg("suspended")
        .arg("public_state")
        .arg("suspended")
        .execute()
        .await
        .unwrap();

    // Seed suspension:current with the reason_code the impl HMGETs.
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(ctx.suspension_current().as_str())
        .arg("suspension_id")
        .arg(uuid::Uuid::new_v4().to_string().as_str())
        .arg("reason_code")
        .arg(reason_code)
        .execute()
        .await
        .unwrap();

    // Add to the per-lane suspended ZSET with the given score.
    let _: i64 = tc
        .client()
        .cmd("ZADD")
        .arg(idx.lane_suspended(lane).as_str())
        .arg(suspended_at_ms.to_string().as_str())
        .arg(eid.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn list_suspended_paginates_and_populates_reason() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let fid = FlowId::new();
    let cfg = test_config();

    // Three executions in the same flow (=> same partition under
    // RFC-011 co-location), two lanes to exercise the cross-lane
    // SCAN + merge path.
    let eid_a = ExecutionId::for_flow(&fid, &cfg);
    let eid_b = ExecutionId::for_flow(&fid, &cfg);
    let eid_c = ExecutionId::for_flow(&fid, &cfg);

    let lane_1 = LaneId::new("list-susp-lane-1");
    let lane_2 = LaneId::new("list-susp-lane-2");

    // Score ordering: a (1000) < b (2000) < c (3000), so pagination
    // by limit=2 returns [a, b] then [c].
    seed_suspended(&tc, &eid_a, &lane_1, 1000, "signal").await;
    seed_suspended(&tc, &eid_b, &lane_2, 2000, "timer").await;
    seed_suspended(&tc, &eid_c, &lane_1, 3000, "children").await;

    let backend = build_backend(&tc).await;
    let partition = execution_partition(&eid_a, &cfg);
    let pkey = PartitionKey::from(&partition);

    // Page 1: first two entries ordered by score.
    let page1 = backend
        .list_suspended(pkey.clone(), None, 2)
        .await
        .expect("list_suspended page 1");
    assert_eq!(page1.entries.len(), 2, "page 1 should hold 2 entries");
    assert_eq!(page1.entries[0].execution_id, eid_a);
    assert_eq!(page1.entries[0].suspended_at_ms, 1000);
    assert_eq!(page1.entries[0].reason, "signal");
    assert_eq!(page1.entries[1].execution_id, eid_b);
    assert_eq!(page1.entries[1].suspended_at_ms, 2000);
    assert_eq!(page1.entries[1].reason, "timer");
    let cursor = page1
        .next_cursor
        .clone()
        .expect("page 1 should yield a next cursor");

    // Page 2: remaining entry.
    let page2 = backend
        .list_suspended(pkey.clone(), Some(cursor.clone()), 2)
        .await
        .expect("list_suspended page 2");
    assert_eq!(page2.entries.len(), 1, "page 2 should hold 1 entry");
    assert_eq!(page2.entries[0].execution_id, eid_c);
    assert_eq!(page2.entries[0].suspended_at_ms, 3000);
    assert_eq!(page2.entries[0].reason, "children");
    assert!(
        page2.next_cursor.is_none(),
        "page 2 should exhaust the partition, got cursor={:?}",
        page2.next_cursor
    );

    // Page 3: resuming past the last-seen execution returns empty.
    let page3 = backend
        .list_suspended(pkey, Some(page2.entries[0].execution_id.clone()), 2)
        .await
        .expect("list_suspended page 3");
    assert!(page3.entries.is_empty(), "exhausted scan should be empty");
    assert!(page3.next_cursor.is_none());
}
