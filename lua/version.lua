-- FlowFabric library version check
-- Returns the library version string. Used by the loader to detect
-- whether the library is loaded and at the expected version.

redis.register_function('ff_version', function(keys, args)
  return '1'
end)
