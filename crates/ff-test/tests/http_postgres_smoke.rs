//! RFC-017 Wave 8 Stage E1: HTTP smoke against the Postgres backend.
//!
//! Exercises the Stage-E1 `Server::start_postgres_branch` boot path by
//! spinning a real `ff-server` HTTP listener + `PostgresBackend` on an
//! ephemeral port and driving create-flow / add-execution / stage-edge
//! / apply-dep through `reqwest`. Proves the Postgres dial branch is
//! wired end-to-end for the 5 migrated ingress routes that dispatch
//! through `server.backend()`.
//!
//! # Running
//!
//! ```
//! FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
//!   cargo test -p ff-test --test http_postgres_smoke \
//!     --features postgres-e2e -- --nocapture
//! ```
//!
//! # Postgres provisioning on the dev box
//!
//! Uses the native Postgres at `localhost:5432` (already running; not
//! podman/docker). The `ff_e2e_http_smoke` database was pre-created by
//! a prior Stage D2 agent; `ALTER ROLE postgres PASSWORD 'postgres'`
//! brought auth to a known state for this smoke. `apply_migrations`
//! runs idempotently inside the server boot so a fresh database is
//! also fine.
//!
//! # Stage E1 coverage + deferrals
//!
//! **Covered (runs end-to-end through HTTP → Postgres):**
//! * `POST /v1/flows` → `backend.create_flow`
//! * `POST /v1/executions` × 3 → `backend.create_execution`
//! * `POST /v1/flows/{id}/members` × 3 → `backend.add_execution_to_flow`
//! * `POST /v1/flows/{id}/edges` × 2 → `backend.stage_dependency_edge`
//! * `POST /v1/flows/{id}/edges/apply` × 2 → `backend.apply_dependency_to_child`
//!
//! **Deferred (NOT tested — architectural dependency on later stages):**
//! * `POST /v1/flows/{id}/cancel` — `Server::cancel_flow_inner` still
//!   calls the Valkey-only `fcall_with_reload` + `self.client.hmget` /
//!   `smembers`. Stage **E2** lifts `cancel_flow` onto the trait.
//! * `GET /v1/executions/{id}` / `.../state` / `.../result` — all use
//!   `self.client.hgetall` / `hget` / `cmd("GET")`. Stage **E2**
//!   removes the `Client` field + migrates these reads.
//! * `GET /v1/flows/{id}` — route does not exist today; adding it is
//!   a separate ingress-read task post-E4.
//! * `claim_for_worker` — scheduler is Valkey-only; Stage **E3** wires
//!   the Postgres-native equivalent.
//!
//! Exercising only the migrated ingress paths keeps the smoke honest:
//! every assertion hits a Postgres row; no bypassed contract.

#![cfg(feature = "postgres-e2e")]

use std::sync::Arc;

use ff_core::contracts::*;
use ff_core::partition::{execution_partition, PartitionConfig};
use ff_core::types::*;
use reqwest::StatusCode;
use sqlx::Executor;
use tokio::task::AbortHandle;

const LANE: &str = "pg-smoke-lane";
const NS: &str = "pg-smoke-ns";

fn postgres_url() -> String {
    std::env::var("FF_POSTGRES_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/ff_e2e_http_smoke".into())
}

/// Truncate the relations the smoke writes to so the test is
/// re-runnable against the same database.
async fn cleanup_postgres() {
    let pool = sqlx::PgPool::connect(&postgres_url())
        .await
        .expect("connect to cleanup Postgres");
    // Best-effort: ignore errors from relations a fresh DB hasn't
    // created yet — `apply_migrations` will bring them up on boot.
    let _ = pool
        .execute(
            "TRUNCATE ff_edge, ff_edge_group, ff_exec_core, ff_flow_core, ff_flow_member, \
             ff_lane_registry, ff_pending_cancel_groups, ff_cancel_dispatch_outbox \
             RESTART IDENTITY CASCADE",
        )
        .await;
}

struct PgHttpSmoke {
    client: reqwest::Client,
    base_url: String,
    _abort_handle: AbortHandle,
}

impl PgHttpSmoke {
    async fn setup() -> Self {
        // CI / dev-override combo required for the §9.0 hard-gate;
        // Stage E4 flips `BACKEND_STAGE_READY` and this override goes
        // away.
        // SAFETY: set_var is unsafe in Rust 2024 because env-vars are
        // process-global and concurrent reads in other threads are
        // racy. This test is the sole writer at boot time and the
        // server reads the values synchronously during `start`.
        unsafe {
            std::env::set_var("FF_BACKEND_ACCEPT_UNREADY", "1");
            std::env::set_var("FF_ENV", "development");
        }

        cleanup_postgres().await;

        let partition_config = PartitionConfig::default();
        let config = ff_server::config::ServerConfig {
            host: "localhost".into(),
            port: 6379,
            tls: false,
            cluster: false,
            partition_config,
            lanes: vec![LaneId::new(LANE)],
            listen_addr: "127.0.0.1:0".into(),
            engine_config: ff_engine::EngineConfig {
                partition_config,
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
            backend: ff_server::config::BackendKind::Postgres,
            postgres: {
                let mut p = ff_server::config::PostgresServerConfig::default();
                p.url = postgres_url();
                p.pool_size = 10;
                p
            },
        };

        let server = ff_server::server::Server::start(config)
            .await
            .expect("Postgres Server::start failed");
        let server = Arc::new(server);
        let app = ff_server::api::router(server.clone(), &["*".to_owned()], None)
            .expect("router builds with wildcard CORS");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind test listener");
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let _abort_handle = handle.abort_handle();

        PgHttpSmoke {
            client: reqwest::Client::new(),
            base_url,
            _abort_handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

#[tokio::test]
#[serial_test::serial]
async fn http_postgres_smoke() {
    let smoke = PgHttpSmoke::setup().await;

    // 1. healthz — sanity that the process is alive.
    let resp = smoke
        .client
        .get(smoke.url("/healthz"))
        .send()
        .await
        .expect("GET /healthz");
    assert_eq!(resp.status(), StatusCode::OK, "healthz must be 200");
    eprintln!("[e1-smoke] GET /healthz → {}", resp.status());

    // 2. Create a flow.
    let partition_config = PartitionConfig::default();
    let flow_id = FlowId::new();
    let now = TimestampMs::now();
    let create_flow_args = CreateFlowArgs {
        flow_id: flow_id.clone(),
        flow_kind: "pg_smoke".into(),
        namespace: Namespace::new(NS),
        now,
    };
    let resp = smoke
        .client
        .post(smoke.url("/v1/flows"))
        .json(&create_flow_args)
        .send()
        .await
        .expect("POST /v1/flows");
    assert!(
        resp.status().is_success(),
        "create_flow → {}: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );
    eprintln!("[e1-smoke] POST /v1/flows → {}", flow_id);

    // 3. POST 3 executions pinned to the flow's partition (RFC-011
    //    co-location contract — every exec on the same {fp:N}).
    let mut eids: Vec<ExecutionId> = Vec::new();
    for idx in 0..3 {
        let eid = ExecutionId::for_flow(&flow_id, &partition_config);
        let args = CreateExecutionArgs {
            execution_id: eid.clone(),
            namespace: Namespace::new(NS),
            lane_id: LaneId::new(LANE),
            execution_kind: "pg_smoke".into(),
            input_payload: b"{}".to_vec(),
            payload_encoding: Some("json".into()),
            priority: 5,
            creator_identity: "pg-smoke".into(),
            idempotency_key: None,
            tags: Default::default(),
            policy: None,
            delay_until: None,
            execution_deadline_at: None,
            partition_id: execution_partition(&eid, &partition_config).index,
            now,
        };
        let resp = smoke
            .client
            .post(smoke.url("/v1/executions"))
            .json(&args)
            .send()
            .await
            .expect("POST /v1/executions");
        assert!(
            resp.status().is_success(),
            "create_execution[{idx}] → {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
        eprintln!("[e1-smoke] POST /v1/executions[{idx}] → {}", eid);

        // 3a. Add execution to the flow.
        let add_args = AddExecutionToFlowArgs {
            flow_id: flow_id.clone(),
            execution_id: eid.clone(),
            now,
        };
        let resp = smoke
            .client
            .post(smoke.url(&format!("/v1/flows/{flow_id}/members")))
            .json(&add_args)
            .send()
            .await
            .expect("POST /v1/flows/{id}/members");
        assert!(
            resp.status().is_success(),
            "add_execution_to_flow[{idx}] → {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
        eprintln!("[e1-smoke] POST /v1/flows/{}/members[{idx}]", flow_id);
        eids.push(eid);
    }

    // 4. Stage 2 edges: eids[0] → eids[1], eids[0] → eids[2].
    //
    // Each `add_execution_to_flow` in Step 3 bumps `graph_revision`,
    // so after 3 members the flow sits at rev=3. Fetch the current
    // rev directly from Postgres rather than assuming — keeps the
    // test robust against future semantic changes to the add-member
    // path's rev-bump count.
    let rev_pool = sqlx::PgPool::connect(&postgres_url())
        .await
        .expect("connect to fetch starting graph_revision");
    let starting_rev: i64 =
        sqlx::query_scalar("SELECT graph_revision FROM ff_flow_core WHERE flow_id = $1::uuid")
            .bind(flow_id.to_string())
            .fetch_one(&rev_pool)
            .await
            .expect("SELECT starting graph_revision");
    drop(rev_pool);
    let mut expected_rev: u64 = starting_rev as u64;
    for ui in 0..2 {
        let upstream = &eids[0];
        let downstream = &eids[ui + 1];
        let edge_id = EdgeId::new();
        let stage_args = StageDependencyEdgeArgs {
            flow_id: flow_id.clone(),
            edge_id: edge_id.clone(),
            upstream_execution_id: upstream.clone(),
            downstream_execution_id: downstream.clone(),
            dependency_kind: "success_only".into(),
            data_passing_ref: None,
            expected_graph_revision: expected_rev,
            now,
        };
        let resp = smoke
            .client
            .post(smoke.url(&format!("/v1/flows/{flow_id}/edges")))
            .json(&stage_args)
            .send()
            .await
            .expect("POST /v1/flows/{id}/edges");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "stage_edge[{ui}] → {status}: {body}");
        let staged: StageDependencyEdgeResult =
            serde_json::from_str(&body).expect("parse StageDependencyEdgeResult");
        let new_rev = match staged {
            StageDependencyEdgeResult::Staged {
                new_graph_revision, ..
            } => new_graph_revision,
        };
        expected_rev = new_rev;
        eprintln!("[e1-smoke] POST /v1/flows/{}/edges[{ui}] ({edge_id}) rev={new_rev}", flow_id);

        // 5. Apply the edge to the child so the downstream's dep-count
        //    moves toward unblock. Stage E1 proves the write path; the
        //    Postgres reconciler + claim path (E3) would flip eligibility
        //    to `eligible` end-to-end.
        let apply_args = ApplyDependencyToChildArgs {
            flow_id: flow_id.clone(),
            edge_id: edge_id.clone(),
            downstream_execution_id: downstream.clone(),
            upstream_execution_id: upstream.clone(),
            graph_revision: new_rev,
            dependency_kind: "success_only".into(),
            data_passing_ref: None,
            now,
        };
        let resp = smoke
            .client
            .post(smoke.url(&format!("/v1/flows/{flow_id}/edges/apply")))
            .json(&apply_args)
            .send()
            .await
            .expect("POST /v1/flows/{id}/edges/apply");
        assert!(
            resp.status().is_success(),
            "apply_dep[{ui}] → {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
        eprintln!(
            "[e1-smoke] POST /v1/flows/{}/edges/apply[{ui}] ({edge_id})",
            flow_id
        );
    }

    // 6. Sanity: the flow, its 3 execs, and the 2 edges live in
    //    Postgres. A direct sqlx count is cheaper + more robust than
    //    re-driving HTTP GETs that might route through Valkey-only
    //    handlers (the E2 deferrals).
    let pool = sqlx::PgPool::connect(&postgres_url())
        .await
        .expect("connect to verify Postgres rows");
    let flow_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ff_flow_core WHERE flow_id = $1::uuid")
            .bind(flow_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("SELECT flow_core count");
    assert_eq!(flow_count, 1, "flow_core row count");

    let exec_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ff_exec_core WHERE flow_id = $1::uuid")
            .bind(flow_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("SELECT exec_core count");
    let _ = eids; // silence unused after shape change
    assert_eq!(exec_count, 3, "exec_core row count");

    let edge_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ff_edge WHERE flow_id = $1::uuid")
            .bind(flow_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("SELECT edge count");
    assert_eq!(edge_count, 2, "edge row count");

    eprintln!(
        "[e1-smoke] PASS — flow={} execs={} edges={} (all committed to Postgres)",
        flow_count, exec_count, edge_count
    );
}
