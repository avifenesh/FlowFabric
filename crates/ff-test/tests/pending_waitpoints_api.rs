//! Integration tests for `GET /v1/executions/{id}/pending-waitpoints`.
//!
//! Validates the API gap closure added for the media-pipeline example: a
//! human reviewer needs to authenticate its signal against a waitpoint, but
//! the HMAC token is only returned to the suspending worker. This endpoint
//! exposes it through the same bearer-auth boundary as every other REST
//! call.
//!
//! Run with: cargo test -p ff-test --test pending_waitpoints_api -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_core::keys::{ExecKeyContext, IndexKeys};
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::task::AbortHandle;

/// RFC-017 Stage E4 (v0.8.0 §8) wire DTO — direct serialisation of
/// `PendingWaitpointInfo`, no raw HMAC. Consumers correlate via
/// `(token_kid, token_fingerprint)`. Kept local to the test so
/// consumer-compatibility assertions are explicit about the wire.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PendingWaitpointWire {
    waitpoint_id: WaitpointId,
    waitpoint_key: String,
    state: String,
    #[serde(default)]
    required_signal_names: Vec<String>,
    created_at: TimestampMs,
    #[serde(default)]
    activated_at: Option<TimestampMs>,
    #[serde(default)]
    expires_at: Option<TimestampMs>,
    execution_id: ExecutionId,
    token_kid: String,
    token_fingerprint: String,
}

const LANE: &str = "pwp-lane";
const NS: &str = "pwp-ns";
const WORKER: &str = "pwp-worker";
const WORKER_INST: &str = "pwp-worker-1";

fn config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

// ── HTTP harness ────────────────────────────────────────────────────────

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
        let app = ff_server::api::router(server, &["*".to_owned()], None)
            .expect("router builds with wildcard CORS");

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
        valkey: ff_server::config::ValkeyServerConfig {
            host,
            port,
            tls: ff_test::fixtures::env_flag("FF_TLS"),
            cluster: ff_test::fixtures::env_flag("FF_CLUSTER"),
            skip_library_load: true,
        },
        partition_config: pc,
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: pc,
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

// ── FCALL helpers (self-contained — kept flat so each test reads top-down)

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
        "pwp".to_owned(),
        "0".to_owned(),
        "pwp-runner".to_owned(),
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

    // claim_execution (KEYS 14, ARGV 12) → returns lease_id, epoch, attempt_id.
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

async fn fcall_suspend(
    tc: &TestCluster,
    eid: &ExecutionId,
    lease_id: &str,
    epoch: &str,
    attempt_id: &str,
) -> (WaitpointId, String, String) {
    let partition = execution_partition(eid, &config());
    let ctx = ExecKeyContext::new(&partition, eid);
    let idx = IndexKeys::new(&partition);
    let lane_id = LaneId::new(LANE);
    let wid = WorkerInstanceId::new(WORKER_INST);

    let wp_uuid = uuid::Uuid::new_v4();
    let wp_id = WaitpointId::from_uuid(wp_uuid);
    let wp_key = format!("pwp:{wp_uuid}");

    let keys: Vec<String> = vec![
        ctx.core(),
        ctx.attempt_hash(AttemptIndex::new(0)),
        ctx.lease_current(),
        ctx.lease_history(),
        idx.lease_expiry(),
        idx.worker_leases(&wid),
        ctx.suspension_current(),
        ctx.waitpoint(&wp_id),
        ctx.waitpoint_signals(&wp_id),
        idx.suspension_timeout(),
        idx.pending_waitpoint_expiry(),
        idx.lane_active(&lane_id),
        idx.lane_suspended(&lane_id),
        ctx.waitpoints(),
        ctx.waitpoint_condition(&wp_id),
        idx.attempt_timeout(),
        idx.waitpoint_hmac_secrets(),
    ];
    let resume_condition_json = serde_json::json!({
        "condition_type": "signal_set",
        "required_signal_names": ["pwp_signal"],
        "signal_match_mode": "any",
        "minimum_signal_count": 1,
        "timeout_behavior": "fail",
        "allow_operator_override": true,
    })
    .to_string();
    let resume_policy_json = serde_json::json!({
        "resume_target": "runnable",
        "close_waitpoint_on_resume": true,
        "consume_matched_signals": true,
        "retain_signal_buffer_until_closed": true,
    })
    .to_string();

    let args: Vec<String> = vec![
        eid.to_string(),
        "0".to_owned(),
        attempt_id.to_owned(),
        lease_id.to_owned(),
        epoch.to_owned(),
        uuid::Uuid::new_v4().to_string(),
        wp_id.to_string(),
        wp_key.clone(),
        "waiting_for_signal".to_owned(),
        "worker".to_owned(),
        String::new(),
        resume_condition_json,
        resume_policy_json,
        String::new(),
        "0".to_owned(),
        "fail".to_owned(),
        "1000".to_owned(),
    ];
    let kr: Vec<&str> = keys.iter().map(String::as_str).collect();
    let ar: Vec<&str> = args.iter().map(String::as_str).collect();
    let raw: Value = tc
        .client()
        .fcall("ff_suspend_execution", &kr, &ar)
        .await
        .expect("ff_suspend_execution");
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("suspend: expected Array"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        _ => panic!("suspend: no status"),
    };
    assert_eq!(status, 1, "suspend should succeed: {raw:?}");
    let token = match arr.get(5) {
        Some(Ok(Value::BulkString(b))) => String::from_utf8_lossy(b).into_owned(),
        Some(Ok(Value::SimpleString(s))) => s.clone(),
        _ => panic!("suspend: missing waitpoint_token"),
    };
    (wp_id, wp_key, token)
}

// ── Tests ────────────────────────────────────────────────────────────────

/// Suspend an execution and verify the endpoint returns the sanitised
/// `(token_kid, token_fingerprint)` pair — the raw `waitpoint_token`
/// and `Deprecation: ff-017` header were both removed at v0.8.0.
#[tokio::test]
#[serial_test::serial]
async fn test_list_pending_waitpoints_returns_token_after_suspend() {
    let api = TestApi::setup().await;
    let tc = TestCluster::connect().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, attempt_id) = fcall_create_and_claim(&tc, &eid).await;
    let (wp_id, wp_key, _token) =
        fcall_suspend(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}/pending-waitpoints")))
        .send()
        .await
        .expect("pending-waitpoints request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // RFC-017 Stage E4 (v0.8.0 §8): no more `Deprecation: ff-017`
    // header — the legacy wire field is gone, so the deprecation
    // signal is gone too.
    assert!(
        resp.headers().get("Deprecation").is_none(),
        "v0.8.0 responses must not carry Deprecation: ff-017"
    );

    // Parse as raw JSON first so we can assert on both the *absence*
    // of `waitpoint_token` and the *presence* of the sanitised fields.
    let body: serde_json::Value = resp.json().await.expect("response body parse failed");
    let arr = body.as_array().expect("response is an array");
    assert_eq!(arr.len(), 1, "expected exactly one active waitpoint");
    assert!(
        arr[0].get("waitpoint_token").is_none(),
        "v0.8.0 wire must not carry `waitpoint_token`; got {arr:?}"
    );
    assert!(
        arr[0].get("token_kid").and_then(|v| v.as_str()).is_some(),
        "v0.8.0 wire must carry `token_kid`"
    );
    assert!(
        arr[0].get("token_fingerprint").and_then(|v| v.as_str()).is_some(),
        "v0.8.0 wire must carry `token_fingerprint`"
    );

    let list: Vec<PendingWaitpointWire> =
        serde_json::from_value(body).expect("wire DTO parse failed");
    let entry = &list[0];
    assert_eq!(entry.waitpoint_id, wp_id);
    assert_eq!(entry.waitpoint_key, wp_key);
    assert_eq!(entry.state, "active");
    assert!(entry.activated_at.is_some());
    // Sanitised fields — present on the trait boundary and the wire.
    assert!(!entry.token_kid.is_empty(), "token_kid must be populated");
    assert_eq!(
        entry.token_fingerprint.len(),
        16,
        "token_fingerprint is the first 16 hex chars of the digest"
    );
    // The fcall_suspend helper above passes
    // required_signal_names=["pwp_signal"]; the endpoint should surface
    // that so a reviewer with multiple waitpoints can pick the right
    // one.
    assert_eq!(
        entry.required_signal_names,
        vec!["pwp_signal".to_owned()],
        "required_signal_names from the resume condition should round-trip"
    );
}

/// An execution with no waitpoints returns an empty list.
#[tokio::test]
#[serial_test::serial]
async fn test_list_pending_waitpoints_empty_for_unsuspended() {
    let api = TestApi::setup().await;
    let tc = TestCluster::connect().await;

    let eid = tc.new_execution_id();
    let _ = fcall_create_and_claim(&tc, &eid).await;

    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}/pending-waitpoints")))
        .send()
        .await
        .expect("pending-waitpoints request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let list: Vec<PendingWaitpointWire> =
        resp.json().await.expect("response body parse failed");
    assert!(list.is_empty(), "expected no waitpoints, got {list:?}");
}

/// End-to-end review-CLI path: after suspend, POST /signal with a
/// tampered token must surface as HTTP 400 carrying the `invalid_token`
/// error code. Matches what `review --tamper-token` does in the
/// media-pipeline example.
#[tokio::test]
#[serial_test::serial]
async fn test_signal_delivery_with_tampered_token_rejected() {
    let api = TestApi::setup().await;
    let tc = TestCluster::connect().await;

    let eid = tc.new_execution_id();
    let (lease_id, epoch, attempt_id) = fcall_create_and_claim(&tc, &eid).await;
    let (wp_id, _wp_key, token) =
        fcall_suspend(&tc, &eid, &lease_id, &epoch, &attempt_id).await;

    // Flip the last hex char — matches review.rs's tamper().
    let mut chars: Vec<char> = token.chars().collect();
    let last = chars.last_mut().expect("token non-empty");
    *last = match *last {
        'a' => 'b',
        'A' => 'B',
        '0' => '1',
        _ => '0',
    };
    let tampered: String = chars.into_iter().collect();
    assert_ne!(tampered, token, "tamper must produce a different token");

    let body = serde_json::json!({
        "execution_id": eid,
        "waitpoint_id": wp_id,
        "signal_id": uuid::Uuid::new_v4().to_string(),
        "signal_name": "pwp_signal",
        "signal_category": "human_review",
        "source_type": "human",
        "source_identity": "reviewer-tamper-test",
        "target_scope": "execution",
        "waitpoint_token": tampered,
        "now": TimestampMs::now().0,
    });

    let resp = api
        .client
        .post(api.url(&format!("/v1/executions/{eid}/signal")))
        .json(&body)
        .send()
        .await
        .expect("signal request failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "tampered token must surface as 400"
    );
    let text = resp.text().await.unwrap_or_default();
    assert!(
        text.contains("invalid_token"),
        "expected invalid_token in body, got: {text}"
    );
}

/// A nonexistent execution returns 404.
#[tokio::test]
#[serial_test::serial]
async fn test_list_pending_waitpoints_not_found() {
    let api = TestApi::setup().await;
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let eid = tc.new_execution_id();
    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}/pending-waitpoints")))
        .send()
        .await
        .expect("pending-waitpoints request failed");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
