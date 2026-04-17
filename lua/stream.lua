-- FlowFabric stream append function
-- Reference: RFC-006 (Stream), RFC-010 §4.1 (#20)
--
-- Depends on helpers: ok, err, is_set

---------------------------------------------------------------------------
-- #20  ff_append_frame
--
-- Append a frame to the attempt-scoped output stream. Highest-throughput
-- function — called once per token during LLM streaming. Uses lite lease
-- validation (HMGET, not HGETALL) for minimal overhead. Class B operation.
--
-- KEYS (3): exec_core, stream_data, stream_meta
-- ARGV (13): execution_id, attempt_index, lease_id, lease_epoch,
--            frame_type, ts, payload, encoding, correlation_id,
--            source, retention_maxlen, attempt_id, max_payload_bytes
---------------------------------------------------------------------------
redis.register_function('ff_append_frame', function(keys, args)
  local K = {
    core_key    = keys[1],
    stream_key  = keys[2],
    stream_meta = keys[3],
  }

  local A = {
    execution_id     = args[1],
    attempt_index    = args[2],
    lease_id         = args[3],
    lease_epoch      = args[4],
    frame_type       = args[5],
    ts               = args[6] or "",
    payload          = args[7] or "",
    encoding         = args[8] or "utf8",
    correlation_id   = args[9] or "",
    source           = args[10] or "worker",
    retention_maxlen = tonumber(args[11] or "0"),
    attempt_id       = args[12] or "",
    max_payload_bytes = tonumber(args[13] or "65536"),
  }

  -- 1. Payload size guard (v1 default: 64KB)
  if #A.payload > A.max_payload_bytes then
    return err("retention_limit_exceeded")
  end

  -- 2. Lite lease validation via HMGET (Class B — no full HGETALL)
  local core = redis.call("HMGET", K.core_key,
    "current_attempt_index",   -- [1]
    "current_lease_id",        -- [2]
    "current_lease_epoch",     -- [3]
    "lease_expires_at",        -- [4]
    "lifecycle_phase",         -- [5]
    "ownership_state")         -- [6]

  -- Execution must be active
  if core[5] ~= "active" then
    return err("stream_closed")
  end

  -- Ownership must not be expired/revoked
  if core[6] == "lease_expired_reclaimable" or core[6] == "lease_revoked" then
    return err("stale_owner_cannot_append")
  end

  -- Lease must not be expired (server time check)
  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
  if tonumber(core[4] or "0") <= now_ms then
    return err("stale_owner_cannot_append")
  end

  -- Attempt index must match
  if tostring(core[1]) ~= A.attempt_index then
    return err("stale_owner_cannot_append")
  end

  -- Lease identity must match
  if core[2] ~= A.lease_id or tostring(core[3]) ~= A.lease_epoch then
    return err("stale_owner_cannot_append")
  end

  -- 3. Lazy-create stream metadata on first append
  if redis.call("EXISTS", K.stream_meta) == 0 then
    redis.call("HSET", K.stream_meta,
      "stream_id", A.execution_id .. ":" .. A.attempt_index,
      "execution_id", A.execution_id,
      "attempt_id", A.attempt_id,
      "attempt_index", A.attempt_index,
      "created_at", tostring(now_ms),
      "closed_at", "",
      "closed_reason", "",
      "durability_mode", "durable_full",
      "retention_maxlen", tostring(A.retention_maxlen),
      "last_sequence", "",
      "frame_count", "0",
      "total_bytes", "0",
      "last_frame_at", "")
  end

  -- 4. Check stream not closed
  local closed = redis.call("HGET", K.stream_meta, "closed_at")
  if is_set(closed) then
    return err("stream_closed")
  end

  -- 5. Append frame via XADD
  local ts = A.ts ~= "" and A.ts or tostring(now_ms)
  local xadd_args = {
    K.stream_key, "*",
    "frame_type", A.frame_type,
    "ts", ts,
    "payload", A.payload,
    "encoding", A.encoding,
    "source", A.source,
  }
  -- Only include correlation_id if non-empty (saves memory on high-throughput paths)
  if A.correlation_id ~= "" then
    xadd_args[#xadd_args + 1] = "correlation_id"
    xadd_args[#xadd_args + 1] = A.correlation_id
  end

  local entry_id = redis.call("XADD", unpack(xadd_args))

  -- 6. Update stream metadata.
  --
  -- `frame_count` is the LIFETIME append counter — it is NOT the number
  -- of frames currently retained in the stream. XTRIM below prunes old
  -- entries without decrementing this counter, so on a 10k-cap stream
  -- that has seen 1M appends `frame_count==1_000_000` while `XLEN==10_000`.
  -- Consumers that want the retained count must `XLEN` the stream
  -- directly; `frame_count` is the right number for metering, billing,
  -- per-attempt usage attribution — anything that needs "how much was
  -- produced", not "how much is still here".
  local frame_count = redis.call("HINCRBY", K.stream_meta, "frame_count", 1)
  redis.call("HINCRBY", K.stream_meta, "total_bytes", #A.payload)
  redis.call("HSET", K.stream_meta,
    "last_sequence", entry_id,
    "last_frame_at", tostring(now_ms))

  -- 7. Apply retention trim.
  --
  -- XTRIM MAXLEN `~` (approximate) is the default: it trims at macro-node
  -- boundaries for throughput, so actual retained length floats slightly
  -- above the target. Under a token-per-frame burst this can briefly
  -- hold up to 2x the requested retention.
  --
  -- When the caller explicitly passes `retention_maxlen > 0` they've
  -- opted into a specific bound; honor it EXACTLY with `=`. Bursty LLM
  -- workloads that care about predictable memory pay a small XTRIM-rate
  -- cost for the tighter bound. Default (A.retention_maxlen == 0) still
  -- uses `~` for the lane-level unbounded-growth guard, where throughput
  -- matters more than exact retention.
  local maxlen = A.retention_maxlen
  local trim_op
  if maxlen == 0 then
    maxlen = 10000       -- default cap prevents unbounded growth
    trim_op = "~"
  else
    trim_op = "="        -- caller-supplied bound is honored exactly
  end
  redis.call("XTRIM", K.stream_key, "MAXLEN", trim_op, maxlen)

  return ok(entry_id, tostring(frame_count))
end)

---------------------------------------------------------------------------
-- ff_read_attempt_stream
--
-- Read frames from an attempt-scoped output stream via XRANGE. Non-blocking
-- (safe in Lua Functions). Cluster-safe: stream_key and stream_meta share
-- the {p:N} hash tag.
--
-- Returns an empty array when the stream key does not exist (not an error —
-- the attempt may not have produced frames yet). Also reports
-- (closed_at, closed_reason) from stream_meta so callers can stop polling.
--
-- KEYS (2): stream_data, stream_meta
-- ARGV (3): from_id, to_id, count_limit (must be 1..=HARD_CAP; 0 rejected)
---------------------------------------------------------------------------
redis.register_function('ff_read_attempt_stream', function(keys, args)
  local stream_key  = keys[1]
  local stream_meta = keys[2]

  local from_id = args[1] or "-"
  local to_id   = args[2] or "+"
  local count_limit = tonumber(args[3] or "0")

  -- Explicit reject on zero/negative AND on over-cap. The REST and SDK
  -- layers reject both at their boundary (R2/R3); the Lua check is the
  -- last line of defense for direct FCALL callers (tests, future
  -- consumers) so they get a clear error instead of a silently-clamped
  -- whole-stream read or reply-size blowup.
  --
  -- Before PR#7 this was asymmetric: < 1 rejected with invalid_input,
  -- but > HARD_CAP was silently clamped. That contradicted RFC-006
  -- §Input validation ("both bounds reject, neither silently clamps").
  -- Now both edges reject symmetrically.
  --
  -- HARD_CAP mirrors ff_core::contracts::STREAM_READ_HARD_CAP — keep in sync.
  local HARD_CAP = 10000
  if count_limit == nil or count_limit < 1 then
    return err("invalid_input", "count_limit must be >= 1")
  end
  if count_limit > HARD_CAP then
    return err("invalid_input", "count_limit_exceeds_hard_cap")
  end

  -- Stream may legitimately not exist (never-written attempt). XRANGE on a
  -- missing key returns empty, so no pre-check is needed.
  local entries = redis.call("XRANGE", stream_key, from_id, to_id,
                             "COUNT", count_limit)

  -- Fetch terminal markers from stream_meta. A never-written attempt has
  -- no stream_meta hash; HMGET returns nils for both fields which we
  -- normalize to empty strings on the return path so Rust can decode a
  -- consistent shape.
  local meta = redis.call("HMGET", stream_meta, "closed_at", "closed_reason")
  local closed_at     = meta[1] or ""
  local closed_reason = meta[2] or ""

  -- entries is an array of [entry_id, [f1, v1, f2, v2, ...]].
  -- Return shape: ok(entries, closed_at, closed_reason). Rust parses the
  -- three fields positionally.
  return ok(entries, closed_at, closed_reason)
end)
