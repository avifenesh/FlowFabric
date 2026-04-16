-- FlowFabric quota and rate-limit functions
-- Reference: RFC-008 (Quota), RFC-010 §4.4 (#32)
--
-- Depends on helpers: ok, err, is_set

---------------------------------------------------------------------------
-- ff_create_quota_policy  (on {q:K})
--
-- Create a new quota/rate-limit policy.
-- Idempotent: if EXISTS quota_def → return ok_already_satisfied.
--
-- KEYS (5): quota_def, quota_window_zset, quota_concurrency_counter,
--           admitted_set, quota_policies_index
-- ARGV (5): quota_policy_id, window_seconds, max_requests_per_window,
--           max_concurrent, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_create_quota_policy', function(keys, args)
  local K = {
    def_key          = keys[1],
    window_zset      = keys[2],
    concurrency_key  = keys[3],
    admitted_set     = keys[4],
    policies_index   = keys[5],
  }

  local A = {
    quota_policy_id         = args[1],
    window_seconds          = args[2],
    max_requests_per_window = args[3],
    max_concurrent          = args[4],
    now_ms                  = args[5],
  }

  -- Idempotency: already exists → return immediately
  if redis.call("EXISTS", K.def_key) == 1 then
    return ok_already_satisfied(A.quota_policy_id)
  end

  -- HSET quota definition
  redis.call("HSET", K.def_key,
    "quota_policy_id", A.quota_policy_id,
    "requests_per_window_seconds", A.window_seconds,
    "max_requests_per_window", A.max_requests_per_window,
    "active_concurrency_cap", A.max_concurrent,
    "created_at", A.now_ms)

  -- Init concurrency counter to 0
  redis.call("SET", K.concurrency_key, "0")

  -- Register in partition-level policy index (for cluster-safe discovery)
  redis.call("SADD", K.policies_index, A.quota_policy_id)

  -- admitted_set + quota_window_zset left empty (populated on admission)

  return ok(A.quota_policy_id)
end)

---------------------------------------------------------------------------
-- #32  ff_check_admission_and_record  (on {q:K})
--
-- Idempotent sliding-window rate check + concurrency check.
-- If admitted: ZADD window, SET NX guard, optional INCR concurrency,
-- SADD to admitted_set (for cluster-safe reconciler discovery).
--
-- KEYS (5): window_zset, concurrency_counter, quota_def, admitted_guard_key,
--           admitted_set
-- ARGV (6): now_ms, window_seconds, rate_limit, concurrency_cap,
--           execution_id, jitter_ms
---------------------------------------------------------------------------
redis.register_function('ff_check_admission_and_record', function(keys, args)
  local K = {
    window_zset        = keys[1],
    concurrency_key    = keys[2],
    quota_def          = keys[3],
    admitted_guard_key = keys[4],
    admitted_set       = keys[5],
  }

  local now_ms_n          = require_number(args[1], "now_ms")
  if type(now_ms_n) == "table" then return now_ms_n end
  local window_seconds_n  = require_number(args[2], "window_seconds")
  if type(window_seconds_n) == "table" then return window_seconds_n end
  local rate_limit_n      = require_number(args[3], "rate_limit")
  if type(rate_limit_n) == "table" then return rate_limit_n end
  local concurrency_cap_n = require_number(args[4], "concurrency_cap")
  if type(concurrency_cap_n) == "table" then return concurrency_cap_n end

  local A = {
    now_ms          = now_ms_n,
    window_seconds  = window_seconds_n,
    rate_limit      = rate_limit_n,
    concurrency_cap = concurrency_cap_n,
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
  -- Guard: PX 0 or PX <0 causes Valkey error inside Lua (after ZADD committed).
  if window_ms > 0 then
    redis.call("SET", K.admitted_guard_key, "1", "PX", window_ms, "NX")
  end

  -- 7. Increment concurrency counter if cap is set
  if A.concurrency_cap > 0 then
    redis.call("INCR", K.concurrency_key)
  end

  -- 8. Track in admitted set (for cluster-safe reconciler — replaces SCAN)
  redis.call("SADD", K.admitted_set, A.execution_id)

  return { "ADMITTED" }
end)

---------------------------------------------------------------------------
-- ff_release_admission  (on {q:K})
--
-- Release a previously-recorded admission slot. Called when a claim grant
-- fails after admission was recorded, preventing leaked concurrency slots.
--
-- KEYS (3): admitted_guard_key, admitted_set, concurrency_counter
-- ARGV (1): execution_id
---------------------------------------------------------------------------
redis.register_function('ff_release_admission', function(keys, args)
  local K = {
    admitted_guard_key = keys[1],
    admitted_set       = keys[2],
    concurrency_key    = keys[3],
  }

  local execution_id = args[1]

  -- 1. Delete the guard key (idempotent — DEL returns 0 if absent)
  redis.call("DEL", K.admitted_guard_key)

  -- 2. Remove from admitted set
  redis.call("SREM", K.admitted_set, execution_id)

  -- 3. Decrement concurrency counter (floor at 0)
  local current = tonumber(redis.call("GET", K.concurrency_key) or "0")
  if current > 0 then
    redis.call("DECR", K.concurrency_key)
  end

  return {1, "OK", "released"}
end)
