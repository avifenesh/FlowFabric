//! Integration test for [`ff_sdk::SharedFfBackend`] (issue #174).
//!
//! Spins two [`ff_sdk::FlowFabricWorker`] instances from a single
//! `SharedFfBackend::connect(..)` factory and asserts that they share
//! the same `Arc<ValkeyBackend>` allocation by inspecting
//! [`Arc::strong_count`] on the concrete backend handle the factory
//! hands out.
//!
//! Run with:
//!   cargo test -p ff-test --test shared_ff_backend -- --test-threads=1

use std::sync::Arc;

use ff_core::types::*;
use ff_sdk::{SharedFfBackend, WorkerConfig};
use ff_test::fixtures::TestCluster;

const NS: &str = "shared-ff-ns";

fn worker_config(suffix: &str, lane: &str) -> WorkerConfig {
    WorkerConfig {
        backend: ff_test::fixtures::backend_config_from_env(),
        worker_id: WorkerId::new(format!("shared-ff-w-{suffix}")),
        worker_instance_id: WorkerInstanceId::new(format!("shared-ff-inst-{suffix}")),
        namespace: Namespace::new(NS),
        lanes: vec![LaneId::new(lane)],
        capabilities: Vec::new(),
        lease_ttl_ms: 30_000,
        claim_poll_interval_ms: 100,
        max_concurrent_tasks: 1,
    }
}

/// The factory mints two workers that both hold the same
/// `Arc<ValkeyBackend>` — `strong_count` climbs with each
/// `.worker(..)` call and the workers' backend trait-objects are
/// pointer-equal to the factory's concrete handle.
#[tokio::test]
async fn shared_backend_is_reused_across_workers() {
    // Pre-seed the partition config so `ValkeyBackend::connect_concrete`
    // does not fall back to `PartitionConfig::default()` with a warn log.
    // Uses the same HSET seed the other engine-backend-* tests rely on.
    let _tc = TestCluster::connect().await;
    let cfg = ff_test::fixtures::TEST_PARTITION_CONFIG;
    let _: () = _tc
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

    let shared = SharedFfBackend::connect(ff_test::fixtures::backend_config_from_env())
        .await
        .expect("SharedFfBackend::connect");

    // Baseline: factory holds one strong reference.
    assert_eq!(
        Arc::strong_count(shared.backend_arc()),
        1,
        "freshly-connected factory should own the only strong ref"
    );

    let w1 = shared
        .worker(worker_config("a", "lane-a"))
        .await
        .expect("shared.worker(a)");
    // After one worker: factory + engine-view + completion-view = 3.
    // (FlowFabricWorker stores two Arc<dyn ..> trait-object views
    // onto the same underlying ValkeyBackend allocation.)
    assert_eq!(
        Arc::strong_count(shared.backend_arc()),
        3,
        "one worker adds two trait-object views onto the shared Arc"
    );

    let w2 = shared
        .worker(worker_config("b", "lane-b"))
        .await
        .expect("shared.worker(b)");
    // Two workers: factory + 2 × (engine-view + completion-view) = 5.
    assert_eq!(
        Arc::strong_count(shared.backend_arc()),
        5,
        "second worker adds two more trait-object views onto the same Arc"
    );

    // Sanity: both workers see a backend trait-object and they both
    // exist (we never `.take()` them away). The strong-count check
    // above is the load-bearing assertion; the following lines just
    // exercise the accessors so a future refactor that silently drops
    // backend storage on one path surfaces here.
    assert!(w1.backend().is_some(), "worker-a exposes a backend");
    assert!(w2.backend().is_some(), "worker-b exposes a backend");
    assert!(
        w1.completion_backend().is_some(),
        "worker-a exposes a completion backend"
    );
    assert!(
        w2.completion_backend().is_some(),
        "worker-b exposes a completion backend"
    );

    // Drop one worker; strong_count falls by 2 (engine + completion).
    drop(w1);
    assert_eq!(
        Arc::strong_count(shared.backend_arc()),
        3,
        "dropping one worker releases its two trait-object views"
    );

    drop(w2);
    assert_eq!(
        Arc::strong_count(shared.backend_arc()),
        1,
        "dropping all workers returns us to the factory-only baseline"
    );
}
