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

    let outcome = backend.prepare().await.expect("prepare should succeed");
    match outcome {
        PrepareOutcome::Applied { description } => {
            assert!(
                description.starts_with("FUNCTION LOAD (flowfabric lib v"),
                "unexpected description: {description:?}"
            );
        }
        other => panic!("Valkey prepare must return Applied, got {other:?}"),
    }
}
