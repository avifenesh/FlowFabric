//! Issue #281 — `EngineBackend::prepare()` trait-surface boot step.
//!
//! Live-Valkey coverage:
//!   1. `ValkeyBackend::prepare()` issues `FUNCTION LOAD` and returns
//!      `PrepareOutcome::Applied { description: "FUNCTION LOAD (flowfabric lib v<N>)" }`.
//!
//! The `prepare()` entry point reuses `ff_script::loader::ensure_library`
//! verbatim, so its in-crate retry + idempotency semantics are already
//! covered by the loader's own tests + the live boot path — this test
//! asserts the trait-surface wrapper shape only.
//!
//! Postgres `NoOp` parity lives in the `MockBackend` default-impl
//! test at `crates/ff-server/tests/parity_stage_a.rs`
//! (`prepare_default_impl_returns_noop`); `PostgresBackend::prepare`
//! returns the same `PrepareOutcome::NoOp`.

use std::sync::Arc;

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::PrepareOutcome;
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_test::fixtures::TestCluster;

fn test_config() -> PartitionConfig {
    ff_test::fixtures::TEST_PARTITION_CONFIG
}

#[tokio::test(flavor = "current_thread")]
async fn valkey_prepare_returns_applied() {
    let tc = TestCluster::connect().await;
    let backend: Arc<dyn EngineBackend> =
        ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config());

    // Outer retry loop: `ensure_library` already retries transient
    // transport faults 3× internally, but on slow CI cluster runners
    // the whole budget can exhaust between when a newly-loaded library
    // propagates across primaries. Consumers (cairn) call `prepare`
    // inside their own boot retry loop; this test mirrors that shape.
    const OUTER_ATTEMPTS: u32 = 3;
    let mut last_err = None;
    for _ in 0..OUTER_ATTEMPTS {
        match backend.prepare().await {
            Ok(outcome) => {
                match outcome {
                    PrepareOutcome::Applied { description } => {
                        assert!(
                            description.starts_with("FUNCTION LOAD (flowfabric lib v"),
                            "unexpected description: {description:?}"
                        );
                    }
                    other => panic!("Valkey prepare must return Applied, got {other:?}"),
                }
                return;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
    panic!("prepare failed after {OUTER_ATTEMPTS} attempts: {last_err:?}");
}
