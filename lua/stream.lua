-- FlowFabric stream append function
-- Reference: RFC-006 (Stream), RFC-010 §4.1 (#20), RFC-015 (durability modes)
--
-- Depends on helpers: ok, err, is_set

---------------------------------------------------------------------------
-- RFC-015 JSON Merge Patch (RFC 7396) applier.
--
-- `target` and `patch` are Lua tables produced by `cjson.decode`. The
-- applier mutates `target` in place per RFC 7396:
--   * `patch[k] == cjson.null` → delete `target[k]`
--   * `patch[k]` is a table    → recurse
--   * otherwise                → `target[k] = patch[k]`
--
-- `depth` is a recursion guard (RFC-015 §3.3 step 4: bounded depth 16).
-- A patch whose root is not a JSON object (array / scalar / null) is
-- rejected by the caller per RFC 7396 — only object-rooted patches are
-- valid merge-patch documents.
---------------------------------------------------------------------------
local function apply_merge_patch(target, patch, depth)
  if depth > 16 then
    return nil, "patch_depth_exceeded"
  end
  for k, v in pairs(patch) do
    if v == cjson.null then
      target[k] = nil
    elseif type(v) == "table" and not v[1] and next(v) ~= nil then
      -- Object-valued → recurse. `v[1]` heuristic distinguishes arrays
      -- (numeric-indexed from 1) from objects. RFC 7396 replaces
      -- arrays wholesale (no recursion into array members).
      if type(target[k]) ~= "table" or (target[k][1] ~= nil) then
        target[k] = {}
      end
      local _, ferr = apply_merge_patch(target[k], v, depth + 1)
      if ferr ~= nil then
        return nil, ferr
      end
    else
      -- Scalar, array, or empty-table leaf → replace wholesale.
      target[k] = v
    end
  end
  return target, nil
end

---------------------------------------------------------------------------
-- RFC-015 null-sentinel rewrite. Walks `doc` and replaces scalar string
-- leaves equal to "__ff_null__" with cjson.null. Applied post-merge so
-- callers can encode "set this leaf to JSON null" inside an RFC 7396
-- merge patch (where `null` would otherwise mean "delete key").
---------------------------------------------------------------------------
local FF_NULL_SENTINEL = "__ff_null__"
local function rewrite_null_sentinel(doc, depth)
  if depth > 32 then return end
  if type(doc) ~= "table" then return end
  for k, v in pairs(doc) do
    if type(v) == "string" and v == FF_NULL_SENTINEL then
      doc[k] = cjson.null
    elseif type(v) == "table" then
      rewrite_null_sentinel(v, depth + 1)
    end
  end
end

-- Reject non-object root documents / patches (RFC 7396 requires an
-- object at the root). A bare scalar / array / null patch means
-- "replace the target wholesale" under RFC 7396, but we scope v0.6
-- to the object-root subset per RFC-015 §3.3 step 2.
local function is_json_object(tbl)
  if type(tbl) ~= "table" then return false end
  -- Empty table — treat as an object by convention (cjson would have
  -- decoded `{}` to `cjson.empty_array` in encode-array mode, but
  -- stream.lua uses decode defaults).
  if next(tbl) == nil then return true end
  -- Any numeric-indexed [1] entry → array, reject.
  return tbl[1] == nil
end

---------------------------------------------------------------------------
-- #20  ff_append_frame
--
-- Append a frame to the attempt-scoped output stream. Highest-throughput
-- function — called once per token during LLM streaming. Uses lite lease
-- validation (HMGET, not HGETALL) for minimal overhead. Class B operation.
--
-- KEYS (4): exec_core, stream_data, stream_meta, stream_summary
-- ARGV (19): execution_id, attempt_index, lease_id, lease_epoch,
--            frame_type, ts, payload, encoding, correlation_id,
--            source, retention_maxlen, attempt_id, max_payload_bytes,
--            stream_mode, patch_kind, ttl_ms,
--            maxlen_floor, maxlen_ceiling, ema_alpha
--
-- `stream_mode` (ARGV 14, RFC-015): "" / "durable" → StreamMode::Durable
-- (default). "summary" → DurableSummary (requires `patch_kind` ARGV 15).
-- "best_effort" → BestEffortLive (uses `ttl_ms` ARGV 16 for PEXPIRE and
-- ARGV 17-19 for dynamic MAXLEN sizing per RFC-015 §4.2).
---------------------------------------------------------------------------
redis.register_function('ff_append_frame', function(keys, args)
  local K = {
    core_key      = keys[1],
    stream_key    = keys[2],
    stream_meta   = keys[3],
    stream_summary = keys[4] or "",
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
    stream_mode      = args[14] or "",  -- RFC-015
    patch_kind       = args[15] or "",  -- RFC-015
    ttl_ms           = tonumber(args[16] or "0"),
    -- RFC-015 §4.2 dynamic MAXLEN knobs. Zero / missing → fall back to
    -- the RFC-final defaults.
    maxlen_floor     = tonumber(args[17] or "0"),
    maxlen_ceiling   = tonumber(args[18] or "0"),
    ema_alpha        = tonumber(args[19] or "0"),
  }
  if A.maxlen_floor   == nil or A.maxlen_floor   <= 0 then A.maxlen_floor   = 64    end
  if A.maxlen_ceiling == nil or A.maxlen_ceiling <= 0 then A.maxlen_ceiling = 16384 end
  if A.ema_alpha      == nil or A.ema_alpha      <= 0 or A.ema_alpha > 1.0 then
    A.ema_alpha = 0.2
  end
  if A.maxlen_ceiling < A.maxlen_floor then
    A.maxlen_ceiling = A.maxlen_floor
  end

  -- Normalise empty / unknown mode to "durable" for pre-RFC-015 parity.
  if A.stream_mode == "" then A.stream_mode = "durable" end

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

  if core[5] ~= "active" then
    return err("stream_closed")
  end

  if core[6] == "lease_expired_reclaimable" or core[6] == "lease_revoked" then
    return err("stale_owner_cannot_append")
  end

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
  if tonumber(core[4] or "0") <= now_ms then
    return err("stale_owner_cannot_append")
  end

  if tostring(core[1]) ~= A.attempt_index then
    return err("stale_owner_cannot_append")
  end

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
      "last_frame_at", "",
      "has_durable_frame", "0")  -- RFC-015 §4.1 — PEXPIRE gate
  end

  -- 4. Check stream not closed
  local closed = redis.call("HGET", K.stream_meta, "closed_at")
  if is_set(closed) then
    return err("stream_closed")
  end

  -- 5. RFC-015 §3.3 — DurableSummary delta apply. Done BEFORE XADD so
  -- the stream entry carries the post-merge `summary_version`.
  local summary_version = ""
  if A.stream_mode == "summary" then
    if K.stream_summary == "" then
      return err("invalid_input", "summary mode requires stream_summary key")
    end
    if A.patch_kind ~= "json-merge-patch" and A.patch_kind ~= "" then
      return err("invalid_input", "unsupported patch_kind")
    end

    -- Parse patch payload. RFC 7396 requires an object at the root.
    local ok_decode, patch = pcall(cjson.decode, A.payload)
    if not ok_decode then
      return err("invalid_input", "patch payload is not valid JSON")
    end
    if not is_json_object(patch) then
      return err("invalid_input", "patch must be a JSON object")
    end

    -- Load current summary document (or start from {}).
    local cur_doc_raw = redis.call("HGET", K.stream_summary, "document")
    local doc
    if cur_doc_raw and cur_doc_raw ~= "" then
      local ok_cur, parsed = pcall(cjson.decode, cur_doc_raw)
      if not ok_cur then
        return err("corruption", "stored summary document not decodeable")
      end
      doc = parsed
    else
      doc = {}
    end

    -- Apply merge patch, then rewrite the null sentinel.
    local _, apply_err = apply_merge_patch(doc, patch, 0)
    if apply_err ~= nil then
      return err("invalid_input", apply_err)
    end
    rewrite_null_sentinel(doc, 0)

    -- Encode + persist. Use cjson.encode (not cjson_safe) — input
    -- validation above means we should never encode unencodable.
    local encoded = cjson.encode(doc)
    local new_version = redis.call("HINCRBY", K.stream_summary, "version", 1)
    redis.call("HSET", K.stream_summary,
      "document", encoded,
      "patch_kind", "json-merge-patch",
      "last_updated_ms", tostring(now_ms))
    -- Set first_applied_ms on the very first delta only.
    if new_version == 1 then
      redis.call("HSET", K.stream_summary, "first_applied_ms", tostring(now_ms))
    end
    summary_version = tostring(new_version)
  end

  -- 6. Append frame via XADD with RFC-015 mode fields.
  local ts = A.ts ~= "" and A.ts or tostring(now_ms)
  local xadd_args = {
    K.stream_key, "*",
    "frame_type", A.frame_type,
    "ts", ts,
    "payload", A.payload,
    "encoding", A.encoding,
    "source", A.source,
    "mode", A.stream_mode,
  }
  if A.correlation_id ~= "" then
    xadd_args[#xadd_args + 1] = "correlation_id"
    xadd_args[#xadd_args + 1] = A.correlation_id
  end
  if summary_version ~= "" then
    xadd_args[#xadd_args + 1] = "summary_version"
    xadd_args[#xadd_args + 1] = summary_version
  end
  if A.stream_mode == "best_effort" and A.ttl_ms > 0 then
    xadd_args[#xadd_args + 1] = "ttl_ms"
    xadd_args[#xadd_args + 1] = tostring(A.ttl_ms)
  end

  local entry_id = redis.call("XADD", unpack(xadd_args))

  -- 7. Update stream metadata.
  --
  -- `frame_count` is the LIFETIME append counter — NOT the retained
  -- entry count (XTRIM below can prune without decrementing).
  local frame_count = redis.call("HINCRBY", K.stream_meta, "frame_count", 1)
  redis.call("HINCRBY", K.stream_meta, "total_bytes", #A.payload)
  redis.call("HSET", K.stream_meta,
    "last_sequence", entry_id,
    "last_frame_at", tostring(now_ms))

  -- RFC-015 §4.1: track `has_durable_frame` for the PEXPIRE gate. Any
  -- Durable or DurableSummary append flips the flag (and PERSISTs the
  -- stream key if a prior BestEffortLive had set a TTL).
  if A.stream_mode == "durable" or A.stream_mode == "summary" then
    local prior = redis.call("HGET", K.stream_meta, "has_durable_frame")
    if prior ~= "1" then
      redis.call("HSET", K.stream_meta, "has_durable_frame", "1")
      -- PERSIST any previously-set best-effort TTL on the stream key.
      redis.call("PERSIST", K.stream_key)
    end
  end

  -- 8. Apply retention / TTL policy.
  --
  -- RFC-015 §3.5 — DurableSummary compacts to a ~64-entry live-tail
  -- window. §4.1/§4.2 — BestEffortLive trims to a derived K and may
  -- PEXPIRE the stream key.
  -- Plain Durable keeps the pre-RFC-015 default (10_000 cap w/ `~`).
  if A.stream_mode == "summary" then
    redis.call("XTRIM", K.stream_key, "MAXLEN", "~", 64)
  elseif A.stream_mode == "best_effort" then
    -- RFC-015 §4.2: dynamic MAXLEN sizing from an EMA of append rate.
    --
    --   K = clamp(ceil(ema_rate_hz * ttl_ms / 1000) * 2, floor, ceiling)
    --
    -- EMA state persists on the per-attempt stream_meta Hash so the
    -- estimator survives across Lua invocations. `ema_rate_hz` is Hz;
    -- `last_append_ts_ms` is the wall-clock of the prior XADD.
    local meta = redis.call("HMGET", K.stream_meta,
      "ema_rate_hz", "last_append_ts_ms")
    local prev_rate_hz = tonumber(meta[1] or "")
    local prev_ts_ms   = tonumber(meta[2] or "")

    local rate_hz
    if prev_rate_hz == nil or prev_ts_ms == nil then
      -- First best-effort append: seed rate so the initial K == floor.
      -- floor = ceil(seed * ttl_s * 2) → seed = floor / (ttl_s * 2).
      -- Guard against ttl_ms == 0 to avoid /0.
      local ttl_s = math.max(A.ttl_ms, 1) / 1000.0
      rate_hz = A.maxlen_floor / (ttl_s * 2.0)
    else
      local dt_ms = now_ms - prev_ts_ms
      if dt_ms <= 0 then dt_ms = 1 end
      local inst_hz = 1000.0 / dt_ms
      rate_hz = A.ema_alpha * inst_hz + (1.0 - A.ema_alpha) * prev_rate_hz
    end

    local ttl_s_for_k = math.max(A.ttl_ms, 1) / 1000.0
    local raw_k = math.ceil(rate_hz * ttl_s_for_k * 2.0)
    if raw_k < A.maxlen_floor   then raw_k = A.maxlen_floor   end
    if raw_k > A.maxlen_ceiling then raw_k = A.maxlen_ceiling end
    local target_maxlen = raw_k

    redis.call("XTRIM", K.stream_key, "MAXLEN", "~", target_maxlen)

    redis.call("HSET", K.stream_meta,
      "ema_rate_hz",         tostring(rate_hz),
      "last_append_ts_ms",   tostring(now_ms),
      "maxlen_applied_last", tostring(target_maxlen))

    -- PEXPIRE gate: only set a TTL if the stream has NEVER received a
    -- durable frame. Durable content must not be destroyed by a
    -- best-effort TTL refresh.
    local has_dur = redis.call("HGET", K.stream_meta, "has_durable_frame")
    if (has_dur ~= "1") and A.ttl_ms > 0 then
      redis.call("PEXPIRE", K.stream_key, A.ttl_ms * 2)
    end
  else
    -- Durable (pre-RFC-015 path).
    local maxlen = A.retention_maxlen
    local trim_op
    if maxlen == 0 then
      maxlen = 10000
      trim_op = "~"
    else
      trim_op = "="
    end
    redis.call("XTRIM", K.stream_key, "MAXLEN", trim_op, maxlen)
  end

  -- Success shape: ok(entry_id, frame_count, summary_version). The
  -- third field is empty string for Durable / BestEffortLive and the
  -- post-merge version string for DurableSummary. Pre-RFC-015 callers
  -- that only look at fields [0] / [1] are unaffected.
  return ok(entry_id, tostring(frame_count), summary_version)
end)

---------------------------------------------------------------------------
-- ff_read_attempt_stream
--
-- Read frames from an attempt-scoped output stream via XRANGE. Non-blocking
-- (safe in Lua Functions). Cluster-safe: stream_key and stream_meta share
-- the {p:N} hash tag.
--
-- KEYS (2): stream_data, stream_meta
-- ARGV (4): from_id, to_id, count_limit, visibility
--
-- `visibility` (ARGV 4, RFC-015 §6.1): "" / "all" → no filter.
-- "exclude_best_effort" → drop entries whose XADD `mode` field equals
-- "best_effort". Pre-RFC-015 entries (no `mode` field) count as
-- `durable` per the RFC-015 §8.1 reader fallback.
---------------------------------------------------------------------------
redis.register_function('ff_read_attempt_stream', function(keys, args)
  local stream_key  = keys[1]
  local stream_meta = keys[2]

  local from_id = args[1] or "-"
  local to_id   = args[2] or "+"
  local count_limit = tonumber(args[3] or "0")
  local visibility = args[4] or ""

  local HARD_CAP = 10000
  if count_limit == nil or count_limit < 1 then
    return err("invalid_input", "count_limit must be >= 1")
  end
  if count_limit > HARD_CAP then
    return err("invalid_input", "count_limit_exceeds_hard_cap")
  end

  local entries = redis.call("XRANGE", stream_key, from_id, to_id,
                             "COUNT", count_limit)

  -- RFC-015 §6.1 server-side mode filter.
  if visibility == "exclude_best_effort" then
    local filtered = {}
    for i = 1, #entries do
      local entry = entries[i]
      local fields = entry[2]
      -- fields is a flat array of alternating key/value.
      local mode = "durable"
      for j = 1, #fields - 1, 2 do
        if fields[j] == "mode" then
          mode = fields[j + 1]
          break
        end
      end
      if mode ~= "best_effort" then
        filtered[#filtered + 1] = entry
      end
    end
    entries = filtered
  end

  local meta = redis.call("HMGET", stream_meta, "closed_at", "closed_reason")
  local closed_at     = meta[1] or ""
  local closed_reason = meta[2] or ""

  return ok(entries, closed_at, closed_reason)
end)

---------------------------------------------------------------------------
-- ff_read_summary  (RFC-015 §6.3)
--
-- Read the rolling summary document for an attempt. Non-blocking HGETALL
-- wrapper. Returns empty fields list when no DurableSummary frame has
-- ever been appended (the summary Hash is absent).
--
-- KEYS (1): stream_summary
-- ARGV (0)
---------------------------------------------------------------------------
redis.register_function('ff_read_summary', function(keys, args)
  local summary_key = keys[1]
  if redis.call("EXISTS", summary_key) == 0 then
    return ok("", "0", "", "0", "0")
  end
  local h = redis.call("HMGET", summary_key,
    "document", "version", "patch_kind", "last_updated_ms", "first_applied_ms")
  return ok(h[1] or "", h[2] or "0", h[3] or "",
            h[4] or "0", h[5] or "0")
end)
