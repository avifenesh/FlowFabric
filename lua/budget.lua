-- FlowFabric budget functions
-- Reference: RFC-008 (Budget), RFC-010 §4.3 (#30, #31), §4.1 (#29a, #29b)
--
-- Depends on helpers: ok, err, is_set, hgetall_to_table

---------------------------------------------------------------------------
-- ff_create_budget  (on {b:M})
--
-- Create a new budget policy with hard/soft limits on N dimensions.
-- Idempotent: if EXISTS budget_def → return ok_already_satisfied.
--
-- KEYS (5): budget_def, budget_limits, budget_usage, budget_resets_zset,
--           budget_policies_index
-- ARGV (variable): budget_id, scope_type, scope_id, enforcement_mode,
--   on_hard_limit, on_soft_limit, reset_interval_ms, now_ms,
--   dimension_count, dim_1..dim_N, hard_1..hard_N, soft_1..soft_N
---------------------------------------------------------------------------
redis.register_function('ff_create_budget', function(keys, args)
  local K = {
    def_key         = keys[1],
    limits_key      = keys[2],
    usage_key       = keys[3],
    resets_zset     = keys[4],
    policies_index  = keys[5],
  }

  local A = {
    budget_id         = args[1],
    scope_type        = args[2],
    scope_id          = args[3],
    enforcement_mode  = args[4],
    on_hard_limit     = args[5],
    on_soft_limit     = args[6],
    reset_interval_ms = args[7],
    now_ms            = args[8],
  }

  -- Idempotency: already exists → return immediately
  if redis.call("EXISTS", K.def_key) == 1 then
    return ok_already_satisfied(A.budget_id)
  end

  local dim_count = require_number(args[9], "dim_count")
  if type(dim_count) == "table" then return dim_count end

  -- HSET budget definition
  redis.call("HSET", K.def_key,
    "budget_id", A.budget_id,
    "scope_type", A.scope_type,
    "scope_id", A.scope_id,
    "enforcement_mode", A.enforcement_mode,
    "on_hard_limit", A.on_hard_limit,
    "on_soft_limit", A.on_soft_limit,
    "reset_interval_ms", A.reset_interval_ms,
    "breach_count", "0",
    "soft_breach_count", "0",
    "created_at", A.now_ms,
    "last_updated_at", A.now_ms)

  -- HSET per-dimension hard and soft limits
  for i = 1, dim_count do
    local dim  = args[9 + i]
    local hard = args[9 + dim_count + i]
    local soft = args[9 + 2 * dim_count + i]
    redis.call("HSET", K.limits_key, "hard:" .. dim, hard, "soft:" .. dim, soft)
  end

  -- budget_usage left empty — first report_usage will create fields

  -- Schedule periodic reset if reset_interval_ms > 0
  local interval_ms = tonumber(A.reset_interval_ms)
  if interval_ms > 0 then
    local next_reset_at = tostring(tonumber(A.now_ms) + interval_ms)
    redis.call("HSET", K.def_key, "next_reset_at", next_reset_at)
    redis.call("ZADD", K.resets_zset, tonumber(next_reset_at), A.budget_id)
  end

  -- Register in partition-level policy index (for cluster-safe discovery)
  redis.call("SADD", K.policies_index, A.budget_id)

  return ok(A.budget_id)
end)

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
-- ARGV (variable): dimension_count, dim_1..dim_N, delta_1..delta_N, now_ms, [dedup_key]
---------------------------------------------------------------------------
redis.register_function('ff_report_usage_and_check', function(keys, args)
  local K = {
    usage_key  = keys[1],
    limits_key = keys[2],
    def_key    = keys[3],
  }

  local dim_count = require_number(args[1], "dim_count")
  if type(dim_count) == "table" then return dim_count end
  local now_ms = args[2 * dim_count + 2]
  local dedup_key = args[2 * dim_count + 3] or ""

  -- Idempotency: if dedup_key provided, check for prior application
  if dedup_key ~= "" then
    local existing = redis.call("GET", dedup_key)
    if existing then
      return {1, "ALREADY_APPLIED"}
    end
  end

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
        return {1, "HARD_BREACH", dim, tostring(current), tostring(hard_limit)}
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

  -- Mark dedup key after successful increment (24h TTL)
  if dedup_key ~= "" then
    redis.call("SET", dedup_key, "1", "PX", 86400000)
  end

  if breached_soft then
    redis.call("HINCRBY", K.def_key, "soft_breach_count", 1)
    local soft_val = tonumber(redis.call("HGET", K.limits_key, "soft:" .. breached_soft) or "0")
    local cur_val = tonumber(redis.call("HGET", K.usage_key, breached_soft) or "0")
    return {1, "SOFT_BREACH", breached_soft, tostring(cur_val), tostring(soft_val)}
  end

  return {1, "OK"}
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
