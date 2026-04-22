//! Core types, state enums, partition math, key builders, and error codes for FlowFabric.

pub mod backend;
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
