//! Trait-boundary + HTTP integration tests for
//! [`ff_core::engine_backend::EngineBackend::list_executions`] on the
//! Valkey-backed impl, plus the reshaped `GET /v1/executions` cursor
//! endpoint introduced by issue #182.
//!
//! Covers:
//!
//! 1. Page threading across 3 pages at `limit=5` over 15 executions —
//!    the forward-only cursor returns `next_cursor=Some` twice, then
//!    `None` on the tail page.
//! 2. `next_cursor == None` when the partition holds fewer than
//!    `limit` members.
//! 3. HTTP endpoint (`GET /v1/executions?partition=N&cursor=...&limit=N`)
//!    returns the same page shape as the trait.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_list_executions \
//!       -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::contracts::ListExecutionsPage;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{PartitionConfig, PartitionKey, execution_partition};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::task::AbortHandle;

const LANE: &str = "list-execs-lane";
const NS: &str = "list-execs-ns";

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

fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config())
}

/// Seed `n` executions all colocated to the same flow partition so
/// they land on a single `all_executions` SET. Returns the sorted
/// (lex-ascending) `ExecutionId` list so tests can assert page
/// contents.
async fn seed_executions(tc: &TestCluster, flow_id: &FlowId, n: usize) -> Vec<ExecutionId> {
    let config = test_config();
    let mut ids: Vec<ExecutionId> = Vec::with_capacity(n);
    for _ in 0..n {
        let eid = ExecutionId::for_flow(flow_id, &config);
        create_execution(tc, &eid).await;
        ids.push(eid);
    }
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    ids
}

async fn create_execution(tc: &TestCluster, eid: &ExecutionId) {
    let config = test_config();
    let partition = execution_partition(eid, &config);
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);

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
        LANE.to_owned(),
        "list_execs_kind".to_owned(),
        "0".to_owned(),
        "list-execs-test".to_owned(),
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
}

fn partition_key_for(flow_id: &FlowId) -> PartitionKey {
    let partition = ff_core::partition::flow_partition(flow_id, &test_config());
    PartitionKey::from(&partition)
}

fn partition_index_for(flow_id: &FlowId) -> u16 {
    ff_core::partition::flow_partition(flow_id, &test_config()).index
}

// ─── Trait-boundary tests ───

/// Paginating 15 executions at limit=5 yields 3 pages with
/// `next_cursor` threaded correctly. Final page has `next_cursor=None`.
#[tokio::test]
#[serial_test::serial]
async fn list_executions_paginates_15_at_limit_5() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    let seeded = seed_executions(&tc, &fid, 15).await;
    let partition_key = partition_key_for(&fid);

    // Page 1: cursor=None, limit=5.
    let page1 = backend
        .list_executions(partition_key.clone(), None, 5)
        .await
        .expect("page1");
    assert_eq!(page1.executions.len(), 5, "page1 has 5 members");
    assert_eq!(&page1.executions, &seeded[0..5]);
    assert_eq!(
        page1.next_cursor.as_ref(),
        Some(&seeded[4]),
        "page1 next_cursor is last returned id"
    );

    // Page 2: cursor = page1.next_cursor.
    let page2 = backend
        .list_executions(partition_key.clone(), page1.next_cursor.clone(), 5)
        .await
        .expect("page2");
    assert_eq!(page2.executions.len(), 5, "page2 has 5 members");
    assert_eq!(&page2.executions, &seeded[5..10]);
    assert_eq!(page2.next_cursor.as_ref(), Some(&seeded[9]));

    // Page 3: cursor = page2.next_cursor.
    let page3 = backend
        .list_executions(partition_key.clone(), page2.next_cursor.clone(), 5)
        .await
        .expect("page3");
    assert_eq!(page3.executions.len(), 5, "page3 has 5 members");
    assert_eq!(&page3.executions, &seeded[10..15]);
    assert_eq!(
        page3.next_cursor, None,
        "page3 is final — next_cursor must be None"
    );

    // Page 4: cursor past end yields empty page + None.
    let page4 = backend
        .list_executions(partition_key, Some(seeded[14].clone()), 5)
        .await
        .expect("page4");
    assert!(page4.executions.is_empty());
    assert_eq!(page4.next_cursor, None);
}

/// Empty partition returns an empty page with `next_cursor = None`.
#[tokio::test]
#[serial_test::serial]
async fn list_executions_empty_partition_returns_empty_page() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let backend = build_backend(&tc);

    // Synthesise a partition key for an empty partition.
    let partition_key = PartitionKey::from(&ff_core::partition::Partition {
        family: ff_core::partition::PartitionFamily::Flow,
        index: 0,
    });
    let page = backend
        .list_executions(partition_key, None, 10)
        .await
        .expect("empty partition list");
    assert!(page.executions.is_empty());
    assert_eq!(page.next_cursor, None);
}

/// `limit == 0` is a legitimate no-op probe; returns empty page +
/// `next_cursor=None` without hitting Valkey's all_executions set.
#[tokio::test]
#[serial_test::serial]
async fn list_executions_limit_zero_returns_empty_page() {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;
    let backend = build_backend(&tc);

    let fid = FlowId::new();
    seed_executions(&tc, &fid, 3).await;
    let partition_key = partition_key_for(&fid);

    let page = backend
        .list_executions(partition_key, None, 0)
        .await
        .expect("limit=0");
    assert!(page.executions.is_empty());
    assert_eq!(page.next_cursor, None);
}

// ─── HTTP integration ───

struct TestApi {
    client: reqwest::Client,
    base_url: String,
    abort_handle: AbortHandle,
}

impl Drop for TestApi {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

fn test_server_config() -> ff_server::config::ServerConfig {

    let config = ff_test::fixtures::TEST_PARTITION_CONFIG;
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    ff_server::config::ServerConfig {
        valkey: ff_server::config::ValkeyServerConfig {
            host,
            port,
            tls: ff_test::fixtures::env_flag("FF_TLS"),
            cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
            skip_library_load: true,
        },
        partition_config: config,
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: config,
            lanes: vec![LaneId::new(LANE)],
            ..Default::default()
        },

        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
        backend: ff_server::config::BackendKind::default(),
        postgres: Default::default(),
    }
}

async fn setup_api() -> TestApi {
    let tc = TestCluster::connect().await;
    tc.cleanup().await;
    seed_partition_config(&tc).await;

    let server = ff_server::server::Server::start(test_server_config())
        .await
        .expect("Server::start");
    let server = Arc::new(server);
    let app = ff_server::api::router(server.clone(), &["*".to_owned()], None)
        .expect("router builds");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    TestApi {
        client: reqwest::Client::new(),
        base_url,
        abort_handle: handle.abort_handle(),
    }
}

#[derive(Deserialize, Debug)]
struct HttpPage {
    executions: Vec<ExecutionId>,
    next_cursor: Option<ExecutionId>,
}

/// HTTP endpoint paginates the same way as the trait.
#[tokio::test]
#[serial_test::serial]
async fn list_executions_http_paginates_15_at_limit_5() {
    let api = setup_api().await;
    let tc = TestCluster::connect().await;

    let fid = FlowId::new();
    let seeded = seed_executions(&tc, &fid, 15).await;
    let partition = partition_index_for(&fid);

    // Page 1.
    let url = format!("{}/v1/executions?partition={}&limit=5", api.base_url, partition);
    let resp = api.client.get(&url).send().await.expect("GET page1");
    assert_eq!(resp.status(), StatusCode::OK, "page1 status");
    let page1: HttpPage = resp.json().await.expect("json page1");
    assert_eq!(page1.executions.len(), 5);
    assert_eq!(&page1.executions, &seeded[0..5]);
    let cursor = page1
        .next_cursor
        .as_ref()
        .expect("page1 has next_cursor")
        .clone();

    // Page 2.
    let url = format!(
        "{}/v1/executions?partition={}&limit=5&cursor={}",
        api.base_url, partition, cursor
    );
    let resp = api.client.get(&url).send().await.expect("GET page2");
    let page2: HttpPage = resp.json().await.expect("json page2");
    assert_eq!(page2.executions.len(), 5);
    assert_eq!(&page2.executions, &seeded[5..10]);
    let cursor = page2.next_cursor.expect("page2 has next_cursor");

    // Page 3.
    let url = format!(
        "{}/v1/executions?partition={}&limit=5&cursor={}",
        api.base_url, partition, cursor
    );
    let resp = api.client.get(&url).send().await.expect("GET page3");
    let page3: HttpPage = resp.json().await.expect("json page3");
    assert_eq!(page3.executions.len(), 5);
    assert_eq!(&page3.executions, &seeded[10..15]);
    assert!(
        page3.next_cursor.is_none(),
        "final page's next_cursor must be null"
    );
}

/// Invalid cursor string surfaces as HTTP 400.
#[tokio::test]
#[serial_test::serial]
async fn list_executions_http_rejects_malformed_cursor() {
    let api = setup_api().await;
    let url = format!(
        "{}/v1/executions?partition=0&limit=5&cursor=not-an-execution-id",
        api.base_url
    );
    let resp = api.client.get(&url).send().await.expect("GET malformed");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// Keep an unused-import guard explicit so a future refactor that
// accidentally drops `ListExecutionsPage` from the public API surface
// fails this compile-time witness rather than only the runtime tests.
#[allow(dead_code)]
fn _witness_shape(_p: ListExecutionsPage) {}
