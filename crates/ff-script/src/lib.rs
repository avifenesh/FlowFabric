//! Typed FCALL wrappers and Lua library loader for FlowFabric Valkey Functions.

#[macro_use]
pub mod macros;
pub mod error;
pub mod result;
pub mod loader;
pub mod retry;
pub mod functions;

pub use error::ScriptError;
pub use retry::{is_retryable_kind, kind_to_stable_str};

/// The compiled FlowFabric Lua library source, concatenated by build.rs.
/// Includes the `#!lua name=flowfabric` preamble, all shared helpers,
/// and all registered functions.
pub const LIBRARY_SOURCE: &str = include_str!(concat!(env!("OUT_DIR"), "/flowfabric.lua"));

/// Expected library version. Must match `FCALL ff_version 0` return.
///
/// Bump this (and the string returned by `lua/version.lua`'s `ff_version`)
/// whenever any Lua function's KEYS or ARGV arity changes, or a new
/// function is added. The loader compares this constant against the
/// server's `ff_version` output; a mismatch triggers `FUNCTION LOAD
/// REPLACE` so old binaries attached to a newly-upgraded Valkey don't
/// call into a library whose key signatures they don't agree with.
pub const LIBRARY_VERSION: &str = "2";

// Re-export the trait so callers can use it without reaching into result.
pub use result::FromFcallResult;
