-- FlowFabric signal delivery and resume-claim functions
-- Reference: RFC-005 (Signal), RFC-001 (Execution), RFC-010 §4.1 (#17, #18, #2)
--
-- Depends on helpers: ok, err, ok_duplicate, hgetall_to_table, is_set,
--   initialize_condition, write_condition_hash, evaluate_signal_against_condition,
--   is_condition_satisfied, extract_field

---------------------------------------------------------------------------
-- #17  ff_deliver_signal
--
-- Atomic signal delivery: validate target, check idempotency, record
-- signal, evaluate resume condition, optionally close waitpoint +
-- suspension + transition suspended -> runnable.
--
-- KEYS (14): exec_core, wp_condition, wp_signals_stream,
--            exec_signals_zset, signal_hash, signal_payload,
--            idem_key, waitpoint_hash, suspension_current,
--            eligible_zset, suspended_zset, delayed_zset,
--            suspension_timeout_zset, hmac_secrets
-- ARGV (18): signal_id, execution_id, waitpoint_id, signal_name,
--            signal_category, source_type, source_identity,
--            payload, payload_encoding, idempotency_key,
--            correlation_id, target_scope, created_at,
--            dedup_ttl_ms, resume_delay_ms, signal_maxlen,
--            max_signals_per_execution, waitpoint_token
---------------------------------------------------------------------------
redis.register_function('ff_deliver_signal', function(keys, args)
  local K = {
    core_key              = keys[1],
    wp_condition          = keys[2],
    wp_signals_stream     = keys[3],
    exec_signals_zset     = keys[4],
    signal_hash           = keys[5],
    signal_payload        = keys[6],
    idem_key              = keys[7],
    waitpoint_hash        = keys[8],
    suspension_current    = keys[9],
    eligible_zset         = keys[10],
    suspended_zset        = keys[11],
    delayed_zset          = keys[12],
    suspension_timeout_zset = keys[13],
    hmac_secrets          = keys[14],
  }

  local A = {
    signal_id        = args[1],
    execution_id     = args[2],
    waitpoint_id     = args[3],
    signal_name      = args[4],
    signal_category  = args[5],
    source_type      = args[6],
    source_identity  = args[7],
    payload          = args[8] or "",
    payload_encoding = args[9] or "json",
    idempotency_key  = args[10] or "",
    correlation_id   = args[11] or "",
    target_scope     = args[12] or "waitpoint",
    created_at       = args[13] or "",
    dedup_ttl_ms     = tonumber(args[14] or "86400000"),
    resume_delay_ms  = tonumber(args[15] or "0"),
    signal_maxlen    = tonumber(args[16] or "1000"),
    max_signals      = tonumber(args[17] or "10000"),
    waitpoint_token  = args[18] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution exists
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    return err("execution_not_found")
  end
  local core = hgetall_to_table(raw)

  -- 2. Validate HMAC token FIRST (RFC-004 §Waitpoint Security).
  --
  -- Order matters: lifecycle / waitpoint-state checks below would otherwise
  -- form a state oracle — an attacker presenting ANY token (including an
  -- invalid one) for an arbitrary (execution_id, waitpoint_id) pair could
  -- distinguish "execution is terminal" vs "waitpoint is pending" vs
  -- "waitpoint is closed" by the specific error code returned, without
  -- having to produce a valid HMAC. Auth-first closes that oracle.
  --
  -- Missing-waitpoint is collapsed into `invalid_token` for the same
  -- reason: an unauthenticated caller must not be able to probe which
  -- (execution, waitpoint) tuples exist.
  local wp_for_auth_raw = redis.call("HGETALL", K.waitpoint_hash)
  if #wp_for_auth_raw == 0 then
    return err("invalid_token")
  end
  local wp_for_auth = hgetall_to_table(wp_for_auth_raw)
  if not wp_for_auth.created_at then
    return err("invalid_token")
  end
  local token_err = validate_waitpoint_token(
    K.hmac_secrets, A.waitpoint_token,
    A.waitpoint_id, wp_for_auth.waitpoint_key or "",
    tonumber(wp_for_auth.created_at) or 0, now_ms)
  if token_err then
    -- Operator-visible counter (RFC-004 §Waitpoint Security observability).
    -- Single scalar HSET on exec_core — bounded, amortized-free. Gives
    -- operators a "last time this execution saw an auth failure" field to
    -- correlate with key-rotation drift, client bugs, or attack traffic
    -- without needing to tail Lua slowlog or FCALL error logs.
    redis.call("HSET", K.core_key, "last_hmac_validation_failed_at", tostring(now_ms))
    return err(token_err)
  end

  -- 3. Validate execution is in a signalable state (post-auth).
  local lp = core.lifecycle_phase
  if lp == "terminal" then
    return err("target_not_signalable")
  end

  if lp == "active" or lp == "runnable" or lp == "submitted" then
    -- Not suspended. wp_for_auth was just loaded above; reuse it.
    if wp_for_auth.state == "pending" then
      return err("waitpoint_pending_use_buffer_script")
    end
    if wp_for_auth.state ~= "active" then
      return err("target_not_signalable")
    end
    -- Active waitpoint on non-suspended execution — unusual but valid (race window)
  end

  -- 4. Validate waitpoint condition is open (post-auth).
  local cond_raw = redis.call("HGETALL", K.wp_condition)
  if #cond_raw == 0 then
    return err("waitpoint_not_found")
  end
  local wp_cond = hgetall_to_table(cond_raw)
  if wp_cond.closed == "1" then
    return err("waitpoint_closed")
  end

  -- 4. Signal count limit (prevents unbounded ZSET growth from webhook storms)
  if A.max_signals > 0 then
    local current_count = redis.call("ZCARD", K.exec_signals_zset)
    if current_count >= A.max_signals then
      return err("signal_limit_exceeded")
    end
  end

  -- 5. Idempotency check
  -- Guard: (A.dedup_ttl_ms or 0) handles nil from tonumber("") safely.
  local dedup_ms = A.dedup_ttl_ms or 0
  if A.idempotency_key ~= "" and dedup_ms > 0 then
    local existing = redis.call("GET", K.idem_key)
    if existing then
      return ok_duplicate(existing)
    end
    redis.call("SET", K.idem_key, A.signal_id,
      "PX", dedup_ms, "NX")
  end

  -- 6. Record signal hash
  local created_at = A.created_at ~= "" and A.created_at or tostring(now_ms)
  redis.call("HSET", K.signal_hash,
    "signal_id", A.signal_id,
    "target_execution_id", A.execution_id,
    "target_waitpoint_id", A.waitpoint_id,
    "target_scope", A.target_scope,
    "signal_name", A.signal_name,
    "signal_category", A.signal_category,
    "source_type", A.source_type,
    "source_identity", A.source_identity,
    "correlation_id", A.correlation_id,
    "idempotency_key", A.idempotency_key,
    "created_at", created_at,
    "accepted_at", tostring(now_ms),
    "matched_waitpoint_id", A.waitpoint_id,
    "payload_encoding", A.payload_encoding)

  -- 6b. Store payload separately if present
  if A.payload ~= "" then
    redis.call("SET", K.signal_payload, A.payload)
  end

  -- 7. Append to per-waitpoint signal stream + per-execution signal index
  redis.call("XADD", K.wp_signals_stream, "MAXLEN", "~",
    tostring(A.signal_maxlen), "*",
    "signal_id", A.signal_id,
    "signal_name", A.signal_name,
    "signal_category", A.signal_category,
    "source_type", A.source_type,
    "source_identity", A.source_identity,
    "matched", "0",
    "accepted_at", tostring(now_ms))
  redis.call("ZADD", K.exec_signals_zset, now_ms, A.signal_id)

  -- 8. Evaluate resume condition
  local effect = "appended_to_waitpoint"
  local matched = false

  -- RFC-014 §3.3: composite branch. `wp_condition` stores a `composite=1`
  -- marker when suspension serialized a multi-signal tree. We short-
  -- circuit here and run the composite evaluator (depth-bounded).
  local composite_mode = (wp_cond.composite == "1")
  if composite_mode then
    -- Load tree + waitpoint_key for candidate lookup.
    local susp_raw = redis.call("HGETALL", K.suspension_current)
    local susp = hgetall_to_table(susp_raw)
    local tree_json = wp_cond.tree_json or ""
    local tree = tree_json ~= "" and cjson.decode(tree_json) or nil
    local satisfied_set_key = K.suspension_current .. ":satisfied_set"
    local member_map_key    = K.suspension_current .. ":member_map"

    local signal_for_eval = {
      signal_id = A.signal_id,
      signal_name = A.signal_name,
      source_type = A.source_type,
      source_identity = A.source_identity,
    }
    -- RFC-014 Pattern 3: use THIS waitpoint's own key (loaded into
    -- `wp_for_auth` above, keyed by A.waitpoint_id) so multi-waitpoint
    -- AllOf resolves each leaf against its own wp_key, not the
    -- suspension's primary key.
    local this_wp_key = wp_for_auth.waitpoint_key or susp.waitpoint_key or ""
    local outcome = composite_deliver_signal(
      tree, satisfied_set_key, member_map_key,
      A.waitpoint_id, this_wp_key, signal_for_eval)

    effect = outcome.effect or "appended_to_waitpoint"
    matched = (effect ~= "signal_ignored_not_in_condition"
               and effect ~= "signal_ignored_matcher_failed")

    if outcome.resume then
      -- Close suspension via the standard path (same as single-matcher
      -- resume below). Composite-scoped cleanup follows §3.1.1.
      local lp2 = core.lifecycle_phase
      if lp2 == "suspended" then
        local es, br, bd, ps
        if A.resume_delay_ms > 0 then
          es = "not_eligible_until_time"
          br = "waiting_for_resume_delay"
          bd = "resume delay " .. A.resume_delay_ms .. "ms after signal " .. A.signal_name
          ps = "delayed"
        else
          es = "eligible_now"
          br = "waiting_for_worker"
          bd = ""
          ps = "waiting"
        end
        redis.call("HSET", K.core_key,
          "lifecycle_phase", "runnable",
          "ownership_state", "unowned",
          "eligibility_state", es,
          "blocking_reason", br,
          "blocking_detail", bd,
          "terminal_outcome", "none",
          "attempt_state", "attempt_interrupted",
          "public_state", ps,
          "current_suspension_id", "",
          "current_waitpoint_id", "",
          "last_transition_at", tostring(now_ms),
          "last_mutation_at", tostring(now_ms))

        local priority = tonumber(core.priority or "0")
        local created_at_exec = tonumber(core.created_at or "0")
        redis.call("ZREM", K.suspended_zset, A.execution_id)
        if A.resume_delay_ms > 0 then
          redis.call("ZADD", K.delayed_zset,
            now_ms + A.resume_delay_ms, A.execution_id)
        else
          redis.call("ZADD", K.eligible_zset,
            0 - (priority * 1000000000000) + created_at_exec,
            A.execution_id)
        end
      end

      redis.call("HSET", K.wp_condition,
        "closed", "1",
        "closed_at", tostring(now_ms),
        "closed_reason", "satisfied")
      redis.call("HSET", K.waitpoint_hash,
        "state", "closed",
        "satisfied_at", tostring(now_ms),
        "closed_at", tostring(now_ms),
        "close_reason", "resumed")
      if redis.call("EXISTS", K.suspension_current) == 1 then
        redis.call("HSET", K.suspension_current,
          "satisfied_at", tostring(now_ms),
          "closed_at", tostring(now_ms),
          "close_reason", "resumed",
          "closer_signal_id", outcome.closer or A.signal_id,
          "all_satisfier_signals", outcome.all_satisfiers_json or "[]")
      end
      redis.call("ZREM", K.suspension_timeout_zset, A.execution_id)
      -- RFC-014 §3.1.1 cleanup owner: deliver_signal close path.
      composite_cleanup(satisfied_set_key, member_map_key)
      -- RFC-014 Pattern 3: close any OTHER waitpoints owned by this
      -- suspension (the one the signal arrived on is already closed
      -- via K.waitpoint_hash above). Reread susp to pick up the
      -- additional_waitpoints_json that was seeded at suspend-time.
      local susp_after = redis.call("HGETALL", K.suspension_current)
      if #susp_after > 0 then
        local susp2 = hgetall_to_table(susp_after)
        close_additional_waitpoints(
          K.suspension_current,
          susp2.additional_waitpoints_json or "",
          { "state", "closed",
            "satisfied_at", tostring(now_ms),
            "closed_at", tostring(now_ms),
            "close_reason", "resumed" },
          { "closed", "1",
            "closed_at", tostring(now_ms),
            "closed_reason", "satisfied" })
      end
    end

    -- Record signal hash observed_effect + waitpoint counters + return.
    redis.call("HSET", K.signal_hash, "observed_effect", effect)
    redis.call("HINCRBY", K.waitpoint_hash, "signal_count", 1)
    if matched then
      redis.call("HINCRBY", K.waitpoint_hash, "matched_signal_count", 1)
    end
    redis.call("HSET", K.waitpoint_hash, "last_signal_at", tostring(now_ms))
    if redis.call("EXISTS", K.suspension_current) == 1 and not outcome.resume then
      redis.call("HSET", K.suspension_current, "last_signal_at", tostring(now_ms))
    end
    return ok(A.signal_id, effect)
  end

  local total = tonumber(wp_cond.total_matchers or "0")
  for i = 0, total - 1 do
    local sat_key = "matcher:" .. i .. ":satisfied"
    local name_key = "matcher:" .. i .. ":name"
    if wp_cond[sat_key] == "0" then
      local matcher_name = wp_cond[name_key] or ""
      if matcher_name == "" or matcher_name == A.signal_name then
        -- Mark matcher as satisfied
        redis.call("HSET", K.wp_condition,
          sat_key, "1",
          "matcher:" .. i .. ":signal_id", A.signal_id)
        matched = true
        local new_sat = tonumber(wp_cond.satisfied_count or "0") + 1
        redis.call("HSET", K.wp_condition, "satisfied_count", tostring(new_sat))

        -- Check if overall condition is satisfied
        local mode = wp_cond.match_mode or "any"
        local min_count = tonumber(wp_cond.minimum_signal_count or "1")
        local resume = false
        if mode == "any" then
          resume = (new_sat >= min_count)
        elseif mode == "all" then
          resume = (new_sat >= total)
        else
          -- count(n) mode
          resume = (new_sat >= min_count)
        end

        if resume then
          effect = "resume_condition_satisfied"

          -- OOM-SAFE WRITE ORDERING (per RFC-010 §4.8b):
          -- exec_core HSET is the "point of no return" — write it FIRST.
          -- If OOM kills after exec_core but before closing sub-objects,
          -- execution is runnable (correct) with stale suspension/waitpoint
          -- records (generalized index reconciler catches this).

          -- 9a. Transition execution: suspended -> runnable (WRITE FIRST)
          -- Resume continues the SAME attempt (no new attempt created).
          if lp == "suspended" then
            local es, br, bd, ps
            if A.resume_delay_ms > 0 then
              es = "not_eligible_until_time"
              br = "waiting_for_resume_delay"
              bd = "resume delay " .. A.resume_delay_ms .. "ms after signal " .. A.signal_name
              ps = "delayed"
            else
              es = "eligible_now"
              br = "waiting_for_worker"
              bd = ""
              ps = "waiting"
            end

            -- ALL 7 state vector dimensions
            redis.call("HSET", K.core_key,
              "lifecycle_phase", "runnable",
              "ownership_state", "unowned",
              "eligibility_state", es,
              "blocking_reason", br,
              "blocking_detail", bd,
              "terminal_outcome", "none",
              "attempt_state", "attempt_interrupted",
              "public_state", ps,
              "current_suspension_id", "",
              "current_waitpoint_id", "",
              "last_transition_at", tostring(now_ms),
              "last_mutation_at", tostring(now_ms))

            -- 9b. Update scheduling indexes
            local priority = tonumber(core.priority or "0")
            local created_at_exec = tonumber(core.created_at or "0")
            redis.call("ZREM", K.suspended_zset, A.execution_id)
            if A.resume_delay_ms > 0 then
              redis.call("ZADD", K.delayed_zset,
                now_ms + A.resume_delay_ms, A.execution_id)
            else
              redis.call("ZADD", K.eligible_zset,
                0 - (priority * 1000000000000) + created_at_exec,
                A.execution_id)
            end
          end

          -- 9c. Close waitpoint condition (after exec_core is safe)
          redis.call("HSET", K.wp_condition,
            "closed", "1",
            "closed_at", tostring(now_ms),
            "closed_reason", "satisfied")

          -- 9d. Close waitpoint record
          redis.call("HSET", K.waitpoint_hash,
            "state", "closed",
            "satisfied_at", tostring(now_ms),
            "closed_at", tostring(now_ms),
            "close_reason", "resumed")

          -- 9e. Close suspension record
          if redis.call("EXISTS", K.suspension_current) == 1 then
            redis.call("HSET", K.suspension_current,
              "satisfied_at", tostring(now_ms),
              "closed_at", tostring(now_ms),
              "close_reason", "resumed")
          end

          -- 9f. Remove from suspension timeout index
          redis.call("ZREM", K.suspension_timeout_zset, A.execution_id)
        end
        break
      end
    end
  end

  if not matched then
    effect = "no_op"
  end

  -- 10. Record observed effect on signal
  redis.call("HSET", K.signal_hash, "observed_effect", effect)

  -- 11. Update waitpoint signal counts
  redis.call("HINCRBY", K.waitpoint_hash, "signal_count", 1)
  if matched then
    redis.call("HINCRBY", K.waitpoint_hash, "matched_signal_count", 1)
  end
  redis.call("HSET", K.waitpoint_hash, "last_signal_at", tostring(now_ms))

  -- 12. Update suspension signal summary
  if redis.call("EXISTS", K.suspension_current) == 1 then
    redis.call("HSET", K.suspension_current, "last_signal_at", tostring(now_ms))
  end

  return ok(A.signal_id, effect)
end)

---------------------------------------------------------------------------
-- #18  ff_buffer_signal_for_pending_waitpoint
--
-- Accept signal for a pending (not yet committed) waitpoint.
-- Records the signal but does NOT evaluate resume conditions.
-- When suspend_execution activates the waitpoint, buffered signals
-- are replayed through the full evaluation path.
--
-- KEYS (9): exec_core, wp_condition, wp_signals_stream,
--           exec_signals_zset, signal_hash, signal_payload,
--           idem_key, waitpoint_hash, hmac_secrets
-- ARGV (18): same as ff_deliver_signal (17 + waitpoint_token)
---------------------------------------------------------------------------
redis.register_function('ff_buffer_signal_for_pending_waitpoint', function(keys, args)
  local K = {
    core_key          = keys[1],
    wp_condition      = keys[2],
    wp_signals_stream = keys[3],
    exec_signals_zset = keys[4],
    signal_hash       = keys[5],
    signal_payload    = keys[6],
    idem_key          = keys[7],
    waitpoint_hash    = keys[8],
    hmac_secrets      = keys[9],
  }

  local A = {
    signal_id        = args[1],
    execution_id     = args[2],
    waitpoint_id     = args[3],
    signal_name      = args[4],
    signal_category  = args[5],
    source_type      = args[6],
    source_identity  = args[7],
    payload          = args[8] or "",
    payload_encoding = args[9] or "json",
    idempotency_key  = args[10] or "",
    correlation_id   = args[11] or "",
    target_scope     = args[12] or "waitpoint",
    created_at       = args[13] or "",
    dedup_ttl_ms     = tonumber(args[14] or "86400000"),
    signal_maxlen    = tonumber(args[16] or "1000"),
    max_signals      = tonumber(args[17] or "10000"),
    waitpoint_token  = args[18] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution exists
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    return err("execution_not_found")
  end

  -- 1a. Validate HMAC token against the pending waitpoint's mint-time binding.
  local wp_for_auth = hgetall_to_table(redis.call("HGETALL", K.waitpoint_hash))
  if not wp_for_auth.created_at then
    return err("waitpoint_not_found")
  end
  local token_err = validate_waitpoint_token(
    K.hmac_secrets, A.waitpoint_token,
    A.waitpoint_id, wp_for_auth.waitpoint_key or "",
    tonumber(wp_for_auth.created_at) or 0, now_ms)
  if token_err then
    -- Operator-visible counter mirroring ff_deliver_signal. See comment
    -- there for rationale.
    redis.call("HSET", K.core_key, "last_hmac_validation_failed_at", tostring(now_ms))
    return err(token_err)
  end

  -- 1b. Gate on waitpoint state. ff_deliver_signal blocks replay-after-close
  -- via wp_condition.closed, but wp_condition is not initialized for pending
  -- waitpoints — we must check wp.state directly. Without this, a caller
  -- holding a valid token for a pending waitpoint that has since been
  -- closed/expired can keep appending buffered signals that will replay
  -- when suspend_execution(use_pending=1) later activates the waitpoint.
  if wp_for_auth.state == "closed" or wp_for_auth.state == "expired" then
    return err("waitpoint_closed")
  end

  -- 2. Signal count limit
  if A.max_signals > 0 then
    local current_count = redis.call("ZCARD", K.exec_signals_zset)
    if current_count >= A.max_signals then
      return err("signal_limit_exceeded")
    end
  end

  -- 3. Idempotency check
  -- Guard: (A.dedup_ttl_ms or 0) handles nil from tonumber("") safely.
  local dedup_ms = A.dedup_ttl_ms or 0
  if A.idempotency_key ~= "" and dedup_ms > 0 then
    local existing = redis.call("GET", K.idem_key)
    if existing then
      return ok_duplicate(existing)
    end
    redis.call("SET", K.idem_key, A.signal_id,
      "PX", dedup_ms, "NX")
  end

  -- 4. Record signal hash with tentative effect
  local created_at = A.created_at ~= "" and A.created_at or tostring(now_ms)
  redis.call("HSET", K.signal_hash,
    "signal_id", A.signal_id,
    "target_execution_id", A.execution_id,
    "target_waitpoint_id", A.waitpoint_id,
    "target_scope", A.target_scope,
    "signal_name", A.signal_name,
    "signal_category", A.signal_category,
    "source_type", A.source_type,
    "source_identity", A.source_identity,
    "correlation_id", A.correlation_id,
    "idempotency_key", A.idempotency_key,
    "created_at", created_at,
    "accepted_at", tostring(now_ms),
    "matched_waitpoint_id", A.waitpoint_id,
    "payload_encoding", A.payload_encoding,
    "observed_effect", "buffered_for_pending_waitpoint")

  -- 4b. Store payload separately if present
  if A.payload ~= "" then
    redis.call("SET", K.signal_payload, A.payload)
  end

  -- 5. Append to per-waitpoint signal stream + per-execution signal index
  -- These are recorded so suspend_execution can XRANGE and replay them.
  redis.call("XADD", K.wp_signals_stream, "MAXLEN", "~",
    tostring(A.signal_maxlen), "*",
    "signal_id", A.signal_id,
    "signal_name", A.signal_name,
    "signal_category", A.signal_category,
    "source_type", A.source_type,
    "source_identity", A.source_identity,
    "matched", "0",
    "accepted_at", tostring(now_ms))
  redis.call("ZADD", K.exec_signals_zset, now_ms, A.signal_id)

  -- No resume condition evaluation — waitpoint is pending, not active.

  return ok(A.signal_id, "buffered_for_pending_waitpoint")
end)

---------------------------------------------------------------------------
-- #2  ff_claim_resumed_execution
--
-- Consume claim-grant, resume existing attempt (interrupted -> started),
-- create new lease bound to SAME attempt. Does NOT create a new attempt.
--
-- KEYS (11): exec_core, claim_grant, eligible_zset, lease_expiry_zset,
--            worker_leases, existing_attempt_hash, lease_current,
--            lease_history, active_index, attempt_timeout_zset,
--            execution_deadline_zset
-- ARGV (8): execution_id, worker_id, worker_instance_id, lane,
--           capability_snapshot_hash, lease_id, lease_ttl_ms,
--           remaining_attempt_timeout_ms
---------------------------------------------------------------------------
redis.register_function('ff_claim_resumed_execution', function(keys, args)
  local K = {
    core_key               = keys[1],
    claim_grant_key        = keys[2],
    eligible_zset          = keys[3],
    lease_expiry_key       = keys[4],
    worker_leases_key      = keys[5],
    attempt_hash           = keys[6],
    lease_current_key      = keys[7],
    lease_history_key      = keys[8],
    active_index_key       = keys[9],
    attempt_timeout_key    = keys[10],
    execution_deadline_key = keys[11],
  }

  local lease_ttl_n = require_number(args[7], "lease_ttl_ms")
  if type(lease_ttl_n) == "table" then return lease_ttl_n end

  local A = {
    execution_id                  = args[1],
    worker_id                     = args[2],
    worker_instance_id            = args[3],
    lane                          = args[4],
    capability_snapshot_hash      = args[5] or "",
    lease_id                      = args[6],
    lease_ttl_ms                  = lease_ttl_n,
    remaining_attempt_timeout_ms  = args[8] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution exists
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- 2. Must be runnable
  if core.lifecycle_phase ~= "runnable" then
    return err("execution_not_leaseable")
  end

  -- 3. Must be attempt_interrupted (resumed after suspension/delay)
  if core.attempt_state ~= "attempt_interrupted" then
    return err("not_a_resumed_execution")
  end

  -- 4. Validate claim grant
  local grant_raw = redis.call("HGETALL", K.claim_grant_key)
  if #grant_raw == 0 then
    return err("invalid_claim_grant")
  end
  local grant = hgetall_to_table(grant_raw)

  -- Validate grant matches (grant key is execution-scoped, so only check worker_id)
  if grant.worker_id ~= A.worker_id then
    return err("invalid_claim_grant")
  end

  -- Check grant expiry
  if is_set(grant.grant_expires_at) and tonumber(grant.grant_expires_at) < now_ms then
    redis.call("DEL", K.claim_grant_key)
    return err("claim_grant_expired")
  end

  -- Consume grant (DEL)
  redis.call("DEL", K.claim_grant_key)

  -- 5. Resume existing attempt: attempt_interrupted -> started
  --    Same attempt continues — no new attempt_index.
  local att_idx = core.current_attempt_index
  local att_id = core.current_attempt_id
  local epoch = tonumber(core.current_lease_epoch or "0") + 1
  local expires_at = now_ms + A.lease_ttl_ms
  local renewal_deadline = now_ms + math.floor(A.lease_ttl_ms * 2 / 3)

  redis.call("HSET", K.attempt_hash,
    "attempt_state", "started",
    "resumed_at", tostring(now_ms),
    "lease_id", A.lease_id,
    "lease_epoch", tostring(epoch),
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "suspended_at", "",
    "suspension_id", "")

  -- 6. Create new lease bound to same attempt
  redis.call("DEL", K.lease_current_key)
  redis.call("HSET", K.lease_current_key,
    "lease_id", A.lease_id,
    "lease_epoch", tostring(epoch),
    "execution_id", A.execution_id,
    "attempt_id", att_id,
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "acquired_at", tostring(now_ms),
    "expires_at", tostring(expires_at),
    "last_renewed_at", tostring(now_ms),
    "renewal_deadline", tostring(renewal_deadline))

  -- 7. Update exec_core — ALL 7 state vector dimensions
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "active",
    "ownership_state", "leased",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "blocking_detail", "",
    "terminal_outcome", "none",
    "attempt_state", "running_attempt",
    "public_state", "active",
    "current_lease_id", A.lease_id,
    "current_lease_epoch", tostring(epoch),
    "current_worker_id", A.worker_id,
    "current_worker_instance_id", A.worker_instance_id,
    "current_lane", A.lane,
    "lease_acquired_at", tostring(now_ms),
    "lease_expires_at", tostring(expires_at),
    "lease_last_renewed_at", tostring(now_ms),
    "lease_renewal_deadline", tostring(renewal_deadline),
    "lease_expired_at", "",
    "lease_revoked_at", "",
    "lease_revoke_reason", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- 8. Update indexes
  redis.call("ZREM", K.eligible_zset, A.execution_id)
  redis.call("ZADD", K.lease_expiry_key, expires_at, A.execution_id)
  redis.call("SADD", K.worker_leases_key, A.execution_id)
  redis.call("ZADD", K.active_index_key, expires_at, A.execution_id)

  -- 9. ZADD attempt_timeout with remaining timeout
  if is_set(A.remaining_attempt_timeout_ms) then
    local remaining = tonumber(A.remaining_attempt_timeout_ms)
    if remaining > 0 then
      redis.call("ZADD", K.attempt_timeout_key,
        now_ms + remaining, A.execution_id)
    end
  end

  -- 10. Lease history event
  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", 1000, "*",
    "event", "acquired",
    "lease_id", A.lease_id,
    "lease_epoch", tostring(epoch),
    "attempt_index", att_idx,
    "attempt_id", att_id,
    "worker_id", A.worker_id,
    "reason", "claim_resumed",
    "ts", tostring(now_ms))

  return ok(A.lease_id, tostring(epoch), tostring(expires_at),
            att_id, att_idx, "resumed")
end)
