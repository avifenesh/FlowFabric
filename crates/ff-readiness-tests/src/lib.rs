//! FlowFabric readiness tests — release-gate smoke tests.
//!
//! All tests live in `tests/*.rs` behind `#[cfg(feature = "readiness")]`
//! and `#[ignore]`. They run only in the dedicated CI matrix cell
//! (PR-D4, not yet landed). No business logic lives in this crate —
//! the harness modules under `pub mod` are the only library surface.
//!
//! Run locally:
//! ```bash
//! cargo test -p ff-readiness-tests --features readiness -- --ignored --test-threads=1
//! ```

pub mod evidence;
pub mod process;
pub mod server;
pub mod valkey;
pub mod worker;
