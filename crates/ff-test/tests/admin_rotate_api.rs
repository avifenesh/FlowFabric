//! Integration tests for [`ff_sdk::FlowFabricAdminClient::rotate_waitpoint_secret`].
//!
//! Spins up a real `ff-server` in-process (same axum router, same
//! `Server` impl), points the SDK's admin client at it, and drives
//! the rotate endpoint end-to-end. Covers:
//!
//!   1. Happy path — response body decodes into the SDK type and
//!      mirrors the server's `RotateWaitpointSecretResult` shape.
//!   2. Valkey state — after a successful rotate, the
//!      `ff:sec:{p:N}:waitpoint_hmac` hash has `current_kid`
//!      promoted, `previous_kid` set, and `previous_expires_at`
//!      populated.
//!   3. Validation — `new_kid` containing `':'` surfaces as
//!      `SdkError::AdminApi { status: 400, message }`.
//!   4. Auth — with `api_token` configured on the server, calling
//!      without a bearer yields `AdminApi { status: 401 }`; with
//!      the correct bearer succeeds.
//!
//! Run with: `cargo test -p ff-test --test admin_rotate_api -- --test-threads=1`

use std::sync::Arc;

use ff_core::keys::IndexKeys;
use ff_core::partition::Partition;
use ff_test::fixtures::TestCluster;
use tokio::task::AbortHandle;

use ff_sdk::{
    FlowFabricAdminClient, RotateWaitpointSecretRequest, RotateWaitpointSecretResponse, SdkError,
};

// ── HTTP harness ────────────────────────────────────────────────

struct TestApi {
    base_url: String,
    abort_handle: AbortHandle,
}

impl Drop for TestApi {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

impl TestApi {
    async fn setup_with(api_token: Option<String>) -> Self {
        let _tc = TestCluster::connect().await;
        // Rotation touches every execution partition; a CLEAN
        // per-partition install runs via Server::start. Don't
        // cleanup() here — that'd wipe fixtures other serial tests
        // share. We're only inspecting per-partition keys, and the
        // rotation is idempotent, so co-resident state is safe.

        let config = test_server_config(api_token.clone());
        let server = ff_server::server::Server::start(config)
            .await
            .expect("Server::start failed");
        let server = Arc::new(server);
        let app = ff_server::api::router(server, &["*".to_owned()], api_token.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        TestApi {
            base_url,
            abort_handle: handle.abort_handle(),
        }
    }
}

fn test_server_config(api_token: Option<String>) -> ff_server::config::ServerConfig {
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
        lanes: vec![ff_core::types::LaneId::new("admin-rotate-lane")],
        listen_addr: "127.0.0.1:0".into(),
        engine_config: ff_engine::EngineConfig {
            partition_config: pc,
            lanes: vec![ff_core::types::LaneId::new("admin-rotate-lane")],
            ..Default::default()
        },
        skip_library_load: true,
        cors_origins: vec!["*".to_owned()],
        api_token,
        // Known-weak all-zeros secret — fine for tests, documented
        // trivial-to-forge in RFC-004. Server bootstrap refuses to
        // start without a secret, so we supply one. Every partition
        // has this as its initial `current_kid = "default"` value.
        waitpoint_hmac_secret:
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        waitpoint_hmac_grace_ms: 86_400_000,
        max_concurrent_stream_ops: 64,
    }
}

// ── Tests ───────────────────────────────────────────────────────

/// Happy path: no auth on the server, SDK client w/o token, rotate
/// succeeds and the response decodes into the SDK type with the
/// server-native shape.
#[tokio::test]
#[serial_test::serial]
async fn test_rotate_waitpoint_secret_happy_path() {
    let api = TestApi::setup_with(None).await;

    let client = FlowFabricAdminClient::new(&api.base_url)
        .expect("build admin client");
    let new_kid = format!("kid-{}", ff_core::types::ExecutionId::new());
    let req = RotateWaitpointSecretRequest {
        new_kid: new_kid.clone(),
        new_secret_hex:
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20".to_owned(),
    };

    let resp: RotateWaitpointSecretResponse =
        client.rotate_waitpoint_secret(req).await.expect("rotate should succeed");

    assert_eq!(
        resp.new_kid, new_kid,
        "server should echo back the new_kid we installed"
    );
    assert!(
        resp.rotated as usize + resp.in_progress.len() > 0,
        "at least one partition must have rotated or been in-progress; got {resp:?}"
    );
    assert!(
        resp.failed.is_empty(),
        "no partition should have failed on a healthy test cluster: {:?}",
        resp.failed
    );
}

/// State verification: after a rotate, the `ff:sec:{p:N}:waitpoint_hmac`
/// hash on every rotated partition has `current_kid` promoted to the
/// new kid, `previous_kid` set, and `previous_expires_at` populated.
#[tokio::test]
#[serial_test::serial]
async fn test_rotate_waitpoint_secret_updates_valkey_state() {
    let api = TestApi::setup_with(None).await;
    let tc = TestCluster::connect().await;

    let client = FlowFabricAdminClient::new(&api.base_url)
        .expect("build admin client");
    let new_kid = format!("kid-{}", ff_core::types::ExecutionId::new());
    let req = RotateWaitpointSecretRequest {
        new_kid: new_kid.clone(),
        new_secret_hex:
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899".to_owned(),
    };

    let resp = client.rotate_waitpoint_secret(req).await.expect("rotate");
    assert!(resp.rotated > 0, "need at least one rotated partition to verify state");

    // Loop over every partition the response reported as `rotated`.
    // Partition 0 succeeding doesn't prove partitions 1..N rotated —
    // a server-side bug that skipped the fan-out after the first
    // partition would still pass a single-partition check. This
    // asserts per-partition state for the full rotated set.
    //
    // `failed` partitions are intentionally NOT asserted against
    // (the rotation didn't touch them so their state is unchanged)
    // but the happy-path test already guarantees failed.is_empty()
    // on a healthy cluster; here we'd just skip them defensively.
    //
    // `in_progress` partitions are ALSO skipped — their state is
    // transient (the concurrent rotation may be mid-fanout) and
    // the server contract only promises eventual convergence for
    // that set, not a post-call snapshot.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let expected_secret_hex =
        "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";

    // Collect the partition indices we expect to have fully rotated.
    // The server returns `rotated: u16` as a COUNT, not a list, so
    // we iterate `0..num_execution_partitions` and assert on every
    // partition NOT in the `failed` or `in_progress` sets.
    let pc = ff_test::fixtures::TEST_PARTITION_CONFIG;
    let skip: std::collections::HashSet<u16> = resp
        .failed
        .iter()
        .copied()
        .chain(resp.in_progress.iter().copied())
        .collect();

    let mut checked = 0u16;
    for p_idx in 0..pc.num_execution_partitions {
        if skip.contains(&p_idx) {
            continue;
        }
        let partition = Partition {
            family: ff_core::partition::PartitionFamily::Execution,
            index: p_idx,
        };
        let idx = IndexKeys::new(&partition);
        let hmac_key = idx.waitpoint_hmac_secrets();

        // current_kid promoted.
        let current_kid: Option<String> = tc.client()
            .cmd("HGET")
            .arg(&hmac_key)
            .arg("current_kid")
            .execute()
            .await
            .expect("HGET current_kid");
        assert_eq!(
            current_kid.as_deref(),
            Some(new_kid.as_str()),
            "partition {p_idx}: current_kid must be promoted to new_kid"
        );

        // previous_kid set (non-empty).
        let previous_kid: Option<String> = tc.client()
            .cmd("HGET")
            .arg(&hmac_key)
            .arg("previous_kid")
            .execute()
            .await
            .expect("HGET previous_kid");
        assert!(
            previous_kid.as_deref().map(|s| !s.is_empty()).unwrap_or(false),
            "partition {p_idx}: previous_kid must be set; got {previous_kid:?}"
        );

        // previous_expires_at populated and in the future.
        let previous_expires_at: Option<String> = tc.client()
            .cmd("HGET")
            .arg(&hmac_key)
            .arg("previous_expires_at")
            .execute()
            .await
            .expect("HGET previous_expires_at");
        let expires_ms: i64 = previous_expires_at
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                panic!(
                    "partition {p_idx}: previous_expires_at should be numeric; got {previous_expires_at:?}"
                )
            });
        assert!(
            expires_ms > now_ms,
            "partition {p_idx}: previous_expires_at ({expires_ms}) must be in the future (now={now_ms})"
        );

        // new_kid's secret field carries the hex we sent.
        let secret_field = format!("secret:{new_kid}");
        let secret: Option<String> = tc.client()
            .cmd("HGET")
            .arg(&hmac_key)
            .arg(&secret_field)
            .execute()
            .await
            .expect("HGET secret:<new_kid>");
        assert_eq!(
            secret.as_deref(),
            Some(expected_secret_hex),
            "partition {p_idx}: secret:<new_kid> must carry the new hex"
        );

        checked += 1;
    }

    assert_eq!(
        checked, resp.rotated,
        "number of partitions verified ({checked}) must equal resp.rotated ({})",
        resp.rotated
    );
}

/// Validation: `new_kid` containing `':'` (the server's field
/// separator) is rejected with 400 + the expected error message.
#[tokio::test]
#[serial_test::serial]
async fn test_rotate_waitpoint_secret_rejects_bad_new_kid() {
    let api = TestApi::setup_with(None).await;

    let client = FlowFabricAdminClient::new(&api.base_url)
        .expect("build admin client");
    let req = RotateWaitpointSecretRequest {
        new_kid: "bad:kid".to_owned(),
        new_secret_hex:
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20".to_owned(),
    };

    match client.rotate_waitpoint_secret(req).await {
        Err(SdkError::AdminApi {
            status,
            message,
            retryable,
            ..
        }) => {
            assert_eq!(status, 400, "bad new_kid should be a 400");
            assert!(
                message.contains("new_kid") && message.contains(':'),
                "message should mention new_kid + ':' separator; got {message:?}"
            );
            assert!(retryable.is_none(), "400s should have retryable=None");
        }
        other => panic!("expected AdminApi 400, got {other:?}"),
    }
}

/// Auth: server configured with an `api_token`. Without a bearer
/// the SDK gets 401; with the bearer the call succeeds. Guards
/// the critical contract that `with_token` actually plumbs the
/// Authorization header — regression here ships a broken deploy.
#[tokio::test]
#[serial_test::serial]
async fn test_rotate_waitpoint_secret_enforces_bearer_token() {
    let token = "s3cret-admin-token";
    let api = TestApi::setup_with(Some(token.to_owned())).await;

    // 1. No-token client → 401.
    let unauthed = FlowFabricAdminClient::new(&api.base_url)
        .expect("build no-auth client");
    let req_for_unauth = RotateWaitpointSecretRequest {
        new_kid: format!("kid-unauth-{}", ff_core::types::ExecutionId::new()),
        new_secret_hex:
            "cafebabedeadbeef0011223344556677cafebabedeadbeef0011223344556677".to_owned(),
    };
    match unauthed.rotate_waitpoint_secret(req_for_unauth).await {
        Err(SdkError::AdminApi { status, .. }) => {
            assert_eq!(status, 401, "missing bearer should be 401");
        }
        other => panic!("expected AdminApi 401, got {other:?}"),
    }

    // 2. Bearer-configured client → 200.
    let authed = FlowFabricAdminClient::with_token(&api.base_url, token)
        .expect("build auth client");
    let new_kid = format!("kid-auth-{}", ff_core::types::ExecutionId::new());
    let req_for_auth = RotateWaitpointSecretRequest {
        new_kid: new_kid.clone(),
        new_secret_hex:
            "1122334455667788aabbccddeeff00111122334455667788aabbccddeeff0011".to_owned(),
    };
    let resp = authed
        .rotate_waitpoint_secret(req_for_auth)
        .await
        .expect("bearer-configured client must be able to rotate");
    assert_eq!(resp.new_kid, new_kid);
}
