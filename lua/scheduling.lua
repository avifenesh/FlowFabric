-- FlowFabric scheduling functions
-- Reference: RFC-009 (Scheduling), RFC-010 §4 (function inventory)
--
-- Depends on helpers: ok, err, hgetall_to_table, is_set, validate_lease

---------------------------------------------------------------------------
-- Capability matching helpers (local to scheduling.lua)
-- Bounds CAPS_MAX_BYTES / CAPS_MAX_TOKENS live in helpers.lua and are
-- enforced symmetrically on worker caps here and on required caps in
-- ff_create_execution so neither side can smuggle in an oversized list.
---------------------------------------------------------------------------

-- Parse a capability CSV into a {token=true} set. Empty/nil → empty set.
-- Returns (set, nil) on success or (nil, err_tuple) on bound violation.
--
-- Empty tokens (from stray separators like "a,,b") are skipped BEFORE the
-- count check so a legitimate list punctuated by noise isn't rejected.
-- Real oversize input still fails because #csv > CAPS_MAX_BYTES catches it
-- before this loop runs.
local function parse_capability_csv(csv, kind)
  if csv == nil or csv == "" then
    return {}, nil
  end
  if #csv > CAPS_MAX_BYTES then
    return nil, err("invalid_capabilities", kind .. ":too_many_bytes")
  end
  local set = {}
  local n = 0
  for token in string.gmatch(csv, "([^,]+)") do
    if #token > 0 then
      n = n + 1
      if n > CAPS_MAX_TOKENS then
        return nil, err("invalid_capabilities", kind .. ":too_many_tokens")
      end
      set[token] = true
    end
  end
  return set, nil
end

-- Return sorted CSV of tokens present in `required` but missing from
-- `worker_caps`. Empty result means worker satisfies all requirements.
local function missing_capabilities(required, worker_caps)
  local missing = {}
  for cap, _ in pairs(required) do
    if not worker_caps[cap] then
      missing[#missing + 1] = cap
    end
  end
  table.sort(missing)
  return table.concat(missing, ",")
end

---------------------------------------------------------------------------
-- #25  ff_issue_claim_grant
--
-- Scheduler issues a claim grant for an eligible execution.
-- Validates execution is eligible, writes grant hash with TTL,
-- removes from eligible set.
--
-- KEYS (3): exec_core, claim_grant_key, eligible_zset
-- ARGV (9): execution_id, worker_id, worker_instance_id,
--           lane_id, capability_hash, grant_ttl_ms,
--           route_snapshot_json, admission_summary,
--           worker_capabilities_csv  -- sorted CSV of worker caps (option a)
--
-- Capability matching (RFC-009):
--   If exec_core.required_capabilities (sorted CSV on exec_core) is empty,
--   any worker matches (backwards compat). Otherwise the worker's sorted
--   CSV must be a superset.
--   On mismatch: Lua stamps `last_capability_mismatch_at` (single scalar
--   field, idempotent write — no unbounded counter) and returns
--   err("capability_mismatch", missing_csv). The scheduler side MUST
--   then block the execution off the eligible ZSET (see
--   ff_block_execution_for_admission with reason `waiting_for_capability`),
--   otherwise ZRANGEBYSCORE keeps returning the same top-of-zset every
--   tick and 100 workers × 1 tick/s = hot-loop starvation. RFC-009 §564.
---------------------------------------------------------------------------
redis.register_function('ff_issue_claim_grant', function(keys, args)
  local K = {
    core_key       = keys[1],
    claim_grant    = keys[2],
    eligible_zset  = keys[3],
  }

  local A = {
    execution_id            = args[1],
    worker_id               = args[2],
    worker_instance_id      = args[3],
    lane_id                 = args[4],
    capability_hash         = args[5] or "",
    route_snapshot_json     = args[7] or "",
    admission_summary       = args[8] or "",
    worker_capabilities_csv = args[9] or "",
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

  -- 4. Capability matching. On miss we stamp a SINGLE bounded field —
  -- `last_capability_mismatch_at` — so operators can SCAN for stuck
  -- executions via `HGET last_capability_mismatch_at < now - 1h` without
  -- needing a counter. An earlier version HINCRBY'd a counter; that was
  -- dropped because combined with the hot-loop bug (executions staying in
  -- the eligible ZSET after mismatch) the counter grew unboundedly (2.4M
  -- increments/day on one stuck exec_core under 100 workers). An HSET of
  -- a fixed field is idempotent w.r.t. size.
  --
  -- The scheduler MUST block the execution off the eligible ZSET after
  -- this err returns; otherwise the next tick picks the same top-of-zset
  -- and we wasted this validation. See Scheduler::claim_for_worker.
  local required_set, req_err = parse_capability_csv(
    core.required_capabilities or "", "required")
  if req_err then return req_err end
  local worker_set, wrk_err = parse_capability_csv(
    A.worker_capabilities_csv, "worker")
  if wrk_err then return wrk_err end
  if next(required_set) ~= nil then
    local missing = missing_capabilities(required_set, worker_set)
    if missing ~= "" then
      redis.call("HSET", K.core_key,
        "last_capability_mismatch_at", tostring(now_ms))
      return err("capability_mismatch", missing)
    end
  end

  -- 5. Write grant hash with TTL
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

  -- 6. Do NOT ZREM from eligible here. ff_claim_execution does the ZREM
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
-- RFC-025  ff_register_worker
--
-- Atomic worker-registry registration. Replaces the SDK preamble's
-- three separate round trips (SET NX alive + SET caps + SADD index)
-- with a single FCALL so a concurrent `mark_worker_dead` cannot
-- interleave and leave a zombie `:caps` key behind.
--
-- Idempotent overwrite: re-registering the same instance_id refreshes
-- TTL + caps + lanes and returns "refreshed". A fresh boot or a
-- post-TTL re-registration returns "registered".
--
-- KEYS (3): alive_key (`ff:worker:{ns}:{inst}:alive`),
--           caps_key  (`ff:worker:{ns}:{inst}:caps`),
--           index_key (`ff:idx:{ns}:workers`)
-- ARGV (6): instance_id, worker_id, lanes_csv (sorted),
--           caps_csv (sorted), ttl_ms, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_register_worker', function(keys, args)
  local K = {
    alive_key = keys[1],
    caps_key  = keys[2],
    index_key = keys[3],
  }

  local ttl_n = require_number(args[5], "ttl_ms")
  if type(ttl_n) == "table" then return ttl_n end
  local now_n = require_number(args[6], "now_ms")
  if type(now_n) == "table" then return now_n end

  local A = {
    instance_id     = args[1],
    worker_id       = args[2],
    lanes_csv       = args[3] or "",
    caps_csv        = args[4] or "",
    ttl_ms          = ttl_n,
    now_ms          = now_n,
  }

  local prior_alive = redis.call("EXISTS", K.alive_key)

  -- Idempotent overwrite — no NX. Hot-restart with new caps is allowed.
  redis.call("SET", K.alive_key, "1", "PX", tostring(A.ttl_ms))

  -- HSET overwrites individual fields, so caps/lanes/ttl hot-swap on
  -- a Refreshed path.
  redis.call("HSET", K.caps_key,
    "worker_id", A.worker_id,
    "lanes_csv", A.lanes_csv,
    "caps_csv", A.caps_csv,
    "ttl_ms", tostring(A.ttl_ms),
    "registered_at_ms", tostring(A.now_ms),
    "last_heartbeat_ms", tostring(A.now_ms))

  -- Match the alive-key TTL on the caps hash so both age out together
  -- when the worker goes silent. Prevents a stale caps hash from
  -- outliving the alive guard.
  redis.call("PEXPIRE", K.caps_key, tostring(A.ttl_ms))

  -- Index membership is TTL-free; mark_worker_dead or a follow-up
  -- sweep removes it. SMEMBERS enumeration drives list_workers.
  redis.call("SADD", K.index_key, A.instance_id)

  if prior_alive == 0 then
    return ok("registered")
  else
    return ok("refreshed")
  end
end)

---------------------------------------------------------------------------
-- RFC-025  ff_heartbeat_worker  (#502, Lua v34)
--
-- Collapse the 4-round-trip heartbeat path (HGET ttl_ms + PEXPIRE alive
-- + PEXPIRE caps + HSET last_heartbeat_ms) into a single FCALL.
--
-- Idempotent: returns `not_registered` when the caps hash is absent
-- (no prior registration OR TTL reaped OR operator-initiated
-- mark_worker_dead landed); caller re-registers. Returns
-- `refreshed <ttl_ms>` on success; Rust wrapper derives
-- `next_expiry_ms = now_ms + ttl_ms`.
--
-- Cluster safety: alive_key and caps_key both live under
-- `ff:worker:<ns>:<inst>:…` — same instance_id within a namespace.
-- They DO hash to different slots without explicit `{…}` hash tags
-- today; this FCALL is therefore intended for single-node Valkey
-- deployments OR cluster deployments where per-namespace slot-pinning
-- has been added. The pre-FCALL sequential path used two separate
-- PEXPIRE commands on the same two keys — identical slot-span
-- constraint. This FCALL does NOT newly introduce a cross-slot read;
-- it merely bundles existing ones.
--
-- KEYS (2): alive_key (`ff:worker:{ns}:{inst}:alive`),
--           caps_key  (`ff:worker:{ns}:{inst}:caps`)
-- ARGV (1): now_ms
---------------------------------------------------------------------------
redis.register_function('ff_heartbeat_worker', function(keys, args)
  local alive_key = keys[1]
  local caps_key  = keys[2]

  local now_n = require_number(args[1], "now_ms")
  if type(now_n) == "table" then return now_n end

  local ttl_raw = redis.call("HGET", caps_key, "ttl_ms")
  if not ttl_raw or ttl_raw == false then
    return ok("not_registered")
  end
  local ttl_ms = tonumber(ttl_raw)
  if not ttl_ms then
    return ok("not_registered")
  end

  -- PEXPIRE returns 0 when the key is absent (TTL elapsed or
  -- mark_worker_dead raced). Treat that as NotRegistered so the caller
  -- re-registers, matching the pre-FCALL behaviour.
  local applied = redis.call("PEXPIRE", alive_key, tostring(ttl_ms))
  if applied == 0 then
    return ok("not_registered")
  end

  -- Caps-hash TTL refresh. Ignore the reply: if the caps hash was
  -- reaped between HGET and PEXPIRE, list_workers / unblock_scanner
  -- will skip the indexed instance on the next cycle and the worker
  -- will re-register on the next heartbeat.
  redis.call("PEXPIRE", caps_key, tostring(ttl_ms))

  -- Stamp observed heartbeat so list_workers returns a real
  -- `last_heartbeat_ms` (not `registered_at_ms`).
  redis.call("HSET", caps_key, "last_heartbeat_ms", tostring(now_n))

  return ok("refreshed", tostring(ttl_ms))
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
      core.terminal_outcome or "",
      core.current_lease_epoch or "",
      core.lifecycle_phase or "",
      core.current_attempt_id or "")
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

  local now_ms_n = require_number(args[2], "now_ms")
  if type(now_ms_n) == "table" then return now_ms_n end

  local A = {
    execution_id = args[1],
    now_ms       = now_ms_n,
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
-- TODO(batch-c): This function has NO production Rust caller as of Batch B.
-- The reclaim scanner that would invoke it (to recover leases from crashed
-- workers) is scheduled for cairn Batch C. A worker dying mid-execution today
-- leaves its execution stuck in `lease_expired_reclaimable` until operator
-- intervention. Test-only callers exist in crates/ff-test/tests to exercise
-- the Lua side. When the scheduler reclaim integration lands, the caller
-- must apply the same block-on-capability-mismatch pattern used by
-- `ff-scheduler::Scheduler::claim_for_worker` (see the IMPORTANT note
-- below) — otherwise an unmatchable reclaim recycles every scanner tick.
--
-- Scheduler issues a reclaim grant for an expired/revoked execution.
-- Similar to ff_issue_claim_grant but validates reclaimable state.
--
-- KEYS (3): exec_core, claim_grant_key, lease_expiry_zset
-- ARGV (9): execution_id, worker_id, worker_instance_id,
--           lane_id, capability_hash, grant_ttl_ms,
--           route_snapshot_json, admission_summary,
--           worker_capabilities_csv
--
-- Capability matching identical to ff_issue_claim_grant: reclaiming a lease
-- must respect the execution's required_capabilities just like an initial
-- claim, so a re-issuance to a non-matching worker is blocked here too.
--
-- IMPORTANT: on capability_mismatch this function does NOT remove the exec
-- from the lease_expiry pool. The reclaim SCANNER (to be added in Rust) MUST
-- detect capability_mismatch and move the execution into blocked_route with
-- reason `waiting_for_capable_worker` (mirroring the claim-grant path). If
-- the scanner instead re-attempts the same execution every tick, a reclaim
-- hot-loop develops that is analogous to the claim-path hot-loop and
-- identical in cost (wasted FCALLs + log volume). Lease_expiry as an index
-- has no natural sweeping mechanism for post-mismatch promotion — the
-- scheduler-side block + periodic sweep owns the lifecycle.
---------------------------------------------------------------------------
redis.register_function('ff_issue_reclaim_grant', function(keys, args)
  local K = {
    core_key       = keys[1],
    claim_grant    = keys[2],
    lease_expiry   = keys[3],
  }

  local A = {
    execution_id            = args[1],
    worker_id               = args[2],
    worker_instance_id      = args[3],
    lane_id                 = args[4],
    capability_hash         = args[5] or "",
    route_snapshot_json     = args[7] or "",
    admission_summary       = args[8] or "",
    worker_capabilities_csv = args[9] or "",
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

  -- Capability matching — same policy as issue_claim_grant: stamp
  -- last_capability_mismatch_at (single scalar) on miss so ops can surface
  -- stuck reclaims via SCAN. Scheduler MUST also block-out the exec from
  -- the lease_expiry reclaim pool; otherwise the reclaim scanner hits the
  -- same mismatch every cycle. See Scheduler::reclaim_for_worker.
  local required_set, req_err = parse_capability_csv(
    core.required_capabilities or "", "required")
  if req_err then return req_err end
  local worker_set, wrk_err = parse_capability_csv(
    A.worker_capabilities_csv, "worker")
  if wrk_err then return wrk_err end
  if next(required_set) ~= nil then
    local missing = missing_capabilities(required_set, worker_set)
    if missing ~= "" then
      redis.call("HSET", K.core_key,
        "last_capability_mismatch_at", tostring(now_ms))
      return err("capability_mismatch", missing)
    end
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

  -- Return the authoritative server-side `grant_expires_at` so callers
  -- surface the server's clock (not their own `now + grant_ttl_ms`) in
  -- `ReclaimGrant::expires_at_ms`. Under clock skew these diverge; per
  -- RFC-024 §3.1 grant-carried fields come from server.
  return ok(A.execution_id, tostring(grant_expires_at))
end)

---------------------------------------------------------------------------
-- #40  ff_issue_grant_and_claim   (cairn #454 Phase 3d)
--
-- Backend-atomic composition of `ff_issue_claim_grant` + claim (fresh or
-- resumed). Operator-driven: the caller (cairn control plane) takes
-- ownership of a specific execution in one FCALL so a crash between
-- grant-issue and claim cannot leak a grant. Per cairn #454 comment
-- 4355865937 this method is deliberately NOT default-impl chained —
-- atomicity is the whole point.
--
-- Capability matching is SKIPPED here. This is an operator-override
-- path: the caller asserts authority to claim, not worker-capability
-- eligibility. This diverges from `ff_issue_claim_grant` where a
-- capability miss is the primary guard.
--
-- Dispatch: if `core.attempt_state == "attempt_interrupted"` we route
-- through the resume-claim body (same attempt continues); otherwise
-- the fresh-claim body creates a new attempt. The caller sees the
-- unified tuple `{lease_id, lease_epoch, attempt_index}` regardless.
--
-- KEYS (11): exec_core, claim_grant, eligible_zset, lease_expiry_zset,
--            worker_leases, attempts_zset, lease_current, lease_history,
--            active_index, attempt_timeout_zset, execution_deadline_zset
--            Attempt hash / usage / policy keys are NOT declared — the
--            Lua computes `next_att_idx` (fresh) or reads
--            `current_attempt_index` (resume) internally and builds the
--            attempt keys dynamically. Since all attempt keys share the
--            `{tag}` hash (tag-based slot binding) they live in the same
--            cluster slot as `exec_core`, so not listing them in KEYS is
--            cluster-safe.
-- ARGV (11): execution_id, worker_id, worker_instance_id, lane,
--            capability_snapshot_hash, lease_id, lease_ttl_ms,
--            attempt_id, attempt_policy_json, attempt_timeout_ms,
--            execution_deadline_at
--            `renew_before_ms` is derived as `lease_ttl_ms * 2 / 3`
--            (same formula `claim_impl` applies on the Rust side).
---------------------------------------------------------------------------
redis.register_function('ff_issue_grant_and_claim', function(keys, args)
  local K = {
    core_key               = keys[1],
    claim_grant            = keys[2],
    eligible_zset          = keys[3],
    lease_expiry_key       = keys[4],
    worker_leases_key      = keys[5],
    attempts_zset          = keys[6],
    lease_current_key      = keys[7],
    lease_history_key      = keys[8],
    active_index_key       = keys[9],
    attempt_timeout_key    = keys[10],
    execution_deadline_key = keys[11],
  }

  local lease_ttl_n = require_number(args[7], "lease_ttl_ms")
  if type(lease_ttl_n) == "table" then return lease_ttl_n end

  local A = {
    execution_id          = args[1],
    worker_id             = args[2],
    worker_instance_id    = args[3],
    lane                  = args[4],
    capability_hash       = args[5] or "",
    lease_id              = args[6],
    lease_ttl_ms          = lease_ttl_n,
    attempt_id            = args[8],
    attempt_policy_json   = args[9] or "",
    attempt_timeout_ms    = args[10] or "",
    execution_deadline_at = args[11] or "",
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Validate execution exists + is runnable.
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "runnable" then
    return err("execution_not_eligible")
  end
  if core.ownership_state ~= "unowned" then
    return err("lease_conflict")
  end
  if core.terminal_outcome ~= "none" then
    return err("execution_not_eligible")
  end

  -- Defense-in-depth: invariant A3 (no running attempt while claiming).
  if core.attempt_state == "running_attempt" then
    return err("active_attempt_exists")
  end

  -- 2. Block double-grant. The composed flow always DEL's the grant
  -- at the end; a pre-existing grant means someone else raced.
  if redis.call("EXISTS", K.claim_grant) == 1 then
    return err("grant_already_exists")
  end

  -- 3. Write grant hash with TTL. Grant TTL == lease TTL (ARGV[7]) —
  -- the grant is consumed in the same FCALL so TTL reuse is safe and
  -- keeps the trait method's single-knob surface (`lease_duration_ms`).
  local grant_expires_at = now_ms + A.lease_ttl_ms
  redis.call("HSET", K.claim_grant,
    "worker_id", A.worker_id,
    "worker_instance_id", A.worker_instance_id,
    "lane_id", A.lane,
    "capability_hash", A.capability_hash,
    "route_snapshot_json", "",
    "admission_summary", "",
    "created_at", tostring(now_ms),
    "grant_expires_at", tostring(grant_expires_at))
  -- Set a finite TTL on the grant hash as a safety net: the happy path
  -- DEL's the key at the end of this FCALL, but if the Lua errors
  -- between the HSET above and the DEL below (e.g. unpack_policy
  -- throws, a dynamic attempt-key write hits WRONGTYPE, or TIME goes
  -- backwards and a downstream call fails), the grant would otherwise
  -- leak forever and trip the `grant_already_exists` guard on every
  -- retry. PEXPIRE bounds the leak at `lease_ttl_ms` — the only TTL
  -- knob this method accepts — matching the single-knob contract
  -- promised by the trait surface.
  redis.call("PEXPIRE", K.claim_grant, A.lease_ttl_ms)

  -- 4. Dispatch — resume-claim vs fresh-claim. Mirrors the dispatch
  --    on `ff_claim_execution` (`use_claim_resumed_execution` signal).
  local tag = string.match(K.core_key, "(%b{})")
  local next_epoch = tonumber(core.current_lease_epoch or "0") + 1
  local expires_at = now_ms + A.lease_ttl_ms
  local renewal_deadline = now_ms + math.floor(A.lease_ttl_ms * 2 / 3)

  if core.attempt_state == "attempt_interrupted" then
    -- ── Resume-claim path (mirrors ff_claim_resumed_execution body) ──
    local att_idx = core.current_attempt_index
    local att_id = core.current_attempt_id
    local att_key = "ff:attempt:" .. tag .. ":" .. A.execution_id .. ":" .. tostring(att_idx)

    -- Consume grant
    redis.call("DEL", K.claim_grant)

    -- Resume attempt: interrupted -> started.
    redis.call("HSET", att_key,
      "attempt_state", "started",
      "resumed_at", tostring(now_ms),
      "lease_id", A.lease_id,
      "lease_epoch", tostring(next_epoch),
      "worker_id", A.worker_id,
      "worker_instance_id", A.worker_instance_id,
      "suspended_at", "",
      "suspension_id", "")

    -- New lease record bound to same attempt.
    redis.call("DEL", K.lease_current_key)
    redis.call("HSET", K.lease_current_key,
      "lease_id", A.lease_id,
      "lease_epoch", tostring(next_epoch),
      "execution_id", A.execution_id,
      "attempt_id", att_id,
      "worker_id", A.worker_id,
      "worker_instance_id", A.worker_instance_id,
      "acquired_at", tostring(now_ms),
      "expires_at", tostring(expires_at),
      "last_renewed_at", tostring(now_ms),
      "renewal_deadline", tostring(renewal_deadline))

    -- Update exec_core — ALL 7 state vector dims.
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
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    -- Indexes.
    redis.call("ZREM", K.eligible_zset, A.execution_id)
    redis.call("ZADD", K.lease_expiry_key, expires_at, A.execution_id)
    redis.call("SADD", K.worker_leases_key, A.execution_id)
    redis.call("ZADD", K.active_index_key, expires_at, A.execution_id)

    -- Lease history event.
    redis.call("XADD", K.lease_history_key, "MAXLEN", "~", 1000, "*",
      "event", "acquired",
      "lease_id", A.lease_id,
      "lease_epoch", tostring(next_epoch),
      "attempt_index", att_idx,
      "attempt_id", att_id,
      "worker_id", A.worker_id,
      "reason", "issue_grant_and_claim_resumed",
      "ts", tostring(now_ms))

    return ok(A.lease_id, tostring(next_epoch), tostring(att_idx))
  end

  -- ── Fresh-claim path (mirrors ff_claim_execution body) ──
  -- Verify execution is in eligible set (TOCTOU parity with ff_issue_claim_grant).
  local score = redis.call("ZSCORE", K.eligible_zset, A.execution_id)
  if not score then
    -- Roll back grant (not strictly needed: eligible-set miss on a
    -- runnable+unowned+non-terminal exec means index drift; surface
    -- the same err class as ff_issue_claim_grant rather than silently
    -- leaving a grant).
    redis.call("DEL", K.claim_grant)
    return err("execution_not_in_eligible_set")
  end

  if core.eligibility_state ~= "eligible_now" then
    redis.call("DEL", K.claim_grant)
    return err("execution_not_eligible")
  end

  local next_att_idx = tonumber(core.total_attempt_count or "0")
  local att_key = "ff:attempt:" .. tag .. ":" .. A.execution_id .. ":" .. tostring(next_att_idx)
  local att_usage_key = att_key .. ":usage"
  local att_policy_key = att_key .. ":policy"

  -- Attempt type from attempt_state (retry / replay / initial).
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

  -- Consume grant (DEL).
  redis.call("DEL", K.claim_grant)

  -- Create attempt record.
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

  -- Initialize usage counters.
  redis.call("HSET", att_usage_key, "last_usage_report_seq", "0")

  -- Attempt policy snapshot (if present).
  if is_set(A.attempt_policy_json) then
    local policy_flat = unpack_policy(A.attempt_policy_json)
    if #policy_flat > 0 then
      redis.call("HSET", att_policy_key, unpack(policy_flat))
    end
  end

  -- Lease record.
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

  -- Exec core — ALL 7 state vector dims.
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
    "started_at",
      (core.started_at ~= nil and core.started_at ~= "") and core.started_at or tostring(now_ms),
    "pending_retry_reason", "",
    "pending_replay_reason", "",
    "pending_replay_requested_by", "",
    "pending_previous_attempt_index", "",
    "last_transition_at", tostring(now_ms),
    "last_mutation_at", tostring(now_ms))

  -- Indexes.
  redis.call("ZREM", K.eligible_zset, A.execution_id)
  redis.call("ZADD", K.lease_expiry_key, expires_at, A.execution_id)
  redis.call("SADD", K.worker_leases_key, A.execution_id)
  redis.call("ZADD", K.active_index_key, expires_at, A.execution_id)

  if is_set(A.attempt_timeout_ms) and A.attempt_timeout_ms ~= "0" then
    redis.call("ZADD", K.attempt_timeout_key,
      now_ms + tonumber(A.attempt_timeout_ms), A.execution_id)
  end
  if is_set(A.execution_deadline_at) and A.execution_deadline_at ~= "0" then
    redis.call("ZADD", K.execution_deadline_key,
      tonumber(A.execution_deadline_at), A.execution_id)
  end

  redis.call("XADD", K.lease_history_key, "MAXLEN", "~", 1000, "*",
    "event", "acquired",
    "lease_id", A.lease_id,
    "lease_epoch", tostring(next_epoch),
    "attempt_id", A.attempt_id,
    "attempt_index", tostring(next_att_idx),
    "worker_id", A.worker_id,
    "reason", "issue_grant_and_claim",
    "ts", tostring(now_ms))

  return ok(A.lease_id, tostring(next_epoch), tostring(next_att_idx))
end)
