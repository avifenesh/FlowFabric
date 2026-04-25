//! RFC-018 Stage A integration test: `PostgresBackend::capabilities`.
//!
//! Asserts that a `PostgresBackend` reports `family = "postgres"` and
//! the v0.9 `Supports` shape: ingress + flow bulk cancel + seed/rotate
//! HMAC + scheduler + RFC-019 subscriptions + streaming are `true`;
//! operator control + execution reads + budget / quota admin +
//! list_pending_waitpoints + cancel_flow_header + ack_cancel_member +
//! prepare remain `false` pending Wave 9.
//!
//! Pool-only (via `from_pool`); no LISTEN/NOTIFY or scanner wiring
//! required, which keeps this test independent of the per-family
//! fixtures. Ignore-gated on `FF_PG_TEST_URL`.

use ff_backend_postgres::{apply_migrations, PostgresBackend};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn capabilities_reports_postgres_family_and_v09_supports() {
    let Ok(url) = std::env::var("FF_PG_TEST_URL") else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    let backend = PostgresBackend::from_pool(pool, PartitionConfig::default());
    let caps = backend.capabilities();

    assert_eq!(caps.identity.family, "postgres");
    assert_eq!(caps.identity.rfc017_stage, "E-shipped");

    // ── Supported at v0.9 ──
    assert!(caps.supports.cancel_flow_wait_timeout);
    assert!(caps.supports.cancel_flow_wait_indefinite);
    assert!(caps.supports.rotate_waitpoint_hmac_secret_all);
    assert!(caps.supports.seed_waitpoint_hmac_secret);
    assert!(caps.supports.claim_for_worker);
    assert!(caps.supports.subscribe_lease_history);
    assert!(caps.supports.subscribe_completion);
    assert!(caps.supports.subscribe_signal_delivery);
    assert!(caps.supports.stream_durable_summary);
    assert!(caps.supports.stream_best_effort_live);

    // ── Wave 9 — deferred ──
    assert!(!caps.supports.cancel_execution);
    assert!(!caps.supports.change_priority);
    assert!(!caps.supports.replay_execution);
    assert!(!caps.supports.revoke_lease);
    assert!(!caps.supports.read_execution_state);
    assert!(!caps.supports.read_execution_info);
    assert!(!caps.supports.get_execution_result);
    assert!(!caps.supports.budget_admin);
    assert!(!caps.supports.quota_admin);
    assert!(!caps.supports.list_pending_waitpoints);
    assert!(!caps.supports.cancel_flow_header);
    assert!(!caps.supports.ack_cancel_member);
    // Postgres prepare() is NoOp; `prepare` bool says "non-trivial
    // work" and stays false.
    assert!(!caps.supports.prepare);
    // Deferred per #311.
    assert!(!caps.supports.subscribe_instance_tags);
}
