//! RFC-024 PR-1 parity test: Valkey `read_execution_context` on a
//! non-existent execution returns `EngineError::Validation { kind:
//! InvalidInput, .. }` — matching the PG + SQLite siblings.
//!
//! The SDK worker only calls `read_execution_context` post-claim, so a
//! missing row is an invariant violation rather than a silent empty
//! read. This test pins that contract on the Valkey backend.
//!
//! Ignore-gated (live Valkey at 127.0.0.1:6379 required), matching the
//! sibling live tests in `capabilities.rs`.

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::partition::PartitionConfig;
use ff_core::types::{ExecutionId, FlowId};

#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn read_execution_context_missing_returns_validation_invalid_input() {
    let backend = ValkeyBackend::connect(BackendConfig::valkey("127.0.0.1", 6379))
        .await
        .expect("connect ValkeyBackend");

    // Mint a fresh id that was never written to the backend.
    let config = PartitionConfig::default();
    let eid = ExecutionId::for_flow(&FlowId::new(), &config);

    let err = backend
        .read_execution_context(&eid)
        .await
        .expect_err("missing execution must error, not return an empty context");

    match err {
        EngineError::Validation { kind, detail } => {
            assert_eq!(
                kind,
                ValidationKind::InvalidInput,
                "Valkey missing-row must use ValidationKind::InvalidInput for parity \
                 with PG + SQLite siblings; got detail={detail}"
            );
            assert!(
                detail.contains("read_execution_context"),
                "detail should name the op: got {detail}"
            );
        }
        other => panic!(
            "expected EngineError::Validation{{InvalidInput}} for missing execution; got {other:?}"
        ),
    }
}
