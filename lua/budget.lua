-- FlowFabric budget functions
-- Reference: RFC-008 (Budget), RFC-010 §4.3 (#30, #31), §4.1 (#29a, #29b)
--
-- Depends on helpers: ok, err, is_set, hgetall_to_table

---------------------------------------------------------------------------
-- #30  ff_report_usage_and_check  (on {b:M})
--
-- Check-before-increment: read current usage, check hard limits. If any
-- dimension would breach, return HARD_BREACH without incrementing. If safe,
-- HINCRBY all dimensions, then check soft limits.
--
-- Atomic Lua serialization on {b:M} guarantees zero overshoot.
--
-- KEYS (3): budget_usage, budget_limits, budget_def
-- ARGV (variable): dimension_count, dim_1..dim_N, delta_1..delta_N, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_report_usage_and_check', function(keys, args)
  local K = {
    usage_key  = keys[1],
    limits_key = keys[2],
    def_key    = keys[3],
  }

  local dim_count = tonumber(args[1])
  local now_ms = args[2 * dim_count + 2]

  -- Phase 1: CHECK all dimensions BEFORE any increment.
  -- If any hard limit would be breached, reject the entire report.
  for i = 1, dim_count do
    local dim = args[1 + i]
    local delta = tonumber(args[1 + dim_count + i])
    local current = tonumber(redis.call("HGET", K.usage_key, dim) or "0")
    local new_total = current + delta

    local hard_limit = redis.call("HGET", K.limits_key, "hard:" .. dim)
    if hard_limit and hard_limit ~= "" and hard_limit ~= false then
      local limit_val = tonumber(hard_limit)
      if limit_val > 0 and new_total > limit_val then
        -- Record breach metadata but DO NOT increment
        redis.call("HINCRBY", K.def_key, "breach_count", 1)
        redis.call("HSET", K.def_key,
          "last_breach_at", now_ms,
          "last_breach_dim", dim,
          "last_updated_at", now_ms)
        local action = redis.call("HGET", K.def_key, "on_hard_limit") or "fail"
        return { "HARD_BREACH", dim, action, tostring(current), tostring(hard_limit) }
      end
    end
  end

  -- Phase 2: No hard breach detected — safe to increment all dimensions.
  local breached_soft = nil
  for i = 1, dim_count do
    local dim = args[1 + i]
    local delta = tonumber(args[1 + dim_count + i])
    local new_val = redis.call("HINCRBY", K.usage_key, dim, delta)

    -- Check soft limit (advisory — increment still happens)
    local soft_limit = redis.call("HGET", K.limits_key, "soft:" .. dim)
    if soft_limit and soft_limit ~= "" and soft_limit ~= false then
      local limit_val = tonumber(soft_limit)
      if limit_val > 0 and new_val > limit_val then
        if not breached_soft then
          breached_soft = dim
        end
      end
    end
  end

  -- Update metadata
  redis.call("HSET", K.def_key, "last_updated_at", now_ms)

  if breached_soft then
    redis.call("HINCRBY", K.def_key, "soft_breach_count", 1)
    local action = redis.call("HGET", K.def_key, "on_soft_limit") or "warn"
    return { "SOFT_BREACH", breached_soft, action }
  end

  return { "OK" }
end)

---------------------------------------------------------------------------
-- #31  ff_reset_budget  (on {b:M})
--
-- Scanner-called periodic reset. Zero all usage fields, record reset,
-- compute next_reset_at, re-score in reset index.
--
-- KEYS (3): budget_def, budget_usage, budget_resets_zset
-- ARGV (2): budget_id, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_reset_budget', function(keys, args)
  local K = {
    def_key       = keys[1],
    usage_key     = keys[2],
    resets_zset   = keys[3],
  }

  local A = {
    budget_id = args[1],
    now_ms    = args[2],
  }

  -- 1. Read usage fields and zero them
  local usage_fields = redis.call("HKEYS", K.usage_key)
  if #usage_fields > 0 then
    local zero_args = {}
    for _, field in ipairs(usage_fields) do
      zero_args[#zero_args + 1] = field
      zero_args[#zero_args + 1] = "0"
    end
    redis.call("HSET", K.usage_key, unpack(zero_args))
  end

  -- 2. Update budget_def: last_reset_at, reset_count
  redis.call("HINCRBY", K.def_key, "reset_count", 1)
  redis.call("HSET", K.def_key,
    "last_reset_at", A.now_ms,
    "last_updated_at", A.now_ms,
    "last_breach_at", "",
    "last_breach_dim", "")

  -- 3. Compute next_reset_at from reset_interval_ms
  local interval_ms = tonumber(redis.call("HGET", K.def_key, "reset_interval_ms") or "0")
  local next_reset_at = "0"
  if interval_ms > 0 then
    next_reset_at = tostring(tonumber(A.now_ms) + interval_ms)
    redis.call("HSET", K.def_key, "next_reset_at", next_reset_at)
    redis.call("ZADD", K.resets_zset, tonumber(next_reset_at), A.budget_id)
  else
    -- No recurring reset — remove from schedule
    redis.call("ZREM", K.resets_zset, A.budget_id)
  end

  return ok(next_reset_at)
end)

---------------------------------------------------------------------------
-- #29b  ff_unblock_execution  (on {p:N})
--
-- Re-evaluate blocked execution, set eligible_now, ZREM blocked set,
-- ZADD eligible. All 7 dims.
--
-- KEYS (3): exec_core, blocked_zset, eligible_zset
-- ARGV (3): execution_id, now_ms, expected_blocking_reason
---------------------------------------------------------------------------
redis.register_function('ff_unblock_execution', function(keys, args)
  local K = {
    core_key     = keys[1],
    blocked_zset = keys[2],
    eligible_zset = keys[3],
  }

  local A = {
    execution_id             = args[1],
    now_ms                   = args[2],
    expected_blocking_reason = args[3] or "",
  }

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- 2. Must be runnable
  if core.lifecycle_phase ~= "runnable" then
    return err("execution_not_eligible")
  end

  -- 3. Must be blocked
  local es = core.eligibility_state
  if es ~= "blocked_by_budget" and es ~= "blocked_by_quota"
     and es ~= "blocked_by_route" and es ~= "blocked_by_operator" then
    return err("execution_not_eligible")
  end

  -- 4. Validate expected blocking reason (prevent stale unblock)
  if A.expected_blocking_reason ~= "" then
    if core.blocking_reason ~= A.expected_blocking_reason then
      return err("execution_not_eligible")
    end
  end

  -- 5. Transition: all 7 dims
  local priority = tonumber(core.priority or "0")
  local created_at = tonumber(core.created_at or "0")
  local score = 0 - (priority * 1000000000000) + created_at

  redis.call("HSET", K.core_key,
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", "eligible_now",
    "blocking_reason", "waiting_for_worker",
    "blocking_detail", "",
    "terminal_outcome", "none",
    "attempt_state", core.attempt_state or "pending_first_attempt",
    "public_state", "waiting",
    "last_transition_at", A.now_ms,
    "last_mutation_at", A.now_ms)

  -- 6. Move from blocked to eligible
  redis.call("ZREM", K.blocked_zset, A.execution_id)
  redis.call("ZADD", K.eligible_zset, score, A.execution_id)

  return ok("unblocked")
end)

---------------------------------------------------------------------------
-- #29a  ff_block_execution_for_admission  (on {p:N})
--
-- Parameterized block: set eligibility/blocking for budget/quota/route/lane
-- denial, ZREM eligible, ZADD target blocked set. All 7 dims.
--
-- KEYS (3): exec_core, eligible_zset, target_blocked_zset
-- ARGV (4): execution_id, blocking_reason, blocking_detail, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_block_execution_for_admission', function(keys, args)
  local K = {
    core_key       = keys[1],
    eligible_zset  = keys[2],
    blocked_zset   = keys[3],
  }

  local A = {
    execution_id    = args[1],
    blocking_reason = args[2],
    blocking_detail = args[3] or "",
    now_ms          = args[4],
  }

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- 2. Must be runnable
  if core.lifecycle_phase ~= "runnable" then
    if core.lifecycle_phase == "terminal" then
      return err("execution_not_active", core.terminal_outcome or "", core.current_lease_epoch or "")
    end
    return err("execution_not_eligible")
  end

  -- 3. Map blocking_reason to eligibility_state
  local REASON_TO_ELIGIBILITY = {
    waiting_for_budget         = "blocked_by_budget",
    waiting_for_quota          = "blocked_by_quota",
    waiting_for_capable_worker = "blocked_by_route",
    paused_by_operator         = "blocked_by_operator",
    paused_by_policy           = "blocked_by_lane_state",
  }
  local eligibility = REASON_TO_ELIGIBILITY[A.blocking_reason]
  if not eligibility then
    return err("invalid_blocking_reason")
  end

  -- 4. Transition: all 7 dims
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", eligibility,
    "blocking_reason", A.blocking_reason,
    "blocking_detail", A.blocking_detail,
    "terminal_outcome", "none",
    "attempt_state", core.attempt_state or "pending_first_attempt",
    "public_state", "rate_limited",
    "last_transition_at", A.now_ms,
    "last_mutation_at", A.now_ms)

  -- 5. Move from eligible to blocked
  redis.call("ZREM", K.eligible_zset, A.execution_id)
  redis.call("ZADD", K.blocked_zset, tonumber(A.now_ms), A.execution_id)

  return ok("blocked")
end)
