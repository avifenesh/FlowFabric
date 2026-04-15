-- FlowFabric quota and rate-limit functions
-- Reference: RFC-008 (Quota), RFC-010 §4.4 (#32)
--
-- Depends on helpers: ok, err, is_set

---------------------------------------------------------------------------
-- #32  ff_check_admission_and_record  (on {q:K})
--
-- Idempotent sliding-window rate check + concurrency check.
-- If admitted: ZADD window, SET NX guard, optional INCR concurrency.
--
-- KEYS (4): window_zset, concurrency_counter, quota_def, admitted_guard_key
-- ARGV (6): now_ms, window_seconds, rate_limit, concurrency_cap,
--           execution_id, jitter_ms
---------------------------------------------------------------------------
redis.register_function('ff_check_admission_and_record', function(keys, args)
  local K = {
    window_zset        = keys[1],
    concurrency_key    = keys[2],
    quota_def          = keys[3],
    admitted_guard_key = keys[4],
  }

  local A = {
    now_ms          = tonumber(args[1]),
    window_seconds  = tonumber(args[2]),
    rate_limit      = tonumber(args[3]),
    concurrency_cap = tonumber(args[4]),
    execution_id    = args[5],
    jitter_ms       = tonumber(args[6] or "0"),
  }

  local window_ms = A.window_seconds * 1000

  -- 1. Idempotency guard: already admitted in this window?
  if redis.call("EXISTS", K.admitted_guard_key) == 1 then
    return { "ALREADY_ADMITTED" }
  end

  -- 2. Sliding window: remove expired entries
  redis.call("ZREMRANGEBYSCORE", K.window_zset, "-inf", A.now_ms - window_ms)

  -- 3. Check rate limit
  if A.rate_limit > 0 then
    local current_count = redis.call("ZCARD", K.window_zset)
    if current_count >= A.rate_limit then
      -- Compute retry_after from oldest entry
      local oldest = redis.call("ZRANGE", K.window_zset, 0, 0, "WITHSCORES")
      local retry_after_ms = 0
      if #oldest >= 2 then
        retry_after_ms = tonumber(oldest[2]) + window_ms - A.now_ms
        if retry_after_ms < 0 then retry_after_ms = 0 end
      end
      local jitter = 0
      if A.jitter_ms > 0 then
        jitter = math.random(0, A.jitter_ms)
      end
      return { "RATE_EXCEEDED", tostring(retry_after_ms + jitter) }
    end
  end

  -- 4. Check concurrency cap
  if A.concurrency_cap > 0 then
    local active = tonumber(redis.call("GET", K.concurrency_key) or "0")
    if active >= A.concurrency_cap then
      return { "CONCURRENCY_EXCEEDED" }
    end
  end

  -- 5. Admit: record in sliding window (execution_id as member — idempotent)
  redis.call("ZADD", K.window_zset, A.now_ms, A.execution_id)

  -- 6. Set admitted guard key with TTL = window size
  redis.call("SET", K.admitted_guard_key, "1", "PX", window_ms, "NX")

  -- 7. Increment concurrency counter if cap is set
  if A.concurrency_cap > 0 then
    redis.call("INCR", K.concurrency_key)
  end

  return { "ADMITTED" }
end)
