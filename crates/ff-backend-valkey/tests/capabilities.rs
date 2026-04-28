//! RFC-018 Stage A integration test: `ValkeyBackend::capabilities`.
//!
//! Asserts that a backend dialed via `ValkeyBackend::connect` reports
//! `family = "valkey"` and gates `supports.claim_for_worker` to `false`
//! (scheduler not wired), while a backend dialed via
//! `ValkeyBackend::with_embedded_scheduler` promotes that field to
//! `true`. Ignore-gated (live Valkey at 127.0.0.1:6379 required),
//! matching the sibling live tests in
//! `crates/ff-backend-valkey/src/lib.rs::tests`.

use std::sync::Arc;

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn capabilities_reports_valkey_family() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("127.0.0.1", 6379))
        .await
        .expect("connect ValkeyBackend for capabilities smoke");
    let caps = backend.capabilities();
    assert_eq!(caps.identity.family, "valkey");
    // Stage marker — v0.9.x post-RFC-017 Stage E ship.
    assert_eq!(caps.identity.rfc017_stage, "E-shipped");
    // Always-supported fields that do not depend on scheduler wiring.
    assert!(caps.supports.cancel_execution);
    assert!(caps.supports.cancel_flow_wait_timeout);
    assert!(caps.supports.cancel_flow_wait_indefinite);
    assert!(caps.supports.budget_admin);
    assert!(caps.supports.quota_admin);
    assert!(caps.supports.subscribe_lease_history);
    assert!(caps.supports.subscribe_completion);
    assert!(caps.supports.subscribe_signal_delivery);
    // Deferred per #311.
    assert!(!caps.supports.subscribe_instance_tags);
    // RFC-024 PR-F: reclaim surface is live on Valkey.
    assert!(
        caps.supports.issue_reclaim_grant,
        "RFC-024 PR-F flips supports.issue_reclaim_grant true on Valkey"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn claim_for_worker_is_false_without_scheduler() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("127.0.0.1", 6379))
        .await
        .expect("connect ValkeyBackend");
    let caps = backend.capabilities();
    assert!(
        !caps.supports.claim_for_worker,
        "without a wired scheduler, claim_for_worker dispatch returns \
         EngineError::Unavailable; supports.claim_for_worker must be false"
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn claim_for_worker_is_true_with_embedded_scheduler() {
    let metrics = Arc::new(ff_observability::Metrics::new());
    let backend = ValkeyBackend::with_embedded_scheduler(
        BackendConfig::valkey("127.0.0.1", 6379),
        metrics,
    )
    .await
    .expect("with_embedded_scheduler for capabilities smoke");
    let caps = backend.capabilities();
    assert_eq!(caps.identity.family, "valkey");
    assert!(caps.supports.claim_for_worker);
}
