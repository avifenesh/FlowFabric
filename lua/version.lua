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
-- `return 'X'` literal below at compile time (see crates/ff-script/build.rs).
-- Do NOT maintain a separate copy in crates/ff-script/src/lib.rs — there
-- is no Rust copy; `env!("FLOWFABRIC_LUA_VERSION")` reads this file.
-- Extract contract: the body MUST contain exactly one `return 'X'` literal
-- with single quotes (not double). Breaking the contract fails build.rs
-- with a pointer to this file.

redis.register_function('ff_version', function(keys, args)
  return '5'
end)
