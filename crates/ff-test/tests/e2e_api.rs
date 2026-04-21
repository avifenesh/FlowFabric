//! REST API integration tests for FlowFabric.
//!
//! Spins up a real Server + axum HTTP listener on a random port,
//! then exercises the API with reqwest.
//!
//! Run with: cargo test -p ff-test --test e2e_api -- --test-threads=1
//!
//! TODO: Untested API endpoints (tested at Server/FCALL layer but not HTTP):
//! - POST /v1/executions/{id}/revoke-lease
//! - GET  /v1/executions?partition=N (list_executions)
//! - POST /v1/budgets/{id}/usage (report_usage)
//! - POST /v1/budgets/{id}/reset (reset_budget)
//! - POST /v1/flows/{id}/edges (stage_dependency_edge)
//! - POST /v1/flows/{id}/edges/apply (apply_dependency_to_child)
//!
//! TODO: Untested SDK methods (no test at any layer):
//! - ClaimedTask::move_to_waiting_children()
//!
//! TODO: Untested Server methods (tested via FCALL but not typed Server API):
//! - Server::revoke_lease()
//! - Server::reset_budget()
//! - Server::list_executions()

use std::sync::Arc;

use ff_core::contracts::*;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::task::AbortHandle;

// ─── Test constants ──

const LANE: &str = "api-test-lane";
const NS: &str = "api-ns";

// ─── Test harness ──

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
        // 1. Connect TestCluster (loads Lua library + flushes)
        let tc = TestCluster::connect().await;
        tc.cleanup().await;

        // 2. Build Server (skip_library_load since TestCluster already loaded it)
        let config = test_server_config();
        let server = ff_server::server::Server::start(config)
            .await
            .expect("Server::start failed");

        let server = Arc::new(server);
        let app = ff_server::api::router(server.clone(), &["*".to_owned()], None);

        // 3. Bind to random port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        // 4. Spawn axum in background
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let abort_handle = handle.abort_handle();

        let client = reqwest::Client::new();

        TestApi {
            client,
            base_url,
            abort_handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

fn test_server_config() -> ff_server::config::ServerConfig {
    let config = ff_test::fixtures::TEST_PARTITION_CONFIG;
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
        lanes: vec![LaneId::new(LANE)],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: config,
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

// ─── Helpers ──

/// Create an execution via API, return the ExecutionId. Panics on failure.
async fn api_create_execution(api: &TestApi, eid: &ExecutionId, priority: i32) {
    let args = CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new(NS),
        lane_id: LaneId::new(LANE),
        execution_kind: "api_test".into(),
        input_payload: b"{}".to_vec(),
        payload_encoding: None,
        priority,
        creator_identity: "api-test".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: ff_core::partition::execution_partition(eid, &ff_test::fixtures::TEST_PARTITION_CONFIG).index,
        now: TimestampMs::now(),
    };
    let resp = api.client.post(api.url("/v1/executions")).json(&args).send().await
        .expect("create execution failed");
    assert!(resp.status().is_success(), "create returned {}", resp.status());
}

// ─── Tests ──

/// GET /healthz returns 200 + {"status":"ok"}.
#[tokio::test]
#[serial_test::serial]
async fn test_api_healthz() {
    let api = TestApi::setup().await;

    let resp = api
        .client
        .get(api.url("/healthz"))
        .send()
        .await
        .expect("healthz request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    #[derive(Deserialize)]
    struct Health {
        status: String,
    }
    let body: Health = resp.json().await.expect("healthz JSON parse failed");
    assert_eq!(body.status, "ok");
}

/// POST /v1/executions creates, then GET /v1/executions/{id} reads it back.
#[tokio::test]
#[serial_test::serial]
async fn test_api_create_and_get_execution() {
    let api = TestApi::setup().await;

    let eid = ExecutionId::solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG);
    let now = TimestampMs::now();
    let args = CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new(NS),
        lane_id: LaneId::new(LANE),
        execution_kind: "llm_call".into(),
        input_payload: b"{}".to_vec(),
        payload_encoding: Some("json".into()),
        priority: 5,
        creator_identity: "api-test".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: ff_core::partition::execution_partition(&eid, &ff_test::fixtures::TEST_PARTITION_CONFIG).index,
        now,
    };

    // POST create
    let resp = api
        .client
        .post(api.url("/v1/executions"))
        .json(&args)
        .send()
        .await
        .expect("create execution request failed");

    assert_eq!(resp.status(), StatusCode::CREATED);

    let result: CreateExecutionResult = resp.json().await.expect("create result parse failed");
    match &result {
        CreateExecutionResult::Created { execution_id, .. } => {
            assert_eq!(execution_id, &eid);
        }
        CreateExecutionResult::Duplicate { .. } => panic!("expected Created, got Duplicate"),
    }

    // GET execution by ID
    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}")))
        .send()
        .await
        .expect("get execution request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let info: ExecutionInfo = resp.json().await.expect("execution info parse failed");
    assert_eq!(info.execution_id, eid);
    assert_eq!(info.namespace, NS);
    assert_eq!(info.lane_id, LANE);
    assert_eq!(info.priority, 5);
    assert_eq!(info.execution_kind, "llm_call");
}

/// POST /v1/executions/{id}/cancel transitions to cancelled.
#[tokio::test]
#[serial_test::serial]
async fn test_api_cancel_execution() {
    let api = TestApi::setup().await;

    // Create first
    let eid = ExecutionId::solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG);
    let now = TimestampMs::now();
    let create_args = CreateExecutionArgs {
        execution_id: eid.clone(),
        namespace: Namespace::new(NS),
        lane_id: LaneId::new(LANE),
        execution_kind: "cancellable".into(),
        input_payload: b"{}".to_vec(),
        payload_encoding: None,
        priority: 0,
        creator_identity: "api-test".into(),
        idempotency_key: None,
        tags: Default::default(),
        policy: None,
        delay_until: None,
        execution_deadline_at: None,
        partition_id: ff_core::partition::execution_partition(&eid, &ff_test::fixtures::TEST_PARTITION_CONFIG).index,
        now,
    };

    let resp = api
        .client
        .post(api.url("/v1/executions"))
        .json(&create_args)
        .send()
        .await
        .expect("create request failed");
    assert!(resp.status().is_success());

    // Cancel
    let cancel_args = CancelExecutionArgs {
        execution_id: eid.clone(), // will be overridden by path
        reason: "api-test-cancel".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::now(),
    };

    let resp = api
        .client
        .post(api.url(&format!("/v1/executions/{eid}/cancel")))
        .json(&cancel_args)
        .send()
        .await
        .expect("cancel request failed");

    assert_eq!(resp.status(), StatusCode::OK);

    let result: CancelExecutionResult = resp.json().await.expect("cancel result parse failed");
    match result {
        CancelExecutionResult::Cancelled { execution_id, .. } => {
            assert_eq!(execution_id, eid);
        }
    }

    // Verify state via GET
    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{eid}/state")))
        .send()
        .await
        .expect("get state request failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let state: ff_core::state::PublicState = resp.json().await.expect("state parse failed");
    assert_eq!(state, ff_core::state::PublicState::Cancelled);
}

/// POST /v1/flows creates a flow.
#[tokio::test]
#[serial_test::serial]
async fn test_api_create_flow() {
    let api = TestApi::setup().await;

    let flow_id = FlowId::new();
    let args = CreateFlowArgs {
        flow_id: flow_id.clone(),
        flow_kind: "pipeline".into(),
        namespace: Namespace::new(NS),
        now: TimestampMs::now(),
    };

    let resp = api
        .client
        .post(api.url("/v1/flows"))
        .json(&args)
        .send()
        .await
        .expect("create flow request failed");

    assert_eq!(resp.status(), StatusCode::CREATED);

    let result: CreateFlowResult = resp.json().await.expect("flow result parse failed");
    match result {
        CreateFlowResult::Created { flow_id: fid } => {
            assert_eq!(fid, flow_id);
        }
        CreateFlowResult::AlreadySatisfied { .. } => panic!("expected Created"),
    }
}

/// GET /v1/executions/{random-uuid} returns 404.
#[tokio::test]
#[serial_test::serial]
async fn test_api_not_found() {
    let api = TestApi::setup().await;

    let fake_id = ExecutionId::solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG);
    let resp = api
        .client
        .get(api.url(&format!("/v1/executions/{fake_id}")))
        .send()
        .await
        .expect("not-found request failed");

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    #[derive(Deserialize)]
    struct ErrBody {
        error: String,
    }
    let body: ErrBody = resp.json().await.expect("error body parse failed");
    assert!(
        body.error.contains("not found"),
        "expected 'not found' in error, got: {}",
        body.error
    );
}

/// POST /v1/budgets creates a budget.
#[tokio::test]
#[serial_test::serial]
async fn test_api_create_budget() {
    let api = TestApi::setup().await;

    let budget_id = BudgetId::new();
    let args = CreateBudgetArgs {
        budget_id: budget_id.clone(),
        scope_type: "lane".into(),
        scope_id: "test-lane".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 3_600_000,
        dimensions: vec!["tokens".into(), "cost".into()],
        hard_limits: vec![1000, 50],
        soft_limits: vec![800, 40],
        now: TimestampMs::now(),
    };

    let resp = api.client.post(api.url("/v1/budgets")).json(&args).send().await
        .expect("create budget request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let result: CreateBudgetResult = resp.json().await.expect("budget result parse failed");
    match result {
        CreateBudgetResult::Created { budget_id: bid } => assert_eq!(bid, budget_id),
        CreateBudgetResult::AlreadySatisfied { .. } => panic!("expected Created"),
    }
}

/// Create budget, then GET /v1/budgets/{id} and verify fields.
#[tokio::test]
#[serial_test::serial]
async fn test_api_get_budget_status() {
    let api = TestApi::setup().await;

    let budget_id = BudgetId::new();
    let args = CreateBudgetArgs {
        budget_id: budget_id.clone(),
        scope_type: "namespace".into(),
        scope_id: "prod".into(),
        enforcement_mode: "strict".into(),
        on_hard_limit: "fail".into(),
        on_soft_limit: "warn".into(),
        reset_interval_ms: 0,
        dimensions: vec!["requests".into()],
        hard_limits: vec![500],
        soft_limits: vec![400],
        now: TimestampMs::now(),
    };

    let resp = api.client.post(api.url("/v1/budgets")).json(&args).send().await
        .expect("create budget failed");
    assert!(resp.status().is_success());

    // GET status
    let resp = api.client.get(api.url(&format!("/v1/budgets/{budget_id}"))).send().await
        .expect("get budget status failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let status: BudgetStatus = resp.json().await.expect("budget status parse failed");
    assert_eq!(status.budget_id, budget_id.to_string());
    assert_eq!(status.scope_type, "namespace");
    assert_eq!(status.scope_id, "prod");
    assert_eq!(status.enforcement_mode, "strict");
    assert_eq!(status.hard_limits.get("requests"), Some(&500));
    assert_eq!(status.soft_limits.get("requests"), Some(&400));
    assert_eq!(status.breach_count, 0);
}

/// POST /v1/quotas creates a quota policy.
#[tokio::test]
#[serial_test::serial]
async fn test_api_create_quota_policy() {
    let api = TestApi::setup().await;

    let qid = QuotaPolicyId::new();
    let args = CreateQuotaPolicyArgs {
        quota_policy_id: qid.clone(),
        window_seconds: 60,
        max_requests_per_window: 100,
        max_concurrent: 10,
        now: TimestampMs::now(),
    };

    let resp = api.client.post(api.url("/v1/quotas")).json(&args).send().await
        .expect("create quota request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let result: CreateQuotaPolicyResult = resp.json().await.expect("quota result parse failed");
    match result {
        CreateQuotaPolicyResult::Created { quota_policy_id } => assert_eq!(quota_policy_id, qid),
        CreateQuotaPolicyResult::AlreadySatisfied { .. } => panic!("expected Created"),
    }
}

// NOTE: test_api_deliver_signal skipped — requires claim → suspend → signal
// chain via FCALL, which is covered by test_server_deliver_signal in e2e_lifecycle.rs.
// Adding here would duplicate ~60 lines of FCALL setup for a thin HTTP wrapper test.

/// PUT /v1/executions/{id}/priority changes priority.
#[tokio::test]
#[serial_test::serial]
async fn test_api_change_priority() {
    let api = TestApi::setup().await;

    let eid = ExecutionId::solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG);
    api_create_execution(&api, &eid, 0).await;

    // Change priority
    let resp = api.client
        .put(api.url(&format!("/v1/executions/{eid}/priority")))
        .json(&serde_json::json!({ "new_priority": 500 }))
        .send()
        .await
        .expect("change priority request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let result: ChangePriorityResult = resp.json().await.expect("priority result parse failed");
    match result {
        ChangePriorityResult::Changed { execution_id } => assert_eq!(execution_id, eid),
    }

    // Verify via GET
    let resp = api.client.get(api.url(&format!("/v1/executions/{eid}"))).send().await
        .expect("get execution failed");
    let info: ExecutionInfo = resp.json().await.expect("info parse failed");
    assert_eq!(info.priority, 500);
}

/// Create → cancel (terminal) → replay → verify back to Waiting.
#[tokio::test]
#[serial_test::serial]
async fn test_api_replay_execution() {
    let api = TestApi::setup().await;

    let eid = ExecutionId::solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG);
    api_create_execution(&api, &eid, 0).await;

    // Cancel to make terminal
    let cancel_args = CancelExecutionArgs {
        execution_id: eid.clone(),
        reason: "setup-for-replay".into(),
        source: CancelSource::OperatorOverride,
        lease_id: None,
        lease_epoch: None,
        attempt_id: None,
        now: TimestampMs::now(),
    };
    let resp = api.client
        .post(api.url(&format!("/v1/executions/{eid}/cancel")))
        .json(&cancel_args)
        .send()
        .await
        .expect("cancel request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    // Replay
    let resp = api.client
        .post(api.url(&format!("/v1/executions/{eid}/replay")))
        .send()
        .await
        .expect("replay request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let result: ReplayExecutionResult = resp.json().await.expect("replay result parse failed");
    match result {
        ReplayExecutionResult::Replayed { public_state } => {
            assert_eq!(public_state, ff_core::state::PublicState::Waiting);
        }
    }

    // Verify state is back to Waiting
    let resp = api.client
        .get(api.url(&format!("/v1/executions/{eid}/state")))
        .send()
        .await
        .expect("get state failed");
    let state: ff_core::state::PublicState = resp.json().await.expect("state parse failed");
    assert_eq!(state, ff_core::state::PublicState::Waiting);
}

/// Create flow + execution, POST /v1/flows/{id}/members, verify Added.
#[tokio::test]
#[serial_test::serial]
async fn test_api_add_execution_to_flow() {
    let api = TestApi::setup().await;

    // Create flow
    let flow_id = FlowId::new();
    let flow_args = CreateFlowArgs {
        flow_id: flow_id.clone(),
        flow_kind: "pipeline".into(),
        namespace: Namespace::new(NS),
        now: TimestampMs::now(),
    };
    let resp = api.client.post(api.url("/v1/flows")).json(&flow_args).send().await
        .expect("create flow failed");
    assert!(resp.status().is_success());

    // Create execution co-located with the flow's partition.
    // Post-RFC-011 phase 3: ff_add_execution_to_flow is an atomic
    // single-FCALL that takes exec_core as KEYS[4] and requires the
    // exec's hash-tag to match the flow's. Mint via for_flow.
    let eid = ExecutionId::for_flow(&flow_id, &ff_test::fixtures::TEST_PARTITION_CONFIG);
    api_create_execution(&api, &eid, 0).await;

    // Add to flow
    let add_args = AddExecutionToFlowArgs {
        flow_id: flow_id.clone(),
        execution_id: eid.clone(),
        now: TimestampMs::now(),
    };
    let resp = api.client
        .post(api.url(&format!("/v1/flows/{flow_id}/members")))
        .json(&add_args)
        .send()
        .await
        .expect("add to flow request failed");
    assert_eq!(resp.status(), StatusCode::CREATED);

    let result: AddExecutionToFlowResult = resp.json().await.expect("add result parse failed");
    match result {
        AddExecutionToFlowResult::Added { execution_id, new_node_count } => {
            assert_eq!(execution_id, eid);
            assert_eq!(new_node_count, 1);
        }
        AddExecutionToFlowResult::AlreadyMember { .. } => panic!("expected Added"),
    }
}

/// Create flow, POST /v1/flows/{id}/cancel, verify Cancelled.
#[tokio::test]
#[serial_test::serial]
async fn test_api_cancel_flow() {
    let api = TestApi::setup().await;

    let flow_id = FlowId::new();
    let flow_args = CreateFlowArgs {
        flow_id: flow_id.clone(),
        flow_kind: "pipeline".into(),
        namespace: Namespace::new(NS),
        now: TimestampMs::now(),
    };
    let resp = api.client.post(api.url("/v1/flows")).json(&flow_args).send().await
        .expect("create flow failed");
    assert!(resp.status().is_success());

    // Cancel flow
    let cancel_args = CancelFlowArgs {
        flow_id: flow_id.clone(),
        reason: "api-test-cancel".into(),
        cancellation_policy: "cancel_all".into(),
        now: TimestampMs::now(),
    };
    let resp = api.client
        .post(api.url(&format!("/v1/flows/{flow_id}/cancel")))
        .json(&cancel_args)
        .send()
        .await
        .expect("cancel flow request failed");
    assert_eq!(resp.status(), StatusCode::OK);

    let result: CancelFlowResult = resp.json().await.expect("cancel flow result parse failed");
    // Accept Cancelled or CancellationScheduled — the default HTTP path
    // returns CancellationScheduled when the flow has members, Cancelled
    // otherwise. PartiallyCancelled is only ever produced by the
    // sync-wait path on per-member failures, not by this default-dispatch
    // smoke (and no failure is injected here), so reject it explicitly.
    match result {
        CancelFlowResult::Cancelled { cancellation_policy, .. }
        | CancelFlowResult::CancellationScheduled { cancellation_policy, .. } => {
            assert_eq!(cancellation_policy, "cancel_all");
        }
        other @ CancelFlowResult::PartiallyCancelled { .. } => {
            panic!("unexpected PartiallyCancelled on default-dispatch cancel: {other:?}")
        }
    }
}

/// R5 smoke: concurrency ceiling for stream ops surfaces as HTTP 429
/// Too Many Requests when a burst exceeds `max_concurrent_stream_ops`.
/// Verifies the `stream_semaphore` + `ConcurrencyLimitExceeded("stream_ops", N)
/// → 429` path in `api::tail_attempt_stream`.
///
/// We build a Server with `max_concurrent_stream_ops=2`, fire 5 tail
/// calls with a long-ish block so they all stay in flight, and assert
/// that at least 3 of them come back as 429.
#[tokio::test]
#[serial_test::serial]
async fn test_stream_semaphore_returns_429_on_burst() {
    // Build a bespoke server with a tiny stream-op ceiling.
    let tc = TestCluster::connect().await;
    tc.cleanup().await;

    let mut config = test_server_config();
    config.max_concurrent_stream_ops = 2;
    let server = ff_server::server::Server::start(config)
        .await
        .expect("Server::start failed");
    let server = Arc::new(server);
    let app = ff_server::api::router(server.clone(), &["*".to_owned()], None);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let _abort = handle.abort_handle();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    // Fire 5 concurrent tails on a never-written stream with block_ms=2000.
    //
    // Expected outcome under the `xread_block_lock` serializer:
    //   * 2 calls acquire a stream_semaphore permit (ceiling=2) and
    //     queue on the XREAD BLOCK Mutex. They serialize: first dispatches
    //     immediately, second waits ~2s for the first to finish. Both
    //     return 200 with empty frames + closed_at=None after ~2s and
    //     ~4s respectively (full block_ms budget preserved on both).
    //   * 3 calls fail the try_acquire_owned fast and return 429 in
    //     microseconds.
    //
    // Before the serialization Mutex landed (R5-b), the second
    // permit-holder would spuriously time out: ferriskey's per-call
    // request_timeout (block_ms + 500ms) started client-side while
    // Valkey was still servicing the first BLOCK on the pipelined mux,
    // so the second call's timeout fired before its turn at the server.
    // Observed as HTTP 500 `valkey: timed out`. The Mutex ensures each
    // call's timeout doesn't start until it holds the lock.
    let eid = ExecutionId::solo(&LaneId::new(LANE), &ff_test::fixtures::TEST_PARTITION_CONFIG);
    let url = format!(
        "{base_url}/v1/executions/{eid}/attempts/0/stream/tail?block_ms=2000&limit=10"
    );

    let mut handles = Vec::new();
    for _ in 0..5 {
        let c = client.clone();
        let u = url.clone();
        handles.push(tokio::spawn(async move { c.get(&u).send().await }));
    }

    let mut ok_like = 0;
    let mut too_many = 0;
    for h in handles {
        let resp = h.await.expect("join").expect("send");
        let status = resp.status().as_u16();
        match status {
            200 => ok_like += 1,
            429 => too_many += 1,
            other => {
                let body = resp.text().await.unwrap_or_default();
                panic!("unexpected status {other}: {body}");
            }
        }
    }

    // At most 2 can have acquired permits; the remaining 3 must 429.
    assert!(
        too_many >= 3,
        "expected >=3 429 responses, got too_many={too_many} ok_like={ok_like}"
    );
    assert!(
        ok_like <= 2,
        "expected <=2 200 responses (matches max_concurrent_stream_ops), got {ok_like}"
    );

    _abort.abort();
}
