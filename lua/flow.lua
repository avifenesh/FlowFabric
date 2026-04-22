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
-- ff_create_flow  (on {fp:N})
--
-- Create a new flow container. Idempotent: if flow_core already exists,
-- returns ok_already_satisfied.
--
-- KEYS (3): flow_core, members_set, flow_index
-- ARGV (4): flow_id, flow_kind, namespace, now_ms (IGNORED — see note below)
--
-- NOTE: ARGV[4] (`now_ms`) is accepted for caller compatibility but NOT
-- used for stored timestamps. We read server time via redis.call("TIME")
-- so created_at / last_mutation_at agree with fields written by other
-- Lua functions (ff_complete_execution etc.) under client clock skew.
---------------------------------------------------------------------------
redis.register_function('ff_create_flow', function(keys, args)
  local K = {
    flow_core   = keys[1],
    members_set = keys[2],
    flow_index  = keys[3],
  }

  local A = {
    flow_id   = args[1],
    flow_kind = args[2],
    namespace = args[3],
    -- args[4] is client-provided now_ms; intentionally ignored.
  }

  -- Server time (not client-provided) so created_at / last_mutation_at
  -- agree with timestamps written by ff_complete_execution and peers.
  local now_ms = server_time_ms()

  -- Maintain flow_index BEFORE the idempotency guard. SADD is itself
  -- idempotent (no-op on existing members), and hoisting it heals any
  -- pre-existing flow_core that was created before this index was
  -- introduced — no migration script required.
  redis.call("SADD", K.flow_index, A.flow_id)

  -- Idempotency: if flow already exists, return already_satisfied
  if redis.call("EXISTS", K.flow_core) == 1 then
    return ok_already_satisfied(A.flow_id)
  end

  -- Create flow core record
  redis.call("HSET", K.flow_core,
    "flow_id", A.flow_id,
    "flow_kind", A.flow_kind,
    "namespace", A.namespace,
    "graph_revision", 0,
    "node_count", 0,
    "edge_count", 0,
    "public_flow_state", "open",
    "created_at", now_ms,
    "last_mutation_at", now_ms)

  return ok(A.flow_id)
end)

---------------------------------------------------------------------------
-- ff_add_execution_to_flow  (on {fp:N} — single atomic FCALL)
--
-- Add a member execution to a flow AND stamp the flow_id back-pointer
-- on exec_core in one atomic commit. Per RFC-011 §7.3, exec keys
-- co-locate with their parent flow's partition under hash-tag routing,
-- so exec_core shares the `{fp:N}` hash-tag with flow_core / members_set
-- / flow_index. All four KEYS hash to the same slot; no CROSSSLOT.
--
-- KEYS (4): flow_core, members_set, flow_index, exec_core
-- ARGV (3): flow_id, execution_id, now_ms (IGNORED — server time used)
--
-- Validates-before-writing: flow_not_found / flow_already_terminal
-- early-returns fire BEFORE any write (step 1 below). On those error
-- paths, zero state mutates — atomicity by construction at the Lua
-- level (Valkey scripting contract: no redis.call() before error_reply
-- means nothing to roll back). See RFC-011 §7.3.1 tests for the
-- structural pin.
--
-- Invariant (post-RFC-011): a successful call commits BOTH the flow-
-- index updates AND the exec_core.flow_id stamp in one atomic unit.
-- Readers can assume exec_core.flow_id == flow_id iff the exec is in
-- members_set. The pre-RFC-011 two-phase contract + §5.5 orphan-window
-- + issue #21 reconciliation-scanner plan are all superseded.
---------------------------------------------------------------------------
redis.register_function('ff_add_execution_to_flow', function(keys, args)
  local K = {
    flow_core   = keys[1],
    members_set = keys[2],
    flow_index  = keys[3],
    exec_core   = keys[4],
  }

  local A = {
    flow_id      = args[1],
    execution_id = args[2],
    -- args[3] is client-provided now_ms; intentionally ignored in favour
    -- of redis.call("TIME") to keep last_mutation_at consistent with
    -- timestamps stamped by ff_complete_execution and peers.
  }

  local now_ms = server_time_ms()

  -- 1. Validate flow exists and is not terminal, and execution exists.
  --    Validates-before-writing: no redis.call() writes happen before
  --    these guards, so the error paths commit zero state (symmetric
  --    with step 2's cross-flow guard on AlreadyMember).
  local raw = redis.call("HGETALL", K.flow_core)
  if #raw == 0 then return err("flow_not_found") end
  local flow = hgetall_to_table(raw)
  local pfs = flow.public_flow_state or ""
  if pfs == "cancelled" or pfs == "completed" or pfs == "failed" then
    return err("flow_already_terminal")
  end
  -- Execution must exist — otherwise step 5's HSET exec_core would
  -- silently create a hash for a non-existent exec, leading to an
  -- inconsistent members_set ↔ exec_core state. Symmetric with the
  -- flow_not_found guard above.
  if redis.call("EXISTS", K.exec_core) == 0 then
    return err("execution_not_found")
  end

  -- Self-heal flow_index for LIVE flows only. The projector may have
  -- SREMd this flow after observing an all-terminal sample, yet the
  -- flow is still "open" per flow_core and can accept new members.
  -- Re-add idempotently so the next projector cycle picks the flow
  -- back up. Runs only after the terminal-state guard above so we do
  -- not resurrect cancelled/completed/failed flows into the active
  -- index. Same {fp:N} slot as the other KEYS, so atomic with the
  -- membership mutation below.
  redis.call("SADD", K.flow_index, A.flow_id)

  -- 2. Idempotency: already a member of THIS flow's members_set.
  --    Still stamp exec_core.flow_id defensively — an earlier call
  --    may have committed members_set but crashed before exec_core
  --    HSET under the legacy two-phase shape. Stamping here is a
  --    no-op if flow_id already matches; heals any pre-RFC-011
  --    orphans encountered on a rolling upgrade.
  --
  --    Cross-flow guard on the orphan case: if exec_core.flow_id is
  --    already set to a DIFFERENT flow (corrupted-state orphan that
  --    IS in this flow's members_set but stamped wrong), refuse
  --    instead of silently re-stamping. Symmetric with step 3's
  --    guard on the not-yet-a-member branch — catches the same
  --    invariant violation earlier in the path. Empty existing
  --    flow_id goes through the heal path normally.
  if redis.call("SISMEMBER", K.members_set, A.execution_id) == 1 then
    local existing = redis.call("HGET", K.exec_core, "flow_id")
    if existing and existing ~= "" and existing ~= A.flow_id then
      return err("already_member_of_different_flow:" .. existing)
    end
    redis.call("HSET", K.exec_core, "flow_id", A.flow_id)
    local nc = redis.call("HGET", K.flow_core, "node_count") or "0"
    return ok_already_satisfied(A.execution_id, nc)
  end

  -- 3. Cross-flow guard: if exec_core.flow_id is already set to a
  --    DIFFERENT flow, refuse — silently re-stamping would orphan
  --    the other flow's accounting. An exec belongs to at most one
  --    flow at a time per RFC-007.
  local existing_flow_id = redis.call("HGET", K.exec_core, "flow_id")
  if existing_flow_id and existing_flow_id ~= "" and existing_flow_id ~= A.flow_id then
    return err("already_member_of_different_flow:" .. existing_flow_id)
  end

  -- 4. Add to membership set
  redis.call("SADD", K.members_set, A.execution_id)

  -- 5. Stamp the flow_id back-pointer on exec_core. Co-located with
  --    the flow's partition under RFC-011 §7.3 hash-tag routing; this
  --    HSET is part of the same atomic FCALL as the SADD above.
  redis.call("HSET", K.exec_core, "flow_id", A.flow_id)

  -- 6. Increment node_count and graph_revision
  local new_nc = redis.call("HINCRBY", K.flow_core, "node_count", 1)
  local new_rev = redis.call("HINCRBY", K.flow_core, "graph_revision", 1)
  redis.call("HSET", K.flow_core, "last_mutation_at", now_ms)

  return ok(A.execution_id, tostring(new_nc))
end)

---------------------------------------------------------------------------
-- ff_cancel_flow  (on {fp:N})
--
-- Cancel a flow. Returns the member list for the caller to dispatch
-- individual cancellations cross-partition.
--
-- KEYS (3): flow_core, members_set, flow_index (RESERVED — see below)
-- ARGV (4): flow_id, reason, cancellation_policy, now_ms (IGNORED —
--   server time used so `cancelled_at` agrees with peer Lua fields)
--
-- KEYS[3] (flow_index) is accepted for caller-compatibility with the
-- shared FlowStructOpKeys wrapper, but this function does NOT mutate
-- flow_index. The projector is the sole SREM writer (see the "4b" note
-- in the body below).
---------------------------------------------------------------------------
redis.register_function('ff_cancel_flow', function(keys, args)
  local K = {
    flow_core         = keys[1],
    members_set       = keys[2],
    -- keys[3] is flow_index; present in KEYS for wrapper symmetry but
    -- unused in this function (see rationale near the end of the body).
    pending_cancels   = keys[4],  -- SET populated only on first terminalization
    cancel_backlog    = keys[5],  -- per-fp ZSET tracking flows owing members
  }

  local A = {
    flow_id              = args[1],
    reason               = args[2],
    cancellation_policy  = args[3],
    -- args[4] is client-provided now_ms; intentionally ignored.
  }

  -- grace_ms must be a finite non-negative integer. Same guard as
  -- ff_rotate_waitpoint_hmac_secret: reject NaN, ±inf, negative,
  -- non-integer, or >2^53-1. math.floor alone doesn't catch
  -- infinities (math.floor(math.huge) == math.huge) which would
  -- stamp "inf" into cancel_backlog and permanently poison the entry.
  -- Default 30000 (30s) if the arg is omitted or empty string.
  local grace_ms
  if args[5] == nil or args[5] == "" then
    grace_ms = 30000
  else
    local g = tonumber(args[5])
    if not g
        or g ~= g                          -- NaN
        or g < 0
        or g > 9007199254740991            -- 2^53 - 1
        or g ~= math.floor(g) then
      return err("invalid_grace_ms")
    end
    grace_ms = g
  end
  A.grace_ms = grace_ms

  local now_ms = server_time_ms()

  -- 1. Validate flow exists
  local raw = redis.call("HGETALL", K.flow_core)
  if #raw == 0 then return err("flow_not_found") end
  local flow = hgetall_to_table(raw)

  -- 2. Check not already terminal
  local pfs = flow.public_flow_state or ""
  if pfs == "cancelled" or pfs == "completed" or pfs == "failed" then
    return err("flow_already_terminal")
  end

  -- 3. Get all member execution IDs
  local members = redis.call("SMEMBERS", K.members_set)

  -- 4. Update flow state
  -- cancellation_policy is persisted so an AlreadyTerminal retry can
  -- return the authoritative stored policy instead of echoing the
  -- caller's retry intent.
  --
  -- NOTE: this field is persisted from this library version onward.
  -- Flows cancelled before this deploy reach public_flow_state='cancelled'
  -- without a cancellation_policy value. The Rust caller detects the
  -- empty field on HMGET and falls back to args.cancellation_policy, so
  -- no backfill migration is needed.
  redis.call("HSET", K.flow_core,
    "public_flow_state", "cancelled",
    "cancelled_at", now_ms,
    "cancel_reason", A.reason,
    "cancellation_policy", A.cancellation_policy,
    "last_mutation_at", now_ms)

  -- Do NOT SREM flow_index here. Member cancellations dispatch
  -- asynchronously from ff-server; flow_projector needs to keep
  -- projecting the flow while those cancels land so the summary
  -- reflects the real progression (running/blocked → cancelled). The
  -- projector owns the SREM once it observes sampled==true_total
  -- all-terminal (see crates/ff-engine/src/scanner/flow_projector.rs).
  -- A projector-owned SREM is also the right place because it is
  -- the only writer that can prove every member has actually reached
  -- terminal state. Removing the entry here would freeze the summary
  -- at whatever snapshot was current when cancel_flow fired.

  -- 5. Durable backlog for async member-cancel dispatch.
  --
  -- Only cancel_all policy dispatches per-member cancels; other policies
  -- mark the flow terminal and leave members to the flow_projector /
  -- retention. Skip the backlog writes outside cancel_all.
  --
  -- If a process crashes between `CancellationScheduled` returning and
  -- the in-process dispatch finishing, OR if one member's cancel hits a
  -- permanent error that the bounded retry can't recover, the member
  -- would otherwise escape cancellation. Tracking the owed members in
  -- a persistent SET + partition-level ZSET lets the cancel_reconciler
  -- scanner drain the remainder on its interval.
  --
  -- Score = now + grace_ms so the reconciler doesn't race the live
  -- dispatch that's about to start — live dispatch SREMs as it
  -- succeeds; reconciler only picks up flows whose grace has elapsed.
  if A.cancellation_policy == "cancel_all" and #members > 0 then
    -- SADD chunked: Lua's unpack() arg limit is ~8000 on some builds
    -- and some Valkey deployments enforce max-args lower. Chunk at
    -- 1000 to stay well under both without a noticeable cost.
    local i = 1
    while i <= #members do
      local chunk_end = math.min(i + 999, #members)
      local sadd_args = {}
      for j = i, chunk_end do
        sadd_args[#sadd_args + 1] = members[j]
      end
      redis.call("SADD", K.pending_cancels, unpack(sadd_args))
      i = chunk_end + 1
    end
    redis.call("ZADD", K.cancel_backlog, now_ms + A.grace_ms, A.flow_id)
  end

  -- 6. Return: ok(cancellation_policy, member1, member2, ...)
  -- Build array manually to include variable member list.
  local result = {1, "OK", A.cancellation_policy}
  for _, eid in ipairs(members) do
    result[#result + 1] = eid
  end

  return result
end)

---------------------------------------------------------------------------
-- ff_ack_cancel_member  (on {fp:N})
--
-- Record that one flow member's cancel has been committed. Called by
-- the live dispatch after each successful cancel_member_execution AND
-- by the cancel_reconciler scanner after it catches up on crash-
-- orphaned members. Atomically SREMs from pending_cancels and ZREMs
-- the flow from the partition backlog when the set is empty.
--
-- KEYS (2): pending_cancels, cancel_backlog
-- ARGV (2): eid, flow_id
---------------------------------------------------------------------------
redis.register_function('ff_ack_cancel_member', function(keys, args)
  local pending = keys[1]
  local backlog = keys[2]
  local eid     = args[1]
  local flow_id = args[2]

  redis.call("SREM", pending, eid)
  if redis.call("EXISTS", pending) == 0 then
    redis.call("ZREM", backlog, flow_id)
  end

  return ok()
end)

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

  -- 2b. Reject mutations on terminal flows
  local pfs = flow.public_flow_state or ""
  if pfs == "cancelled" or pfs == "completed" or pfs == "failed" then
    return err("flow_already_terminal")
  end

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
-- KEYS (7): exec_core, deps_meta, unresolved_set, dep_hash,
--           eligible_zset, blocked_deps_zset, deps_all_edges
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
    deps_all_edges   = keys[7],
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
  -- Register edge in the per-execution all-edges index (cluster-safe
  -- retention discovery; retained across resolve, purged wholesale on
  -- retention trim).
  redis.call("SADD", K.deps_all_edges, A.edge_id)
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
-- On satisfaction, if the edge was staged with a non-empty
-- `data_passing_ref`, atomically COPYs the upstream's result key into
-- the downstream's input_payload key before flipping the child to
-- eligible. Upstream + downstream are guaranteed co-located on the
-- same {fp:N} slot by flow membership (RFC-011 §7.3).
--
-- KEYS (11): exec_core, deps_meta, unresolved_set, dep_hash,
--            eligible_zset, terminal_zset, blocked_deps_zset,
--            attempt_hash, stream_meta, downstream_payload,
--            upstream_result
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
    downstream_payload = keys[10],
    upstream_result    = keys[11],
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
    if #raw == 0 then return ok("satisfied", "") end
    local core = hgetall_to_table(raw)

    -- Server-side data_passing_ref resolution (Batch C item 3). When
    -- the edge was staged with a non-empty `data_passing_ref`, replace
    -- the downstream's input_payload with the upstream's result. COPY
    -- is a single-slot server-internal op (no round-trip to Lua
    -- memory) so large result payloads don't inflate the FCALL's
    -- working set.
    --
    -- Terminal-child guard: a late satisfaction can race with the
    -- child being cancelled or skipped. Don't overwrite the payload
    -- of a child that has already reached a terminal state — it's at
    -- best pointless (the worker will never read it) and at worst
    -- noisy for post-mortem debugging.
    --
    -- Write-ordering note (RFC-010 §4.8b): COPY runs BEFORE the
    -- eligibility transition below so a crash between the two leaves
    -- the child blocked (or late-satisfied on reconciler retry) with
    -- the correct payload rather than eligible with a stale one.
    --
    -- Void-completion path: if the upstream called complete(None), the
    -- result key does not exist — COPY returns 0 and data_injected
    -- stays empty, leaving the child's original input_payload intact.
    local data_injected = ""
    if is_set(dep.data_passing_ref)
       and core.terminal_outcome == "none" then
      local copied = redis.call(
        "COPY", K.upstream_result, K.downstream_payload, "REPLACE")
      if copied == 1 then
        data_injected = "data_injected"
      end
    end

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

    return ok("satisfied", data_injected)
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
      -- NOTE: If the child is active (worker holding lease), this FCALL runs
      -- on {p:N} (exec partition) so we CAN write exec_core and attempt_hash.
      -- However, lease_current, lease_expiry_zset, worker_leases, and
      -- active_index also live on {p:N} — but the KEYS array for this
      -- function does not include them (only 9 KEYS).  Cleaning them here
      -- would require adding more KEYS slots and pre-reading worker_instance_id
      -- to construct the worker_leases key.  Instead, lease cleanup is
      -- delegated to the lease_expiry scanner (1.5s default interval):
      --   1. lease_expiry_scanner sees expired lease → ff_mark_lease_expired_if_due
      --   2. Worker's renewal sees terminal → stops with terminal error
      --   3. ff_expire_execution (attempt_timeout/deadline scanner) does full cleanup
      -- Race window: between this skip and scanner cleanup, exec_core is
      -- terminal(skipped) but stale entries remain in active/lease indexes.
      -- Bounded by lease_expiry_interval (default 1.5s).  Index reconciler
      -- detects and logs any residual inconsistency at 45s intervals.
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

    -- Push-based DAG promotion (bridge-event gap report §1.3 analogue).
    -- A child skipped due to an impossible upstream is an FF-initiated
    -- terminal transition: cairn never calls anything for the skip, so
    -- without a PUBLISH the skip's own downstream edges only resolve
    -- via the 15s dependency_reconciler safety net. Symmetric with the
    -- other terminal sites (ff_complete_execution et al.). Gated on
    -- `is_set(core.flow_id)` — a skip on a standalone exec would be a
    -- bug upstream (standalones have no edges), but the gate keeps the
    -- invariant consistent with the other emit sites. Also gated on
    -- `is_set(core.execution_id)`: the ff-backend-valkey subscriber
    -- fails to parse an empty execution_id and silently drops the
    -- message, reintroducing reconciler-latency for that exec.
    if is_set(core.flow_id) and is_set(core.execution_id) then
      local payload = cjson.encode({
        execution_id = core.execution_id,
        flow_id = core.flow_id,
        outcome = "skipped",
      })
      redis.call("PUBLISH", "ff:dag:completions", payload)
    end
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

  -- 3. Check replay limit (read from policy, same pattern as ff_reclaim_execution)
  local replay_count = tonumber(core.replay_count or "0")
  local max_replays = 10  -- default
  local policy_key = string.gsub(K.core_key, ":core$", ":policy")
  local policy_raw = redis.call("GET", policy_key)
  if policy_raw then
    local ok_p, pol = pcall(cjson.decode, policy_raw)
    if ok_p and type(pol) == "table" then
      max_replays = tonumber(pol.max_replay_count) or 10
    end
  end
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

---------------------------------------------------------------------------
-- ff_set_flow_tags  (issue #58.4)
--
-- Write caller-supplied tag fields to the flow's separate tags key
-- (`ff:flow:{fp:N}:<flow_id>:tags`). Mirrors `ff_set_execution_tags`.
--
-- Lazy migration (Option 1(a)): BEFORE writing, any existing fields on
-- `flow_core` whose name matches the reserved namespace
-- `^[a-z][a-z0-9_]*%.` are moved to `tags_key` and HDEL'd from
-- `flow_core`. Heals pre-58.4 flows that stored `<caller>.<field>` tags
-- inline on `flow_core`. Idempotent: after first call, no fields match.
--
-- Tag keys MUST match `^[a-z][a-z0-9_]*%.`; violations fail-closed with
-- `invalid_tag_key` (no migration, no write).
--
-- KEYS (2): flow_core, tags_key
-- ARGV (>=2, even): k1, v1, k2, v2, ...
---------------------------------------------------------------------------
redis.register_function('ff_set_flow_tags', function(keys, args)
  local K = {
    flow_core = keys[1],
    tags_key  = keys[2],
  }

  local n = #args
  if n == 0 or (n % 2) ~= 0 then
    return err("invalid_input", "tags must be non-empty even-length key/value pairs")
  end

  if redis.call("EXISTS", K.flow_core) == 0 then
    return err("flow_not_found")
  end

  -- Require `<caller>.<field>` with at least one non-dot char after the
  -- first dot (same rule as `ff_set_execution_tags`). Suffix may contain
  -- further dots.
  for i = 1, n, 2 do
    local k = args[i]
    if type(k) ~= "string" or not string.find(k, "^[a-z][a-z0-9_]*%.[^.]") then
      return err("invalid_tag_key", tostring(k))
    end
  end

  -- Lazy migration: only HGETALL the core hash once per flow. A sentinel
  -- `tags_migrated=1` field on `flow_core` short-circuits subsequent
  -- calls so tag writes on well-formed flows stay O(1) instead of paying
  -- an O(n) scan of every flow_core field. The sentinel itself is
  -- dot-free snake_case — it matches FF's own-field rule, not the
  -- reserved caller namespace, so it can't be confused with a tag.
  local migrated = redis.call("HGET", K.flow_core, "tags_migrated")
  if migrated ~= "1" then
    local flat = redis.call("HGETALL", K.flow_core)
    local to_migrate = {}
    local to_delete = {}
    for i = 1, #flat, 2 do
      local fname = flat[i]
      if type(fname) == "string" and string.find(fname, "^[a-z][a-z0-9_]*%.[^.]") then
        to_migrate[#to_migrate + 1] = fname
        to_migrate[#to_migrate + 1] = flat[i + 1]
        to_delete[#to_delete + 1] = fname
      end
    end
    if #to_migrate > 0 then
      redis.call("HSET", K.tags_key, unpack(to_migrate))
      redis.call("HDEL", K.flow_core, unpack(to_delete))
    end
    redis.call("HSET", K.flow_core, "tags_migrated", "1")
  end

  redis.call("HSET", K.tags_key, unpack(args))

  local now_ms = server_time_ms()
  redis.call("HSET", K.flow_core, "last_mutation_at", tostring(now_ms))

  return ok(tostring(n / 2))
end)
