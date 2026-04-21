-- FlowFabric lease management functions
-- Reference: RFC-003 (Lease and Fencing), RFC-010 §4 (function inventory)
--
-- Depends on helpers: ok, err, ok_already_satisfied, hgetall_to_table,
--   is_set, validate_lease, mark_expired

---------------------------------------------------------------------------
-- #6  ff_renew_lease
--
-- Extends a still-valid lease.  Preserves lease_id and lease_epoch.
-- KEYS (4): exec_core, lease_current, lease_history, lease_expiry_zset
-- ARGV (7): execution_id, attempt_index, attempt_id,
--           lease_id, lease_epoch,
--           lease_ttl_ms, lease_history_grace_ms
---------------------------------------------------------------------------
redis.register_function('ff_renew_lease', function(keys, args)
  -- Positional KEYS
  local K = {
    core_key         = keys[1],
    lease_current    = keys[2],
    lease_history    = keys[3],
    lease_expiry_key = keys[4],
  }

  -- Positional ARGV
  local lease_ttl_n = require_number(args[6], "lease_ttl_ms")
  if type(lease_ttl_n) == "table" then return lease_ttl_n end
  local grace_n = require_number(args[7], "lease_history_grace_ms")
  if type(grace_n) == "table" then return grace_n end

  local A = {
    execution_id         = args[1],
    attempt_index        = args[2],
    attempt_id           = args[3],
    lease_id             = args[4],
    lease_epoch          = args[5],
    lease_ttl_ms         = lease_ttl_n,
    lease_history_grace_ms = grace_n,
  }

  -- Server time
  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    return err("execution_not_found")
  end
  local core = hgetall_to_table(raw)

  -- Validate lifecycle
  if core.lifecycle_phase ~= "active" then
    return err("execution_not_active",
      core.terminal_outcome or "",
      core.current_lease_epoch or "",
      core.lifecycle_phase or "",
      core.current_attempt_id or "")
  end

  -- Check revocation
  if core.ownership_state == "lease_revoked" or is_set(core.lease_revoked_at) then
    return err("lease_revoked")
  end

  -- Check expiry (if expired, mark and reject)
  if tonumber(core.lease_expires_at or "0") <= now_ms then
    mark_expired(
      { core_key = K.core_key, lease_history_key = K.lease_history },
      core, now_ms, 1000)
    return err("lease_expired")
  end

  -- Validate caller identity
  if tostring(core.current_attempt_index) ~= A.attempt_index then
    return err("stale_lease")
  end
  if core.current_attempt_id ~= A.attempt_id then
    return err("stale_lease")
  end
  if core.current_lease_id ~= A.lease_id then
    return err("stale_lease")
  end
  if tostring(core.current_lease_epoch) ~= A.lease_epoch then
    return err("stale_lease")
  end

  -- Compute new deadlines
  local new_expires_at = now_ms + A.lease_ttl_ms
  local new_renewal_deadline = now_ms + math.floor(A.lease_ttl_ms * 2 / 3)

  -- Update exec_core
  redis.call("HSET", K.core_key,
    "lease_last_renewed_at", tostring(now_ms),
    "lease_renewal_deadline", tostring(new_renewal_deadline),
    "lease_expires_at", tostring(new_expires_at),
    "last_mutation_at", tostring(now_ms))

  -- Update lease_current
  redis.call("HSET", K.lease_current,
    "last_renewed_at", tostring(now_ms),
    "renewal_deadline", tostring(new_renewal_deadline),
    "expires_at", tostring(new_expires_at))
  redis.call("PEXPIREAT", K.lease_current,
    new_expires_at + A.lease_history_grace_ms)

  -- Update lease_expiry index
  redis.call("ZADD", K.lease_expiry_key, new_expires_at, A.execution_id)

  -- Renewal history event OFF for v1 (RFC-010 §4.8g).
  -- The exec_core field lease_last_renewed_at provides the latest renewal
  -- timestamp without stream overhead.  Enable per-lane when detailed
  -- ownership audit trails are needed.

  return ok(tostring(new_expires_at))
end)

---------------------------------------------------------------------------
-- #28  ff_mark_lease_expired_if_due
--
-- Called by the lease expiry scanner.  Re-validates that the lease is
-- actually expired (guards against renewal since the ZRANGEBYSCORE read).
-- Idempotent: no-op if already expired/reclaimed/revoked or not yet due.
--
-- KEYS (4): exec_core, lease_current, lease_expiry_zset, lease_history
-- ARGV (1): execution_id
---------------------------------------------------------------------------
redis.register_function('ff_mark_lease_expired_if_due', function(keys, args)
  -- Positional KEYS
  local K = {
    core_key         = keys[1],
    lease_current    = keys[2],
    lease_expiry_key = keys[3],
    lease_history    = keys[4],
  }

  local execution_id = args[1]

  -- Server time
  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    -- Execution gone — clean up stale index entry
    redis.call("ZREM", K.lease_expiry_key, execution_id)
    return ok_already_satisfied("execution_not_found")
  end
  local core = hgetall_to_table(raw)

  -- Guard: not active → nothing to expire
  if core.lifecycle_phase ~= "active" then
    -- Stale index entry: execution already left active phase.
    -- Clean up and return.
    redis.call("ZREM", K.lease_expiry_key, execution_id)
    return ok_already_satisfied("not_active")
  end

  -- Guard: already marked expired or ownership already cleared
  if core.ownership_state == "lease_expired_reclaimable" then
    return ok_already_satisfied("already_expired")
  end
  if core.ownership_state == "lease_revoked" then
    return ok_already_satisfied("already_revoked")
  end
  if core.ownership_state == "unowned" then
    -- Shouldn't happen for active phase, but defensive
    redis.call("ZREM", K.lease_expiry_key, execution_id)
    return ok_already_satisfied("unowned")
  end

  -- Check if lease is actually expired
  local expires_at = tonumber(core.lease_expires_at or "0")
  if expires_at > now_ms then
    -- Lease was renewed since the scanner read the index.
    -- The ZADD in renew_lease already updated the score.
    return ok_already_satisfied("not_yet_expired")
  end

  -- Mark expired using shared helper
  -- Sets ownership_state=lease_expired_reclaimable, blocking_reason,
  -- blocking_detail, lease_expired_at, XADD lease_history "expired" event.
  mark_expired(
    { core_key = K.core_key, lease_history_key = K.lease_history },
    core, now_ms, 1000)

  -- NOTE: Do NOT ZREM from lease_expiry_key here.
  -- The entry stays so the scheduler (or a subsequent scanner cycle)
  -- can discover reclaimable executions from the same index.
  -- reclaim_execution or cancel_execution removes it.

  return ok("marked_expired")
end)

---------------------------------------------------------------------------
-- #8  ff_revoke_lease
--
-- Operator-initiated lease revocation.  Immediate — no grace period.
-- Does NOT terminal-transition the execution.  It clears the current
-- owner and places the execution into lease_revoked ownership condition.
-- The scheduler or operator must subsequently reclaim or cancel.
--
-- KEYS (5): exec_core, lease_current, lease_history,
--           lease_expiry_zset, worker_leases
-- ARGV (3): execution_id, expected_lease_id (or "" to skip check),
--           revoke_reason
---------------------------------------------------------------------------
redis.register_function('ff_revoke_lease', function(keys, args)
  -- Positional KEYS
  local K = {
    core_key         = keys[1],
    lease_current    = keys[2],
    lease_history    = keys[3],
    lease_expiry_key = keys[4],
    worker_leases    = keys[5],
  }

  -- Positional ARGV
  local A = {
    execution_id     = args[1],
    expected_lease_id = args[2],
    revoke_reason    = args[3],
  }

  -- Server time
  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    return err("execution_not_found")
  end
  local core = hgetall_to_table(raw)

  -- Must be active + leased
  if core.lifecycle_phase ~= "active" then
    return err("execution_not_active",
      core.terminal_outcome or "",
      core.current_lease_epoch or "",
      core.lifecycle_phase or "",
      core.current_attempt_id or "")
  end
  if core.ownership_state ~= "leased" then
    if core.ownership_state == "lease_revoked" then
      return ok_already_satisfied("already_revoked")
    end
    if core.ownership_state == "lease_expired_reclaimable" then
      return ok_already_satisfied("already_expired")
    end
    return err("no_active_lease")
  end

  -- Optional lease_id check (for targeted revocation)
  if is_set(A.expected_lease_id) and core.current_lease_id ~= A.expected_lease_id then
    return err("stale_lease",
      "expected " .. A.expected_lease_id .. " but current is " .. (core.current_lease_id or ""))
  end

  -- Capture identity for history before clearing
  local lease_id = core.current_lease_id or ""
  local lease_epoch = core.current_lease_epoch or ""
  local attempt_index = core.current_attempt_index or ""
  local attempt_id = core.current_attempt_id or ""
  local worker_id = core.current_worker_id or ""
  local worker_instance_id = core.current_worker_instance_id or ""

  -- Update exec_core: all 7 state vector dimensions + public_state
  redis.call("HSET", K.core_key,
    -- 7 state vector dimensions
    "lifecycle_phase",   "active",
    "ownership_state",   "lease_revoked",
    "eligibility_state", core.eligibility_state or "not_applicable",
    "blocking_reason",   "waiting_for_worker",
    "blocking_detail",   "lease revoked: " .. (A.revoke_reason or "operator"),
    "terminal_outcome",  "none",
    "attempt_state",     "attempt_interrupted",
    -- Derived public state (RFC-001 §2.4 D3: active → public_state=active)
    "public_state",      "active",
    -- Revocation fields
    "lease_revoked_at",    tostring(now_ms),
    "lease_revoke_reason", A.revoke_reason or "operator",
    "last_mutation_at",    tostring(now_ms))

  -- DEL lease_current
  redis.call("DEL", K.lease_current)

  -- ZREM from lease expiry index
  redis.call("ZREM", K.lease_expiry_key, A.execution_id)

  -- SREM from worker leases set
  redis.call("SREM", K.worker_leases, A.execution_id)

  -- Append revoked event to lease history
  redis.call("XADD", K.lease_history, "MAXLEN", "~", 1000, "*",
    "event",              "revoked",
    "lease_id",           lease_id,
    "lease_epoch",        lease_epoch,
    "attempt_index",      attempt_index,
    "attempt_id",         attempt_id,
    "worker_id",          worker_id,
    "worker_instance_id", worker_instance_id,
    "reason",             A.revoke_reason or "operator",
    "ts",                 tostring(now_ms))

  return ok("revoked", lease_id, lease_epoch)
end)
-- NOTE: ff_issue_reclaim_grant is in scheduling.lua
-- NOTE: ff_reclaim_execution is in execution.lua
