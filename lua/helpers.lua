-- FlowFabric shared library-local helpers
-- These are local functions available to all registered functions in the
-- flowfabric library. They are NOT independently FCALL-able.
-- Reference: RFC-010 §4.8

---------------------------------------------------------------------------
-- Time
---------------------------------------------------------------------------

-- Returns the Valkey server time as milliseconds. Always prefer this over
-- a caller-supplied now_ms for fields that are used in retention windows,
-- eligibility scoring, lease expiry, or any cross-execution causal
-- comparison. Client-supplied timestamps are trivially skewable and
-- produce observability drift when compared against fields written by
-- other Lua functions (which already use redis.call("TIME")).
local function server_time_ms()
  local t = redis.call("TIME")
  return tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
end

---------------------------------------------------------------------------
-- Return wrappers (§4.9)
---------------------------------------------------------------------------

local function ok(...)
  return {1, "OK", ...}
end

local function err(...)
  return {0, ...}
end

-- Require a numeric value from ARGV. Returns the number on success or
-- an err() tuple on failure. Callers must check: if type(n) == "table"
-- then return n end  (the table IS the err tuple).
local function require_number(val, name)
  local n = tonumber(val)
  if n == nil then
    return err("invalid_input", name .. " must be a number, got: " .. tostring(val))
  end
  return n
end

local function ok_already_satisfied(...)
  return {1, "ALREADY_SATISFIED", ...}
end

local function ok_duplicate(...)
  return {1, "DUPLICATE", ...}
end

---------------------------------------------------------------------------
-- Data access
---------------------------------------------------------------------------

-- Converts HGETALL flat array {k1, v1, k2, v2, ...} to a Lua dict table.
-- All RFC pseudocode uses core.field syntax which requires this conversion.
local function hgetall_to_table(flat)
  local t = {}
  for i = 1, #flat, 2 do
    t[flat[i]] = flat[i + 1]
  end
  return t
end

-- Safe nil/empty check. Valkey hashes cannot store nil: HGET on a missing
-- field returns false (via Lua), and cleared fields store "". This helper
-- handles both cases plus actual nil for fields absent from hgetall_to_table.
local function is_set(v)
  return v ~= nil and v ~= false and v ~= ""
end

---------------------------------------------------------------------------
-- Lease validation (most widely shared — prevents copy-paste drift)
-- RFC-010 §4.8: 7+ functions use this: complete, fail, suspend, delay,
-- move_to_waiting_children, append_frame, report_usage.
---------------------------------------------------------------------------

-- Validates that the caller holds a valid, non-expired, non-revoked lease.
-- Returns an error tuple on failure, or nil on success.
-- @param core   table from hgetall_to_table(HGETALL exec_core)
-- @param argv   table with lease_id, lease_epoch, attempt_id
-- @param now_ms current timestamp in milliseconds
local function validate_lease(core, argv, now_ms)
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
  if core.current_lease_id ~= argv.lease_id then
    return err("stale_lease")
  end
  if core.current_lease_epoch ~= argv.lease_epoch then
    return err("stale_lease")
  end
  if core.current_attempt_id ~= argv.attempt_id then
    return err("stale_lease")
  end
  return nil
end

-- Sets ownership_state to lease_expired_reclaimable. Idempotent.
-- @param keys   table with core_key, lease_history_key
-- @param core   table from hgetall_to_table
-- @param now_ms current timestamp in milliseconds
-- @param maxlen MAXLEN for lease_history stream
local function mark_expired(keys, core, now_ms, maxlen)
  if core.ownership_state == "lease_expired_reclaimable" then
    return -- idempotent
  end
  -- ALL 7 dims (preserve lifecycle_phase=active, eligibility_state, terminal_outcome=none)
  redis.call("HSET", keys.core_key,
    "lifecycle_phase", core.lifecycle_phase or "active",     -- preserve
    "ownership_state", "lease_expired_reclaimable",
    "eligibility_state", core.eligibility_state or "not_applicable", -- preserve
    "blocking_reason", "waiting_for_worker",
    "blocking_detail", "lease expired, awaiting reclaim",
    "terminal_outcome", core.terminal_outcome or "none",     -- preserve
    "attempt_state", "attempt_interrupted",
    "public_state", "active",
    "lease_expired_at", now_ms,
    "last_mutation_at", now_ms)
  redis.call("XADD", keys.lease_history_key, "MAXLEN", "~", maxlen, "*",
    "event", "expired",
    "lease_id", core.current_lease_id or "",
    "lease_epoch", core.current_lease_epoch or "",
    "attempt_index", core.current_attempt_index or "",
    "attempt_id", core.current_attempt_id or "",
    "worker_id", core.current_worker_id or "",
    "worker_instance_id", core.current_worker_instance_id or "",
    "ts", now_ms)
end

-- Validates lease AND atomically marks expired if lease has lapsed.
-- Use this variant for write-path callers (complete, fail, suspend, delay,
-- move_to_waiting_children) that have the lease_history key available.
-- For read-only callers (append_frame, report_usage) use validate_lease.
-- @param core   table from hgetall_to_table
-- @param argv   table with lease_id, lease_epoch, attempt_id
-- @param now_ms current timestamp in milliseconds
-- @param keys   table with core_key, lease_history_key
-- @param maxlen MAXLEN for lease_history stream
local function validate_lease_and_mark_expired(core, argv, now_ms, keys, maxlen)
  if core.lifecycle_phase ~= "active" then
    return err("execution_not_active",
      core.terminal_outcome or "", core.current_lease_epoch or "")
  end
  if core.ownership_state == "lease_revoked" then
    return err("lease_revoked")
  end
  if tonumber(core.lease_expires_at or "0") <= now_ms then
    mark_expired(keys, core, now_ms, maxlen)
    return err("lease_expired")
  end
  if core.current_lease_id ~= argv.lease_id then
    return err("stale_lease")
  end
  if core.current_lease_epoch ~= argv.lease_epoch then
    return err("stale_lease")
  end
  if core.current_attempt_id ~= argv.attempt_id then
    return err("stale_lease")
  end
  return nil
end

-- Consolidates the ~15-line lease release block shared by 7 functions.
-- DEL lease_current, ZREM lease_expiry + worker_leases + active_index,
-- clear lease fields on exec_core, XADD lease_history "released".
-- @param keys    table with lease_current_key, lease_expiry_key,
--                worker_leases_key, active_index_key, lease_history_key,
--                attempt_timeout_key, core_key
-- @param core    table from hgetall_to_table
-- @param reason  string reason for release (e.g. "completed", "suspend")
-- @param now_ms  current timestamp in milliseconds
-- @param maxlen  MAXLEN for lease_history stream
local function clear_lease_and_indexes(keys, core, reason, now_ms, maxlen)
  local eid = core.execution_id or ""

  -- DEL lease record
  redis.call("DEL", keys.lease_current_key)

  -- ZREM/SREM from scheduling indexes
  redis.call("ZREM", keys.lease_expiry_key, eid)
  redis.call("SREM", keys.worker_leases_key, eid)
  redis.call("ZREM", keys.active_index_key, eid)
  redis.call("ZREM", keys.attempt_timeout_key, eid)

  -- Clear lease fields on exec_core (including stale expiry/revocation markers)
  redis.call("HSET", keys.core_key,
    "current_lease_id", "",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "lease_expires_at", "",
    "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "lease_expired_at", "",
    "lease_revoked_at", "",
    "lease_revoke_reason", "",
    "last_mutation_at", now_ms)

  -- Lease history event
  redis.call("XADD", keys.lease_history_key, "MAXLEN", "~", maxlen, "*",
    "event", "released",
    "lease_id", core.current_lease_id or "",
    "lease_epoch", core.current_lease_epoch or "",
    "attempt_index", core.current_attempt_index or "",
    "attempt_id", core.current_attempt_id or "",
    "reason", reason,
    "ts", now_ms)
end

---------------------------------------------------------------------------
-- Defensive index cleanup
-- RFC-010 §4.8: ZREM execution_id from all scheduling + timeout indexes
-- except except_key. ~14 ZREM/SREM calls.
---------------------------------------------------------------------------

-- @param keys       table with all index key names
-- @param eid        execution_id string
-- @param except_key optional key to skip (the target index for this transition)
local function defensive_zrem_all_indexes(keys, eid, except_key)
  -- Each index key and whether it uses ZREM or SREM
  local zrem_keys = {
    keys.eligible_key,
    keys.delayed_key,
    keys.active_index_key,
    keys.suspended_key,
    keys.terminal_key,
    keys.blocked_deps_key,
    keys.blocked_budget_key,
    keys.blocked_quota_key,
    keys.blocked_route_key,
    keys.blocked_operator_key,
    keys.lease_expiry_key,
    keys.suspension_timeout_key,
    keys.attempt_timeout_key,
    keys.execution_deadline_key,
  }
  for _, k in ipairs(zrem_keys) do
    if k and k ~= except_key then
      redis.call("ZREM", k, eid)
    end
  end
  -- worker_leases is a SET, not ZSET
  if keys.worker_leases_key and keys.worker_leases_key ~= except_key then
    redis.call("SREM", keys.worker_leases_key, eid)
  end
end

---------------------------------------------------------------------------
-- Suspension reason → blocking_reason mapping
-- RFC-004 §Suspension Reason Categories
---------------------------------------------------------------------------

local REASON_TO_BLOCKING = {
  waiting_for_signal       = "waiting_for_signal",
  waiting_for_approval     = "waiting_for_approval",
  waiting_for_callback     = "waiting_for_callback",
  waiting_for_tool_result  = "waiting_for_tool_result",
  waiting_for_operator_review = "paused_by_operator",
  paused_by_policy         = "paused_by_policy",
  paused_by_budget         = "waiting_for_budget",
  step_boundary            = "waiting_for_signal",
  manual_pause             = "paused_by_operator",
}

local function map_reason_to_blocking(reason_code)
  return REASON_TO_BLOCKING[reason_code] or "waiting_for_signal"
end

---------------------------------------------------------------------------
-- Resume condition evaluation (shared module)
-- RFC-004 §Resume Condition Model, RFC-005 §8.3
---------------------------------------------------------------------------

-- Parse resume_condition_json into a matcher table used by
-- evaluate_signal_against_condition and is_condition_satisfied.
-- @param json  JSON string of the resume condition
-- @return table with matchers array, match_mode, minimum_signal_count, etc.
local function initialize_condition(json)
  local spec = cjson.decode(json)
  local matchers = {}
  local names = spec.required_signal_names or {}
  if #names == 0 then
    -- Empty required_signal_names acts as wildcard: ANY signal satisfies the condition.
    -- To require explicit operator resume (no signal match), pass a sentinel name
    -- that no real signal will match, or use a different resume mechanism.
    matchers[1] = { name = "", satisfied = false, signal_id = "" }
  else
    for i, name in ipairs(names) do
      matchers[i] = { name = name, satisfied = false, signal_id = "" }
    end
  end
  return {
    condition_type       = spec.condition_type or "signal_set",
    match_mode           = spec.signal_match_mode or "any",
    minimum_signal_count = tonumber(spec.minimum_signal_count or "1"),
    total_matchers       = #names > 0 and #names or 1,
    satisfied_count      = 0,
    matchers             = matchers,
    closed               = false,
  }
end

-- Write condition state to a dedicated condition hash key.
-- @param key    Valkey key for the condition hash
-- @param cond   condition table from initialize_condition
-- @param now_ms current timestamp
local function write_condition_hash(key, cond, now_ms)
  local fields = {
    "condition_type", cond.condition_type,
    "match_mode", cond.match_mode,
    "minimum_signal_count", tostring(cond.minimum_signal_count),
    "total_matchers", tostring(cond.total_matchers),
    "satisfied_count", tostring(cond.satisfied_count),
    "closed", cond.closed and "1" or "0",
    "updated_at", tostring(now_ms),
  }
  for i = 1, cond.total_matchers do
    local m = cond.matchers[i]
    local idx = i - 1  -- external field names remain 0-based for wire compat
    fields[#fields + 1] = "matcher:" .. idx .. ":name"
    fields[#fields + 1] = m.name
    fields[#fields + 1] = "matcher:" .. idx .. ":satisfied"
    fields[#fields + 1] = m.satisfied and "1" or "0"
    fields[#fields + 1] = "matcher:" .. idx .. ":signal_id"
    fields[#fields + 1] = m.signal_id
  end
  redis.call("HSET", key, unpack(fields))
end

-- Match a signal against the condition's matchers. Mutates cond in-place.
-- @param cond        condition table from initialize_condition
-- @param signal_name signal name string
-- @param signal_id   signal ID string
-- @return true if this signal matched a matcher, false otherwise
local function evaluate_signal_against_condition(cond, signal_name, signal_id)
  for i = 1, cond.total_matchers do
    local m = cond.matchers[i]
    if not m.satisfied then
      -- Empty name = wildcard matcher (matches any signal)
      if m.name == "" or m.name == signal_name then
        m.satisfied = true
        m.signal_id = signal_id or ""
        cond.satisfied_count = cond.satisfied_count + 1
        return true
      end
    end
  end
  return false
end

-- Check if the overall condition is satisfied based on mode.
-- @param cond  condition table
-- @return true if condition is satisfied
local function is_condition_satisfied(cond)
  local mode = cond.match_mode
  local min_count = cond.minimum_signal_count
  if mode == "any" then
    return cond.satisfied_count >= min_count
  elseif mode == "all" then
    return cond.satisfied_count >= cond.total_matchers
  end
  -- count(n) mode — same as any with minimum_signal_count = n
  return cond.satisfied_count >= min_count
end

-- Extract a named field from a Valkey Stream entry's flat field array.
-- Stream entries return {id, {field1, val1, field2, val2, ...}}.
-- This operates on the inner flat array.
-- @param fields flat array from stream entry[2]
-- @param name   field name to extract
-- @return value string or nil
local function extract_field(fields, name)
  for i = 1, #fields, 2 do
    if fields[i] == name then
      return fields[i + 1]
    end
  end
  return nil
end

---------------------------------------------------------------------------
-- Suspension helpers (RFC-004)
---------------------------------------------------------------------------

-- Returns an empty signal summary JSON string for initial suspension record.
local function initial_signal_summary_json()
  return '{"total_count":0,"matched_count":0,"signal_names":[]}'
end

-- Validates that a pending waitpoint can be activated by a suspension.
-- Returns error tuple on failure, nil on success.
-- @param wp_raw   flat array from HGETALL on waitpoint hash
-- @param eid      expected execution_id
-- @param att_idx  expected attempt_index (string)
-- @param now_ms   current timestamp
local function validate_pending_waitpoint(wp_raw, eid, att_idx, now_ms)
  if #wp_raw == 0 then
    return err("waitpoint_not_found")
  end
  local wp = hgetall_to_table(wp_raw)
  if wp.state ~= "pending" then
    return err("waitpoint_not_pending")
  end
  if wp.execution_id ~= eid then
    return err("invalid_waitpoint_for_execution")
  end
  if tostring(wp.attempt_index) ~= tostring(att_idx) then
    return err("invalid_waitpoint_for_execution")
  end
  -- Check if pending waitpoint has expired
  if is_set(wp.expires_at) and tonumber(wp.expires_at) <= now_ms then
    return err("pending_waitpoint_expired")
  end
  return nil
end

-- Validates that a suspension record exists and is open (not closed).
-- Returns error tuple on failure, nil on success. Also returns the parsed table.
-- @param susp_raw flat array from HGETALL on suspension:current
local function assert_active_suspension(susp_raw)
  if #susp_raw == 0 then
    return err("execution_not_suspended")
  end
  local susp = hgetall_to_table(susp_raw)
  if not is_set(susp.suspension_id) then
    return err("execution_not_suspended")
  end
  if is_set(susp.closed_at) then
    return err("execution_not_suspended")
  end
  return nil, susp
end

-- Validates that a waitpoint belongs to the expected execution + suspension.
-- Returns error tuple on failure, nil on success.
-- @param wp_raw   flat array from HGETALL on waitpoint hash
-- @param eid      expected execution_id
-- @param sid      expected suspension_id
-- @param wid      expected waitpoint_id
local function assert_waitpoint_belongs(wp_raw, eid, sid, wid)
  if #wp_raw == 0 then
    return err("waitpoint_not_found")
  end
  local wp = hgetall_to_table(wp_raw)
  if wp.execution_id ~= eid then
    return err("invalid_waitpoint_for_execution")
  end
  if is_set(sid) and wp.suspension_id ~= sid then
    return err("invalid_waitpoint_for_execution")
  end
  if is_set(wid) and wp.waitpoint_id ~= wid then
    return err("invalid_waitpoint_for_execution")
  end
  return nil
end

---------------------------------------------------------------------------
-- Policy
---------------------------------------------------------------------------

-- Decode a JSON policy string into flat key-value pairs suitable for HSET.
-- @param json  JSON string of the policy object
-- @return flat array {k1, v1, k2, v2, ...} for use with redis.call("HSET", key, unpack(...))
local function unpack_policy(json)
  local policy = cjson.decode(json)
  local flat = {}
  for k, v in pairs(policy) do
    flat[#flat + 1] = k
    if type(v) == "table" then
      flat[#flat + 1] = cjson.encode(v)
    else
      flat[#flat + 1] = tostring(v)
    end
  end
  return flat
end
