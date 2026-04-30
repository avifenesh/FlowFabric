//! Integration coverage for
//! [`ff_core::engine_backend::EngineBackend::list_lanes`] on the
//! Valkey-backed impl (issue #184).
//!
//! Lanes are a global SET (`ff:idx:lanes`) rather than a
//! partition-scoped index. The test seeds the SET directly via
//! `SADD`, exercises the trait + SDK wrapper, and verifies:
//!   * sort order (lexicographic by lane name),
//!   * cursor-based pagination (exclusive lower bound),
//!   * final-page `next_cursor == None`,
//!   * `limit == 0` short-circuit,
//!   * empty registry → empty page.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_list_lanes \
//!       -- --test-threads=1

use std::sync::Arc;

use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::LaneId;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config())
}

async fn seed_lanes(tc: &TestCluster, lanes: &[&str]) {
    if lanes.is_empty() {
        return;
    }
    let members: Vec<String> = lanes.iter().map(|s| (*s).to_owned()).collect();
    let _: i64 = tc
        .client()
        .cmd("SADD")
        .arg("ff:idx:lanes")
        .arg(members)
        .execute()
        .await
        .expect("SADD ff:idx:lanes");
}

async fn build_sdk_worker() -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: Some(ff_test::fixtures::backend_config_from_env()),
        worker_id: ff_core::types::WorkerId::new("list-lanes-worker"),
        worker_instance_id: ff_core::types::WorkerInstanceId::new("list-lanes-inst"),
        namespace: ff_core::types::Namespace::new("list-lanes-ns"),
        lanes: vec![LaneId::new("default")],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    partition_config: None,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn list_lanes_paginates_sorted_registry() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    // Seed 5 lanes in non-sorted order. Expected sort:
    //   ["alpha", "bravo", "charlie", "delta", "echo"]
    seed_lanes(&tc, &["charlie", "alpha", "echo", "bravo", "delta"]).await;

    let backend = build_backend(&tc);

    // Page 1 — limit 2, no cursor ⇒ ["alpha", "bravo"], next=Some("bravo").
    let p1 = backend.list_lanes(None, 2).await.expect("list_lanes p1");
    assert_eq!(
        p1.lanes,
        vec![LaneId::new("alpha"), LaneId::new("bravo")],
        "page 1 sort order wrong"
    );
    assert_eq!(p1.next_cursor, Some(LaneId::new("bravo")));

    // Page 2 — continue from "bravo" ⇒ ["charlie", "delta"], next=Some("delta").
    let p2 = backend
        .list_lanes(p1.next_cursor.clone(), 2)
        .await
        .expect("list_lanes p2");
    assert_eq!(
        p2.lanes,
        vec![LaneId::new("charlie"), LaneId::new("delta")]
    );
    assert_eq!(p2.next_cursor, Some(LaneId::new("delta")));

    // Page 3 — continue from "delta" ⇒ ["echo"], final page.
    let p3 = backend
        .list_lanes(p2.next_cursor.clone(), 2)
        .await
        .expect("list_lanes p3");
    assert_eq!(p3.lanes, vec![LaneId::new("echo")]);
    assert_eq!(p3.next_cursor, None, "last page must have no next_cursor");

    // Over-limit returns full set in one page with no next_cursor.
    let full = backend.list_lanes(None, 100).await.expect("list_lanes full");
    assert_eq!(full.lanes.len(), 5);
    assert_eq!(full.next_cursor, None);
    // Still sorted.
    let mut expected = full.lanes.clone();
    expected.sort();
    assert_eq!(full.lanes, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_lanes_empty_registry_returns_empty_page() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let backend = build_backend(&tc);
    let page = backend
        .list_lanes(None, 10)
        .await
        .expect("list_lanes empty");
    assert!(page.lanes.is_empty());
    assert_eq!(page.next_cursor, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_lanes_limit_zero_short_circuits() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_lanes(&tc, &["alpha", "bravo"]).await;

    let backend = build_backend(&tc);
    let page = backend.list_lanes(None, 0).await.expect("list_lanes zero");
    assert!(page.lanes.is_empty());
    assert_eq!(page.next_cursor, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_lanes_sdk_wrapper_matches_trait() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_lanes(&tc, &["a", "b", "c"]).await;

    let worker = build_sdk_worker().await;
    let page = worker.list_lanes(None, 10).await.expect("sdk list_lanes");
    assert_eq!(
        page.lanes,
        vec![LaneId::new("a"), LaneId::new("b"), LaneId::new("c")]
    );
    assert_eq!(page.next_cursor, None);
}
