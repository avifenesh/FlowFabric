//! Integration tests for `GET /v1/executions/{id}/result`.
//!
//! The endpoint reads the raw bytes stored by `ff_complete_execution` at
//! `ctx.result()`. These tests pin three guarantees that a PR#8 review
//! round caught as at risk:
//!
//!   1. **Before completion**: 404.
//!   2. **After completion**: 200 with the exact payload bytes.
//!   3. **Binary payloads**: non-UTF-8 bytes round-trip verbatim.
//!      Earlier code decoded the GET response through `String`, which
//!      rejects non-UTF-8 bytes as a decode error and surfaces as HTTP
//!      500. The current implementation reads `Option<Vec<u8>>` via the
//!      specialized `FromValue` impl for `u8` (see
//!      `ferriskey/src/value.rs` `from_byte_vec`).
//!
//! Run with: `cargo test -p ff-test --test result_api -- --test-threads=1`

use std::sync::Arc;

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use reqwest::StatusCode;
use tokio::task::AbortHandle;

const LANE: &str = "rapi-lane";
const NS: &str = "rapi-ns";
const WORKER: &str = "rapi-worker";
const WORKER_INST: &str = "rapi-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

// ── HTTP harness (mirrors pending_waitpoints_api.rs) ────────────────────

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

impl TestApi {
    async fn setup() -> Self {
        let tc = TestCluster::connect().await;
        tc.cleanup().await;

        let config = test_server_config();
        let server = ff_server::server::Server::start(config)
            .await
            .expect("Server::start failed");
        let server = Arc::new(server);
        let app = ff_server::api::router(server, &["*".to_owned()], None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
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

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

fn test_server_config() -> ff_server::config::ServerConfig {
    let pc = ff_test::fixtures::TEST_PARTITION_CONFIG;
    let host = std::env::var("FF_HOST").unwrap_or_else(|_| "localhost".into());
    let port: u16 = std::env::var("FF_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6379);
    ff_server::config::ServerConfig {
        host,
        port,
        tls: ff_test::fixtures::env_flag("FF_TLS"),
        cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
        partition_config: pc,
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: pc,
            lanes: vec![LaneId::new(LANE)],
            ..Default::default()
        },
        skip_library_load: true,
        cors_origins: vec!["*".to_owned()],
        api_token: None,
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
    }
}

// ── FCALL helpers — copy the minimal create/claim path from
//    pending_waitpoints_api.rs. Complete takes raw bytes so we can
//    exercise the binary case.

async fn fcall_create_and_claim(
    tc: &TestCluster,
    eid: &ExecutionId,
) -> (String, String, String) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    // create_execution (KEYS 8, ARGV 13)
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
        "rapi".to_owned(),
        "0".to_owned(),
        "rapi-runner".to_owned(),
        "{}".to_owned(),
        r#"{"test":true}"#.to_owned(),
        String::new(),
        String::new(),
        "{}".to_owned(),
        String::new(),
        partition.index.to_string(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_create_execution", &kr, &ar)
        .await
        .expect("ff_create_execution");

    // issue_claim_grant (KEYS 3, ARGV 9)
    let grant_keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
    ];
    let grant_args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        "5000".to_owned(),
        String::new(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = grant_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = grant_args.iter().map(String::as_str).collect();
    let _: Value = tc
        .client()
        .fcall("ff_issue_claim_grant", &kr, &ar)
        .await
        .expect("ff_issue_claim_grant");

    // claim_execution (KEYS 14, ARGV 12)
    let lease_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let att_idx = AttemptIndex::new(0);
    let lease_ttl_ms: u64 = 30_000;
    let renew_before = lease_ttl_ms * 2 / 3;
    let claim_keys: Vec<String> = vec![
        ctx.core(),
        ctx.claim_grant(),
        idx.lane_eligible(&lane_id),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        ctx.attempt_hash(att_idx),
        ctx.attempt_usage(att_idx),
        ctx.attempt_policy(att_idx),
        ctx.attempts(),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&lane_id),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];
    let claim_args: Vec<String> = vec![
        eid.to_string(),
        WORKER.to_owned(),
        WORKER_INST.to_owned(),
        LANE.to_owned(),
        String::new(),
        lease_id.clone(),
        lease_ttl_ms.to_string(),
        renew_before.to_string(),
        attempt_id.clone(),
        "{}".to_owned(),
        String::new(),
        String::new(),
    ];
    let kr: Vec<&str> = claim_keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = claim_args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_claim_execution", &kr, &ar)
        .await
        .expect("ff_claim_execution");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("claim: expected Array"),
    };
    let epoch = match arr.get(3) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::Int(n))) => n.to_string(),
        _ => panic!("claim: no epoch"),
    };
    (lease_id, epoch, attempt_id)
}

/// Call ff_complete_execution with an arbitrary byte payload. Each ARGV
/// entry is raw bytes via `Vec<u8>: ToArgs`, so non-UTF-8 result bytes
/// reach Valkey unchanged.
async fn fcall_complete_with_payload(
    tc: &TestCluster,
    eid: &ExecutionId,
    lease_id: &str,
    epoch: &str,
    attempt_id: &str,
    payload: &[u8],
) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);
    let att_idx = AttemptIndex::new(0);

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(att_idx),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        idx.lane_terminal(&lane_id),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lane_active(&lane_id),
        ctx.stream_meta(att_idx),
        ctx.result(),
        idx.attempt_timeout(),
        idx.execution_deadline(),
    ];

    // ARGV (5): execution_id, lease_id, lease_epoch, attempt_id,
    //           result_payload.
    // Slice of Vec<u8> so every entry is sent as raw bytes — essential
    // for the binary-payload case.
    let args_bytes: Vec<Vec<u8>> = vec![
        eid.to_string().into_bytes(),
        lease_id.as_bytes().to_vec(),
        epoch.as_bytes().to_vec(),
        attempt_id.as_bytes().to_vec(),
        payload.to_vec(),
    ];
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();

    let raw: Value = tc
        .client()
        .fcall("ff_complete_execution", &key_refs, &args_bytes)
        .await
        .expect("ff_complete_execution");
    match &raw {
        Value::Array(a) => {
            let status = match a.first() {
                Some(Ok(Value::Int(n))) => *n,
                _ => panic!("complete: no status"),
            };
            assert_eq!(status, 1, "complete should succeed: {raw:?}");
        }
        _ => panic!("complete: expected Array"),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

/// Before the worker completes, /result is 404.
#[tokio::test]
#[serial_test::serial]
async fn test_result_404_before_completion() {
    let api = TestApi::setup().await;
    let tc = TestCluster::connect().await;

    let eid = ExecutionId::new();
    let _ = fcall_create_and_claim(&tc, &eid).await;

    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}/result")))
        .send()
        .await
        .expect("result request failed");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// After completion with JSON bytes, /result returns the exact bytes
/// with application/octet-stream content type.
#[tokio::test]
#[serial_test::serial]
async fn test_result_200_json_payload_byte_exact() {
    let api = TestApi::setup().await;
    let tc = TestCluster::connect().await;

    let eid = ExecutionId::new();
    let (lease_id, epoch, attempt_id) = fcall_create_and_claim(&tc, &eid).await;
    let payload = br#"{"status":"ok","value":42}"#;
    fcall_complete_with_payload(&tc, &eid, &lease_id, &epoch, &attempt_id, payload).await;

    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}/result")))
        .send()
        .await
        .expect("result request failed");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/octet-stream")
    );
    let body = resp.bytes().await.expect("read body");
    assert_eq!(
        body.as_ref(),
        payload,
        "response bytes must match the completion payload exactly"
    );
}

/// Non-UTF-8 binary payload round-trips verbatim.
///
/// Before the binary-safe fix, `Server::get_execution_result` decoded
/// the Valkey GET reply through `Option<String>`. ferriskey's `String`
/// deserializer rejects invalid UTF-8 with a decode error, surfacing as
/// an HTTP 500 — any bincode-encoded f32 vector or compressed artifact
/// completing a stage would have been unreachable through this
/// endpoint. This test locks in the fix.
#[tokio::test]
#[serial_test::serial]
async fn test_result_binary_payload_roundtrips() {
    let api = TestApi::setup().await;
    let tc = TestCluster::connect().await;

    let eid = ExecutionId::new();
    let (lease_id, epoch, attempt_id) = fcall_create_and_claim(&tc, &eid).await;

    // Deliberately include bytes that are not valid UTF-8:
    //   * 0xFF, 0xFE are stand-alone invalid start bytes
    //   * 0x00 is valid UTF-8 but a common bincode framing byte
    //   * 0xC0 0x80 is the overlong NUL that strict UTF-8 rejects
    let mut payload: Vec<u8> = Vec::new();
    payload.extend_from_slice(&[0xFF, 0xFE, 0x00, 0xC0, 0x80, 0x7F, 0x80, 0xA0]);
    // Pad to a realistic embedding-ish size (4 * 384 = 1536 bytes).
    for i in 0..1536u32 {
        payload.push((i & 0xFF) as u8);
    }

    fcall_complete_with_payload(&tc, &eid, &lease_id, &epoch, &attempt_id, &payload).await;

    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}/result")))
        .send()
        .await
        .expect("result request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "binary payloads must not 500 through the endpoint"
    );
    let body = resp.bytes().await.expect("read body");
    assert_eq!(body.len(), payload.len(), "body length mismatch");
    assert_eq!(
        body.as_ref(),
        payload.as_slice(),
        "binary payload must round-trip byte-exact"
    );
}
