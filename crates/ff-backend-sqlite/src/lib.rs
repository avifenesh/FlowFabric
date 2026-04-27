//! `EngineBackend` implementation backed by SQLite — **dev-only**
//! per RFC-023.
//!
//! # Phase 1a scope
//!
//! This crate is the RFC-023 Phase 1a scaffold: construction
//! semantics (`FF_DEV_MODE=1` guard, per-path registry, connection
//! pool, WARN banner), the SQLite transient-busy error classifier
//! skeleton, and an [`EngineBackend`](ff_core::engine_backend::EngineBackend)
//! impl whose data-plane methods return
//! [`EngineError::Unavailable`](ff_core::engine_error::EngineError::Unavailable).
//! Phase 1b lands the hand-ported SQLite-dialect migrations;
//! Phase 2+ replaces the Unavailable stubs with real bodies
//! paralleling `ff-backend-postgres`.
//!
//! # Production guard
//!
//! [`SqliteBackend::new`] refuses to construct without
//! `FF_DEV_MODE=1` in the process environment. The refusal is on
//! the TYPE (RFC-023 §3.3 A3 / §4.5) so every path — embedded
//! library consumers, `ff_server::start_sqlite_branch`, and
//! `ff_sdk::FlowFabricWorker::connect_with` — pays the guard.
//!
//! No ferriskey dep — this crate's transport is `sqlx`.

#![allow(clippy::result_large_err)]

mod backend;
mod budget;
mod completion_subscribe;
mod config;
mod errors;
mod handle_codec;
mod lease_event_subscribe;
mod operator;
mod outbox_cursor;
mod reads;
mod reconcilers;
mod scanner_supervisor;
mod signal_delivery_subscribe;
#[doc(hidden)]
pub mod pubsub;
pub mod queries;
mod registry;
pub mod retry;
mod suspend_ops;
mod tx_util;

pub use backend::SqliteBackend;
pub use errors::{MAX_ATTEMPTS, is_retryable_sqlite_busy};
pub use retry::{IsRetryableBusy, retry_serializable};
pub use scanner_supervisor::{SqliteScannerConfig, SqliteScannerHandle};
