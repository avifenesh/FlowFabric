//! Integration coverage for
//! [`ff_core::engine_backend::EngineBackend::list_flows`] (issue #185)
//! on the Valkey-backed impl.
//!
//! Creates a pile of flows, bins them by partition, picks a partition
//! that lands at least five flows, and exercises the cursor-based
//! pagination contract: first page, second page, terminal cursor.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_list_flows \
//!       -- --test-threads=1

use std::collections::HashMap;

use ferriskey::Value;
use ff_core::contracts::{FlowStatus, FlowSummary};
use ff_core::keys::{FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{flow_partition, PartitionConfig, PartitionKey};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

const LANE: &str = "list-flows-lane";
const NS: &str = "list-flows-ns";

async fn seed_partition_config(tc: &TestCluster) {
    let cfg = test_config();
    let _: () = tc
        .client()
        .cmd("HSET")
        .arg("ff:config:partitions")
        .arg("num_flow_partitions")
        .arg(cfg.num_flow_partitions.to_string().as_str())
        .arg("num_budget_partitions")
        .arg(cfg.num_budget_partitions.to_string().as_str())
        .arg("num_quota_partitions")
        .arg(cfg.num_quota_partitions.to_string().as_str())
        .execute()
        .await
        .unwrap();
}

async fn build_worker(suffix: &str) -> ff_sdk::FlowFabricWorker {
    let cfg = ff_sdk::WorkerConfig {
        backend: ff_test::fixtures::backend_config_from_env(),
        worker_id: WorkerId::new(format!("list-flows-worker-{suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("list-flows-inst-{suffix}")),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(LANE)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    };
    ff_sdk::FlowFabricWorker::connect(cfg).await.unwrap()
}

async fn create_flow(tc: &TestCluster, fid: &FlowId, flow_kind: &str) {
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![ctx.core(), ctx.members(), fidx.flow_index()];
    let now_ms = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        fid.to_string(),
        flow_kind.to_owned(),
        NS.to_owned(),
        now_ms,
    ];
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_flow", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_create_flow failed");
}

async fn cancel_flow_raw(tc: &TestCluster, fid: &FlowId) {
    let config = test_config();
    let partition = flow_partition(fid, &config);
    let ctx = FlowKeyContext::new(&partition, fid);
    let fidx = FlowIndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.members(),
        fidx.flow_index(),
        ctx.pending_cancels(),
        fidx.cancel_backlog(),
    ];
    let now_ms = TimestampMs::now().0.to_string();
    let args: Vec<String> = vec![
        fid.to_string(),
        "operator_requested".to_owned(),
        "cancel_all".to_owned(),
        now_ms,
        String::new(),
    ];
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_cancel_flow", &key_refs, &arg_refs)
        .await
        .expect("FCALL ff_cancel_flow failed");
}

/// Create flows until at least `want` of them land on a single
/// partition; return `(partition_key, flow_ids_on_that_partition)`.
async fn seed_crowded_partition(
    tc: &TestCluster,
    want: usize,
) -> (PartitionKey, Vec<FlowId>) {
    let cfg = test_config();
    let mut by_partition: HashMap<u16, Vec<FlowId>> = HashMap::new();
    // With num_flow_partitions=4 and uniform UUID hashing, ~25% of
    // flows land on each partition. Seeding 48 flows gives us ~12 per
    // partition in expectation — comfortably past `want = 5`.
    for _ in 0..48 {
        let fid = FlowId::new();
        let part = flow_partition(&fid, &cfg);
        create_flow(tc, &fid, "dag").await;
        by_partition.entry(part.index).or_default().push(fid);
    }
    let (index, fids) = by_partition
        .into_iter()
        .filter(|(_, v)| v.len() >= want)
        .max_by_key(|(_, v)| v.len())
        .expect("at least one partition should have enough flows");
    let part = ff_core::partition::Partition {
        family: ff_core::partition::PartitionFamily::Flow,
        index,
    };
    (PartitionKey::from(&part), fids)
}

/// Drain the full partition listing via `list_flows`, following the
/// cursor to completion. Returns the concatenated rows.
async fn drain(
    worker: &ff_sdk::FlowFabricWorker,
    partition: &PartitionKey,
    limit: usize,
) -> Vec<FlowSummary> {
    let mut out: Vec<FlowSummary> = Vec::new();
    let mut cursor: Option<FlowId> = None;
    loop {
        let page = worker
            .list_flows(partition.clone(), cursor.clone(), limit)
            .await
            .expect("list_flows ok");
        out.extend(page.flows.into_iter());
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
        // Safety belt: detect a broken cursor that fails to advance.
        assert!(out.len() < 1000, "list_flows paging failed to terminate");
    }
    out
}

// ─── Tests ───

#[tokio::test]
async fn list_flows_pages_all_seeded_rows() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("drain").await;

    let (partition, seeded) = seed_crowded_partition(&tc, 5).await;

    // Drain with a small page so at least two iterations happen.
    let drained = drain(&worker, &partition, 3).await;
    assert_eq!(
        drained.len(),
        seeded.len(),
        "drained count must match seeded count on the partition"
    );

    // Every seeded flow must appear exactly once.
    let drained_set: std::collections::HashSet<FlowId> =
        drained.iter().map(|f| f.flow_id.clone()).collect();
    let seeded_set: std::collections::HashSet<FlowId> = seeded.into_iter().collect();
    assert_eq!(drained_set, seeded_set);

    // Byte-lexicographic sort must hold across the full listing.
    let ids: Vec<&FlowId> = drained.iter().map(|f| &f.flow_id).collect();
    for pair in ids.windows(2) {
        assert!(
            pair[0].as_bytes() < pair[1].as_bytes(),
            "list_flows must return rows in UUID byte-lexicographic order"
        );
    }

    // Summary shape: created_at is populated, status is Active for
    // freshly-created flows.
    for row in &drained {
        assert!(row.created_at.0 > 0, "created_at must be populated");
        assert_eq!(
            row.status,
            FlowStatus::Active,
            "fresh flows project to FlowStatus::Active"
        );
    }
}

#[tokio::test]
async fn list_flows_cursor_is_exclusive_and_advances() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("cursor").await;

    let (partition, _seeded) = seed_crowded_partition(&tc, 5).await;

    let page1 = worker
        .list_flows(partition.clone(), None, 3)
        .await
        .expect("list_flows page 1");
    assert_eq!(page1.flows.len(), 3);
    assert!(
        page1.next_cursor.is_some(),
        "page 1 must surface a cursor when more rows exist"
    );
    let cursor = page1.next_cursor.clone().unwrap();
    // Cursor is the last row on the page.
    assert_eq!(
        &cursor,
        &page1.flows.last().unwrap().flow_id,
        "next_cursor is the last returned flow_id"
    );

    let page2 = worker
        .list_flows(partition.clone(), Some(cursor.clone()), 3)
        .await
        .expect("list_flows page 2");
    // No overlap with page 1 (cursor is exclusive).
    let page1_ids: Vec<FlowId> = page1.flows.iter().map(|f| f.flow_id.clone()).collect();
    for row in &page2.flows {
        assert!(
            !page1_ids.contains(&row.flow_id),
            "page 2 row {} must not appear in page 1",
            row.flow_id
        );
        assert!(
            row.flow_id.as_bytes() > cursor.as_bytes(),
            "page 2 rows must be strictly greater than the cursor"
        );
    }
}

#[tokio::test]
async fn list_flows_surfaces_cancelled_status() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("cancelled").await;

    // Create a single flow and cancel it. Use its own partition so we
    // don't need to seed a crowded one.
    let fid = FlowId::new();
    create_flow(&tc, &fid, "dag").await;
    cancel_flow_raw(&tc, &fid).await;

    let cfg = test_config();
    let part = flow_partition(&fid, &cfg);
    let pk: PartitionKey = (&part).into();

    let page = worker
        .list_flows(pk, None, 100)
        .await
        .expect("list_flows ok");
    let row = page
        .flows
        .iter()
        .find(|f| f.flow_id == fid)
        .expect("cancelled flow must appear in listing");
    assert_eq!(row.status, FlowStatus::Cancelled);
}

#[tokio::test]
async fn list_flows_empty_partition_returns_empty() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let worker = build_worker("empty").await;

    // A partition we haven't written anything to. Pick partition 0 —
    // cleanup guarantees it's empty.
    let part = ff_core::partition::Partition {
        family: ff_core::partition::PartitionFamily::Flow,
        index: 0,
    };
    let pk: PartitionKey = (&part).into();

    let page = worker
        .list_flows(pk, None, 10)
        .await
        .expect("list_flows ok");
    assert!(page.flows.is_empty());
    assert!(page.next_cursor.is_none());
}
