//! RFC-018 Stage A integration test: `ValkeyBackend::capabilities_matrix`.
//!
//! Asserts that a backend dialed via `ValkeyBackend::connect` reports
//! `family = "valkey"` and gates `Capability::ClaimForWorker` as
//! `Partial` (scheduler not wired), while a backend dialed via
//! `ValkeyBackend::with_embedded_scheduler` promotes that row to
//! `Supported`. Ignore-gated (live Valkey at 127.0.0.1:6379 required),
//! matching the sibling live tests in
//! `crates/ff-backend-valkey/src/lib.rs::tests`.

use std::sync::Arc;

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;
use ff_core::capability::{Capability, CapabilityStatus};

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn capabilities_matrix_reports_valkey_family() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("127.0.0.1", 6379))
        .await
        .expect("connect ValkeyBackend for capabilities smoke");
    let m = backend.capabilities_matrix();
    assert_eq!(m.identity.family, "valkey");
    // Stage marker — v0.8.x post-RFC-017 Stage E ship.
    assert_eq!(m.identity.rfc017_stage, "E-shipped");
    // Always-supported rows that do not depend on scheduler wiring.
    assert_eq!(m.get(Capability::Ping), CapabilityStatus::Supported);
    assert_eq!(m.get(Capability::CreateFlow), CapabilityStatus::Supported);
    assert_eq!(
        m.get(Capability::CancelFlowWaitTimeout),
        CapabilityStatus::Supported
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn claim_for_worker_is_partial_without_scheduler() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("127.0.0.1", 6379))
        .await
        .expect("connect ValkeyBackend");
    let m = backend.capabilities_matrix();
    match m.get(Capability::ClaimForWorker) {
        CapabilityStatus::Partial { note } => {
            assert!(
                note.contains("with_embedded_scheduler"),
                "expected note to mention with_embedded_scheduler, got: {note}"
            );
        }
        other => panic!(
            "expected ClaimForWorker=Partial without scheduler, got {other:?}"
        ),
    }
    // Partial still counts as "supported-ish" for the predicate.
    assert!(m.supports(Capability::ClaimForWorker));
}

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn claim_for_worker_is_supported_with_embedded_scheduler() {
    let metrics = Arc::new(ff_observability::Metrics::new());
    let backend = ValkeyBackend::with_embedded_scheduler(
        BackendConfig::valkey("127.0.0.1", 6379),
        metrics,
    )
    .await
    .expect("with_embedded_scheduler for capabilities smoke");
    let m = backend.capabilities_matrix();
    assert_eq!(m.identity.family, "valkey");
    assert_eq!(
        m.get(Capability::ClaimForWorker),
        CapabilityStatus::Supported
    );
    assert!(m.supports(Capability::ClaimForWorker));
}
