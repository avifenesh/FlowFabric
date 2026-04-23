-- FlowFabric library version check
-- Returns the library version string. Used by the loader to detect
-- whether the library is loaded and at the expected version.
--
-- Bump this string whenever any registered function's KEYS or ARGV arity
-- changes, or a new function is added. Mismatched versions force
-- `FUNCTION LOAD REPLACE` so old binaries cannot FCALL a library whose key
-- signatures they expect a different shape for.
--
-- SINGLE SOURCE OF TRUTH: Rust's `LIBRARY_VERSION` is extracted from the
-- `return 'X'` literal below by `scripts/gen-ff-script-lua.sh`, which
-- writes `crates/ff-script/src/flowfabric_lua_version`. Rust reads that
-- file via `include_str!`. Do NOT maintain a separate Rust literal.
-- Extract contract: the body MUST contain exactly one `return 'X'` literal
-- with single quotes (not double). CI runs the gen script and diffs; any
-- drift fails the build.

redis.register_function('ff_version', function(keys, args)
  return '20'
end)
