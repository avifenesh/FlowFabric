//! Typed FCALL wrappers and Lua library loader for FlowFabric Valkey Functions.

#[macro_use]
pub mod macros;
pub mod error;
pub mod result;
pub mod loader;
pub mod functions;

pub use error::ScriptError;

/// The compiled FlowFabric Lua library source, concatenated by build.rs.
/// Includes the `#!lua name=flowfabric` preamble, all shared helpers,
/// and all registered functions.
pub const LIBRARY_SOURCE: &str = include_str!(concat!(env!("OUT_DIR"), "/flowfabric.lua"));

/// Expected library version. Must match `FCALL ff_version 0` return.
pub const LIBRARY_VERSION: &str = "1";

// Re-export the trait so callers can use it without reaching into result.
pub use result::FromFcallResult;
