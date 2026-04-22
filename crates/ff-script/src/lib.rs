//! Typed FCALL wrappers and Lua library loader for FlowFabric Valkey Functions.

#[macro_use]
pub mod macros;
pub mod engine_error_ext;
pub mod error;
pub mod result;
pub mod loader;
pub mod retry;
pub mod functions;
pub mod stream_tail;

pub use error::ScriptError;
pub use retry::{is_retryable_kind, kind_to_stable_str};

/// The compiled FlowFabric Lua library source.
///
/// Generated from `lua/*.lua` by `scripts/gen-ff-script-lua.sh` and checked
/// into the crate as `flowfabric.lua` so it ships inside the published
/// tarball. CI (`matrix.yml`) fails if this file drifts from what the
/// script would produce.
pub const LIBRARY_SOURCE: &str = include_str!("flowfabric.lua");

/// Expected library version. Must match `FCALL ff_version 0` return.
///
/// **Single source of truth is `lua/version.lua`.** The gen script
/// extracts the `return 'X'` literal and writes it to
/// `flowfabric_lua_version`. Bump `lua/version.lua` whenever any Lua
/// function's KEYS or ARGV arity changes, or a new function is added.
pub const LIBRARY_VERSION: &str = include_str!("flowfabric_lua_version");

// Re-export the trait so callers can use it without reaching into result.
pub use result::FromFcallResult;
