-- FlowFabric flow coordination and dependency functions
-- Reference: RFC-007 (Flow), RFC-010 §4.1 (#22-24, #35), §4.2 (#29)
--
-- Depends on helpers: ok, err, is_set, hgetall_to_table

---------------------------------------------------------------------------
-- Cycle detection helper
---------------------------------------------------------------------------

-- Max nodes to visit during cycle detection BFS.
local MAX_CYCLE_CHECK_NODES = 1000

-- Detect if adding an edge upstream→downstream would create a cycle.
-- BFS from downstream through existing outgoing edges: if upstream is
-- reachable, the new edge closes a loop (A→B→C→A deadlock).
-- All keys share the same {fp:N} slot (flow-partition co-location).
-- @param flow_prefix  e.g. "ff:flow:{fp:0}:<flow_id>"
-- @param start_eid    downstream of proposed edge (BFS start)
-- @param target_eid   upstream of proposed edge (looking for this)
-- @return true if a cycle would be created
local function detect_cycle(flow_prefix, start_eid, target_eid)
  local visited = {}
  local queue = {start_eid}
  local count = 0

  while #queue > 0 do
    local next_queue = {}
    for _, eid in ipairs(queue) do
      if eid == target_eid then
        return true
      end
      if not visited[eid] then
        visited[eid] = true
        count = count + 1
        if count > MAX_CYCLE_CHECK_NODES then
          return true  -- graph too large to verify; reject conservatively
        end
        local out_key = flow_prefix .. ":out:" .. eid
        local edges = redis.call("SMEMBERS", out_key)
        for _, edge_id in ipairs(edges) do
          local edge_key = flow_prefix .. ":edge:" .. edge_id
          local next_eid = redis.call("HGET", edge_key, "downstream_execution_id")
          if next_eid and next_eid ~= "" and not visited[next_eid] then
            next_queue[#next_queue + 1] = next_eid
          end
        end
      end
    end
    queue = next_queue
  end

  return false
end

---------------------------------------------------------------------------
-- #29  ff_stage_dependency_edge  (on {fp:N})
--
-- Validate membership + topology, check graph_revision, create edge,
-- increment graph_revision.
--
-- KEYS (6): flow_core, members_set, edge_hash, out_adj_set, in_adj_set,
--           grant_hash
-- ARGV (8): flow_id, edge_id, upstream_eid, downstream_eid,
--           dependency_kind, data_passing_ref, expected_graph_revision,
--           now_ms
---------------------------------------------------------------------------
redis.register_function('ff_stage_dependency_edge', function(keys, args)
  local K = {
    flow_core    = keys[1],
    members_set  = keys[2],
    edge_hash    = keys[3],
    out_adj_set  = keys[4],
    in_adj_set   = keys[5],
    grant_hash   = keys[6],
  }

  local A = {
    flow_id                  = args[1],
    edge_id                  = args[2],
    upstream_eid             = args[3],
    downstream_eid           = args[4],
    dependency_kind          = args[5] or "success_only",
    data_passing_ref         = args[6] or "",
    expected_graph_revision  = args[7],
    now_ms                   = args[8],
  }

  -- 1. Reject self-referencing edges
  if A.upstream_eid == A.downstream_eid then
    return err("self_referencing_edge")
  end

  -- 2. Read flow core
  local raw = redis.call("HGETALL", K.flow_core)
  if #raw == 0 then return err("flow_not_found") end
  local flow = hgetall_to_table(raw)

  -- 3. Check graph_revision
  if tostring(flow.graph_revision or "0") ~= A.expected_graph_revision then
    return err("stale_graph_revision")
  end

  -- 4. Verify both executions are members
  if redis.call("SISMEMBER", K.members_set, A.upstream_eid) == 0 then
    return err("execution_not_in_flow")
  end
  if redis.call("SISMEMBER", K.members_set, A.downstream_eid) == 0 then
    return err("execution_not_in_flow")
  end

  -- 4b. Transitive cycle detection: walk from downstream through outgoing
  -- edges to check if upstream is reachable (A→B→C→A deadlock prevention).
  local flow_prefix = string.sub(K.flow_core, 1, -6)  -- strip ":core"
  if detect_cycle(flow_prefix, A.downstream_eid, A.upstream_eid) then
    return err("cycle_detected")
  end

  -- 5. Check edge doesn't already exist
  if redis.call("EXISTS", K.edge_hash) == 1 then
    return err("dependency_already_exists")
  end

  -- 6. Create edge record
  redis.call("HSET", K.edge_hash,
    "edge_id", A.edge_id,
    "flow_id", A.flow_id,
    "upstream_execution_id", A.upstream_eid,
    "downstream_execution_id", A.downstream_eid,
    "dependency_kind", A.dependency_kind,
    "satisfaction_condition", "all_required",
    "data_passing_ref", A.data_passing_ref,
    "edge_state", "pending",
    "created_at", A.now_ms,
    "created_by", "engine")

  -- 7. Update adjacency sets
  redis.call("SADD", K.out_adj_set, A.edge_id)
  redis.call("SADD", K.in_adj_set, A.edge_id)

  -- 8. Increment graph_revision and edge_count
  local new_rev = redis.call("HINCRBY", K.flow_core, "graph_revision", 1)
  redis.call("HINCRBY", K.flow_core, "edge_count", 1)
  redis.call("HSET", K.flow_core, "last_mutation_at", A.now_ms)

  return ok(A.edge_id, tostring(new_rev))
end)

---------------------------------------------------------------------------
-- #22  ff_apply_dependency_to_child  (on {p:N})
--
-- Create dep record on child execution partition, increment unsatisfied
-- count. If child is runnable: set blocked_by_dependencies.
--
-- KEYS (6): exec_core, deps_meta, unresolved_set, dep_hash,
--           eligible_zset, blocked_deps_zset
-- ARGV (7): flow_id, edge_id, upstream_eid, graph_revision,
--           dependency_kind, data_passing_ref, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_apply_dependency_to_child', function(keys, args)
  local K = {
    core_key         = keys[1],
    deps_meta        = keys[2],
    unresolved_set   = keys[3],
    dep_hash         = keys[4],
    eligible_zset    = keys[5],
    blocked_deps_zset = keys[6],
  }

  local A = {
    flow_id          = args[1],
    edge_id          = args[2],
    upstream_eid     = args[3],
    graph_revision   = args[4],
    dependency_kind  = args[5] or "success_only",
    data_passing_ref = args[6] or "",
    now_ms           = args[7],
  }

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- 2. Validate flow membership (RFC-007 assert_flow_membership)
  if is_set(core.flow_id) and core.flow_id ~= A.flow_id then
    return err("execution_already_in_flow")
  end

  -- 3. Idempotency: dep already applied
  if redis.call("EXISTS", K.dep_hash) == 1 then
    return ok("already_applied")
  end

  -- 4. Create dep record
  redis.call("HSET", K.dep_hash,
    "edge_id", A.edge_id,
    "flow_id", A.flow_id,
    "upstream_execution_id", A.upstream_eid,
    "downstream_execution_id", core.execution_id or "",
    "dependency_kind", A.dependency_kind,
    "state", "unsatisfied",
    "data_passing_ref", A.data_passing_ref,
    "last_resolved_at", "")

  -- 5. Update deps:meta
  redis.call("SADD", K.unresolved_set, A.edge_id)
  local unresolved = redis.call("HINCRBY", K.deps_meta, "unsatisfied_required_count", 1)
  redis.call("HSET", K.deps_meta,
    "flow_id", A.flow_id,
    "last_flow_graph_revision", A.graph_revision,
    "last_dependency_update_at", A.now_ms)

  -- 6. If runnable: block by dependencies (ALL 7 dims)
  if core.lifecycle_phase == "runnable" and core.terminal_outcome == "none" then
    redis.call("HSET", K.core_key,
      "lifecycle_phase", core.lifecycle_phase,             -- preserve
      "ownership_state", core.ownership_state or "unowned", -- preserve
      "eligibility_state", "blocked_by_dependencies",
      "blocking_reason", "waiting_for_children",
      "blocking_detail", unresolved .. " dep(s) unresolved incl " .. A.edge_id,
      "terminal_outcome", "none",                          -- preserve
      "attempt_state", core.attempt_state or "pending_first_attempt", -- preserve
      "public_state", "waiting_children",
      "last_transition_at", A.now_ms,
      "last_mutation_at", A.now_ms)
    redis.call("ZREM", K.eligible_zset, core.execution_id or "")
    redis.call("ZADD", K.blocked_deps_zset,
      tonumber(core.created_at or "0"), core.execution_id or "")
  end

  return ok(tostring(unresolved))
end)

---------------------------------------------------------------------------
-- #23  ff_resolve_dependency  (on {p:N})
--
-- Resolve one dependency edge: satisfied (upstream success) or impossible
-- (upstream failed/cancelled/expired). Updates child eligibility.
--
-- KEYS (9): exec_core, deps_meta, unresolved_set, dep_hash,
--           eligible_zset, terminal_zset, blocked_deps_zset,
--           attempt_hash, stream_meta
-- ARGV (3): edge_id, upstream_outcome, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_resolve_dependency', function(keys, args)
  local K = {
    core_key          = keys[1],
    deps_meta         = keys[2],
    unresolved_set    = keys[3],
    dep_hash          = keys[4],
    eligible_zset     = keys[5],
    terminal_zset     = keys[6],
    blocked_deps_zset = keys[7],
    attempt_hash      = keys[8],
    stream_meta       = keys[9],
  }

  local A = {
    edge_id           = args[1],
    upstream_outcome  = args[2],
    now_ms            = args[3],
  }

  -- 1. Read dep record
  local dep_raw = redis.call("HGETALL", K.dep_hash)
  if #dep_raw == 0 then return err("invalid_dependency") end
  local dep = hgetall_to_table(dep_raw)

  -- 2. Already resolved?
  if dep.state == "satisfied" or dep.state == "impossible" then
    return ok("already_resolved")
  end

  -- 3. Satisfaction path (upstream completed successfully)
  if A.upstream_outcome == "success" then
    redis.call("HSET", K.dep_hash,
      "state", "satisfied", "last_resolved_at", A.now_ms)
    redis.call("SREM", K.unresolved_set, A.edge_id)
    local remaining = redis.call("HINCRBY", K.deps_meta,
      "unsatisfied_required_count", -1)
    redis.call("HSET", K.deps_meta, "last_dependency_update_at", A.now_ms)

    -- Check if all deps now satisfied
    local raw = redis.call("HGETALL", K.core_key)
    if #raw == 0 then return ok("satisfied") end
    local core = hgetall_to_table(raw)

    if remaining == 0
       and core.lifecycle_phase == "runnable"
       and core.ownership_state == "unowned"
       and core.terminal_outcome == "none"
       and core.eligibility_state == "blocked_by_dependencies" then
      -- Preserve attempt_state
      local new_attempt_state = core.attempt_state
      if not is_set(new_attempt_state) or new_attempt_state == "none" then
        new_attempt_state = "pending_first_attempt"
      end
      -- ALL 7 dims
      redis.call("HSET", K.core_key,
        "lifecycle_phase", core.lifecycle_phase,             -- preserve (runnable)
        "ownership_state", core.ownership_state or "unowned", -- preserve
        "eligibility_state", "eligible_now",
        "blocking_reason", "waiting_for_worker",
        "blocking_detail", "",
        "terminal_outcome", "none",                          -- preserve
        "attempt_state", new_attempt_state,
        "public_state", "waiting",
        "last_transition_at", A.now_ms,
        "last_mutation_at", A.now_ms)
      redis.call("ZREM", K.blocked_deps_zset, core.execution_id or "")
      local priority = tonumber(core.priority or "0")
      local created_at_ms = tonumber(core.created_at or "0")
      local score = 0 - (priority * 1000000000000) + created_at_ms
      redis.call("ZADD", K.eligible_zset, score, core.execution_id or "")
    end

    return ok("satisfied")
  end

  -- 4. Impossible path (upstream failed/cancelled/expired/skipped)
  redis.call("HSET", K.dep_hash,
    "state", "impossible", "last_resolved_at", A.now_ms)
  redis.call("SREM", K.unresolved_set, A.edge_id)
  redis.call("HINCRBY", K.deps_meta, "unsatisfied_required_count", -1)
  redis.call("HINCRBY", K.deps_meta, "impossible_required_count", 1)
  redis.call("HSET", K.deps_meta, "last_dependency_update_at", A.now_ms)

  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return ok("impossible", "") end
  local core = hgetall_to_table(raw)

  local child_skipped = false

  if core.terminal_outcome == "none" then
    -- Determine attempt_state for skip
    local skip_attempt_state = core.attempt_state or "none"
    if skip_attempt_state == "running_attempt"
       or skip_attempt_state == "attempt_interrupted" then
      skip_attempt_state = "attempt_terminal"
      -- End real attempt + close stream
      redis.call("HSET", K.attempt_hash,
        "attempt_state", "ended_cancelled",
        "ended_at", A.now_ms,
        "failure_reason", "dependency_impossible")
      if redis.call("EXISTS", K.stream_meta) == 1 then
        redis.call("HSET", K.stream_meta,
          "closed_at", A.now_ms,
          "closed_reason", "dependency_impossible")
      end
    elseif is_set(skip_attempt_state) and skip_attempt_state ~= "none" then
      skip_attempt_state = "none"
    end

    redis.call("HSET", K.core_key,
      "lifecycle_phase", "terminal",
      "ownership_state", "unowned",
      "eligibility_state", "not_applicable",
      "blocking_reason", "none",
      "blocking_detail", "",
      "terminal_outcome", "skipped",
      "attempt_state", skip_attempt_state,
      "public_state", "skipped",
      "completed_at", A.now_ms,
      "last_transition_at", A.now_ms,
      "last_mutation_at", A.now_ms)
    redis.call("ZREM", K.blocked_deps_zset, core.execution_id or "")
    redis.call("ZADD", K.terminal_zset, tonumber(A.now_ms), core.execution_id or "")
    child_skipped = true
  end

  return ok("impossible", child_skipped and "child_skipped" or "")
end)

---------------------------------------------------------------------------
-- #24  ff_evaluate_flow_eligibility  (on {p:N})
--
-- Read-only check of execution + dependency state. Class C.
--
-- KEYS (2): exec_core, deps_meta
-- ARGV (0)
---------------------------------------------------------------------------
redis.register_function('ff_evaluate_flow_eligibility', function(keys, args)
  local raw = redis.call("HGETALL", keys[1])
  if #raw == 0 then return ok("not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "runnable" then
    return ok("not_runnable")
  end
  if core.ownership_state ~= "unowned" then
    return ok("owned")
  end
  if core.terminal_outcome ~= "none" then
    return ok("terminal")
  end

  local deps_raw = redis.call("HGETALL", keys[2])
  if #deps_raw == 0 then
    return ok("eligible")
  end
  local deps = hgetall_to_table(deps_raw)

  local impossible = tonumber(deps.impossible_required_count or "0")
  if impossible > 0 then
    return ok("impossible")
  end

  local unresolved = tonumber(deps.unsatisfied_required_count or "0")
  if unresolved > 0 then
    return ok("blocked_by_dependencies")
  end

  return ok("eligible")
end)

---------------------------------------------------------------------------
-- #35  ff_promote_blocked_to_eligible  (on {p:N})
--
-- Promote zero-dep flow member from blocked:dependencies to eligible.
--
-- KEYS (5): exec_core, blocked_deps_zset, eligible_zset, deps_meta,
--           deps_unresolved
-- ARGV (2): execution_id, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_promote_blocked_to_eligible', function(keys, args)
  local K = {
    core_key          = keys[1],
    blocked_deps_zset = keys[2],
    eligible_zset     = keys[3],
    deps_meta         = keys[4],
    deps_unresolved   = keys[5],
  }

  local A = {
    execution_id = args[1],
    now_ms       = args[2],
  }

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  if core.lifecycle_phase ~= "runnable" then
    return err("not_runnable")
  end
  if core.eligibility_state ~= "blocked_by_dependencies" then
    return err("not_blocked_by_deps")
  end
  if core.terminal_outcome ~= "none" then
    return err("terminal")
  end

  -- 2. Verify zero deps
  local unsatisfied = tonumber(
    redis.call("HGET", K.deps_meta, "unsatisfied_required_count") or "0")
  local unresolved_count = redis.call("SCARD", K.deps_unresolved)
  if unsatisfied > 0 or unresolved_count > 0 then
    return err("deps_not_satisfied", tostring(unsatisfied), tostring(unresolved_count))
  end

  -- 3. Preserve attempt_state
  local new_attempt_state = core.attempt_state
  if not is_set(new_attempt_state) or new_attempt_state == "none" then
    new_attempt_state = "pending_first_attempt"
  end

  -- 4. Transition (ALL 7 dims)
  redis.call("HSET", K.core_key,
    "lifecycle_phase", core.lifecycle_phase,             -- preserve (runnable)
    "ownership_state", core.ownership_state or "unowned", -- preserve
    "eligibility_state", "eligible_now",
    "blocking_reason", "waiting_for_worker",
    "blocking_detail", "",
    "terminal_outcome", "none",                          -- preserve
    "attempt_state", new_attempt_state,
    "public_state", "waiting",
    "last_transition_at", A.now_ms,
    "last_mutation_at", A.now_ms)

  redis.call("ZREM", K.blocked_deps_zset, A.execution_id)
  local priority = tonumber(core.priority or "0")
  local created_at_ms = tonumber(core.created_at or "0")
  local score = 0 - (priority * 1000000000000) + created_at_ms
  redis.call("ZADD", K.eligible_zset, score, A.execution_id)

  return ok()
end)

---------------------------------------------------------------------------
-- #12b  ff_replay_execution  (on {p:N})
--
-- Reset a terminal execution for replay. If skipped flow member: reset
-- impossible deps back to unsatisfied, recompute counts, set
-- blocked_by_dependencies instead of eligible_now.
--
-- KEYS (4+N): exec_core, terminal_zset, eligible_zset, lease_history,
--             [blocked_deps_zset, deps_meta, deps_unresolved, dep_edge_0..N]
-- ARGV (2+N): execution_id, now_ms, [edge_id_0..N]
---------------------------------------------------------------------------
redis.register_function('ff_replay_execution', function(keys, args)
  local K = {
    core_key       = keys[1],
    terminal_zset  = keys[2],
    eligible_zset  = keys[3],
    lease_history  = keys[4],
  }

  local A = {
    execution_id = args[1],
    now_ms       = args[2],
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- 2. Must be terminal
  if core.lifecycle_phase ~= "terminal" then
    return err("execution_not_terminal")
  end

  -- 3. Check replay limit
  local replay_count = tonumber(core.replay_count or "0")
  local max_replays = tonumber(core.max_replays or "10")
  if replay_count >= max_replays then
    return err("max_replays_exhausted")
  end

  -- 4. Determine replay path
  local is_skipped_flow_member = (core.terminal_outcome == "skipped") and is_set(core.flow_id)

  if is_skipped_flow_member then
    -- SKIPPED FLOW MEMBER PATH: reset impossible deps → blocked on deps
    local blocked_deps_zset = keys[5]
    local deps_meta         = keys[6]
    local deps_unresolved   = keys[7]

    -- Reset impossible dep edges back to unsatisfied
    local num_edges = #args - 2
    local new_unsatisfied = 0
    for i = 1, num_edges do
      local edge_id = args[2 + i]
      local dep_key = keys[7 + i]  -- dep_edge keys start at KEYS[8]

      local dep_state = redis.call("HGET", dep_key, "state")
      if dep_state == "impossible" then
        redis.call("HSET", dep_key,
          "state", "unsatisfied",
          "last_resolved_at", "")
        redis.call("SADD", deps_unresolved, edge_id)
        new_unsatisfied = new_unsatisfied + 1
      elseif dep_state == "unsatisfied" then
        new_unsatisfied = new_unsatisfied + 1
      end
      -- satisfied edges remain satisfied (upstream already succeeded)
    end

    -- Recompute deps:meta counts
    redis.call("HSET", deps_meta,
      "unsatisfied_required_count", tostring(new_unsatisfied),
      "impossible_required_count", "0",
      "last_dependency_update_at", tostring(now_ms))

    -- Transition: terminal → runnable/blocked_by_dependencies
    redis.call("HSET", K.core_key,
      "lifecycle_phase", "runnable",
      "ownership_state", "unowned",
      "eligibility_state", "blocked_by_dependencies",
      "blocking_reason", "waiting_for_children",
      "blocking_detail", tostring(new_unsatisfied) .. " dep(s) unsatisfied after replay",
      "terminal_outcome", "none",
      "attempt_state", "pending_replay_attempt",
      "public_state", "waiting_children",
      "pending_replay_attempt", "1",
      "replay_count", tostring(replay_count + 1),
      "completed_at", "",
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    -- Move from terminal → blocked:deps
    redis.call("ZREM", K.terminal_zset, A.execution_id)
    redis.call("ZADD", blocked_deps_zset,
      tonumber(core.created_at or "0"), A.execution_id)

    -- Lease history
    redis.call("XADD", K.lease_history, "MAXLEN", "~", 1000, "*",
      "event", "replay_initiated",
      "replay_count", tostring(replay_count + 1),
      "replay_type", "skipped_flow_member",
      "ts", tostring(now_ms))

    return ok(tostring(new_unsatisfied))
  else
    -- NORMAL REPLAY PATH: terminal → runnable/eligible
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
      "attempt_state", "pending_replay_attempt",
      "public_state", "waiting",
      "pending_replay_attempt", "1",
      "replay_count", tostring(replay_count + 1),
      "completed_at", "",
      "last_transition_at", tostring(now_ms),
      "last_mutation_at", tostring(now_ms))

    -- Move from terminal → eligible
    redis.call("ZREM", K.terminal_zset, A.execution_id)
    redis.call("ZADD", K.eligible_zset, score, A.execution_id)

    -- Lease history
    redis.call("XADD", K.lease_history, "MAXLEN", "~", 1000, "*",
      "event", "replay_initiated",
      "replay_count", tostring(replay_count + 1),
      "replay_type", "normal",
      "ts", tostring(now_ms))

    return ok("0")
  end
end)
