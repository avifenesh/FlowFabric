-- FlowFabric execution lifecycle functions
-- Reference: RFC-001 (Execution), RFC-010 §4 (function inventory)
--
-- Depends on helpers: ok, err, hgetall_to_table, is_set,
--   validate_lease, validate_lease_and_mark_expired,
--   clear_lease_and_indexes, defensive_zrem_all_indexes, unpack_policy

---------------------------------------------------------------------------
-- #0  ff_create_execution
--
-- Creates a new execution: core hash, payload, policy, tags, indexes.
-- Idempotent via idempotency key (SET NX with TTL).
--
-- KEYS (8): exec_core, payload_key, policy_key, tags_key,
--           eligible_or_delayed_zset, idem_key,
--           execution_deadline_zset, all_executions_set
-- ARGV (13): execution_id, namespace, lane_id, execution_kind,
--            priority, creator_identity, policy_json,
--            input_payload, delay_until, dedup_ttl_ms,
--            tags_json, execution_deadline_at, partition_id
---------------------------------------------------------------------------
redis.register_function('ff_create_execution', function(keys, args)
  local K = {
    core_key            = keys[1],
    payload_key         = keys[2],
    policy_key          = keys[3],
    tags_key            = keys[4],
    scheduling_zset     = keys[5],  -- eligible or delayed
    idem_key            = keys[6],
    deadline_zset       = keys[7],
    all_executions_set  = keys[8],
  }

  local priority_n = require_number(args[5], "priority")
  if type(priority_n) == "table" then return priority_n end

  local A = {
    execution_id        = args[1],
    namespace           = args[2],
    lane_id             = args[3],
    execution_kind      = args[4],
    priority            = priority_n,
    creator_identity    = args[6],
    policy_json         = args[7],
    input_payload       = args[8],
    delay_until         = args[9],   -- "" or ms timestamp
    dedup_ttl_ms        = args[10],  -- "" or ms
    tags_json           = args[11],  -- "" or JSON object
    execution_deadline_at = args[12], -- "" or ms timestamp
    partition_id        = args[13],
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- Clamp priority to [0, 9000]. The composite eligible-ZSET score formula
  -- -(priority * 1_000_000_000_000) + created_at uses Lua doubles (IEEE 754,
  -- 53-bit mantissa). priority > 9007 overflows the multiplication step;
  -- priority > ~8300 overflows the combined score when created_at is added.
  -- Clamping to 9000 keeps a safe margin while supporting 4+ orders of magnitude.
  if A.priority < 0 then A.priority = 0 end
  if A.priority > 9000 then A.priority = 9000 end

  -- 1. Idempotency check (only when idem_key is a real key, not the noop placeholder)
  if K.idem_key ~= "" and not string.find(K.idem_key, "ff:noop:") then
    local existing = redis.call("GET", K.idem_key)
    if existing then
      return ok_duplicate(existing)
    end
  end

  -- 2. Guard: execution already exists
  if redis.call("EXISTS", K.core_key) == 1 then
    return {1, "DUPLICATE", A.execution_id}
  end

  -- 2b. Policy JSON validation — extract required_capabilities CSV now, BEFORE
  -- any write, so an error aborts without leaving a half-written exec_core.
  -- Fail-CLOSED on any malformed/typed input rather than silently defaulting
  -- to the empty-required wildcard — required_capabilities is security-
  -- sensitive (an empty set matches any worker).
  --
  --   * An empty / "{}" policy_json → no required_capabilities field written
  --     → wildcard match, intentional.
  --   * A non-empty policy_json that fails cjson.decode → fail with
  --     invalid_policy_json. An operator who MEANT "no policy" passes "".
  --   * routing_requirements.required_capabilities present but not an array
  --     → fail with invalid_capabilities:required_not_array.
  --   * Any element that is not a non-empty string → fail with
  --     invalid_capabilities:required:non_string_token. Silent-drop on
  --     `["gpu", null, 42]` would erase real requirements.
  --   * Comma in a token → fail (reserved delimiter, see ff_issue_claim_grant).
  --   * Bounds: same 4096 bytes / 256 tokens ceiling as the worker CSV.
  local required_caps_csv = nil
  if is_set(A.policy_json) and A.policy_json ~= "{}" then
    local ok_decode, policy = pcall(cjson.decode, A.policy_json)
    if not ok_decode then
      return err("invalid_policy_json", "malformed")
    end
    if type(policy) == "table" and policy.routing_requirements ~= nil then
      if type(policy.routing_requirements) ~= "table" then
        return err("invalid_policy_json", "routing_requirements:not_object")
      end
      local caps = policy.routing_requirements.required_capabilities
      if caps ~= nil then
        if type(caps) ~= "table" then
          return err("invalid_capabilities", "required:not_array")
        end
        local list = {}
        for _, cap in ipairs(caps) do
          if type(cap) ~= "string" then
            return err("invalid_capabilities", "required:non_string_token")
          end
          if #cap == 0 then
            return err("invalid_capabilities", "required:empty_token")
          end
          if string.find(cap, ",", 1, true) then
            return err("invalid_capabilities", "required:comma_in_token")
          end
          list[#list + 1] = cap
        end
        if #list > CAPS_MAX_TOKENS then
          return err("invalid_capabilities", "required:too_many_tokens")
        end
        table.sort(list)
        if #list > 0 then
          local csv = table.concat(list, ",")
          if #csv > CAPS_MAX_BYTES then
            return err("invalid_capabilities", "required:too_many_bytes")
          end
          required_caps_csv = csv
        end
      end
    end
  end

  -- 3. Determine initial eligibility
  local lifecycle_phase = "runnable"
  local eligibility_state, blocking_reason, blocking_detail, public_state
  local is_delayed = is_set(A.delay_until) and tonumber(A.delay_until) > now_ms

  if is_delayed then
    eligibility_state = "not_eligible_until_time"
    blocking_reason   = "waiting_for_delay"
    blocking_detail   = "delayed until " .. A.delay_until
    public_state      = "delayed"
  else
    eligibility_state = "eligible_now"
    blocking_reason   = "waiting_for_worker"
    blocking_detail   = ""
    public_state      = "waiting"
  end

  -- 4. Create exec_core — ALL 7 state vector dimensions
  redis.call("HSET", K.core_key,
    "execution_id", A.execution_id,
    "namespace", A.namespace,
    "lane_id", A.lane_id,
    "execution_kind", A.execution_kind,
    "partition_id", A.partition_id,
    "priority", A.priority,
    "creator_identity", A.creator_identity,
    -- 7 state vector dimensions
    "lifecycle_phase", lifecycle_phase,
    "ownership_state", "unowned",
    "eligibility_state", eligibility_state,
    "blocking_reason", blocking_reason,
    "blocking_detail", blocking_detail,
    "terminal_outcome", "none",
    "attempt_state", "pending_first_attempt",
    "public_state", public_state,
    -- accounting
    "total_attempt_count", "0",
    "current_attempt_index", "",
    "current_attempt_id", "",
    "current_lease_id", "",
    "current_lease_epoch", "0",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "current_lane", A.lane_id,
    "retry_count", "0",
    "replay_count", "0",
    "lease_reclaim_count", "0",
    -- timestamps
    "created_at", now_ms,
    "started_at", "",
    "completed_at", "",
    "last_transition_at", now_ms,
    "last_mutation_at", now_ms,
    -- lease fields (cleared)
    "lease_acquired_at", "",
    "lease_expires_at", "",
    "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "lease_expired_at", "",
    "lease_revoked_at", "",
    "lease_revoke_reason", "",
    -- delay
    "delay_until", is_delayed and A.delay_until or "",
    -- suspension (cleared)
    "current_suspension_id", "",
    "current_waitpoint_id", "",
    -- pending lineage (cleared)
    "pending_retry_reason", "",
    "pending_replay_reason", "",
    "pending_replay_requested_by", "",
    "pending_previous_attempt_index", "",
    -- progress
    "progress_pct", "",
    "progress_message", "",
    "progress_updated_at", "",
    -- flow
    "flow_id", "")

  -- 5. Store payload
  if is_set(A.input_payload) then
    redis.call("SET", K.payload_key, A.input_payload)
  end

  -- 6. Store policy + required_capabilities. Validation already ran before
  -- any write (step 2b); here we only persist the pre-validated CSV. See
  -- step 2b for the fail-closed rationale on malformed / typed inputs.
  if is_set(A.policy_json) then
    redis.call("SET", K.policy_key, A.policy_json)
  end
  if required_caps_csv then
    redis.call("HSET", K.core_key, "required_capabilities", required_caps_csv)
  end

  -- 7. Store tags
  if is_set(A.tags_json) and A.tags_json ~= "{}" then
    local ok_decode, tags = pcall(cjson.decode, A.tags_json)
    if ok_decode and type(tags) == "table" then
      local flat = {}
      for k, v in pairs(tags) do
        flat[#flat + 1] = k
        flat[#flat + 1] = tostring(v)
      end
      if #flat > 0 then
        redis.call("HSET", K.tags_key, unpack(flat))
      end
    end
  end

  -- 8. Add to scheduling index
  if is_delayed then
    redis.call("ZADD", K.scheduling_zset, tonumber(A.delay_until), A.execution_id)
  else
    -- Composite score: -(priority * 1_000_000_000_000) + created_at_ms
    local score = 0 - (A.priority * 1000000000000) + now_ms
    redis.call("ZADD", K.scheduling_zset, score, A.execution_id)
  end

  -- 9. SADD to all_executions partition index
  redis.call("SADD", K.all_executions_set, A.execution_id)

  -- 10. Execution deadline index
  if is_set(A.execution_deadline_at) then
    redis.call("ZADD", K.deadline_zset, tonumber(A.execution_deadline_at), A.execution_id)
  end

  -- 11. Set idempotency key with TTL
  -- Guard: PX 0 or PX <0 causes Valkey error ("invalid expire time"),
  -- which would abort the FCALL after exec_core was already written (step 4).
  local dedup_ms = tonumber(A.dedup_ttl_ms) or 0
  if dedup_ms > 0 and K.idem_key ~= "" and not string.find(K.idem_key, "ff:noop:") then
    redis.call("SET", K.idem_key, A.execution_id,
      "PX", dedup_ms)
  end

  return ok(A.execution_id, public_state)
end)

---------------------------------------------------------------------------
-- #1  ff_claim_execution (new attempt)
--
-- Consumes a claim grant, creates a new attempt + lease, transitions
-- runnable → active. Attempt type derived from exec_core attempt_state.
--
-- KEYS (14): exec_core, claim_grant, eligible_zset, lease_expiry_zset,
--            worker_leases, attempt_hash, attempt_usage, attempt_policy,
--            attempts_zset, lease_current, lease_history, active_index,
--            attempt_timeout_zset, execution_deadline_zset
-- ARGV (12): execution_id, worker_id, worker_instance_id, lane,
--            capability_snapshot_hash, lease_id, lease_ttl_ms,
--            renew_before_ms, attempt_id, attempt_policy_json,
--            attempt_timeout_ms, execution_deadline_at
--
-- KNOWN LIMITATION (flow-cancel race): ff_claim_execution reads
-- exec_core on {p:N} but cannot atomically read flow_core on {fp:N}
-- (cross-slot). If ff_cancel_flow fired and its async member dispatch
-- was dropped by a transient Valkey error, the member's exec_core has
-- NOT yet been flipped to terminal — so a worker may still claim it and
-- run it to completion inside a cancelled flow. The flow's own
-- public_flow_state is correctly terminal; only this one member escapes.
-- Mitigations already in place:
--  * cancel_flow(wait=true) avoids the bg dispatch entirely.
--  * ff_apply_dependency_to_child rejects additions to terminal flows,
--    so children cannot be added mid-cancel.
--  * retention eventually trims the stale member.
-- Full fix (flag on exec_core maintained by a broadcast loop) is a
-- deliberate design change deferred past Batch A.
---------------------------------------------------------------------------
redis.register_function('ff_claim_execution', function(keys, args)
  local K = {
    core_key          = keys[1],
    claim_grant       = keys[2],
    eligible_zset     = keys[3],
    lease_expiry_key  = keys[4],
    worker_leases_key = keys[5],
    attempt_hash      = keys[6],
    attempt_usage     = keys[7],
    attempt_policy    = keys[8],
    attempts_zset     = keys[9],
    lease_current_key = keys[10],
    lease_history_key = keys[11],
    active_index_key  = keys[12],
    attempt_timeout_key = keys[13],
    execution_deadline_key = keys[14],
  }

  local lease_ttl_n = require_number(args[7], "lease_ttl_ms")
  if type(lease_ttl_n) == "table" then return lease_ttl_n end
  local renew_before_n = require_number(args[8], "renew_before_ms")
  if type(renew_before_n) == "table" then return renew_before_n end

  local A = {
    execution_id         = args[1],
    worker_id            = args[2],
    worker_instance_id   = args[3],
    lane                 = args[4],
    capability_hash      = args[5],
    lease_id             = args[6],
    lease_ttl_ms         = lease_ttl_n,
    renew_before_ms      = renew_before_n,
    attempt_id           = args[9],
    attempt_policy_json  = args[10],
    attempt_timeout_ms   = args[11],  -- "" or ms
    execution_deadline_at = args[12], -- "" or ms
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution state
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "runnable" then return err("execution_not_leaseable") end
  if core.ownership_state ~= "unowned" then return err("lease_conflict") end
  if core.eligibility_state ~= "eligible_now" then return err("execution_not_leaseable") end
  if core.terminal_outcome ~= "none" then return err("execution_not_leaseable") end

  -- Defense-in-depth: invariant A3
  if core.attempt_state == "running_attempt" then
    return err("active_attempt_exists")
  end
  -- Dispatch: resume-from-suspension must use claim_resumed_execution
  if core.attempt_state == "attempt_interrupted" then
    return err("use_claim_resumed_execution")
  end

  -- 2. Validate and consume claim grant
  local grant_raw = redis.call("HGETALL", K.claim_grant)
  if #grant_raw == 0 then return err("invalid_claim_grant") end
  local grant = hgetall_to_table(grant_raw)

  if grant.worker_id ~= A.worker_id then
    return err("invalid_claim_grant")
  end
  if is_set(grant.grant_expires_at) and tonumber(grant.grant_expires_at) < now_ms then
    return err("claim_grant_expired")
  end
  redis.call("DEL", K.claim_grant)

  -- 3. Compute lease fields
  local next_epoch = tonumber(core.current_lease_epoch or "0") + 1
  local expires_at = now_ms + A.lease_ttl_ms
  local renewal_deadline = now_ms + A.renew_before_ms
  local next_att_idx = tonumber(core.total_attempt_count or "0")

  -- 4. Derive attempt type from exec core attempt_state
  local attempt_type = "initial"
  local lineage_fields = {}
  if core.attempt_state == "pending_retry_attempt" then
    attempt_type = "retry"
    lineage_fields = {
      "retry_reason", core.pending_retry_reason or "",
      "previous_attempt_index", core.pending_previous_attempt_index or ""
    }
  elseif core.attempt_state == "pending_replay_attempt" then
    attempt_type = "replay"
    lineage_fields = {
      "replay_reason", core.pending_replay_reason or "",
      "replay_requested_by", core.pending_replay_requested_by or "",
      "replayed_from_attempt_index", core.pending_previous_attempt_index or ""
    }
  end

  -- 5. Create attempt record
  --    Construct actual attempt key from hash tag + computed index.
  --    KEYS[6] is a placeholder (always index 0); on retry/replay the
  --    actual index is > 0, so we build the real key dynamically.
  --    All attempt keys share the same {p:N} hash tag → same Cluster slot.
  local tag = string.match(K.core_key, "(%b{})")
  local att_key = "ff:attempt:" .. tag .. ":" .. A.execution_id .. ":" .. tostring(next_att_idx)
  local att_usage_key = att_key .. ":usage"
  local att_policy_key = att_key .. ":policy"

  local attempt_fields = {
    "attempt_id", A.attempt_id,
    "execution_id", A.execution_id,
    "attempt_index", tostring(next_att_idx),
    "attempt_type", attempt_type,
    "attempt_state", "started",
    "created_at", tostring(now_ms),
    "started_at", tostring(now_ms),
    "lease_id", A.lease_id,
    "lease_epoch", tostring(next_epoch),
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id
  }
  for _, v in ipairs(lineage_fields) do
    attempt_fields[#attempt_fields + 1] = v
  end
  redis.call("HSET", att_key, unpack(attempt_fields))
  redis.call("ZADD", K.attempts_zset, now_ms, tostring(next_att_idx))

  -- 5b. Initialize attempt usage counters
  redis.call("HSET", att_usage_key,
    "last_usage_report_seq", "0")

  -- 5c. Store attempt policy snapshot
  if is_set(A.attempt_policy_json) then
    local policy_flat = unpack_policy(A.attempt_policy_json)
    if #policy_flat > 0 then
      redis.call("HSET", att_policy_key, unpack(policy_flat))
    end
  end

  -- 6. Create lease record
  redis.call("DEL", K.lease_current_key)
  redis.call("HSET", K.lease_current_key,
    "lease_id", A.lease_id,
    "lease_epoch", tostring(next_epoch),
    "execution_id", A.execution_id,
    "attempt_id", A.attempt_id,
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "acquired_at", tostring(now_ms),
    "expires_at", tostring(expires_at),
    "last_renewed_at", tostring(now_ms),
    "renewal_deadline", tostring(renewal_deadline))

  -- 7. Update execution core — ALL 7 state vector dimensions
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "active",
    "ownership_state", "leased",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "blocking_detail", "",
    "terminal_outcome", "none",
    "attempt_state", "running_attempt",
    "public_state", "active",
    "current_attempt_index", tostring(next_att_idx),
    "total_attempt_count", tostring(next_att_idx + 1),
    "current_attempt_id", A.attempt_id,
    "current_lease_id", A.lease_id,
    "current_lease_epoch", tostring(next_epoch),
    "current_worker_id", A.worker_id,
    "current_worker_instance_id", A.worker_instance_id,
    "current_lane", A.lane,
    "lease_acquired_at", tostring(now_ms),
    "lease_expires_at", tostring(expires_at),
    "lease_last_renewed_at", tostring(now_ms),
    "lease_renewal_deadline", tostring(renewal_deadline),
    "started_at", core.started_at or tostring(now_ms),
    -- Clear pending lineage fields (consumed above)
    "pending_retry_reason", "",
    "pending_replay_reason", "",
    "pending_replay_requested_by", "",
    "pending_previous_attempt_index", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- 8. Update indexes
  redis.call("ZREM", K.eligible_zset, A.execution_id)
  redis.call("ZADD", K.lease_expiry_key, expires_at, A.execution_id)
  redis.call("SADD", K.worker_leases_key, A.execution_id)
  redis.call("ZADD", K.active_index_key, expires_at, A.execution_id)

  -- 8a. Timeout indexes
  if is_set(A.attempt_timeout_ms) and A.attempt_timeout_ms ~= "0" then
    redis.call("ZADD", K.attempt_timeout_key,
      now_ms + tonumber(A.attempt_timeout_ms), A.execution_id)
  end
  if is_set(A.execution_deadline_at) and A.execution_deadline_at ~= "0" then
    redis.call("ZADD", K.execution_deadline_key,
      tonumber(A.execution_deadline_at), A.execution_id)
  end

  -- 9. Lease history event
  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", 1000, "*",
    "event", "acquired",
    "lease_id", A.lease_id,
    "lease_epoch", tostring(next_epoch),
    "attempt_id", A.attempt_id,
    "attempt_index", tostring(next_att_idx),
    "worker_id", A.worker_id,
    "ts", tostring(now_ms))

  return ok(A.lease_id, tostring(next_epoch), tostring(expires_at),
            A.attempt_id, tostring(next_att_idx), attempt_type)
end)

---------------------------------------------------------------------------
-- #3  ff_complete_execution
--
-- Validate lease, end attempt, store result, transition active→terminal.
--
-- KEYS (12): exec_core, attempt_hash, lease_expiry_zset, worker_leases,
--            terminal_zset, lease_current, lease_history, active_index,
--            stream_meta, result_key, attempt_timeout_zset,
--            execution_deadline_zset
-- ARGV (5): execution_id, lease_id, lease_epoch, attempt_id, result_payload
---------------------------------------------------------------------------
redis.register_function('ff_complete_execution', function(keys, args)
  local K = {
    core_key            = keys[1],
    attempt_hash        = keys[2],
    lease_expiry_key    = keys[3],
    worker_leases_key   = keys[4],
    terminal_zset       = keys[5],
    lease_current_key   = keys[6],
    lease_history_key   = keys[7],
    active_index_key    = keys[8],
    stream_meta         = keys[9],
    result_key          = keys[10],
    attempt_timeout_key = keys[11],
    execution_deadline_key = keys[12],
  }

  local A = {
    execution_id  = args[1],
    lease_id      = args[2],
    lease_epoch   = args[3],
    attempt_id    = args[4],
    result_payload = args[5] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- Read + validate lease
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  local lease_err = validate_lease_and_mark_expired(
    core, A, now_ms, K, 1000)
  if lease_err then return lease_err end

  -- End attempt
  redis.call("HSET", K.attempt_hash,
    "attempt_state", "ended_success",
    "ended_at", tostring(now_ms))

  -- Close stream if exists
  if redis.call("EXISTS", K.stream_meta) == 1 then
    redis.call("HSET", K.stream_meta,
      "closed_at", tostring(now_ms),
      "closed_reason", "attempt_success")
  end

  -- Store result
  if A.result_payload ~= "" then
    redis.call("SET", K.result_key, A.result_payload)
  end

  -- Update execution core — ALL 7 state vector dimensions
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "terminal",
    "ownership_state", "unowned",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "blocking_detail", "",
    "terminal_outcome", "success",
    "attempt_state", "attempt_terminal",
    "public_state", "completed",
    "completed_at", tostring(now_ms),
    "current_lease_id", "",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "lease_expires_at", "",
    "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "lease_expired_at", "",
    "lease_revoked_at", "",
    "lease_revoke_reason", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- Update indexes
  redis.call("ZREM", K.lease_expiry_key, A.execution_id)
  redis.call("SREM", K.worker_leases_key, A.execution_id)
  redis.call("ZADD", K.terminal_zset, now_ms, A.execution_id)
  redis.call("ZREM", K.active_index_key, A.execution_id)
  redis.call("ZREM", K.attempt_timeout_key, A.execution_id)
  redis.call("ZREM", K.execution_deadline_key, A.execution_id)

  -- Clean up lease
  redis.call("DEL", K.lease_current_key)
  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", 1000, "*",
    "event", "released",
    "lease_id", A.lease_id,
    "lease_epoch", A.lease_epoch,
    "attempt_index", core.current_attempt_index or "",
    "reason", "completed",
    "ts", tostring(now_ms))

  return ok("completed")
end)

---------------------------------------------------------------------------
-- #12a  ff_cancel_execution
--
-- Cancel from any non-terminal state. Multi-path:
--   active → validate lease or operator override, end attempt, clear lease
--   runnable → defensive ZREM all indexes
--   suspended → close suspension/waitpoint, end attempt
-- All paths: terminal(cancelled), defensive ZREM, ZADD terminal.
--
-- KEYS (21): exec_core, attempt_hash, stream_meta, lease_current,
--            lease_history, lease_expiry_zset, worker_leases,
--            suspension_current, waitpoint_hash, wp_condition,
--            suspension_timeout_zset, terminal_zset,
--            attempt_timeout_zset, execution_deadline_zset,
--            eligible_zset, delayed_zset, blocked_deps_zset,
--            blocked_budget_zset, blocked_quota_zset,
--            blocked_route_zset, blocked_operator_zset
-- ARGV (5): execution_id, reason, source, lease_id, lease_epoch
---------------------------------------------------------------------------
redis.register_function('ff_cancel_execution', function(keys, args)
  local K = {
    core_key              = keys[1],
    attempt_hash          = keys[2],
    stream_meta           = keys[3],
    lease_current_key     = keys[4],
    lease_history_key     = keys[5],
    lease_expiry_key      = keys[6],
    worker_leases_key     = keys[7],
    suspension_current    = keys[8],
    waitpoint_hash        = keys[9],
    wp_condition          = keys[10],
    suspension_timeout_key = keys[11],
    terminal_key          = keys[12],
    attempt_timeout_key   = keys[13],
    execution_deadline_key = keys[14],
    eligible_key          = keys[15],
    delayed_key           = keys[16],
    blocked_deps_key      = keys[17],
    blocked_budget_key    = keys[18],
    blocked_quota_key     = keys[19],
    blocked_route_key     = keys[20],
    blocked_operator_key  = keys[21],
    -- active_index_key and suspended_key are constructed dynamically below
    -- from the hash tag + lane_id (avoids changing KEYS count).
    active_index_key      = nil,
    suspended_key         = nil,
  }

  local A = {
    execution_id = args[1],
    reason       = args[2],
    source       = args[3] or "",  -- "operator_override" or ""
    lease_id     = args[4] or "",
    lease_epoch  = args[5] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- Construct lane:active and lane:suspended keys dynamically from hash tag.
  -- These are not in the 21 KEYS array but defensive_zrem_all_indexes needs
  -- them to clean stale entries when cancelling active or suspended executions.
  local tag = string.match(K.core_key, "(%b{})")
  local lane = core.lane_id or "default"
  K.active_index_key = "ff:idx:" .. tag .. ":lane:" .. lane .. ":active"
  K.suspended_key    = "ff:idx:" .. tag .. ":lane:" .. lane .. ":suspended"

  -- Already terminal
  if core.lifecycle_phase == "terminal" then
    return err("execution_not_active",
      core.terminal_outcome or "", core.current_lease_epoch or "")
  end

  local cancelled_from = core.lifecycle_phase

  -- PATH: Active
  if core.lifecycle_phase == "active" then
    -- Require lease validation or operator override
    if A.source ~= "operator_override" then
      if core.ownership_state == "lease_revoked" then
        return err("lease_revoked")
      end
      if is_set(A.lease_id) then
        if core.current_lease_id ~= A.lease_id then return err("stale_lease") end
        if core.current_lease_epoch ~= A.lease_epoch then return err("stale_lease") end
      end
    end

    -- End attempt
    if is_set(core.current_attempt_index) then
      redis.call("HSET", K.attempt_hash,
        "attempt_state", "ended_cancelled",
        "ended_at", tostring(now_ms),
        "failure_reason", "cancelled: " .. A.reason)
    end

    -- Close stream if exists
    if redis.call("EXISTS", K.stream_meta) == 1 then
      redis.call("HSET", K.stream_meta,
        "closed_at", tostring(now_ms),
        "closed_reason", "attempt_cancelled")
    end

    -- Release lease
    redis.call("DEL", K.lease_current_key)
    redis.call("XADD", K.lease_history_key, "MAXLEN", "~", "1000", "*",
      "event", "released",
      "lease_id", core.current_lease_id or "",
      "lease_epoch", core.current_lease_epoch or "",
      "attempt_index", core.current_attempt_index or "",
      "reason", "cancelled",
      "ts", tostring(now_ms))
  end

  -- PATH: Suspended
  if core.lifecycle_phase == "suspended" then
    -- End attempt (interrupted → ended_cancelled)
    if is_set(core.current_attempt_index) then
      redis.call("HSET", K.attempt_hash,
        "attempt_state", "ended_cancelled",
        "ended_at", tostring(now_ms),
        "failure_reason", "cancelled: " .. A.reason)
    end

    -- Close stream if exists
    if redis.call("EXISTS", K.stream_meta) == 1 then
      redis.call("HSET", K.stream_meta,
        "closed_at", tostring(now_ms),
        "closed_reason", "attempt_cancelled")
    end

    -- Close suspension + waitpoint + wp_condition
    if redis.call("EXISTS", K.suspension_current) == 1 then
      redis.call("HSET", K.suspension_current,
        "closed_at", tostring(now_ms),
        "close_reason", "cancelled")
    end
    if redis.call("EXISTS", K.waitpoint_hash) == 1 then
      redis.call("HSET", K.waitpoint_hash,
        "state", "closed",
        "closed_at", tostring(now_ms),
        "close_reason", "cancelled")
    end
    if redis.call("EXISTS", K.wp_condition) == 1 then
      redis.call("HSET", K.wp_condition,
        "closed", "1",
        "closed_at", tostring(now_ms),
        "closed_reason", "cancelled")
    end
  end

  -- ALL PATHS: exec_core FIRST for terminal transition (§4.8b Rule 2)
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "terminal",
    "ownership_state", "unowned",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "blocking_detail", "",
    "terminal_outcome", "cancelled",
    "attempt_state", is_set(core.current_attempt_index) and "attempt_terminal" or (core.attempt_state or "none"),
    "public_state", "cancelled",
    "cancellation_reason", A.reason,
    "cancelled_by", A.source ~= "" and A.source or A.execution_id,
    "completed_at", tostring(now_ms),
    "current_lease_id", "",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "lease_expires_at", "",
    "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "current_suspension_id", "",
    "current_waitpoint_id", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- Defensive ZREM from ALL scheduling + timeout indexes
  defensive_zrem_all_indexes(K, A.execution_id, K.terminal_key)

  -- ZADD terminal (unconditional)
  redis.call("ZADD", K.terminal_key, now_ms, A.execution_id)

  return ok("cancelled", cancelled_from)
end)

---------------------------------------------------------------------------
-- #30  ff_delay_execution
--
-- Worker delays its own active execution. Releases lease, transitions
-- to runnable + not_eligible_until_time. Same attempt continues (paused).
--
-- KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
--           lease_expiry_zset, worker_leases, active_index,
--           delayed_zset, attempt_timeout_zset
-- ARGV (5): execution_id, lease_id, lease_epoch, attempt_id, delay_until
---------------------------------------------------------------------------
redis.register_function('ff_delay_execution', function(keys, args)
  local K = {
    core_key            = keys[1],
    attempt_hash        = keys[2],
    lease_current_key   = keys[3],
    lease_history_key   = keys[4],
    lease_expiry_key    = keys[5],
    worker_leases_key   = keys[6],
    active_index_key    = keys[7],
    delayed_zset        = keys[8],
    attempt_timeout_key = keys[9],
  }

  local A = {
    execution_id = args[1],
    lease_id     = args[2],
    lease_epoch  = args[3],
    attempt_id   = args[4],
    delay_until  = args[5],
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- Validate lease
  local lease_err = validate_lease_and_mark_expired(
    core, A, now_ms, K, 1000)
  if lease_err then return lease_err end

  -- OOM-SAFE WRITE ORDERING: exec_core FIRST (point of no return)
  -- ALL 7 state vector dimensions
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", "not_eligible_until_time",
    "blocking_reason", "waiting_for_delay",
    "blocking_detail", "delayed until " .. A.delay_until,
    "terminal_outcome", "none",
    "attempt_state", "attempt_interrupted",
    "public_state", "delayed",
    "delay_until", A.delay_until,
    "current_lease_id", "",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "lease_expires_at", "",
    "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- Pause attempt: started → suspended
  redis.call("HSET", K.attempt_hash,
    "attempt_state", "suspended",
    "suspended_at", tostring(now_ms),
    "suspension_id", "worker_delay")

  -- Release lease + update indexes
  redis.call("DEL", K.lease_current_key)
  redis.call("ZREM", K.lease_expiry_key, A.execution_id)
  redis.call("SREM", K.worker_leases_key, A.execution_id)
  redis.call("ZREM", K.active_index_key, A.execution_id)
  redis.call("ZREM", K.attempt_timeout_key, A.execution_id)

  -- Add to delayed set
  redis.call("ZADD", K.delayed_zset, tonumber(A.delay_until), A.execution_id)

  -- Lease history event
  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", "1000", "*",
    "event", "released",
    "lease_id", A.lease_id,
    "lease_epoch", A.lease_epoch,
    "attempt_index", core.current_attempt_index or "",
    "attempt_id", A.attempt_id,
    "reason", "worker_delay",
    "ts", tostring(now_ms))

  return ok(A.delay_until)
end)

---------------------------------------------------------------------------
-- #31  ff_move_to_waiting_children
--
-- Worker blocks on child dependencies. Releases lease, transitions to
-- runnable + blocked_by_dependencies. Same attempt continues (paused).
--
-- KEYS (9): exec_core, attempt_hash, lease_current, lease_history,
--           lease_expiry_zset, worker_leases, active_index,
--           blocked_deps_zset, attempt_timeout_zset
-- ARGV (4): execution_id, lease_id, lease_epoch, attempt_id
---------------------------------------------------------------------------
redis.register_function('ff_move_to_waiting_children', function(keys, args)
  local K = {
    core_key            = keys[1],
    attempt_hash        = keys[2],
    lease_current_key   = keys[3],
    lease_history_key   = keys[4],
    lease_expiry_key    = keys[5],
    worker_leases_key   = keys[6],
    active_index_key    = keys[7],
    blocked_deps_zset   = keys[8],
    attempt_timeout_key = keys[9],
  }

  local A = {
    execution_id = args[1],
    lease_id     = args[2],
    lease_epoch  = args[3],
    attempt_id   = args[4],
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- Validate lease
  local lease_err = validate_lease_and_mark_expired(
    core, A, now_ms, K, 1000)
  if lease_err then return lease_err end

  -- OOM-SAFE WRITE ORDERING: exec_core FIRST (point of no return, §4.8b Rule 2)
  -- ALL 7 state vector dimensions
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", "blocked_by_dependencies",
    "blocking_reason", "waiting_for_children",
    "blocking_detail", "waiting for child executions to complete",
    "terminal_outcome", "none",
    "attempt_state", "attempt_interrupted",
    "public_state", "waiting_children",
    "current_lease_id", "",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "lease_expires_at", "",
    "lease_last_renewed_at", "",
    "lease_renewal_deadline", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- Pause attempt: started → suspended (waiting children, not ended)
  redis.call("HSET", K.attempt_hash,
    "attempt_state", "suspended",
    "suspended_at", tostring(now_ms),
    "suspension_id", "waiting_children")

  -- Release lease + update indexes
  redis.call("DEL", K.lease_current_key)
  redis.call("ZREM", K.lease_expiry_key, A.execution_id)
  redis.call("SREM", K.worker_leases_key, A.execution_id)
  redis.call("ZREM", K.active_index_key, A.execution_id)
  redis.call("ZREM", K.attempt_timeout_key, A.execution_id)

  -- Lease history event
  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", "1000", "*",
    "event", "released",
    "lease_id", A.lease_id,
    "lease_epoch", A.lease_epoch,
    "attempt_index", core.current_attempt_index or "",
    "attempt_id", A.attempt_id,
    "reason", "waiting_children",
    "ts", tostring(now_ms))

  -- Add to blocked:dependencies set
  redis.call("ZADD", K.blocked_deps_zset,
    tonumber(core.created_at or "0"), A.execution_id)

  return ok()
end)

---------------------------------------------------------------------------
-- #4  ff_fail_execution
--
-- Fail an active execution. If retries remain: set pending_retry_attempt
-- + lineage on exec_core (deferred creation — claim_execution creates
-- the attempt, per R21 fix). If max retries reached: terminal failure.
--
-- KEYS (12): exec_core, attempt_hash, lease_expiry_zset, worker_leases,
--            terminal_zset, delayed_zset, lease_current, lease_history,
--            active_index, stream_meta, attempt_timeout_zset,
--            execution_deadline_zset
-- ARGV (7): execution_id, lease_id, lease_epoch, attempt_id,
--           failure_reason, failure_category, retry_policy_json
---------------------------------------------------------------------------
redis.register_function('ff_fail_execution', function(keys, args)
  local K = {
    core_key              = keys[1],
    attempt_hash          = keys[2],
    lease_expiry_key      = keys[3],
    worker_leases_key     = keys[4],
    terminal_key          = keys[5],
    delayed_zset          = keys[6],
    lease_current_key     = keys[7],
    lease_history_key     = keys[8],
    active_index_key      = keys[9],
    stream_meta           = keys[10],
    attempt_timeout_key   = keys[11],
    execution_deadline_key = keys[12],
  }

  local A = {
    execution_id     = args[1],
    lease_id         = args[2],
    lease_epoch      = args[3],
    attempt_id       = args[4],
    failure_reason   = args[5],
    failure_category = args[6],
    retry_policy_json = args[7] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Read + validate lease
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  local lease_err = validate_lease_and_mark_expired(
    core, A, now_ms, K, 1000)
  if lease_err then return lease_err end

  -- 2. End current attempt
  redis.call("HSET", K.attempt_hash,
    "attempt_state", "ended_failure",
    "ended_at", tostring(now_ms),
    "failure_reason", A.failure_reason,
    "failure_category", A.failure_category)

  -- 3. Close stream if exists
  if redis.call("EXISTS", K.stream_meta) == 1 then
    redis.call("HSET", K.stream_meta,
      "closed_at", tostring(now_ms),
      "closed_reason", "attempt_failure")
  end

  -- 4. Determine retry eligibility
  local retry_count = tonumber(core.retry_count or "0")
  local max_retries = 0
  local backoff_ms = 1000
  local can_retry = false

  if is_set(A.retry_policy_json) then
    local ok_decode, policy = pcall(cjson.decode, A.retry_policy_json)
    if ok_decode and type(policy) == "table" then
      max_retries = tonumber(policy.max_retries or "0")
      if retry_count < max_retries then
        can_retry = true
        local bt = policy.backoff or {}
        if bt.type == "exponential" then
          local initial = (tonumber(bt.initial_delay_ms) or 1000)
          local max_d = (tonumber(bt.max_delay_ms) or 60000)
          local mult = (tonumber(bt.multiplier) or 2)
          backoff_ms = math.min(initial * (mult ^ retry_count), max_d)
        elseif bt.type == "fixed" then
          backoff_ms = (tonumber(bt.delay_ms) or 1000)
        end
      end
    end
  end

  if can_retry then
    -- RETRY PATH: deferred attempt creation (R21 fix)
    -- Do NOT create attempt record here. claim_execution creates the
    -- attempt with correct type (retry) by reading pending_retry_attempt.
    local delay_until = now_ms + backoff_ms

    -- ALL 7 state vector dimensions + pending lineage
    redis.call("HSET", K.core_key,
      "lifecycle_phase", "runnable",
      "ownership_state", "unowned",
      "eligibility_state", "not_eligible_until_time",
      "blocking_reason", "waiting_for_retry_backoff",
      "blocking_detail", "retry backoff until " .. tostring(delay_until) ..
        " (attempt " .. (core.current_attempt_index or "0") ..
        " failed: " .. A.failure_reason .. ")",
      "terminal_outcome", "none",
      "attempt_state", "pending_retry_attempt",
      "public_state", "delayed",
      -- Pending lineage for claim_execution to consume
      "pending_retry_reason", A.failure_reason,
      "pending_previous_attempt_index", core.current_attempt_index or "0",
      "retry_count", tostring(retry_count + 1),
      "current_attempt_id", "",
      "current_lease_id", "",
      "current_worker_id", "",
      "current_worker_instance_id", "",
      "lease_expires_at", "",
      "lease_last_renewed_at", "",
      "lease_renewal_deadline", "",
      "delay_until", tostring(delay_until),
      "failure_reason", A.failure_reason,
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    -- Release lease + update indexes
    redis.call("DEL", K.lease_current_key)
    redis.call("ZREM", K.lease_expiry_key, A.execution_id)
    redis.call("SREM", K.worker_leases_key, A.execution_id)
    redis.call("ZREM", K.active_index_key, A.execution_id)
    redis.call("ZREM", K.attempt_timeout_key, A.execution_id)

    -- ZADD to delayed index
    redis.call("ZADD", K.delayed_zset, delay_until, A.execution_id)

    -- Lease history
    redis.call("XADD", K.lease_history_key, "MAXLEN", "~", "1000", "*",
      "event", "released",
      "lease_id", A.lease_id,
      "lease_epoch", A.lease_epoch,
      "attempt_index", core.current_attempt_index or "",
      "reason", "failed_retry_scheduled",
      "ts", tostring(now_ms))

    return ok("retry_scheduled", tostring(delay_until))
  else
    -- TERMINAL PATH
    redis.call("HSET", K.core_key,
      "lifecycle_phase", "terminal",
      "ownership_state", "unowned",
      "eligibility_state", "not_applicable",
      "blocking_reason", "none",
      "blocking_detail", "",
      "terminal_outcome", "failed",
      "attempt_state", "attempt_terminal",
      "public_state", "failed",
      "failure_reason", A.failure_reason,
      "completed_at", tostring(now_ms),
      "current_lease_id", "",
      "current_worker_id", "",
      "current_worker_instance_id", "",
      "lease_expires_at", "",
      "lease_last_renewed_at", "",
      "lease_renewal_deadline", "",
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    -- Release lease + cleanup
    redis.call("DEL", K.lease_current_key)
    redis.call("ZREM", K.lease_expiry_key, A.execution_id)
    redis.call("SREM", K.worker_leases_key, A.execution_id)
    redis.call("ZREM", K.active_index_key, A.execution_id)
    redis.call("ZREM", K.attempt_timeout_key, A.execution_id)
    redis.call("ZREM", K.execution_deadline_key, A.execution_id)
    redis.call("ZADD", K.terminal_key, now_ms, A.execution_id)

    redis.call("XADD", K.lease_history_key, "MAXLEN", "~", "1000", "*",
      "event", "released",
      "lease_id", A.lease_id,
      "lease_epoch", A.lease_epoch,
      "attempt_index", core.current_attempt_index or "",
      "reason", "failed_terminal",
      "ts", tostring(now_ms))

    return ok("terminal_failed")
  end
end)

---------------------------------------------------------------------------
-- #26  ff_reclaim_execution
--
-- Atomically reclaim an expired/revoked execution: interrupt old attempt,
-- create new attempt + new lease.
--
-- KEYS (14): exec_core, claim_grant, old_attempt_hash, old_stream_meta,
--            new_attempt_hash, new_attempt_usage, attempts_zset,
--            lease_current, lease_history, lease_expiry_zset,
--            worker_leases, active_index, attempt_timeout_zset,
--            execution_deadline_zset
-- ARGV (8): execution_id, worker_id, worker_instance_id, lane,
--           lease_id, lease_ttl_ms, attempt_id, attempt_policy_json
---------------------------------------------------------------------------
redis.register_function('ff_reclaim_execution', function(keys, args)
  local K = {
    core_key              = keys[1],
    claim_grant           = keys[2],
    old_attempt_hash      = keys[3],
    old_stream_meta       = keys[4],
    new_attempt_hash      = keys[5],
    new_attempt_usage     = keys[6],
    attempts_zset         = keys[7],
    lease_current_key     = keys[8],
    lease_history_key     = keys[9],
    lease_expiry_key      = keys[10],
    worker_leases_key     = keys[11],
    active_index_key      = keys[12],
    attempt_timeout_key   = keys[13],
    execution_deadline_key = keys[14],
  }

  local reclaim_ttl_n = require_number(args[6], "lease_ttl_ms")
  if type(reclaim_ttl_n) == "table" then return reclaim_ttl_n end

  local A = {
    execution_id        = args[1],
    worker_id           = args[2],
    worker_instance_id  = args[3],
    lane                = args[4],
    lease_id            = args[5],
    lease_ttl_ms        = reclaim_ttl_n,
    attempt_id          = args[7],
    attempt_policy_json = args[8] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution state
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- Must be active + reclaimable
  if core.lifecycle_phase ~= "active" then
    return err("execution_not_reclaimable")
  end
  if core.ownership_state ~= "lease_expired_reclaimable"
    and core.ownership_state ~= "lease_revoked" then
    return err("execution_not_reclaimable")
  end

  -- Check max_reclaim_count
  local reclaim_count = tonumber(core.lease_reclaim_count or "0")
  local max_reclaim = 100  -- default
  -- Read from policy if available
  local policy_key = string.gsub(K.core_key, ":core$", ":policy")
  local policy_raw = redis.call("GET", policy_key)
  if policy_raw then
    local ok_p, policy = pcall(cjson.decode, policy_raw)
    if ok_p and type(policy) == "table" then
      max_reclaim = tonumber(policy.max_reclaim_count or "100")
    end
  end

  if reclaim_count >= max_reclaim then
    -- Terminal: max reclaims exceeded
    redis.call("HSET", K.core_key,
      "lifecycle_phase", "terminal",
      "ownership_state", "unowned",
      "eligibility_state", "not_applicable",
      "blocking_reason", "none",
      "blocking_detail", "",
      "terminal_outcome", "failed",
      "attempt_state", "attempt_terminal",
      "public_state", "failed",
      "failure_reason", "max_reclaims_exceeded",
      "completed_at", tostring(now_ms),
      "current_lease_id", "",
      "current_worker_id", "",
      "current_worker_instance_id", "",
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    redis.call("DEL", K.lease_current_key)
    redis.call("ZREM", K.lease_expiry_key, A.execution_id)
    redis.call("SREM", K.worker_leases_key, A.execution_id)
    -- Dynamic worker_leases SREM: K.worker_leases_key targets the NEW (reclaiming)
    -- worker's set, but the entry is in the OLD (expired) worker's set. Use
    -- current_worker_instance_id from exec_core to SREM from the correct set.
    local wiid = core.current_worker_instance_id or ""
    if wiid ~= "" then
      local tag_wl = string.match(K.core_key, "(%b{})")
      redis.call("SREM", "ff:idx:" .. tag_wl .. ":worker:" .. wiid .. ":leases", A.execution_id)
    end
    redis.call("ZREM", K.active_index_key, A.execution_id)
    redis.call("ZREM", K.attempt_timeout_key, A.execution_id)
    redis.call("ZREM", K.execution_deadline_key, A.execution_id)

    -- ZADD terminal (construct key from hash tag + lane)
    local tag = string.match(K.core_key, "(%b{})")
    local lane = core.lane_id or core.current_lane or "default"
    local terminal_key = "ff:idx:" .. tag .. ":lane:" .. lane .. ":terminal"
    redis.call("ZADD", terminal_key, now_ms, A.execution_id)

    return err("max_retries_exhausted")
  end

  -- 2. Validate and consume claim grant
  local grant_raw = redis.call("HGETALL", K.claim_grant)
  if #grant_raw == 0 then return err("invalid_claim_grant") end
  local grant = hgetall_to_table(grant_raw)
  if grant.worker_id ~= A.worker_id then return err("invalid_claim_grant") end
  redis.call("DEL", K.claim_grant)

  -- 3. Interrupt old attempt
  local old_att_idx = core.current_attempt_index or "0"
  redis.call("HSET", K.old_attempt_hash,
    "attempt_state", "interrupted_reclaimed",
    "ended_at", tostring(now_ms),
    "failure_reason", "lease_" .. (core.ownership_state or "expired"))

  -- Close old stream if exists
  if redis.call("EXISTS", K.old_stream_meta) == 1 then
    redis.call("HSET", K.old_stream_meta,
      "closed_at", tostring(now_ms),
      "closed_reason", "reclaimed")
  end

  -- 4. Create new attempt
  --    Construct actual attempt key from hash tag + computed index
  --    (same dynamic-key pattern as ff_claim_execution).
  local next_epoch = tonumber(core.current_lease_epoch or "0") + 1
  local next_att_idx = tonumber(core.total_attempt_count or "0")
  local expires_at = now_ms + A.lease_ttl_ms
  local renewal_deadline = now_ms + math.floor(A.lease_ttl_ms * 2 / 3)

  local tag = string.match(K.core_key, "(%b{})")
  local att_key = "ff:attempt:" .. tag .. ":" .. A.execution_id .. ":" .. tostring(next_att_idx)
  local att_usage_key = att_key .. ":usage"

  redis.call("HSET", att_key,
    "attempt_id", A.attempt_id,
    "execution_id", A.execution_id,
    "attempt_index", tostring(next_att_idx),
    "attempt_type", "reclaim",
    "attempt_state", "started",
    "created_at", tostring(now_ms),
    "started_at", tostring(now_ms),
    "lease_id", A.lease_id,
    "lease_epoch", tostring(next_epoch),
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "reclaim_reason", "lease_" .. (core.ownership_state or "expired"),
    "previous_attempt_index", old_att_idx)
  redis.call("ZADD", K.attempts_zset, now_ms, tostring(next_att_idx))

  -- Initialize usage counters
  redis.call("HSET", att_usage_key, "last_usage_report_seq", "0")

  -- 5. Create new lease
  redis.call("DEL", K.lease_current_key)
  redis.call("HSET", K.lease_current_key,
    "lease_id", A.lease_id,
    "lease_epoch", tostring(next_epoch),
    "execution_id", A.execution_id,
    "attempt_id", A.attempt_id,
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "acquired_at", tostring(now_ms),
    "expires_at", tostring(expires_at),
    "last_renewed_at", tostring(now_ms),
    "renewal_deadline", tostring(renewal_deadline))

  -- 6. Update exec_core — ALL 7 state vector dimensions
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "active",
    "ownership_state", "leased",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "blocking_detail", "",
    "terminal_outcome", "none",
    "attempt_state", "running_attempt",
    "public_state", "active",
    "current_attempt_index", tostring(next_att_idx),
    "total_attempt_count", tostring(next_att_idx + 1),
    "current_attempt_id", A.attempt_id,
    "current_lease_id", A.lease_id,
    "current_lease_epoch", tostring(next_epoch),
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
    "lease_reclaim_count", tostring(reclaim_count + 1),
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- 7. Update indexes
  redis.call("ZADD", K.lease_expiry_key, expires_at, A.execution_id)
  redis.call("SADD", K.worker_leases_key, A.execution_id)
  redis.call("ZADD", K.active_index_key, expires_at, A.execution_id)

  -- 8. Lease history events
  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", 1000, "*",
    "event", "reclaimed",
    "old_lease_epoch", core.current_lease_epoch or "",
    "new_lease_id", A.lease_id,
    "new_lease_epoch", tostring(next_epoch),
    "new_attempt_id", A.attempt_id,
    "new_attempt_index", tostring(next_att_idx),
    "worker_id", A.worker_id,
    "ts", tostring(now_ms))

  return ok(A.lease_id, tostring(next_epoch), tostring(expires_at),
            A.attempt_id, tostring(next_att_idx), "reclaim")
end)

---------------------------------------------------------------------------
-- #29  ff_expire_execution
--
-- Timeout-based expiration. Handles active, runnable, and suspended phases.
-- Called by the attempt_timeout and execution_deadline scanners.
--
-- KEYS (14): exec_core, attempt_hash, stream_meta, lease_current,
--            lease_history, lease_expiry_zset, worker_leases,
--            active_index, terminal_zset, attempt_timeout_zset,
--            execution_deadline_zset, suspended_zset,
--            suspension_timeout_zset, suspension_current
-- ARGV (2): execution_id, expire_reason
---------------------------------------------------------------------------
redis.register_function('ff_expire_execution', function(keys, args)
  local K = {
    core_key                = keys[1],
    attempt_hash            = keys[2],
    stream_meta             = keys[3],
    lease_current_key       = keys[4],
    lease_history_key       = keys[5],
    lease_expiry_key        = keys[6],
    worker_leases_key       = keys[7],
    active_index_key        = keys[8],
    terminal_key            = keys[9],
    attempt_timeout_key     = keys[10],
    execution_deadline_key  = keys[11],
    suspended_zset          = keys[12],
    suspension_timeout_key  = keys[13],
    suspension_current      = keys[14],
  }

  local A = {
    execution_id = args[1],
    expire_reason = args[2] or "attempt_timeout",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then
    redis.call("ZREM", K.attempt_timeout_key, A.execution_id)
    redis.call("ZREM", K.execution_deadline_key, A.execution_id)
    return ok("not_found_cleaned")
  end
  local core = hgetall_to_table(raw)

  -- Already terminal — no-op
  if core.lifecycle_phase == "terminal" then
    redis.call("ZREM", K.attempt_timeout_key, A.execution_id)
    redis.call("ZREM", K.execution_deadline_key, A.execution_id)
    return ok("already_terminal")
  end

  -- PATH: Active
  if core.lifecycle_phase == "active" then
    -- End attempt
    if is_set(core.current_attempt_index) then
      redis.call("HSET", K.attempt_hash,
        "attempt_state", "ended_failure",
        "ended_at", tostring(now_ms),
        "failure_reason", A.expire_reason)
    end
    -- Close stream
    if redis.call("EXISTS", K.stream_meta) == 1 then
      redis.call("HSET", K.stream_meta,
        "closed_at", tostring(now_ms),
        "closed_reason", "expired")
    end
    -- Release lease
    redis.call("DEL", K.lease_current_key)
    redis.call("ZREM", K.lease_expiry_key, A.execution_id)
    redis.call("SREM", K.worker_leases_key, A.execution_id)
    -- Dynamic worker_leases SREM: the scanner passes empty WorkerInstanceId
    -- in KEYS[7] (it can't know which worker holds the lease). Use the actual
    -- worker_instance_id from exec_core to SREM from the correct set.
    local wiid = core.current_worker_instance_id or ""
    if wiid ~= "" then
      local tag = string.match(K.core_key, "(%b{})")
      redis.call("SREM", "ff:idx:" .. tag .. ":worker:" .. wiid .. ":leases", A.execution_id)
    end
    redis.call("ZREM", K.active_index_key, A.execution_id)
    -- Lease history
    redis.call("XADD", K.lease_history_key, "MAXLEN", "~", "1000", "*",
      "event", "released",
      "lease_id", core.current_lease_id or "",
      "lease_epoch", core.current_lease_epoch or "",
      "attempt_index", core.current_attempt_index or "",
      "reason", "expired:" .. A.expire_reason,
      "ts", tostring(now_ms))
  end

  -- PATH: Suspended
  if core.lifecycle_phase == "suspended" then
    -- End attempt
    if is_set(core.current_attempt_index) then
      redis.call("HSET", K.attempt_hash,
        "attempt_state", "ended_failure",
        "ended_at", tostring(now_ms),
        "failure_reason", A.expire_reason)
    end
    -- Close suspension
    if redis.call("EXISTS", K.suspension_current) == 1 then
      redis.call("HSET", K.suspension_current,
        "closed_at", tostring(now_ms),
        "close_reason", "expired:" .. A.expire_reason)
    end
    -- Close stream
    if redis.call("EXISTS", K.stream_meta) == 1 then
      redis.call("HSET", K.stream_meta,
        "closed_at", tostring(now_ms),
        "closed_reason", "expired")
    end
    -- ZREM from suspended indexes
    redis.call("ZREM", K.suspended_zset, A.execution_id)
    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)
  end

  -- PATH: Runnable (execution_deadline fires while execution is waiting/delayed/blocked)
  -- These indexes are not in the 14 KEYS array, so construct dynamically from
  -- hash tag + lane_id (same C2 pattern as ff_cancel_execution).
  if core.lifecycle_phase == "runnable" then
    local tag = string.match(K.core_key, "(%b{})")
    local lane = core.lane_id or core.current_lane or "default"
    local es = core.eligibility_state or ""

    if es == "eligible_now" then
      redis.call("ZREM", "ff:idx:" .. tag .. ":lane:" .. lane .. ":eligible", A.execution_id)
    elseif es == "not_eligible_until_time" then
      redis.call("ZREM", "ff:idx:" .. tag .. ":lane:" .. lane .. ":delayed", A.execution_id)
    elseif es == "blocked_by_dependencies" then
      redis.call("ZREM", "ff:idx:" .. tag .. ":lane:" .. lane .. ":blocked:dependencies", A.execution_id)
    elseif es == "blocked_by_budget" then
      redis.call("ZREM", "ff:idx:" .. tag .. ":lane:" .. lane .. ":blocked:budget", A.execution_id)
    elseif es == "blocked_by_quota" then
      redis.call("ZREM", "ff:idx:" .. tag .. ":lane:" .. lane .. ":blocked:quota", A.execution_id)
    elseif es == "blocked_by_route" then
      redis.call("ZREM", "ff:idx:" .. tag .. ":lane:" .. lane .. ":blocked:route", A.execution_id)
    elseif es == "blocked_by_operator" then
      redis.call("ZREM", "ff:idx:" .. tag .. ":lane:" .. lane .. ":blocked:operator", A.execution_id)
    else
      -- Defensive catch-all: handles blocked_by_lane_state and any future
      -- eligibility states. ZREM from ALL runnable-state indexes — idempotent.
      local lp = "ff:idx:" .. tag .. ":lane:" .. lane
      redis.call("ZREM", lp .. ":eligible", A.execution_id)
      redis.call("ZREM", lp .. ":delayed", A.execution_id)
      redis.call("ZREM", lp .. ":blocked:dependencies", A.execution_id)
      redis.call("ZREM", lp .. ":blocked:budget", A.execution_id)
      redis.call("ZREM", lp .. ":blocked:quota", A.execution_id)
      redis.call("ZREM", lp .. ":blocked:route", A.execution_id)
      redis.call("ZREM", lp .. ":blocked:operator", A.execution_id)
    end
  end

  -- ALL PATHS: terminal transition
  local att_state = "attempt_terminal"
  if not is_set(core.current_attempt_index) then
    att_state = core.attempt_state or "none"
  end

  redis.call("HSET", K.core_key,
    "lifecycle_phase", "terminal",
    "ownership_state", "unowned",
    "eligibility_state", "not_applicable",
    "blocking_reason", "none",
    "blocking_detail", "",
    "terminal_outcome", "expired",
    "attempt_state", att_state,
    "public_state", "expired",
    "failure_reason", A.expire_reason,
    "completed_at", tostring(now_ms),
    "current_lease_id", "",
    "current_worker_id", "",
    "current_worker_instance_id", "",
    "lease_expires_at", "",
    "current_suspension_id", "",
    "current_waitpoint_id", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- Cleanup timeout indexes + ZADD terminal
  redis.call("ZREM", K.attempt_timeout_key, A.execution_id)
  redis.call("ZREM", K.execution_deadline_key, A.execution_id)
  redis.call("ZADD", K.terminal_key, now_ms, A.execution_id)

  return ok("expired", core.lifecycle_phase)
end)
