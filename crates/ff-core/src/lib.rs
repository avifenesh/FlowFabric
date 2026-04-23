//! Core types, state enums, partition math, key builders, and error codes for FlowFabric.

pub mod backend;
pub mod completion_backend;
pub mod contracts;
pub mod engine_backend;
pub mod engine_error;
pub mod error;
pub mod hash;
pub mod keys;
pub mod partition;
pub mod policy;
pub mod state;
pub mod types;

// Convenience re-export so consumers can write `ff_core::EngineError`.
pub use engine_error::EngineError;
// #88: backend-agnostic transport error carried across public
// ff-sdk / ff-server surfaces. Kept alongside `EngineError` so
// consumers can `use ff_core::{BackendError, BackendErrorKind}`.
pub use engine_error::{BackendError, BackendErrorKind};
