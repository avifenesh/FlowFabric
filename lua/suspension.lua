-- FlowFabric suspension and waitpoint functions
-- Reference: RFC-004 (Suspension), RFC-005 (Signal), RFC-010 §4
--
-- Depends on helpers: ok, err, ok_already_satisfied, hgetall_to_table,
--   is_set, validate_lease_and_mark_expired, clear_lease_and_indexes,
--   map_reason_to_blocking, initialize_condition, write_condition_hash,
--   evaluate_signal_against_condition, is_condition_satisfied,
--   extract_field, initial_signal_summary_json, validate_pending_waitpoint,
--   assert_active_suspension, assert_waitpoint_belongs

---------------------------------------------------------------------------
-- #13  ff_suspend_execution
--
-- Validate lease, release ownership, create suspension + waitpoint
-- (or activate pending), init condition, transition active → suspended.
-- Mints the waitpoint HMAC token (RFC-004 §Waitpoint Security) returned
-- alongside the waitpoint_id for external signal delivery.
--
-- KEYS (18): exec_core, attempt_record, lease_current, lease_history,
--            lease_expiry_zset, worker_leases, suspension_current,
--            waitpoint_hash, waitpoint_signals, suspension_timeout_zset,
--            pending_wp_expiry_zset, active_index, suspended_zset,
--            waitpoint_history, wp_condition, attempt_timeout_zset,
--            hmac_secrets, dedup_hash (RFC-013)
-- ARGV (19): execution_id, attempt_index, attempt_id, lease_id,
--            lease_epoch, suspension_id, waitpoint_id, waitpoint_key,
--            reason_code, requested_by, timeout_at, resume_condition_json,
--            resume_policy_json, continuation_metadata_pointer,
--            use_pending_waitpoint, timeout_behavior, lease_history_maxlen,
--            idempotency_key (RFC-013), dedup_ttl_ms (RFC-013)
---------------------------------------------------------------------------
redis.register_function('ff_suspend_execution', function(keys, args)
  local K = {
    core_key              = keys[1],
    attempt_record        = keys[2],
    lease_current_key     = keys[3],
    lease_history_key     = keys[4],
    lease_expiry_key      = keys[5],
    worker_leases_key     = keys[6],
    suspension_current    = keys[7],
    waitpoint_hash        = keys[8],
    waitpoint_signals     = keys[9],
    suspension_timeout_key = keys[10],
    pending_wp_expiry_key = keys[11],
    active_index_key      = keys[12],
    suspended_zset        = keys[13],
    waitpoint_history     = keys[14],
    wp_condition          = keys[15],
    attempt_timeout_key   = keys[16],
    hmac_secrets          = keys[17],
    dedup_hash            = keys[18],
  }

  local A = {
    execution_id              = args[1],
    attempt_index             = args[2],
    attempt_id                = args[3] or "",
    lease_id                  = args[4] or "",
    lease_epoch               = args[5] or "",
    suspension_id             = args[6],
    waitpoint_id              = args[7],
    waitpoint_key             = args[8],
    reason_code               = args[9],
    requested_by              = args[10],
    timeout_at                = args[11] or "",
    resume_condition_json     = args[12],
    resume_policy_json        = args[13],
    continuation_metadata_ptr = args[14] or "",
    use_pending_waitpoint     = args[15] or "",
    timeout_behavior          = args[16] or "fail",
    lease_history_maxlen      = tonumber(args[17] or "1000"),
    idempotency_key           = args[18] or "",
    dedup_ttl_ms              = tonumber(args[19] or "0"),
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- RFC-013 §9.2 — idempotency_key dedup branch. When the caller set an
  -- idempotency_key, check the dedup hash first: a hit short-circuits with
  -- the previously-serialized outcome verbatim, performing no state
  -- mutation. On miss, fall through to the canonical path and write the
  -- outcome into the dedup hash with TTL after commit.
  local _dedup_active = (A.idempotency_key ~= "" and K.dedup_hash and K.dedup_hash ~= "")
  if _dedup_active then
    local stored = redis.call("HGET", K.dedup_hash, "outcome")
    if stored == false then stored = nil end
    if stored then
      -- Stored format pins v=1 per §9.2 "Serialized-outcome stability":
      -- "<status>\t<suspension_id>\t<waitpoint_id>\t<waitpoint_key>\t<waitpoint_token>".
      local parts = {}
      local idx = 1
      for piece in string.gmatch(stored, "([^\t]*)\t?") do
        parts[idx] = piece
        idx = idx + 1
        if idx > 5 then break end
      end
      if #parts >= 5 then
        if parts[1] == "ALREADY_SATISFIED" then
          return ok_already_satisfied(parts[2], parts[3], parts[4], parts[5])
        else
          return ok(parts[2], parts[3], parts[4], parts[5])
        end
      end
      -- Malformed entry: fall through and treat as miss.
    end
  end

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- RFC #58.5 — suspend_execution is lease-bound. Hard-reject empty or
  -- partial fence triples; no operator-override escape hatch.
  local fence, must_check_or_err = resolve_lease_fence(core, A)
  if not fence then return must_check_or_err end
  if not must_check_or_err then return err("fence_required") end

  -- 2. Validate lease (full check incl. expiry + revocation + identity)
  local lease_err = validate_lease_and_mark_expired(
    core, A, now_ms, K, A.lease_history_maxlen)
  if lease_err then return lease_err end

  -- 3. Validate attempt binding
  if tostring(core.current_attempt_index) ~= A.attempt_index then
    return err("invalid_lease_for_suspend")
  end

  -- 4. Check for existing suspension: reject if open, archive if closed
  if redis.call("EXISTS", K.suspension_current) == 1 then
    local closed = redis.call("HGET", K.suspension_current, "closed_at")
    if not is_set(closed) then
      return err("already_suspended")
    end
    -- Previous suspension is closed. Archive old waitpoint_id for cleanup,
    -- then DEL the stale record before creating a new one.
    local old_wp = redis.call("HGET", K.suspension_current, "waitpoint_id")
    if is_set(old_wp) then
      redis.call("SADD", K.waitpoint_history, old_wp)
    end
    redis.call("DEL", K.suspension_current)
  end

  -- 5. Create or activate waitpoint
  local waitpoint_id = A.waitpoint_id
  local waitpoint_key = A.waitpoint_key
  local waitpoint_token = ""

  if A.use_pending_waitpoint == "1" then
    -- Activate existing pending waitpoint
    local wp_raw = redis.call("HGETALL", K.waitpoint_hash)
    local wp_err = validate_pending_waitpoint(wp_raw, A.execution_id, A.attempt_index, now_ms)
    if wp_err then return wp_err end

    -- Read waitpoint_id, waitpoint_key, and existing token from pending record.
    -- Token was minted at ff_create_pending_waitpoint time with that record's
    -- created_at; we MUST keep using it so signals buffered before activation
    -- validate against the same binding.
    local wp = hgetall_to_table(wp_raw)
    waitpoint_id = wp.waitpoint_id
    waitpoint_key = wp.waitpoint_key
    -- A pending waitpoint without a minted token is either a pre-HMAC-upgrade
    -- record or a corrupted write. Activating with an empty token would return
    -- "" to the SDK, and every subsequent signal delivery would reject with
    -- missing_token — fail-closed at the security boundary but silent about
    -- the real degraded state. Surface the degradation AT the activation
    -- point so operators see it immediately.
    if not is_set(wp.waitpoint_token) then
      return err("waitpoint_not_token_bound")
    end
    waitpoint_token = wp.waitpoint_token

    -- Activate the pending waitpoint
    redis.call("HSET", K.waitpoint_hash,
      "suspension_id", A.suspension_id,
      "state", "active",
      "activated_at", tostring(now_ms),
      "expires_at", is_set(A.timeout_at) and A.timeout_at or "")
    redis.call("ZREM", K.pending_wp_expiry_key, waitpoint_id)

    -- CRITICAL: Evaluate buffered signals that arrived while waitpoint was pending.
    -- If early signals already satisfy the resume condition, skip suspension entirely.
    local buffered = redis.call("XRANGE", K.waitpoint_signals, "-", "+")
    if #buffered > 0 then
      local wp_cond = initialize_condition(A.resume_condition_json)
      for _, entry in ipairs(buffered) do
        local fields = entry[2]
        local sig_name = extract_field(fields, "signal_name")
        local sig_id = extract_field(fields, "signal_id")
        evaluate_signal_against_condition(wp_cond, sig_name, sig_id)
      end
      if is_condition_satisfied(wp_cond) then
        -- Resume condition already met by buffered signals — skip suspension.
        redis.call("HSET", K.waitpoint_hash,
          "state", "closed", "satisfied_at", tostring(now_ms),
          "closed_at", tostring(now_ms), "close_reason", "resumed")
        write_condition_hash(K.wp_condition, wp_cond, now_ms)
        -- Do NOT release lease, do NOT change execution state.
        -- RFC-013 §9.2 — write dedup outcome before returning.
        if _dedup_active then
          local payload = "ALREADY_SATISFIED\t" .. A.suspension_id .. "\t" .. waitpoint_id ..
            "\t" .. waitpoint_key .. "\t" .. waitpoint_token
          redis.call("HSET", K.dedup_hash, "outcome", payload)
          if A.dedup_ttl_ms > 0 then
            redis.call("PEXPIRE", K.dedup_hash, A.dedup_ttl_ms)
          end
        end
        return ok_already_satisfied(A.suspension_id, waitpoint_id, waitpoint_key, waitpoint_token)
      end
      -- Condition not yet satisfied — proceed with suspension.
      -- Write partial condition state (some matchers may be satisfied).
      write_condition_hash(K.wp_condition, wp_cond, now_ms)
    else
      -- No buffered signals — init condition from scratch
      local wp_cond = initialize_condition(A.resume_condition_json)
      write_condition_hash(K.wp_condition, wp_cond, now_ms)
    end
  else
    -- Create new waitpoint — mint HMAC token bound to (waitpoint_id,
    -- waitpoint_key, created_at). The created_at written here is what the
    -- signal-delivery path reads back for HMAC validation.
    local token, token_err = mint_waitpoint_token(
      K.hmac_secrets, waitpoint_id, waitpoint_key, now_ms)
    if not token then return err(token_err) end
    waitpoint_token = token

    redis.call("HSET", K.waitpoint_hash,
      "waitpoint_id", waitpoint_id,
      "execution_id", A.execution_id,
      "attempt_index", A.attempt_index,
      "suspension_id", A.suspension_id,
      "waitpoint_key", waitpoint_key,
      "waitpoint_token", waitpoint_token,
      "state", "active",
      "created_at", tostring(now_ms),
      "activated_at", tostring(now_ms),
      "expires_at", is_set(A.timeout_at) and A.timeout_at or "",
      "signal_count", "0",
      "matched_signal_count", "0",
      "last_signal_at", "")

    -- Initialize condition hash from resume condition spec
    local wp_cond = initialize_condition(A.resume_condition_json)
    write_condition_hash(K.wp_condition, wp_cond, now_ms)
  end

  -- 6. Record waitpoint_id in mandatory history set (required for cleanup cascade)
  redis.call("SADD", K.waitpoint_history, waitpoint_id)

  -- OOM-SAFE WRITE ORDERING (per RFC-010 §4.8b):
  -- exec_core HSET is the "point of no return" — write it FIRST.

  -- 7. Transition exec_core (FIRST — point of no return, all 7 dims)
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "suspended",
    "ownership_state", "unowned",
    "eligibility_state", "not_applicable",
    "blocking_reason", map_reason_to_blocking(A.reason_code),
    "blocking_detail", "suspended: waitpoint " .. waitpoint_id .. " awaiting " .. A.reason_code,
    "terminal_outcome", "none",
    "attempt_state", "attempt_interrupted",
    "public_state", "suspended",
    "current_lease_id", "",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "lease_expires_at", "",
    "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "current_suspension_id", A.suspension_id,
    "current_waitpoint_id", waitpoint_id,
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- 8. Pause the attempt: started -> suspended (RFC-002 suspend_attempt)
  redis.call("HSET", K.attempt_record,
    "attempt_state", "suspended",
    "suspended_at", tostring(now_ms),
    "suspension_id", A.suspension_id)

  -- 9. Release lease + update indexes
  redis.call("DEL", K.lease_current_key)
  redis.call("ZREM", K.lease_expiry_key, A.execution_id)
  redis.call("SREM", K.worker_leases_key, A.execution_id)
  redis.call("ZREM", K.active_index_key, A.execution_id)
  redis.call("ZREM", K.attempt_timeout_key, A.execution_id)
  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", A.lease_history_maxlen, "*",
    "event", "released",
    "lease_id", A.lease_id,
    "lease_epoch", A.lease_epoch,
    "attempt_index", A.attempt_index,
    "attempt_id", core.current_attempt_id or "",
    "reason", "suspend",
    "ts", tostring(now_ms))

  -- 10. Create suspension record
  redis.call("HSET", K.suspension_current,
    "suspension_id", A.suspension_id,
    "execution_id", A.execution_id,
    "attempt_index", A.attempt_index,
    "waitpoint_id", waitpoint_id,
    "waitpoint_key", waitpoint_key,
    "reason_code", A.reason_code,
    "requested_by", A.requested_by,
    "created_at", tostring(now_ms),
    "timeout_at", A.timeout_at,
    "timeout_behavior", A.timeout_behavior,
    "resume_condition_json", A.resume_condition_json,
    "resume_policy_json", A.resume_policy_json,
    "continuation_metadata_pointer", A.continuation_metadata_ptr,
    "buffered_signal_summary_json", initial_signal_summary_json(),
    "last_signal_at", "",
    "satisfied_at", "",
    "closed_at", "",
    "close_reason", "")

  -- 10b. RFC-014 §3.1: seed composite member_map (write-once) when the
  -- resume condition carries a composite tree. No-op for single-matcher
  -- / operator / timeout conditions.
  do
    local spec_ok, spec = pcall(cjson.decode, A.resume_condition_json)
    if spec_ok and type(spec) == "table" and spec.composite then
      seed_composite_member_map(
        K.suspension_current .. ":member_map",
        spec.tree,
        waitpoint_key)
    end
  end

  -- 11. Add to per-lane suspended index + suspension timeout
  -- Score: timeout_at if set, otherwise MAX for "no timeout" ordering
  redis.call("ZADD", K.suspended_zset,
    is_set(A.timeout_at) and tonumber(A.timeout_at) or 9999999999999,
    A.execution_id)

  if is_set(A.timeout_at) then
    redis.call("ZADD", K.suspension_timeout_key, tonumber(A.timeout_at), A.execution_id)
  end

  -- RFC-013 §9.2 — write dedup outcome before returning.
  if _dedup_active then
    local payload = "OK\t" .. A.suspension_id .. "\t" .. waitpoint_id ..
      "\t" .. waitpoint_key .. "\t" .. waitpoint_token
    redis.call("HSET", K.dedup_hash, "outcome", payload)
    if A.dedup_ttl_ms > 0 then
      redis.call("PEXPIRE", K.dedup_hash, A.dedup_ttl_ms)
    end
  end

  return ok(A.suspension_id, waitpoint_id, waitpoint_key, waitpoint_token)
end)

---------------------------------------------------------------------------
-- #14  ff_resume_execution
--
-- Transition suspended → runnable. Called after signal satisfies condition
-- or by operator override. Closes suspension + waitpoint, updates indexes.
--
-- KEYS (8): exec_core, suspension_current, waitpoint_hash,
--           waitpoint_signals, suspension_timeout_zset,
--           eligible_zset, delayed_zset, suspended_zset
-- ARGV (3): execution_id, trigger_type, resume_delay_ms
---------------------------------------------------------------------------
redis.register_function('ff_resume_execution', function(keys, args)
  local K = {
    core_key               = keys[1],
    suspension_current     = keys[2],
    waitpoint_hash         = keys[3],
    waitpoint_signals      = keys[4],
    suspension_timeout_key = keys[5],
    eligible_zset          = keys[6],
    delayed_zset           = keys[7],
    suspended_zset         = keys[8],
  }

  local A = {
    execution_id   = args[1],
    trigger_type   = args[2] or "signal",     -- "signal", "operator", "auto_resume"
    resume_delay_ms = tonumber(args[3] or "0"),
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Read + validate execution is suspended
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "suspended" then
    return err("execution_not_suspended")
  end

  -- 2. Validate active suspension
  local susp_raw = redis.call("HGETALL", K.suspension_current)
  local susp_err, susp = assert_active_suspension(susp_raw)
  if susp_err then return susp_err end

  -- 3. Compute eligibility based on resume_delay_ms
  local eligibility_state = "eligible_now"
  local blocking_reason = "waiting_for_worker"
  local blocking_detail = ""
  local public_state = "waiting"

  if A.resume_delay_ms > 0 then
    eligibility_state = "not_eligible_until_time"
    blocking_reason = "waiting_for_resume_delay"
    blocking_detail = "resume delay " .. tostring(A.resume_delay_ms) .. "ms"
    public_state = "delayed"
  end

  -- OOM-SAFE WRITE ORDERING: exec_core FIRST (point of no return)

  -- 4. Transition exec_core (FIRST — all 7 dims)
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", eligibility_state,
    "blocking_reason", blocking_reason,
    "blocking_detail", blocking_detail,
    "terminal_outcome", "none",
    "attempt_state", "attempt_interrupted",
    "public_state", public_state,
    "current_suspension_id", "",
    "current_waitpoint_id", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- 5. Update scheduling indexes
  redis.call("ZREM", K.suspended_zset, A.execution_id)
  if A.resume_delay_ms > 0 then
    redis.call("ZADD", K.delayed_zset,
      now_ms + A.resume_delay_ms, A.execution_id)
  else
    -- ZADD eligible with composite priority score
    local priority = tonumber(core.priority or "0")
    local created_at = tonumber(core.created_at or "0")
    local score = 0 - (priority * 1000000000000) + created_at
    redis.call("ZADD", K.eligible_zset, score, A.execution_id)
  end

  -- 6. Close sub-objects (safe to lose on OOM — stale but not zombie)
  -- Close waitpoint
  redis.call("HSET", K.waitpoint_hash,
    "state", "closed",
    "satisfied_at", tostring(now_ms),
    "closed_at", tostring(now_ms),
    "close_reason", "resumed")

  -- Close suspension
  redis.call("HSET", K.suspension_current,
    "satisfied_at", tostring(now_ms),
    "closed_at", tostring(now_ms),
    "close_reason", "resumed")

  -- Remove from suspension timeout index
  redis.call("ZREM", K.suspension_timeout_key, A.execution_id)

  -- RFC-014 §3.1.1 composite cleanup owner: operator-driven resume path.
  composite_cleanup(
    K.suspension_current .. ":satisfied_set",
    K.suspension_current .. ":member_map")

  return ok(public_state)
end)

---------------------------------------------------------------------------
-- #15  ff_create_pending_waitpoint
--
-- Pre-create a waitpoint before suspension commits. The waitpoint is
-- externally addressable by waitpoint_key so early signals can be buffered.
-- Requires the caller to still hold the active lease.
-- Mints the waitpoint HMAC token up front so early signals targeting the
-- pending waitpoint can be authenticated via ff_buffer_signal_for_pending_waitpoint.
--
-- KEYS (4): exec_core, waitpoint_hash, pending_wp_expiry_zset, hmac_secrets
-- ARGV (5): execution_id, attempt_index, waitpoint_id, waitpoint_key,
--           expires_at
---------------------------------------------------------------------------
redis.register_function('ff_create_pending_waitpoint', function(keys, args)
  local K = {
    core_key              = keys[1],
    waitpoint_hash        = keys[2],
    pending_wp_expiry_key = keys[3],
    hmac_secrets          = keys[4],
  }

  local A = {
    execution_id  = args[1],
    attempt_index = args[2],
    waitpoint_id  = args[3],
    waitpoint_key = args[4],
    expires_at    = args[5],
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution is active with a lease
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "active" then
    return err("execution_not_active",
      core.terminal_outcome or "",
      core.current_lease_epoch or "",
      core.lifecycle_phase or "",
      core.current_attempt_id or "")
  end
  if core.ownership_state ~= "leased" then
    return err("no_active_lease")
  end
  -- Validate attempt binding
  if tostring(core.current_attempt_index) ~= A.attempt_index then
    return err("stale_lease")
  end

  -- 2. Guard: waitpoint already exists
  if redis.call("EXISTS", K.waitpoint_hash) == 1 then
    local existing_state = redis.call("HGET", K.waitpoint_hash, "state")
    if existing_state == "pending" or existing_state == "active" then
      return err("waitpoint_already_exists")
    end
    -- Old closed/expired waitpoint — safe to overwrite
  end

  -- 3. Mint HMAC token bound to (waitpoint_id, waitpoint_key, now_ms).
  -- The suspension activation path will reuse this token unchanged.
  local waitpoint_token, token_err = mint_waitpoint_token(
    K.hmac_secrets, A.waitpoint_id, A.waitpoint_key, now_ms)
  if not waitpoint_token then return err(token_err) end

  -- 4. Create pending waitpoint
  redis.call("HSET", K.waitpoint_hash,
    "waitpoint_id", A.waitpoint_id,
    "execution_id", A.execution_id,
    "attempt_index", A.attempt_index,
    "suspension_id", "",
    "waitpoint_key", A.waitpoint_key,
    "waitpoint_token", waitpoint_token,
    "state", "pending",
    "created_at", tostring(now_ms),
    "activated_at", "",
    "satisfied_at", "",
    "closed_at", "",
    "expires_at", A.expires_at,
    "close_reason", "",
    "signal_count", "0",
    "matched_signal_count", "0",
    "last_signal_at", "")

  -- 5. Add to pending waitpoint expiry index
  redis.call("ZADD", K.pending_wp_expiry_key,
    tonumber(A.expires_at), A.waitpoint_id)

  return ok(A.waitpoint_id, A.waitpoint_key, waitpoint_token)
end)

---------------------------------------------------------------------------
-- #16/#19  ff_expire_suspension  (Overlap group D — one script)
--
-- Apply timeout behavior when suspension timeout fires.
-- Re-validates that execution is still suspended and timeout is due.
-- Handles all 5 timeout behaviors:
--   fail    → terminal(failed)
--   cancel  → terminal(cancelled)
--   expire  → terminal(expired)
--   auto_resume → close + resume to runnable
--   escalate → mutate suspension to operator-review
--
-- KEYS (12): exec_core, suspension_current, waitpoint_hash, wp_condition,
--            attempt_hash, stream_meta, suspension_timeout_zset,
--            suspended_zset, terminal_zset, eligible_zset, delayed_zset,
--            lease_history
-- ARGV (1): execution_id
---------------------------------------------------------------------------
redis.register_function('ff_expire_suspension', function(keys, args)
  local K = {
    core_key               = keys[1],
    suspension_current     = keys[2],
    waitpoint_hash         = keys[3],
    wp_condition           = keys[4],
    attempt_hash           = keys[5],
    stream_meta            = keys[6],
    suspension_timeout_key = keys[7],
    suspended_zset         = keys[8],
    terminal_key           = keys[9],
    eligible_zset          = keys[10],
    delayed_zset           = keys[11],
    lease_history_key      = keys[12],
  }

  local A = {
    execution_id = args[1],
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Read + validate execution is still suspended
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)
    return ok("not_found_cleaned")
  end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "suspended" then
    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)
    return ok("not_suspended_cleaned")
  end

  -- 2. Read suspension and validate it's still open
  local susp_raw = redis.call("HGETALL", K.suspension_current)
  local susp_err, susp = assert_active_suspension(susp_raw)
  if susp_err then
    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)
    return ok("no_active_suspension_cleaned")
  end

  -- 3. Check timeout is actually due
  local timeout_at = tonumber(susp.timeout_at or "0")
  if timeout_at == 0 or timeout_at > now_ms then
    return ok("not_yet_due")
  end

  -- 4. Read timeout behavior
  local behavior = susp.timeout_behavior or "fail"

  -- 5. Apply behavior
  if behavior == "auto_resume" or behavior == "auto_resume_with_timeout_signal" then
    -- auto_resume: close suspension + resume to runnable (like ff_resume_execution)

    -- OOM-SAFE: exec_core FIRST
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
      "attempt_state", "attempt_interrupted",
      "public_state", "waiting",
      "current_suspension_id", "",
      "current_waitpoint_id", "",
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    -- Update indexes
    redis.call("ZREM", K.suspended_zset, A.execution_id)
    redis.call("ZADD", K.eligible_zset, score, A.execution_id)

    -- Close sub-objects
    redis.call("HSET", K.waitpoint_hash,
      "state", "closed",
      "closed_at", tostring(now_ms),
      "close_reason", "timed_out_auto_resume")
    redis.call("HSET", K.wp_condition,
      "closed", "1",
      "closed_at", tostring(now_ms),
      "closed_reason", "timed_out_auto_resume")
    redis.call("HSET", K.suspension_current,
      "closed_at", tostring(now_ms),
      "close_reason", "timed_out_auto_resume")

    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)

    -- RFC-014 §3.1.1 composite cleanup owner: expire (auto_resume) path.
    composite_cleanup(
      K.suspension_current .. ":satisfied_set",
      K.suspension_current .. ":member_map")

    return ok("auto_resume", "waiting")

  elseif behavior == "escalate" then
    -- escalate: mutate suspension to operator-review, keep suspended (ALL 7 dims)

    redis.call("HSET", K.core_key,
      "lifecycle_phase", "suspended",                        -- preserve
      "ownership_state", "unowned",                          -- preserve
      "eligibility_state", "not_applicable",                 -- preserve
      "blocking_reason", "paused_by_operator",
      "blocking_detail", "suspension escalated: timeout at " .. tostring(timeout_at),
      "terminal_outcome", "none",                            -- preserve
      "attempt_state", core.attempt_state or "attempt_interrupted", -- preserve
      "public_state", "suspended",                           -- preserve
      "last_mutation_at", tostring(now_ms))

    redis.call("HSET", K.suspension_current,
      "reason_code", "waiting_for_operator_review",
      "timeout_at", "",
      "timeout_behavior", "")

    -- Remove from timeout index (no longer has a timeout)
    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)

    return ok("escalate", "suspended")

  else
    -- Terminal paths: fail, cancel, expire

    local terminal_outcome
    local public_state_val
    local close_reason

    if behavior == "cancel" then
      terminal_outcome = "cancelled"
      public_state_val = "cancelled"
      close_reason = "timed_out_cancel"
    elseif behavior == "expire" then
      terminal_outcome = "expired"
      public_state_val = "expired"
      close_reason = "timed_out_expire"
    else
      -- Default: fail
      terminal_outcome = "failed"
      public_state_val = "failed"
      close_reason = "timed_out_fail"
    end

    -- OOM-SAFE: exec_core FIRST (§4.8b Rule 2)
    redis.call("HSET", K.core_key,
      "lifecycle_phase", "terminal",
      "ownership_state", "unowned",
      "eligibility_state", "not_applicable",
      "blocking_reason", "none",
      "blocking_detail", "",
      "terminal_outcome", terminal_outcome,
      "attempt_state", "attempt_terminal",
      "public_state", public_state_val,
      "failure_reason", "suspension_timeout:" .. behavior,
      "completed_at", tostring(now_ms),
      "current_suspension_id", "",
      "current_waitpoint_id", "",
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    -- End attempt: suspended → ended_failure/ended_cancelled
    if is_set(core.current_attempt_index) then
      local att_end_state = "ended_failure"
      if behavior == "cancel" then
        att_end_state = "ended_cancelled"
      end
      redis.call("HSET", K.attempt_hash,
        "attempt_state", att_end_state,
        "ended_at", tostring(now_ms),
        "failure_reason", "suspension_timeout:" .. behavior,
        "suspended_at", "",
        "suspension_id", "")
    end

    -- Close stream if exists
    if redis.call("EXISTS", K.stream_meta) == 1 then
      redis.call("HSET", K.stream_meta,
        "closed_at", tostring(now_ms),
        "closed_reason", "suspension_timeout")
    end

    -- Close sub-objects
    redis.call("HSET", K.waitpoint_hash,
      "state", "closed",
      "closed_at", tostring(now_ms),
      "close_reason", close_reason)
    redis.call("HSET", K.wp_condition,
      "closed", "1",
      "closed_at", tostring(now_ms),
      "closed_reason", close_reason)
    redis.call("HSET", K.suspension_current,
      "closed_at", tostring(now_ms),
      "close_reason", close_reason)

    -- RFC-014 §3.1.1 composite cleanup owner: expire (terminal) paths.
    composite_cleanup(
      K.suspension_current .. ":satisfied_set",
      K.suspension_current .. ":member_map")

    -- Remove from suspension indexes, add to terminal
    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)
    redis.call("ZREM", K.suspended_zset, A.execution_id)
    redis.call("ZADD", K.terminal_key, now_ms, A.execution_id)

    -- Push-based DAG promotion (bridge-event gap report §1.3 analogue).
    -- Suspension-timeout terminals (fail / cancel / expire) are FF-
    -- initiated transitions that cairn cannot observe via its
    -- call-then-emit pattern. Without a PUBLISH, flow-bound children
    -- only unblock via the dependency_reconciler safety net (15s).
    -- Gated on `is_set(core.flow_id)` — standalone executions never
    -- have downstream edges. Outcome matches terminal_outcome
    -- ("failed" / "cancelled" / "expired").
    if is_set(core.flow_id) then
      local payload = cjson.encode({
        execution_id = A.execution_id,
        flow_id = core.flow_id,
        outcome = terminal_outcome,
      })
      redis.call("PUBLISH", "ff:dag:completions", payload)
    end

    return ok(behavior, public_state_val)
  end
end)

---------------------------------------------------------------------------
-- #36  ff_close_waitpoint
--
-- Proactive close of pending or active waitpoint. Used by workers that
-- created a pending waitpoint but decided not to suspend.
--
-- KEYS (3): exec_core, waitpoint_hash, pending_wp_expiry_zset
-- ARGV (2): waitpoint_id, reason
---------------------------------------------------------------------------
redis.register_function('ff_close_waitpoint', function(keys, args)
  local K = {
    core_key              = keys[1],
    waitpoint_hash        = keys[2],
    pending_wp_expiry_key = keys[3],
  }

  local A = {
    waitpoint_id = args[1],
    reason       = args[2] or "proactive_close",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Read waitpoint
  local wp_raw = redis.call("HGETALL", K.waitpoint_hash)
  if #wp_raw == 0 then
    return err("waitpoint_not_found")
  end
  local wp = hgetall_to_table(wp_raw)

  -- 2. Validate state is pending or active
  if wp.state ~= "pending" and wp.state ~= "active" then
    if wp.state == "closed" or wp.state == "expired" then
      return ok("already_closed")
    end
    return err("waitpoint_not_open")
  end

  -- 3. Close the waitpoint
  redis.call("HSET", K.waitpoint_hash,
    "state", "closed",
    "closed_at", tostring(now_ms),
    "close_reason", A.reason)

  -- 4. Remove from pending expiry index (no-op if not pending)
  redis.call("ZREM", K.pending_wp_expiry_key, A.waitpoint_id)

  return ok()
end)

---------------------------------------------------------------------------
-- ff_rotate_waitpoint_hmac_secret
--
-- Install a new waitpoint HMAC signing kid on a single partition. The
-- server-side admin endpoint (ff-server POST /v1/admin/rotate-waitpoint-secret)
-- delegates to this FCALL per partition. Direct-Valkey consumers (e.g.
-- cairn-rs, issue #49) invoke it themselves across every partition.
--
-- FCALL atomicity is per-shard and per-call; previous implementations used
-- a SETNX lock + read-modify-write from Rust. Here the script IS the
-- atomicity boundary, so no lock is needed.
--
-- KEYS (1): hmac_secrets  (ff:sec:{p:N}:waitpoint_hmac)
-- ARGV (3): new_kid, new_secret_hex, grace_ms
--
-- `now_ms` is derived server-side from `redis.call("TIME")` to match
-- the rest of the library (lua/flow.lua, validate_waitpoint_token), so
-- GC and validation agree on "now". `grace_ms` is a duration, not a
-- clock value, so taking it from ARGV is safe — operators set it via
-- FF_WAITPOINT_HMAC_GRACE_MS (ff-server) or pass their own value.
--
-- Outcomes:
--   ok("rotated", previous_kid_or_empty, new_kid, gc_count)
--   ok("noop",    kid)                           -- exact replay (same kid + secret)
--   err("rotation_conflict", kid)                -- same kid, different secret
--   err("invalid_kid")                           -- empty or contains ':'
--   err("invalid_secret_hex")                    -- empty / odd length / non-hex
--   err("invalid_grace_ms")                      -- not a non-negative integer
--
-- Authoritative implementation of waitpoint HMAC rotation: idempotent
-- replay, torn-write repair, orphan GC across expired kids. INVARIANT:
-- expires_at:<new_kid> is never written (current_kid has no expiry).
-- ff-server's admin endpoint delegates to this FCALL — single source
-- of truth lives here, not in Rust.
---------------------------------------------------------------------------
redis.register_function('ff_rotate_waitpoint_hmac_secret', function(keys, args)
  local hmac_key       = keys[1]
  local new_kid        = args[1]
  local new_secret_hex = args[2]
  local grace_ms_s     = args[3]

  -- Combined validation (feedback: single condition is more readable).
  if type(new_kid) ~= "string" or new_kid == "" or new_kid:find(":", 1, true) then
    return err("invalid_kid")
  end
  if type(new_secret_hex) ~= "string"
      or new_secret_hex == ""
      or #new_secret_hex % 2 ~= 0
      or new_secret_hex:find("[^0-9a-fA-F]") then
    return err("invalid_secret_hex")
  end

  -- grace_ms must be a finite non-negative integer. The math.floor check
  -- rejects decimals but NOT infinities (math.floor(math.huge) == math.huge
  -- which would stamp "inf" into expires_at:*). Cap at 2^53-1 so the
  -- stored value stays within the IEEE-754 double-precision integer range
  -- AND within i64, keeping the Rust parser happy. 2^53-1 ms is ~285 years,
  -- far beyond any operational grace window.
  local grace_ms = tonumber(grace_ms_s)
  if not grace_ms
      or grace_ms ~= grace_ms -- NaN
      or grace_ms < 0
      or grace_ms > 9007199254740991 -- 2^53 - 1
      or grace_ms ~= math.floor(grace_ms) then
    return err("invalid_grace_ms")
  end

  -- Server-side time via redis.call("TIME"); never trust caller-supplied
  -- timestamps for expiry decisions (consistency with flow.lua + helpers.lua
  -- and with validate_waitpoint_token).
  local now_ms          = server_time_ms()
  local prev_expires_at = now_ms + grace_ms

  local current_kid = redis.call("HGET", hmac_key, "current_kid")
  if current_kid == false then current_kid = nil end

  -- Idempotency branch: same kid already installed.
  if current_kid == new_kid then
    local stored = redis.call("HGET", hmac_key, "secret:" .. new_kid)
    if stored == false then stored = nil end
    if stored == new_secret_hex then
      return ok("noop", new_kid)
    elseif stored then
      return err("rotation_conflict", new_kid)
    else
      -- Torn-write repair: current_kid=new_kid but secret:<new_kid> missing.
      redis.call("HSET", hmac_key, "secret:" .. new_kid, new_secret_hex)
      return ok("noop", new_kid)
    end
  end

  -- Orphan GC: HGETALL once, collect kids whose expires_at has passed,
  -- then a single HDEL. One entry per distinct kid ever installed plus
  -- a handful of scalars; bounded in practice.
  local raw = redis.call("HGETALL", hmac_key)
  local expired_fields = {}
  local gc_count = 0
  for i = 1, #raw, 2 do
    local field = raw[i]
    local value = raw[i + 1]
    if field:sub(1, 11) == "expires_at:" then
      local kid = field:sub(12)
      local exp = tonumber(value)
      -- GC strictness MUST match validate_waitpoint_token's `exp < now_ms`
      -- (lua/helpers.lua). Reaping on `exp <= now_ms` would delete a kid
      -- that the validator still considers in-grace at the boundary
      -- exp == now_ms, causing tokens that should validate to fail.
      if not exp or exp <= 0 or exp < now_ms then
        expired_fields[#expired_fields + 1] = "expires_at:" .. kid
        expired_fields[#expired_fields + 1] = "secret:" .. kid
        gc_count = gc_count + 1
      end
    end
  end
  if #expired_fields > 0 then
    redis.call("HDEL", hmac_key, unpack(expired_fields))
  end

  -- Promote current → previous. INVARIANT: expires_at:<new_kid> is NEVER
  -- written — current_kid has no expiry entry.
  local prev_expires_str = tostring(prev_expires_at)
  if current_kid then
    redis.call("HSET", hmac_key,
      "previous_kid", current_kid,
      "previous_expires_at", prev_expires_str,
      "expires_at:" .. current_kid, prev_expires_str,
      "current_kid", new_kid,
      "secret:" .. new_kid, new_secret_hex)
  else
    redis.call("HSET", hmac_key,
      "current_kid", new_kid,
      "secret:" .. new_kid, new_secret_hex)
  end

  return ok("rotated", current_kid or "", new_kid, tostring(gc_count))
end)

---------------------------------------------------------------------------
-- ff_list_waitpoint_hmac_kids
--
-- Read-back for the waitpoint HMAC keystore. Insulates consumers (cairn
-- admin UI, ff-server audit surface) from the hash-field naming so future
-- layout changes don't break them.
--
-- KEYS (1): hmac_secrets  (ff:sec:{p:N}:waitpoint_hmac)
-- ARGV: none
--
-- Returns ok(current_kid_or_empty, n, kid1, exp1_ms, kid2, exp2_ms, ...)
-- "verifying" kids = those with a FUTURE expires_at:<kid> entry. Kids
-- whose grace has already elapsed are NOT reported here — the contract
-- promises kids that still validate tokens, so listing expired kids
-- would mislead operators. Expired entries are swept by orphan GC on
-- the next rotation. current_kid is excluded (it never has an expiry).
-- Uninitialized → ok("", 0).
---------------------------------------------------------------------------
redis.register_function('ff_list_waitpoint_hmac_kids', function(keys, args)
  local hmac_key = keys[1]
  local raw = redis.call("HGETALL", hmac_key)
  local now_ms = server_time_ms()

  local current_kid = ""
  local pairs_out = {}
  local n = 0
  for i = 1, #raw, 2 do
    local field = raw[i]
    local value = raw[i + 1]
    if field == "current_kid" then
      current_kid = value
    elseif field:sub(1, 11) == "expires_at:" then
      local kid = field:sub(12)
      local exp = tonumber(value)
      -- Match validator's `exp < now_ms` rejection rule so a kid listed
      -- as verifying really does still validate tokens at call time.
      if exp and exp > 0 and exp >= now_ms then
        pairs_out[#pairs_out + 1] = kid
        pairs_out[#pairs_out + 1] = value
        n = n + 1
      end
    end
  end

  return ok(current_kid, tostring(n), unpack(pairs_out))
end)
