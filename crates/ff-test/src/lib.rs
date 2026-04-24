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
