-- FlowFabric scheduling functions
-- Reference: RFC-009 (Scheduling), RFC-010 §4 (function inventory)
--
-- Depends on helpers: ok, err, hgetall_to_table, is_set, validate_lease

---------------------------------------------------------------------------
-- #25  ff_issue_claim_grant
--
-- Scheduler issues a claim grant for an eligible execution.
-- Validates execution is eligible, writes grant hash with TTL,
-- removes from eligible set.
--
-- KEYS (3): exec_core, claim_grant_key, eligible_zset
-- ARGV (8): execution_id, worker_id, worker_instance_id,
--           lane_id, capability_hash, grant_ttl_ms,
--           route_snapshot_json, admission_summary
---------------------------------------------------------------------------
redis.register_function('ff_issue_claim_grant', function(keys, args)
  local K = {
    core_key       = keys[1],
    claim_grant    = keys[2],
    eligible_zset  = keys[3],
  }

  local A = {
    execution_id        = args[1],
    worker_id           = args[2],
    worker_instance_id  = args[3],
    lane_id             = args[4],
    capability_hash     = args[5] or "",
    route_snapshot_json = args[7] or "",
    admission_summary   = args[8] or "",
  }

  local grant_ttl_n = require_number(args[6], "grant_ttl_ms")
  if type(grant_ttl_n) == "table" then return grant_ttl_n end
  A.grant_ttl_ms = grant_ttl_n

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution exists and is eligible
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "runnable" then
    return err("execution_not_eligible")
  end
  if core.eligibility_state ~= "eligible_now" then
    return err("execution_not_eligible")
  end

  -- 2. Check no existing grant (prevent double-grant)
  if redis.call("EXISTS", K.claim_grant) == 1 then
    return err("grant_already_exists")
  end

  -- 3. Verify execution is in eligible set (TOCTOU guard)
  local score = redis.call("ZSCORE", K.eligible_zset, A.execution_id)
  if not score then
    return err("execution_not_in_eligible_set")
  end

  -- 4. Write grant hash with TTL
  local grant_expires_at = now_ms + A.grant_ttl_ms
  redis.call("HSET", K.claim_grant,
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "lane_id", A.lane_id,
    "capability_hash", A.capability_hash,
    "route_snapshot_json", A.route_snapshot_json,
    "admission_summary", A.admission_summary,
    "created_at", tostring(now_ms),
    "grant_expires_at", tostring(grant_expires_at))
  redis.call("PEXPIREAT", K.claim_grant, grant_expires_at)

  -- 5. Do NOT ZREM from eligible here. ff_claim_execution does the ZREM
  -- when consuming the grant. If the grant expires unconsumed, the execution
  -- remains in the eligible set and is re-discovered by the next scheduler
  -- cycle. This prevents the "orphaned grant" stuck state where an execution
  -- is in no scheduling index after grant expiry.

  return ok(A.execution_id)
end)

---------------------------------------------------------------------------
-- #32  ff_change_priority
--
-- Update priority and re-score in eligible ZSET.
-- Only works for runnable + eligible_now executions.
--
-- KEYS (2): exec_core, eligible_zset
-- ARGV (2): execution_id, new_priority
---------------------------------------------------------------------------
redis.register_function('ff_change_priority', function(keys, args)
  local K = {
    core_key      = keys[1],
    eligible_zset = keys[2],
  }

  local new_priority_n = require_number(args[2], "new_priority")
  if type(new_priority_n) == "table" then return new_priority_n end

  local A = {
    execution_id = args[1],
    new_priority = new_priority_n,
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Read and validate
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "runnable" then
    return err("execution_not_eligible")
  end
  if core.eligibility_state ~= "eligible_now" then
    return err("execution_not_eligible")
  end

  local old_priority = tonumber(core.priority or "0")

  -- Clamp to safe range (same as ff_create_execution)
  if A.new_priority < 0 then A.new_priority = 0 end
  if A.new_priority > 9000 then A.new_priority = 9000 end

  -- 2. Update exec_core priority
  redis.call("HSET", K.core_key,
    "priority", tostring(A.new_priority),
    "last_mutation_at", tostring(now_ms))

  -- 3. Re-score in eligible ZSET
  -- Composite score: -(priority * 1_000_000_000_000) + created_at_ms
  local created_at = tonumber(core.created_at or "0")
  local new_score = 0 - (A.new_priority * 1000000000000) + created_at
  redis.call("ZADD", K.eligible_zset, new_score, A.execution_id)

  return ok(tostring(old_priority), tostring(A.new_priority))
end)

---------------------------------------------------------------------------
-- #33  ff_update_progress
--
-- Update progress fields on exec_core. Validate lease (lite check:
-- lease_id + epoch only — attempt_id not required per §4 Class B).
--
-- KEYS (1): exec_core
-- ARGV (5): execution_id, lease_id, lease_epoch,
--           progress_pct, progress_message
---------------------------------------------------------------------------
redis.register_function('ff_update_progress', function(keys, args)
  local K = {
    core_key = keys[1],
  }

  local A = {
    execution_id    = args[1],
    lease_id        = args[2],
    lease_epoch     = args[3],
    progress_pct    = args[4] or "",
    progress_message = args[5] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- Read and validate
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "active" then
    return err("execution_not_active",
      core.terminal_outcome or "", core.current_lease_epoch or "")
  end
  if core.ownership_state == "lease_revoked" then
    return err("lease_revoked")
  end
  if tonumber(core.lease_expires_at or "0") <= now_ms then
    return err("lease_expired")
  end
  if core.current_lease_id ~= A.lease_id then
    return err("stale_lease")
  end
  if core.current_lease_epoch ~= A.lease_epoch then
    return err("stale_lease")
  end

  -- Update progress fields
  local fields = { "last_mutation_at", tostring(now_ms), "progress_updated_at", tostring(now_ms) }
  if is_set(A.progress_pct) then
    fields[#fields + 1] = "progress_pct"
    fields[#fields + 1] = A.progress_pct
  end
  if is_set(A.progress_message) then
    fields[#fields + 1] = "progress_message"
    fields[#fields + 1] = A.progress_message
  end
  redis.call("HSET", K.core_key, unpack(fields))

  return ok()
end)

---------------------------------------------------------------------------
-- #27  ff_promote_delayed
--
-- Promote a delayed execution to eligible when its delay_until has passed.
-- Called by the delayed promoter scanner.
-- Preserves attempt_state (may be pending_retry, pending_first, or
-- attempt_interrupted from delay_execution).
--
-- KEYS (3): exec_core, delayed_zset, eligible_zset
-- ARGV (2): execution_id, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_promote_delayed', function(keys, args)
  local K = {
    core_key      = keys[1],
    delayed_zset  = keys[2],
    eligible_zset = keys[3],
  }

  local A = {
    execution_id = args[1],
    now_ms       = tonumber(args[2]),
  }

  -- Read and validate
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    -- Execution gone — clean up stale index entry
    redis.call("ZREM", K.delayed_zset, A.execution_id)
    return ok("not_found_cleaned")
  end
  local core = hgetall_to_table(raw)

  -- Must be runnable + not_eligible_until_time
  if core.lifecycle_phase ~= "runnable" then
    redis.call("ZREM", K.delayed_zset, A.execution_id)
    return ok("not_runnable_cleaned")
  end
  if core.eligibility_state ~= "not_eligible_until_time" then
    redis.call("ZREM", K.delayed_zset, A.execution_id)
    return ok("not_delayed_cleaned")
  end

  -- Check delay_until has actually passed
  local delay_until = tonumber(core.delay_until or "0")
  if delay_until > A.now_ms then
    return ok("not_yet_due")
  end

  -- Promote: update 6 of 7 state vector dimensions.
  -- attempt_state is DELIBERATELY PRESERVED (not written). This is the 7th dim.
  --
  -- WHY: The caller that put this execution into the delayed set already set
  -- the attempt_state to reflect what should happen on next claim:
  --   * pending_retry_attempt  — from ff_fail_execution (retry backoff expired)
  --   * pending_replay_attempt — from ff_replay_execution (replay delay expired)
  --   * attempt_interrupted    — from ff_delay_execution (worker self-delay)
  --   * pending_first_attempt  — from ff_create_execution (initial delay_until)
  -- Overwriting it here would lose this routing information and break
  -- claim_execution's attempt_type derivation (initial vs retry vs replay)
  -- and the claim dispatch routing (claim_execution vs claim_resumed_execution).
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", "eligible_now",
    "blocking_reason", "waiting_for_worker",
    "blocking_detail", "",
    "terminal_outcome", "none",
    -- attempt_state: NOT WRITTEN — see comment above
    "public_state", "waiting",
    "delay_until", "",
    "last_transition_at", tostring(A.now_ms),
    "last_mutation_at", tostring(A.now_ms))

  -- ZREM from delayed, ZADD to eligible with composite priority score
  redis.call("ZREM", K.delayed_zset, A.execution_id)
  local priority = tonumber(core.priority or "0")
  local created_at = tonumber(core.created_at or "0")
  local score = 0 - (priority * 1000000000000) + created_at
  redis.call("ZADD", K.eligible_zset, score, A.execution_id)

  return ok("promoted")
end)

---------------------------------------------------------------------------
-- #26  ff_issue_reclaim_grant
--
-- Scheduler issues a reclaim grant for an expired/revoked execution.
-- Similar to ff_issue_claim_grant but validates reclaimable state.
--
-- KEYS (3): exec_core, claim_grant_key, lease_expiry_zset
-- ARGV (8): execution_id, worker_id, worker_instance_id,
--           lane_id, capability_hash, grant_ttl_ms,
--           route_snapshot_json, admission_summary
---------------------------------------------------------------------------
redis.register_function('ff_issue_reclaim_grant', function(keys, args)
  local K = {
    core_key       = keys[1],
    claim_grant    = keys[2],
    lease_expiry   = keys[3],
  }

  local A = {
    execution_id        = args[1],
    worker_id           = args[2],
    worker_instance_id  = args[3],
    lane_id             = args[4],
    capability_hash     = args[5] or "",
    route_snapshot_json = args[7] or "",
    admission_summary   = args[8] or "",
  }

  local grant_ttl_n = require_number(args[6], "grant_ttl_ms")
  if type(grant_ttl_n) == "table" then return grant_ttl_n end
  A.grant_ttl_ms = grant_ttl_n

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- Validate execution exists and is reclaimable
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "active" then
    return err("execution_not_reclaimable")
  end
  if core.ownership_state ~= "lease_expired_reclaimable"
    and core.ownership_state ~= "lease_revoked" then
    return err("execution_not_reclaimable")
  end

  -- Check no existing grant
  if redis.call("EXISTS", K.claim_grant) == 1 then
    return err("grant_already_exists")
  end

  -- Write grant hash with TTL
  local grant_expires_at = now_ms + A.grant_ttl_ms
  redis.call("HSET", K.claim_grant,
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "lane_id", A.lane_id,
    "capability_hash", A.capability_hash,
    "route_snapshot_json", A.route_snapshot_json,
    "admission_summary", A.admission_summary,
    "created_at", tostring(now_ms),
    "grant_expires_at", tostring(grant_expires_at))
  redis.call("PEXPIREAT", K.claim_grant, grant_expires_at)

  -- Do NOT ZREM from lease_expiry — stays for scheduler discovery

  return ok(A.execution_id)
end)
