-- FlowFabric library version check
-- Returns the library version string. Used by the loader to detect
-- whether the library is loaded and at the expected version.
--
-- Bump this string (and `LIBRARY_VERSION` in crates/ff-script/src/lib.rs)
-- whenever any registered function's KEYS or ARGV arity changes, or a new
-- function is added. Mismatched versions force `FUNCTION LOAD REPLACE` so
-- old binaries cannot FCALL a library whose key signatures they expect a
-- different shape for.

redis.register_function('ff_version', function(keys, args)
  return '2'
end)
