//! FlowFabric integration test harness.
//!
//! Provides:
//! - [`fixtures::TestCluster`] — managed Valkey connection with library loading and cleanup
//! - [`assertions`] — assertion macros for state vectors, index membership, attempts, leases
//! - [`helpers`] — shorthand builders for creating test executions and waiting for state
//!
//! # Quick start
//!
//! ```rust,ignore
//! use ff_test::fixtures::TestCluster;
//! use ff_test::helpers::{CreateTestExecutionParams, create_test_execution_direct};
//! use ff_test::assert_execution_state;
//! use ff_core::state::*;
//!
//! #[tokio::test]
//! async fn test_create_execution() {
//!     let tc = TestCluster::connect().await;
//!     tc.cleanup().await;
//!
//!     let params = CreateTestExecutionParams::new(&tc);
//!     let eid = create_test_execution_direct(&tc, &params).await;
//!
//!     assert_execution_state!(&tc, &eid,
//!         lifecycle_phase = LifecyclePhase::Runnable,
//!         public_state = PublicState::Waiting,
//!     );
//! }
//! ```

pub mod assertions;
pub mod backend_matrix;
pub mod fixtures;
pub mod helpers;

// Re-export key types for convenience in tests
pub use ff_core::state::*;
pub use ff_core::types::*;

use std::sync::OnceLock;

/// Guard so the global subscriber is installed at most once per
/// integration-test binary.
static TRACING_INIT: OnceLock<()> = OnceLock::new();

/// Install a process-wide `tracing_subscriber::fmt` subscriber,
/// gated on the `RUST_LOG` env var.
///
/// Rationale (PR #429 follow-up): PRs #426/#427/#429 shipped three
/// layers of `tracing::warn!` observability meant to diagnose the
/// arm-cluster `FUNCTION LOAD -> READONLY` flake. Those traces never
/// reached CI logs because no `ff-test` integration-test binary
/// installed a subscriber. This helper wires a fmt subscriber with
/// `EnvFilter::from_default_env()` so setting `RUST_LOG` at CI (or
/// locally) produces visible output. If `RUST_LOG` is unset the
/// helper is a no-op — zero allocation, zero cost.
///
/// Idempotent: the first caller installs the global default; later
/// calls are ignored via `OnceLock`. This does not interfere with
/// the thread-local scoped subscriber used in
/// `engine_backend_observability.rs` (`tracing::subscriber::set_default`
/// wins within its scope; the global serves everything else).
///
/// Called automatically from [`fixtures::TestCluster::connect`] so
/// every integration test picks it up without per-test boilerplate.
pub fn init_test_tracing() {
    TRACING_INIT.get_or_init(|| {
        // Treat unset OR empty as "no subscriber". GitHub Actions
        // matrix-gated env expressions (`${{ cond && 'x' || '' }}`)
        // set the var to an empty string on the false branch rather
        // than leaving it unset, and we don't want a passing cell to
        // pay the subscriber-init cost for an empty filter.
        match std::env::var_os("RUST_LOG") {
            Some(v) if !v.is_empty() => {}
            _ => return,
        }
        // `try_init` is the non-panicking form — if some other
        // subscriber was already installed (e.g. a test opted to
        // set one up itself before calling a ff-test helper), we
        // leave it alone.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    });
}

#[cfg(test)]
mod init_test_tracing_tests {
    use super::init_test_tracing;

    // Unit-level sanity check: when `RUST_LOG` is unset, the helper
    // returns without panicking and does not install a subscriber.
    // When it IS set, `try_init()` succeeds on the first call and
    // subsequent calls are ignored via the `OnceLock`.
    //
    // Full end-to-end verification (a `warn!` line actually appears
    // on the captured test writer) is exercised via
    // `RUST_LOG=... cargo test -- --nocapture` in CI; see the
    // cluster cell's `RUST_LOG` env on the integration-tests step.
    #[test]
    fn init_is_noop_without_rust_log() {
        // We cannot reliably manipulate the process env in a
        // parallel test binary, so just confirm the call is safe
        // and idempotent. The real branch coverage comes from the
        // CI run with `RUST_LOG` set.
        init_test_tracing();
        init_test_tracing();
    }
}
