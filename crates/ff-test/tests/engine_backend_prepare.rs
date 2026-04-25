//! Issue #281 — `EngineBackend::prepare()` trait-surface boot step.
//!
//! Live-Valkey coverage:
//!   1. `ValkeyBackend::prepare()` issues `FUNCTION LOAD` and returns
//!      `PrepareOutcome::Applied { description: "FUNCTION LOAD (flowfabric lib v<N>)" }`.
//!   2. Calling `prepare()` a second time is idempotent — returns
//!      the same `Applied` outcome with no spurious errors.
//!
//! Postgres `NoOp` parity lives in
//! `crates/ff-backend-postgres` unit tests (no live PG required).

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
async fn valkey_prepare_returns_applied_and_is_idempotent() {
    let tc = TestCluster::connect().await;
    let backend: Arc<dyn EngineBackend> =
        ValkeyBackend::from_client_and_partitions(tc.client().clone(), test_config());

    let first = backend
        .prepare()
        .await
        .expect("first prepare should succeed");
    match &first {
        PrepareOutcome::Applied { description } => {
            assert!(
                description.starts_with("FUNCTION LOAD (flowfabric lib v"),
                "unexpected description: {description:?}"
            );
        }
        other => panic!("Valkey prepare must return Applied, got {other:?}"),
    }

    let second = backend
        .prepare()
        .await
        .expect("second prepare should succeed (idempotent)");
    assert_eq!(first, second, "prepare must be idempotent");
}
