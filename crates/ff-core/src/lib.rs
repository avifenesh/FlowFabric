//! Core types, state enums, partition math, key builders, and error codes for FlowFabric.

pub mod backend;
pub mod capability;
pub mod caps;
pub mod completion_backend;
pub mod contracts;
pub mod engine_backend;
pub mod engine_error;
pub mod error;
pub mod handle_codec;
pub mod hash;
pub mod keys;
pub mod partition;
pub mod policy;
pub mod state;
pub mod stream_subscribe;
pub mod types;
pub mod waitpoint_hmac;

// Convenience re-export so consumers can write `ff_core::EngineError`.
pub use engine_error::EngineError;
// #88: backend-agnostic transport error carried across public
// ff-sdk / ff-server surfaces. Kept alongside `EngineError` so
// consumers can `use ff_core::{BackendError, BackendErrorKind}`.
pub use engine_error::{BackendError, BackendErrorKind};
// DX (HHH v0.3.4 re-smoke): re-export `ScannerFilter` at crate root
// so consumers can write `ff_core::ScannerFilter` instead of the
// longer `ff_core::backend::ScannerFilter` path.
pub use backend::ScannerFilter;
