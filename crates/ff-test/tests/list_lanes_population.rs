//! Regression coverage for issue #203 — the global lanes registry
//! (`ff:idx:lanes`) must be populated by both the server-boot seed
//! (ServerConfig.lanes) AND the Lua `ff_create_execution` first-sight
//! SADD. Before this fix, `EngineBackend::list_lanes` returned an
//! empty page on a freshly-booted FlowFabric even after executions
//! had been submitted, because nothing wrote to `ff:idx:lanes`.
//!
//! Run with:
//!   cargo test -p ff-test --test list_lanes_population \
//!       -- --test-threads=1

use std::collections::HashSet;
use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{PartitionConfig, execution_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

const NS: &str = "list-lanes-pop-ns";

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

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

fn server_config_with_lanes(lanes: Vec<LaneId>) -> ff_server::config::ServerConfig {
    let config = test_config();
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    let tls = ff_test::fixtures::env_flag("FF_TLS");
    let cluster = ff_test::fixtures::env_flag("FF_CLUSTER");
    ff_server::config::ServerConfig {
        host,
        port,
        tls,
        cluster,
        partition_config: config,
        lanes: lanes.clone(),
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: config,
            lanes,
            ..Default::default()
        },
        // `skip_library_load=true` is honoured by other ff-test tests; the
        // TestCluster fixture pre-loads the library. The lanes-index seed
        // runs unconditionally regardless of this flag (it does not depend
        // on the Lua library).
        skip_library_load: true,
        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
    }
}

fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config())
}

/// Submit an execution on `lane` via the same FCALL path production uses,
/// so the first-sight SADD inside `ff_create_execution` gets exercised.
async fn create_execution_on_lane(tc: &TestCluster, lane: &str) -> ExecutionId {
    let config = test_config();
    let lane_id = LaneId::new(lane);
    let eid = ExecutionId::solo(&lane_id, &config);
    let partition = execution_partition(&eid, &config);
    let ctx = ExecKeyContext::new(&partition, &eid);
    let idx = IndexKeys::new(&partition);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.payload(),
        ctx.policy(),
        ctx.tags(),
        idx.lane_eligible(&lane_id),
        ctx.noop(),
        idx.execution_deadline(),
        idx.all_executions(),
    ];
    let args: Vec<String> = vec![
        eid.to_string(),
        NS.to_owned(),
        lane.to_owned(),
        "list_lanes_pop_kind".to_owned(),
        "0".to_owned(),
        "list-lanes-pop-test".to_owned(),
        "{}".to_owned(),
        String::new(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let ar: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("FCALL ff_create_execution");
    eid
}

/// End-to-end coverage: Server::start must seed `ff:idx:lanes` from
/// ServerConfig.lanes, AND a subsequent execution on a lane NOT in that
/// list must be added by the Lua first-sight SADD.
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial]
async fn list_lanes_populated_by_server_boot_and_lua_first_sight() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;

    let seeded = vec![
        LaneId::new("alpha"),
        LaneId::new("beta"),
        LaneId::new("gamma"),
    ];
    let config = server_config_with_lanes(seeded.clone());

    // Boot the server. Post-start, `ff:idx:lanes` must hold exactly the
    // configured 3 lanes.
    let _server = ff_server::server::Server::start(config)
        .await
        .expect("Server::start");

    let backend = build_backend(&tc);

    // Assert #1 — configured lanes are visible immediately after boot.
    let page = backend
        .list_lanes(None, 100)
        .await
        .expect("list_lanes after boot");
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        page.lanes.len(),
        3,
        "expected 3 seeded lanes, got {:?}",
        page.lanes
    );
    let lane_set: HashSet<&str> = page.lanes.iter().map(|l| l.as_str()).collect();
    assert!(lane_set.contains("alpha"));
    assert!(lane_set.contains("beta"));
    assert!(lane_set.contains("gamma"));

    // Assert #2 — submitting an execution on a lane NOT in the configured
    // set adds that lane to the registry via Lua first-sight SADD.
    let _eid = create_execution_on_lane(&tc, "delta").await;

    let page2 = backend
        .list_lanes(None, 100)
        .await
        .expect("list_lanes after create");
    assert_eq!(page2.next_cursor, None);
    assert_eq!(
        page2.lanes.len(),
        4,
        "expected 4 lanes after create, got {:?}",
        page2.lanes
    );
    let lane_set2: HashSet<&str> = page2.lanes.iter().map(|l| l.as_str()).collect();
    assert!(lane_set2.contains("alpha"));
    assert!(lane_set2.contains("beta"));
    assert!(lane_set2.contains("gamma"));
    assert!(
        lane_set2.contains("delta"),
        "Lua first-sight SADD failed to register 'delta'"
    );
}
