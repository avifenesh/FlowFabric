//! RFC-023 §4.3 / §9 capability-matrix snapshot — Phase 1a subset.
//!
//! Phase 1a asserts that `capabilities()` reports
//! `identity.family == "sqlite"` and every `Supports::*` flag is
//! `false`. Phase 4 release PR flips the flags in lockstep with
//! trait body landings.

use ff_backend_sqlite::SqliteBackend;
use ff_core::engine_backend::EngineBackend;
use serial_test::serial;

#[tokio::test]
#[serial(ff_dev_mode)]
async fn capabilities_identity_and_supports() {
    // SAFETY: test-only env mutation; `serial_test` serialises across
    // the `ff_dev_mode` key so no other test races this.
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let backend = SqliteBackend::new(":memory:")
        .await
        .expect("sqlite init under FF_DEV_MODE=1");

    let caps = backend.capabilities();
    assert_eq!(caps.identity.family, "sqlite");
    // Phase 1a rfc017_stage label.
    assert_eq!(caps.identity.rfc017_stage, "Phase-1a");

    // Every support flag must be false at Phase 1a — Phase 4 flips
    // them when trait bodies ship.
    assert_eq!(caps.supports, ff_core::capability::Supports::none());

    assert_eq!(backend.backend_label(), "sqlite");
}
