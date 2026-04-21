#!lua name=flowfabric

-- source: lua/helpers.lua
-- FlowFabric shared library-local helpers
-- These are local functions available to all registered functions in the
-- flowfabric library. They are NOT independently FCALL-able.
-- Reference: RFC-010 §4.8, RFC-004 §Waitpoint Security (HMAC tokens)

---------------------------------------------------------------------------
-- Capability CSV bounds (RFC-009 §7.5)
---------------------------------------------------------------------------
-- Shared ceiling for BOTH the worker-side CSV (ff_issue_claim_grant ARGV[9])
-- AND the execution-side CSV (exec_core.required_capabilities). Defense in
-- depth against runaway field sizes: a 10k-token list turns into a multi-MB
-- HSET value and a per-candidate O(N) atomic scan that blocks the shard.
--
-- Inclusivity: these are MAXIMUM accepted values. `#csv == CAPS_MAX_BYTES`
-- and `n == CAPS_MAX_TOKENS` are accepted; one more rejects. Rust-side
-- ingress (ff-sdk::FlowFabricWorker::connect, ff-scheduler::Scheduler::
-- claim_for_worker, ff-core::policy::RoutingRequirements deserialization
-- via lua/execution.lua) enforces the same ceilings so the Lua check is a
-- defense-in-depth backstop, not the primary validator.
local CAPS_MAX_BYTES  = 4096
local CAPS_MAX_TOKENS = 256

---------------------------------------------------------------------------
-- Hex / binary helpers (for HMAC-SHA1 token derivation)
---------------------------------------------------------------------------

-- Convert a hex string to a binary (byte) string. Accepts mixed case.
-- Returns nil on ANY malformed input: non-string, odd length, OR any
-- non-hex char (including whitespace, unicode, control chars). Callers
-- treat nil as invalid_secret.
--
-- Rust side (ServerConfig) already validates the env secret as even-length
-- 0-9a-fA-F, but an operator writing directly to Valkey (or a torn write
-- during rotation) could bypass that validator. We refuse the conversion
-- here instead of silently coercing bad pairs to 0 bytes (which would
-- produce a bogus but valid-looking MAC).
local function hex_to_bytes(hex)
  if type(hex) ~= "string" or #hex % 2 ~= 0 then
    return nil
  end
  local out = {}
  for i = 1, #hex - 1, 2 do
    local byte = tonumber(hex:sub(i, i + 1), 16)
    if not byte then
      return nil
    end
    out[#out + 1] = string.char(byte)
  end
  return table.concat(out)
end

-- XOR two equal-length byte strings. Used for HMAC key-pad construction.
local function xor_bytes(a, b)
  local out = {}
  for i = 1, #a do
    out[i] = string.char(bit.bxor(a:byte(i), b:byte(i)))
  end
  return table.concat(out)
end

-- HMAC-SHA1(key_hex, message) → lowercase hex digest (40 chars), or nil on
-- malformed key_hex (odd-length / non-string). Callers must treat nil as
-- an invalid-secret error — never pass it to HSET / concat / return.
-- Reference: RFC 2104. SHA1 block size = 64 bytes.
local function hmac_sha1_hex(key_hex, message)
  local key = hex_to_bytes(key_hex)
  if not key then
    return nil
  end
  local block_size = 64
  if #key > block_size then
    -- Reduce oversized key via SHA1 (per RFC 2104). sha1hex output is 40
    -- lowercase hex chars, so the inner hex_to_bytes cannot fail.
    key = hex_to_bytes(redis.sha1hex(key))
  end
  if #key < block_size then
    key = key .. string.rep("\0", block_size - #key)
  end
  local ipad = string.rep(string.char(0x36), block_size)
  local opad = string.rep(string.char(0x5c), block_size)
  local inner = redis.sha1hex(xor_bytes(key, ipad) .. message)
  return redis.sha1hex(xor_bytes(key, opad) .. hex_to_bytes(inner))
end

-- Constant-time string equality. Returns true iff strings are equal in
-- both length and content. Uses XOR-accumulation to avoid early-exit
-- timing leaks on byte mismatches during HMAC token validation.
-- Reference: Remote timing attacks on authentication (e.g., CVE-2011-3389 class).
--
-- Safety note on the length check: a length-mismatch early return reveals
-- whether the presented string matches the expected length, which is a
-- timing side channel IF attacker-controlled length is used to probe the
-- expected length. In this codebase the caller normalizes to a fixed shape
-- BEFORE reaching here — validate_waitpoint_token already requires
-- #presented == 40 (SHA1 hex digest length) at the parsing boundary, so
-- any input reaching constant_time_eq has a length already known to be 40
-- by the attacker. The only length variation here is on `expected`, which
-- is server-computed and constant. Hence this early return does not leak
-- secret-dependent timing.
local function constant_time_eq(a, b)
  if type(a) ~= "string" or type(b) ~= "string" then
    return false
  end
  if #a ~= #b then
    return false
  end
  local acc = 0
  for i = 1, #a do
    acc = bit.bor(acc, bit.bxor(a:byte(i), b:byte(i)))
  end
  return acc == 0
end

---------------------------------------------------------------------------
-- Waitpoint HMAC tokens (RFC-004 §Waitpoint Security)
---------------------------------------------------------------------------
--
-- Token format: "kid:40hex"  — kid identifies which key signed the token,
-- enabling zero-downtime rotation. ANY kid present in the secrets hash
-- with a future `expires_at:<kid>` (or the current kid, which has no
-- expiry) accepts tokens. This supports rapid rotation: rotating A→B→C
-- within a grace window keeps A's secret validatable as long as
-- expires_at:A is still future.
--
-- HMAC input binds (waitpoint_id | waitpoint_key | created_at_ms) with a
-- pipe delimiter so field-boundary confusion cannot produce collisions
-- across waitpoints.
--
-- Secret storage: per-partition replicated hash at
--   ff:sec:{p:N}:waitpoint_hmac
-- Fields:
--   current_kid               — the kid minting new tokens (no expiry)
--   secret:<kid>              — hex-encoded HMAC key for each kid ever installed
--   expires_at:<kid>          — unix ms; accept tokens under <kid> iff exp > now_ms
--                               INVARIANT: expires_at:<current_kid> is NEVER written
--   previous_kid              — observability/audit only: the kid immediately
--                               preceding current_kid (NOT the only acceptable one)
--   previous_expires_at       — observability/audit only: matches
--                               expires_at:<previous_kid>
--
-- Replication is required for Valkey cluster mode (all FCALL KEYS must
-- hash to the same slot); rotation fans out across partitions.
---------------------------------------------------------------------------

-- Read the hmac_secrets hash. Returns a table with:
--   current_kid, current_secret — the minting kid (nil if not initialized)
--   kid_secrets = { [kid] = { secret = <hex>, expires_at = <ms or nil> } }
--     includes current_kid (expires_at = nil → no expiry)
--     includes every secret:<kid> present in the hash
--   previous_kid, previous_secret, previous_expires_at — kept for back-compat
--     (audit log / observability); validate path does NOT depend on them.
-- Returns nil if the hash is absent.
local function load_waitpoint_secrets(secrets_key)
  local raw = redis.call("HGETALL", secrets_key)
  if #raw == 0 then
    return nil
  end
  local t = {}
  for i = 1, #raw, 2 do
    t[raw[i]] = raw[i + 1]
  end
  local out = {
    current_kid = t.current_kid,
    previous_kid = t.previous_kid,
    previous_expires_at = t.previous_expires_at,
    kid_secrets = {},
  }
  if out.current_kid then
    out.current_secret = t["secret:" .. out.current_kid]
  end
  if out.previous_kid then
    out.previous_secret = t["secret:" .. out.previous_kid]
  end
  -- Multi-kid scan: every secret:<kid> becomes a validation candidate.
  -- current_kid has no expiry entry (intentional — it's always valid).
  -- Other kids are accepted iff expires_at:<kid> is set AND > now_ms; the
  -- expiry check runs in validate_waitpoint_token so we simply carry the
  -- raw expires_at string here.
  for k, v in pairs(t) do
    if k:sub(1, 7) == "secret:" then
      local kid = k:sub(8)
      if kid ~= "" then
        out.kid_secrets[kid] = {
          secret = v,
          expires_at = t["expires_at:" .. kid],
        }
      end
    end
  end
  return out
end

-- Build the HMAC input string. Pipe delimiter prevents concatenation
-- collisions across distinct (waitpoint_id, waitpoint_key) pairs.
local function waitpoint_hmac_input(waitpoint_id, waitpoint_key, created_at_ms)
  return waitpoint_id .. "|" .. waitpoint_key .. "|" .. tostring(created_at_ms)
end

-- Mint a waitpoint token using the current kid.
-- Returns (token, kid) on success or (nil, error_code) on failure.
-- Defense-in-depth: returns a typed error for missing secrets_key / missing
-- secrets hash so external callers that construct FCALL KEYS by hand cannot
-- produce the "arguments must be strings or integers" Lua panic via nil.
local function mint_waitpoint_token(secrets_key, waitpoint_id, waitpoint_key, created_at_ms)
  if type(secrets_key) ~= "string" or secrets_key == "" then
    return nil, "invalid_keys_missing_hmac"
  end
  local secrets = load_waitpoint_secrets(secrets_key)
  if not secrets or not secrets.current_kid or not secrets.current_secret then
    return nil, "hmac_secret_not_initialized"
  end
  local input = waitpoint_hmac_input(waitpoint_id, waitpoint_key, created_at_ms)
  local digest = hmac_sha1_hex(secrets.current_secret, input)
  if not digest then
    return nil, "invalid_secret"
  end
  return secrets.current_kid .. ":" .. digest, secrets.current_kid
end

-- Validate a waitpoint token against the (waitpoint_id, waitpoint_key,
-- created_at_ms) that were bound at mint time. Accepts tokens signed with
-- current_kid, or previous_kid if previous_expires_at has not passed.
-- Returns nil on success or an error code string on failure.
local function validate_waitpoint_token(
  secrets_key, token, waitpoint_id, waitpoint_key, created_at_ms, now_ms
)
  if type(secrets_key) ~= "string" or secrets_key == "" then
    return "invalid_keys_missing_hmac"
  end
  if type(token) ~= "string" or token == "" then
    return "missing_token"
  end
  local sep = token:find(":", 1, true)
  if not sep or sep < 2 or sep >= #token then
    return "invalid_token"
  end
  local kid = token:sub(1, sep - 1)
  local presented = token:sub(sep + 1)
  if #presented ~= 40 then
    -- SHA1 hex digest is always 40 chars.
    return "invalid_token"
  end

  local secrets = load_waitpoint_secrets(secrets_key)
  if not secrets or not secrets.current_kid then
    return "hmac_secret_not_initialized"
  end

  -- Multi-kid validation. ANY secret:<kid> present in the hash is a
  -- candidate IF:
  --   - kid == current_kid (no expiry, always valid), OR
  --   - expires_at:<kid> is a positive integer AND > now_ms.
  --
  -- Rationale: rapid rotation (A→B→C inside a grace window) must keep
  -- in-flight A-signed tokens valid. The previous 2-slot model
  -- (current + previous) evicted A as soon as B became previous, even
  -- though expires_at:A was still future. RFC-004 §Waitpoint Security
  -- promises grace duration, not "grace until next rotation".
  --
  -- Fail-CLOSED on malformed expires_at: a corrupted/non-numeric value
  -- means "no affirmative unexpired proof" — reject.
  local secret = nil
  local expiry_state = nil  -- "known_kid_expired" | "unknown_kid"
  if kid == secrets.current_kid then
    secret = secrets.current_secret
  else
    local entry = secrets.kid_secrets and secrets.kid_secrets[kid]
    if entry then
      local exp = tonumber(entry.expires_at)
      if not exp or exp <= 0 or exp < now_ms then
        -- secret:<kid> is present but its grace has elapsed (or was
        -- never recorded). Distinguishable from unknown_kid so the
        -- caller can log the more-actionable "token_expired".
        expiry_state = "known_kid_expired"
      else
        secret = entry.secret
      end
    else
      expiry_state = "unknown_kid"
    end
  end

  if not secret then
    if expiry_state == "known_kid_expired" then
      return "token_expired"
    end
    return "invalid_token"
  end

  local input = waitpoint_hmac_input(waitpoint_id, waitpoint_key, created_at_ms)
  local expected = hmac_sha1_hex(secret, input)
  if not expected then
    return "invalid_secret"
  end
  if not constant_time_eq(expected, presented) then
    return "invalid_token"
  end
  return nil
end

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
    -- See validate_lease_and_mark_expired for the full detail layout.
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
--
-- Also writes `closed_at`/`closed_reason="lease_expired"` on the attempt's
-- `stream_meta` hash when that stream exists, so `tail_stream` consumers
-- observe the terminal signal without having to fall back to polling
-- `execution_state`. This matters for the permanent-failure case (worker
-- OOM or node dead, no replacement reclaims): the reclaim path that
-- normally writes `closed_reason="reclaimed"` may never run, and without
-- this signal the tail poll loop waits forever.
--
-- Write order: we only write stream_meta if its existing `closed_at` is
-- empty. A later `ff_reclaim_execution` that overwrites `closed_reason`
-- to "reclaimed" still wins because it unconditionally HSETs the field;
-- this function intentionally does NOT overwrite a pre-existing close.
--
-- Key construction: the stream_meta key is derived from core_key's
-- `{p:N}` hash tag + current_attempt_index. All three keys share the
-- same hash tag, so this stays single-slot in cluster mode despite not
-- being declared in KEYS upfront — mirrors the dynamic attempt/lane key
-- construction in `ff_create_execution`.
--
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

  -- Close stream_meta (if the stream was lazily created) so tail_stream
  -- consumers receive the terminal signal. Core key format:
  --   ff:exec:{p:N}:<eid>:core
  -- Stream meta key format:
  --   ff:stream:{p:N}:<eid>:<attempt_index>:meta
  local att_idx = core.current_attempt_index
  if att_idx ~= nil and att_idx ~= "" then
    local tag_open  = string.find(keys.core_key, "{", 1, true)
    local tag_close = tag_open and string.find(keys.core_key, "}", tag_open, true)
    if tag_open and tag_close then
      local tag = string.sub(keys.core_key, tag_open, tag_close)
      -- After `}:` comes `<eid>:core`. Walk past the `}:` delimiter.
      local after_tag = string.sub(keys.core_key, tag_close + 2)
      local eid_end = string.find(after_tag, ":core", 1, true)
      if eid_end then
        local eid = string.sub(after_tag, 1, eid_end - 1)
        local stream_meta_key = "ff:stream:" .. tag .. ":" .. eid
                                .. ":" .. tostring(att_idx) .. ":meta"
        if redis.call("EXISTS", stream_meta_key) == 1 then
          local existing_closed_at = redis.call("HGET", stream_meta_key, "closed_at")
          if not is_set(existing_closed_at) then
            redis.call("HSET", stream_meta_key,
              "closed_at", tostring(now_ms),
              "closed_reason", "lease_expired")
          end
        end
      end
    end
  end
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
    -- Enriched error detail lets the SDK reconcile a replay of a terminal
    -- operation after a network drop: if the caller's (lease_epoch,
    -- attempt_id) match what's stored and the outcome matches what they
    -- asked for, treat the "error" as a successful replay. See
    -- parse_terminal_replay() on the Rust side. Detail slots:
    --   idx 2: terminal_outcome         (e.g. "success", "failed", "cancelled", "none")
    --   idx 3: current_lease_epoch      (persists across terminal; cleared for retry-scheduled)
    --   idx 4: lifecycle_phase          ("terminal" vs "runnable" disambiguates
    --                                    terminal_failed from retry_scheduled replay)
    --   idx 5: current_attempt_id       (preserved on terminal, cleared on retry;
    --                                    per-attempt replay guard)
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


-- source: lua/version.lua
-- FlowFabric library version check
-- Returns the library version string. Used by the loader to detect
-- whether the library is loaded and at the expected version.
--
-- Bump this string whenever any registered function's KEYS or ARGV arity
-- changes, or a new function is added. Mismatched versions force
-- `FUNCTION LOAD REPLACE` so old binaries cannot FCALL a library whose key
-- signatures they expect a different shape for.
--
-- SINGLE SOURCE OF TRUTH: Rust's `LIBRARY_VERSION` is extracted from the
-- `return 'X'` literal below by `scripts/gen-ff-script-lua.sh`, which
-- writes `crates/ff-script/src/flowfabric_lua_version`. Rust reads that
-- file via `include_str!`. Do NOT maintain a separate Rust literal.
-- Extract contract: the body MUST contain exactly one `return 'X'` literal
-- with single quotes (not double). CI runs the gen script and diffs; any
-- drift fails the build.

redis.register_function('ff_version', function(keys, args)
  return '10'
end)


-- source: lua/lease.lua
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


-- source: lua/execution.lua
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
  --   * ASCII control byte (0x00-0x1F, 0x7F) or space (0x20) → fail
  --     (invalid_capabilities:required:control_or_whitespace). Mirrors the
  --     Rust ingress in ff-sdk::FlowFabricWorker::connect and
  --     ff-scheduler::Scheduler::claim_for_worker (R3 relaxed printable-ASCII
  --     check to allow UTF-8 printable above 0x7F while still rejecting
  --     whitespace/control). This is the "last line of defense" for admin
  --     direct-HSET bypass: a required cap containing "\n" or "\0" is
  --     impossible to type, impossible to debug, and would silently pin an
  --     execution as unclaimable forever.
  --   * Bounds: same 4096 bytes / 256 tokens ceiling as the worker CSV.
  --
  -- Note on UTF-8: the byte-range test rejects ASCII control + space but
  -- accepts every byte ≥ 0x21 except 0x7F (DEL). UTF-8 multibyte sequences
  -- use only bytes 0x80-0xBF (continuation) or 0xC0-0xFD (lead), all above
  -- 0x7F, so i18n caps like "东京-gpu" pass through intact. See RFC-009 §7.5.
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
          -- Reject ASCII control (0x00-0x1F), DEL (0x7F), and space (0x20).
          -- Iterating byte-by-byte: any byte in 0x00..=0x20 or == 0x7F fails.
          for i = 1, #cap do
            local b = cap:byte(i)
            if b <= 0x20 or b == 0x7F then
              return err("invalid_capabilities", "required:control_or_whitespace")
            end
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
    -- Preserve the first-claim timestamp across retries; only fall
    -- back to `now_ms` when the stored value is empty (initial state
    -- written at HSET, line 208, or after a reset). In Lua the empty
    -- string is truthy, so `core.started_at or now_ms` would wedge at
    -- "" forever on the first claim — explicit emptiness check fixes
    -- the scenario 4 stage_latency bench (exec_core surfaced this via
    -- the new ExecutionInfo.started_at REST field).
    "started_at", (core.started_at ~= nil and core.started_at ~= "") and core.started_at or tostring(now_ms),
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

  -- Push-based DAG promotion (Batch C item 6). Only publish when the
  -- execution actually belongs to a flow — standalone executions never
  -- have downstream edges for the engine to resolve. The engine's
  -- CompletionListener scanner SUBSCRIBEs to ff:dag:completions and
  -- calls dispatch_dependency_resolution per message. The
  -- dependency_reconciler interval scan is retained as a safety net
  -- for: missed messages during listener restart, ungated state
  -- (flow_id empty on older executions), and cluster broadcast gaps.
  if is_set(core.flow_id) then
    local payload = cjson.encode({
      execution_id = A.execution_id,
      flow_id = core.flow_id,
      outcome = "success",
    })
    redis.call("PUBLISH", "ff:dag:completions", payload)
  end

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

  -- Already terminal. Enriched error detail mirrors validate_lease_and_mark_expired
  -- so SDK-side replay reconciliation works for cancel the same as for
  -- complete/fail.
  if core.lifecycle_phase == "terminal" then
    return err("execution_not_active",
      core.terminal_outcome or "",
      core.current_lease_epoch or "",
      "terminal",
      core.current_attempt_id or "")
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

  -- Push-based DAG promotion (Batch C item 6). Skip the publish when
  -- the cancel came from `flow_cascade` — that source means a parent
  -- already propagated and the engine is walking the edge graph; an
  -- additional publish here would trigger redundant dispatch work.
  -- (Not a correctness issue since dispatch is idempotent, but it
  -- avoids unnecessary load under wide fan-out cancellations.)
  if is_set(core.flow_id) and A.source ~= "flow_cascade" then
    local payload = cjson.encode({
      execution_id = A.execution_id,
      flow_id = core.flow_id,
      outcome = "cancelled",
    })
    redis.call("PUBLISH", "ff:dag:completions", payload)
  end

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

    -- Push-based DAG promotion (Batch C item 6). See ff_complete_execution
    -- for rationale. A terminal-failed upstream triggers
    -- child-skip cascades via ff_resolve_dependency on the receiver side.
    if is_set(core.flow_id) then
      local payload = cjson.encode({
        execution_id = A.execution_id,
        flow_id = core.flow_id,
        outcome = "failed",
      })
      redis.call("PUBLISH", "ff:dag:completions", payload)
    end

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
  -- SREM from the OLD worker's leases set before SADD to the new one.
  -- K.worker_leases_key targets the NEW (reclaiming) worker, but the
  -- execution is currently in the OLD worker's set via the original
  -- claim's SADD. Without this SREM the execution stays indexed under
  -- the old worker even though exec_core now reports a new owner,
  -- breaking operator drain visibility and worker→execution lookup.
  -- Same dynamic-key pattern as the max-reclaim branch above and as
  -- ff_expire_execution's cleanup path. Skipped when the same worker
  -- reclaims its own execution (idempotent no-op anyway).
  local old_wiid = core.current_worker_instance_id or ""
  if old_wiid ~= "" and old_wiid ~= A.worker_instance_id then
    local tag_wl = string.match(K.core_key, "(%b{})")
    redis.call("SREM",
      "ff:idx:" .. tag_wl .. ":worker:" .. old_wiid .. ":leases",
      A.execution_id)
  end
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

  -- Push-based DAG promotion (Batch C item 6 follow-up, issue #44).
  -- Same shape as ff_complete_execution / ff_fail_execution /
  -- ff_cancel_execution — emit only for flow-bound executions. Covers
  -- all three phases (active, suspended, runnable) because the terminal
  -- HSET + ZADD above is shared across them. Without this, a never-
  -- claimed flow-bound execution that hits execution_deadline relies on
  -- the dependency_reconciler safety net (15s default) to unblock
  -- children, spiking DAG latency on the timeout path.
  if is_set(core.flow_id) then
    local payload = cjson.encode({
      execution_id = A.execution_id,
      flow_id = core.flow_id,
      outcome = "expired",
    })
    redis.call("PUBLISH", "ff:dag:completions", payload)
  end

  return ok("expired", core.lifecycle_phase)
end)


-- source: lua/scheduling.lua
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

  return ok(A.execution_id)
end)


-- source: lua/suspension.lua
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
-- KEYS (17): exec_core, attempt_record, lease_current, lease_history,
--            lease_expiry_zset, worker_leases, suspension_current,
--            waitpoint_hash, waitpoint_signals, suspension_timeout_zset,
--            pending_wp_expiry_zset, active_index, suspended_zset,
--            waitpoint_history, wp_condition, attempt_timeout_zset,
--            hmac_secrets
-- ARGV (17): execution_id, attempt_index, attempt_id, lease_id,
--            lease_epoch, suspension_id, waitpoint_id, waitpoint_key,
--            reason_code, requested_by, timeout_at, resume_condition_json,
--            resume_policy_json, continuation_metadata_pointer,
--            use_pending_waitpoint, timeout_behavior, lease_history_maxlen
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
  }

  local A = {
    execution_id              = args[1],
    attempt_index             = args[2],
    attempt_id                = args[3],
    lease_id                  = args[4],
    lease_epoch               = args[5],
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
  }

  local t = redis.call("TIME")
  local now_ms = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

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

  -- 11. Add to per-lane suspended index + suspension timeout
  -- Score: timeout_at if set, otherwise MAX for "no timeout" ordering
  redis.call("ZADD", K.suspended_zset,
    is_set(A.timeout_at) and tonumber(A.timeout_at) or 9999999999999,
    A.execution_id)

  if is_set(A.timeout_at) then
    redis.call("ZADD", K.suspension_timeout_key, tonumber(A.timeout_at), A.execution_id)
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

    -- Remove from suspension indexes, add to terminal
    redis.call("ZREM", K.suspension_timeout_key, A.execution_id)
    redis.call("ZREM", K.suspended_zset, A.execution_id)
    redis.call("ZADD", K.terminal_key, now_ms, A.execution_id)

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


-- source: lua/signal.lua
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


-- source: lua/stream.lua
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


-- source: lua/budget.lua
-- FlowFabric budget functions
-- Reference: RFC-008 (Budget), RFC-010 §4.3 (#30, #31), §4.1 (#29a, #29b)
--
-- Depends on helpers: ok, err, is_set, hgetall_to_table

---------------------------------------------------------------------------
-- ff_create_budget  (on {b:M})
--
-- Create a new budget policy with hard/soft limits on N dimensions.
-- Idempotent: if EXISTS budget_def → return ok_already_satisfied.
--
-- KEYS (5): budget_def, budget_limits, budget_usage, budget_resets_zset,
--           budget_policies_index
-- ARGV (variable): budget_id, scope_type, scope_id, enforcement_mode,
--   on_hard_limit, on_soft_limit, reset_interval_ms, now_ms,
--   dimension_count, dim_1..dim_N, hard_1..hard_N, soft_1..soft_N
---------------------------------------------------------------------------
redis.register_function('ff_create_budget', function(keys, args)
  local K = {
    def_key         = keys[1],
    limits_key      = keys[2],
    usage_key       = keys[3],
    resets_zset     = keys[4],
    policies_index  = keys[5],
  }

  local A = {
    budget_id         = args[1],
    scope_type        = args[2],
    scope_id          = args[3],
    enforcement_mode  = args[4],
    on_hard_limit     = args[5],
    on_soft_limit     = args[6],
    reset_interval_ms = args[7],
    now_ms            = args[8],
  }

  -- Maintain budget_policies_index BEFORE the idempotency guard. SADD is
  -- itself idempotent (no-op on existing members), and hoisting it heals
  -- any pre-existing budget_def that was created before this index was
  -- introduced — no migration script required.
  redis.call("SADD", K.policies_index, A.budget_id)

  -- Idempotency: already exists → return immediately
  if redis.call("EXISTS", K.def_key) == 1 then
    return ok_already_satisfied(A.budget_id)
  end

  local dim_count = require_number(args[9], "dim_count")
  if type(dim_count) == "table" then return dim_count end

  -- HSET budget definition
  redis.call("HSET", K.def_key,
    "budget_id", A.budget_id,
    "scope_type", A.scope_type,
    "scope_id", A.scope_id,
    "enforcement_mode", A.enforcement_mode,
    "on_hard_limit", A.on_hard_limit,
    "on_soft_limit", A.on_soft_limit,
    "reset_interval_ms", A.reset_interval_ms,
    "breach_count", "0",
    "soft_breach_count", "0",
    "created_at", A.now_ms,
    "last_updated_at", A.now_ms)

  -- HSET per-dimension hard and soft limits
  for i = 1, dim_count do
    local dim  = args[9 + i]
    local hard = args[9 + dim_count + i]
    local soft = args[9 + 2 * dim_count + i]
    redis.call("HSET", K.limits_key, "hard:" .. dim, hard, "soft:" .. dim, soft)
  end

  -- budget_usage left empty — first report_usage will create fields

  -- Schedule periodic reset if reset_interval_ms > 0
  local interval_ms = tonumber(A.reset_interval_ms)
  if interval_ms > 0 then
    local next_reset_at = tostring(tonumber(A.now_ms) + interval_ms)
    redis.call("HSET", K.def_key, "next_reset_at", next_reset_at)
    redis.call("ZADD", K.resets_zset, tonumber(next_reset_at), A.budget_id)
  end

  return ok(A.budget_id)
end)

---------------------------------------------------------------------------
-- #30  ff_report_usage_and_check  (on {b:M})
--
-- Check-before-increment: read current usage, check hard limits. If any
-- dimension would breach, return HARD_BREACH without incrementing. If safe,
-- HINCRBY all dimensions, then check soft limits.
--
-- Atomic Lua serialization on {b:M} guarantees zero overshoot.
--
-- KEYS (3): budget_usage, budget_limits, budget_def
-- ARGV (variable): dimension_count, dim_1..dim_N, delta_1..delta_N, now_ms, [dedup_key]
---------------------------------------------------------------------------
redis.register_function('ff_report_usage_and_check', function(keys, args)
  local K = {
    usage_key  = keys[1],
    limits_key = keys[2],
    def_key    = keys[3],
  }

  local dim_count = require_number(args[1], "dim_count")
  if type(dim_count) == "table" then return dim_count end
  local now_ms = args[2 * dim_count + 2]
  local dedup_key = args[2 * dim_count + 3] or ""

  -- Idempotency: if dedup_key provided, check for prior application
  if dedup_key ~= "" then
    local existing = redis.call("GET", dedup_key)
    if existing then
      return {1, "ALREADY_APPLIED"}
    end
  end

  -- Phase 1: CHECK all dimensions BEFORE any increment.
  -- If any hard limit would be breached, reject the entire report.
  for i = 1, dim_count do
    local dim = args[1 + i]
    local delta = tonumber(args[1 + dim_count + i])
    local current = tonumber(redis.call("HGET", K.usage_key, dim) or "0")
    local new_total = current + delta

    local hard_limit = redis.call("HGET", K.limits_key, "hard:" .. dim)
    if hard_limit and hard_limit ~= "" and hard_limit ~= false then
      local limit_val = tonumber(hard_limit)
      if limit_val > 0 and new_total > limit_val then
        -- Record breach metadata but DO NOT increment
        redis.call("HINCRBY", K.def_key, "breach_count", 1)
        redis.call("HSET", K.def_key,
          "last_breach_at", now_ms,
          "last_breach_dim", dim,
          "last_updated_at", now_ms)
        return {1, "HARD_BREACH", dim, tostring(current), tostring(hard_limit)}
      end
    end
  end

  -- Phase 2: No hard breach detected — safe to increment all dimensions.
  local breached_soft = nil
  for i = 1, dim_count do
    local dim = args[1 + i]
    local delta = tonumber(args[1 + dim_count + i])
    local new_val = redis.call("HINCRBY", K.usage_key, dim, delta)

    -- Check soft limit (advisory — increment still happens)
    local soft_limit = redis.call("HGET", K.limits_key, "soft:" .. dim)
    if soft_limit and soft_limit ~= "" and soft_limit ~= false then
      local limit_val = tonumber(soft_limit)
      if limit_val > 0 and new_val > limit_val then
        if not breached_soft then
          breached_soft = dim
        end
      end
    end
  end

  -- Update metadata
  redis.call("HSET", K.def_key, "last_updated_at", now_ms)

  -- Mark dedup key after successful increment (24h TTL)
  if dedup_key ~= "" then
    redis.call("SET", dedup_key, "1", "PX", 86400000)
  end

  if breached_soft then
    redis.call("HINCRBY", K.def_key, "soft_breach_count", 1)
    local soft_val = tonumber(redis.call("HGET", K.limits_key, "soft:" .. breached_soft) or "0")
    local cur_val = tonumber(redis.call("HGET", K.usage_key, breached_soft) or "0")
    return {1, "SOFT_BREACH", breached_soft, tostring(cur_val), tostring(soft_val)}
  end

  return {1, "OK"}
end)

---------------------------------------------------------------------------
-- #31  ff_reset_budget  (on {b:M})
--
-- Scanner-called periodic reset. Zero all usage fields, record reset,
-- compute next_reset_at, re-score in reset index.
--
-- KEYS (3): budget_def, budget_usage, budget_resets_zset
-- ARGV (2): budget_id, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_reset_budget', function(keys, args)
  local K = {
    def_key       = keys[1],
    usage_key     = keys[2],
    resets_zset   = keys[3],
  }

  local A = {
    budget_id = args[1],
    now_ms    = args[2],
  }

  -- 1. Read usage fields and zero them
  local usage_fields = redis.call("HKEYS", K.usage_key)
  if #usage_fields > 0 then
    local zero_args = {}
    for _, field in ipairs(usage_fields) do
      zero_args[#zero_args + 1] = field
      zero_args[#zero_args + 1] = "0"
    end
    redis.call("HSET", K.usage_key, unpack(zero_args))
  end

  -- 2. Update budget_def: last_reset_at, reset_count
  redis.call("HINCRBY", K.def_key, "reset_count", 1)
  redis.call("HSET", K.def_key,
    "last_reset_at", A.now_ms,
    "last_updated_at", A.now_ms,
    "last_breach_at", "",
    "last_breach_dim", "")

  -- 3. Compute next_reset_at from reset_interval_ms
  local interval_ms = tonumber(redis.call("HGET", K.def_key, "reset_interval_ms") or "0")
  local next_reset_at = "0"
  if interval_ms > 0 then
    next_reset_at = tostring(tonumber(A.now_ms) + interval_ms)
    redis.call("HSET", K.def_key, "next_reset_at", next_reset_at)
    redis.call("ZADD", K.resets_zset, tonumber(next_reset_at), A.budget_id)
  else
    -- No recurring reset — remove from schedule
    redis.call("ZREM", K.resets_zset, A.budget_id)
  end

  return ok(next_reset_at)
end)

---------------------------------------------------------------------------
-- #29b  ff_unblock_execution  (on {p:N})
--
-- Re-evaluate blocked execution, set eligible_now, ZREM blocked set,
-- ZADD eligible. All 7 dims.
--
-- KEYS (3): exec_core, blocked_zset, eligible_zset
-- ARGV (3): execution_id, now_ms, expected_blocking_reason
---------------------------------------------------------------------------
redis.register_function('ff_unblock_execution', function(keys, args)
  local K = {
    core_key     = keys[1],
    blocked_zset = keys[2],
    eligible_zset = keys[3],
  }

  local A = {
    execution_id             = args[1],
    now_ms                   = args[2],
    expected_blocking_reason = args[3] or "",
  }

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- 2. Must be runnable
  if core.lifecycle_phase ~= "runnable" then
    return err("execution_not_eligible")
  end

  -- 3. Must be blocked
  local es = core.eligibility_state
  if es ~= "blocked_by_budget" and es ~= "blocked_by_quota"
     and es ~= "blocked_by_route" and es ~= "blocked_by_operator" then
    return err("execution_not_eligible")
  end

  -- 4. Validate expected blocking reason (prevent stale unblock)
  if A.expected_blocking_reason ~= "" then
    if core.blocking_reason ~= A.expected_blocking_reason then
      return err("execution_not_eligible")
    end
  end

  -- 5. Transition: all 7 dims
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
    "attempt_state", core.attempt_state or "pending_first_attempt",
    "public_state", "waiting",
    "last_transition_at", A.now_ms,
    "last_mutation_at", A.now_ms)

  -- 6. Move from blocked to eligible
  redis.call("ZREM", K.blocked_zset, A.execution_id)
  redis.call("ZADD", K.eligible_zset, score, A.execution_id)

  return ok("unblocked")
end)

---------------------------------------------------------------------------
-- #29a  ff_block_execution_for_admission  (on {p:N})
--
-- Parameterized block: set eligibility/blocking for budget/quota/route/lane
-- denial, ZREM eligible, ZADD target blocked set. All 7 dims.
--
-- KEYS (3): exec_core, eligible_zset, target_blocked_zset
-- ARGV (4): execution_id, blocking_reason, blocking_detail, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_block_execution_for_admission', function(keys, args)
  local K = {
    core_key       = keys[1],
    eligible_zset  = keys[2],
    blocked_zset   = keys[3],
  }

  local A = {
    execution_id    = args[1],
    blocking_reason = args[2],
    blocking_detail = args[3] or "",
    now_ms          = args[4],
  }

  -- 1. Read execution core
  local raw = redis.call("HGETALL", K.core_key)
  if #raw == 0 then return err("execution_not_found") end
  local core = hgetall_to_table(raw)

  -- 2. Must be runnable
  if core.lifecycle_phase ~= "runnable" then
    if core.lifecycle_phase == "terminal" then
      return err("execution_not_active",
        core.terminal_outcome or "",
        core.current_lease_epoch or "",
        "terminal",
        core.current_attempt_id or "")
    end
    return err("execution_not_eligible")
  end

  -- 3. Map blocking_reason to eligibility_state
  local REASON_TO_ELIGIBILITY = {
    waiting_for_budget         = "blocked_by_budget",
    waiting_for_quota          = "blocked_by_quota",
    waiting_for_capable_worker = "blocked_by_route",
    paused_by_operator         = "blocked_by_operator",
    paused_by_policy           = "blocked_by_lane_state",
  }
  local eligibility = REASON_TO_ELIGIBILITY[A.blocking_reason]
  if not eligibility then
    return err("invalid_blocking_reason")
  end

  -- 4. Transition: all 7 dims
  redis.call("HSET", K.core_key,
    "lifecycle_phase", "runnable",
    "ownership_state", "unowned",
    "eligibility_state", eligibility,
    "blocking_reason", A.blocking_reason,
    "blocking_detail", A.blocking_detail,
    "terminal_outcome", "none",
    "attempt_state", core.attempt_state or "pending_first_attempt",
    "public_state", "rate_limited",
    "last_transition_at", A.now_ms,
    "last_mutation_at", A.now_ms)

  -- 5. Move from eligible to blocked
  redis.call("ZREM", K.eligible_zset, A.execution_id)
  redis.call("ZADD", K.blocked_zset, tonumber(A.now_ms), A.execution_id)

  return ok("blocked")
end)


-- source: lua/quota.lua
-- FlowFabric quota and rate-limit functions
-- Reference: RFC-008 (Quota), RFC-010 §4.4 (#32)
--
-- Depends on helpers: ok, err, is_set

---------------------------------------------------------------------------
-- ff_create_quota_policy  (on {q:K})
--
-- Create a new quota/rate-limit policy.
-- Idempotent: if EXISTS quota_def → return ok_already_satisfied.
--
-- KEYS (5): quota_def, quota_window_zset, quota_concurrency_counter,
--           admitted_set, quota_policies_index
-- ARGV (5): quota_policy_id, window_seconds, max_requests_per_window,
--           max_concurrent, now_ms
---------------------------------------------------------------------------
redis.register_function('ff_create_quota_policy', function(keys, args)
  local K = {
    def_key          = keys[1],
    window_zset      = keys[2],
    concurrency_key  = keys[3],
    admitted_set     = keys[4],
    policies_index   = keys[5],
  }

  local A = {
    quota_policy_id         = args[1],
    window_seconds          = args[2],
    max_requests_per_window = args[3],
    max_concurrent          = args[4],
    now_ms                  = args[5],
  }

  -- Idempotency: already exists → return immediately
  if redis.call("EXISTS", K.def_key) == 1 then
    return ok_already_satisfied(A.quota_policy_id)
  end

  -- HSET quota definition
  redis.call("HSET", K.def_key,
    "quota_policy_id", A.quota_policy_id,
    "requests_per_window_seconds", A.window_seconds,
    "max_requests_per_window", A.max_requests_per_window,
    "active_concurrency_cap", A.max_concurrent,
    "created_at", A.now_ms)

  -- Init concurrency counter to 0
  redis.call("SET", K.concurrency_key, "0")

  -- Register in partition-level policy index (for cluster-safe discovery)
  redis.call("SADD", K.policies_index, A.quota_policy_id)

  -- admitted_set + quota_window_zset left empty (populated on admission)

  return ok(A.quota_policy_id)
end)

---------------------------------------------------------------------------
-- #32  ff_check_admission_and_record  (on {q:K})
--
-- Idempotent sliding-window rate check + concurrency check.
-- If admitted: ZADD window, SET NX guard, optional INCR concurrency,
-- SADD to admitted_set (for cluster-safe reconciler discovery).
--
-- KEYS (5): window_zset, concurrency_counter, quota_def, admitted_guard_key,
--           admitted_set
-- ARGV (6): now_ms, window_seconds, rate_limit, concurrency_cap,
--           execution_id, jitter_ms
---------------------------------------------------------------------------
redis.register_function('ff_check_admission_and_record', function(keys, args)
  local K = {
    window_zset        = keys[1],
    concurrency_key    = keys[2],
    quota_def          = keys[3],
    admitted_guard_key = keys[4],
    admitted_set       = keys[5],
  }

  local now_ms_n          = require_number(args[1], "now_ms")
  if type(now_ms_n) == "table" then return now_ms_n end
  local window_seconds_n  = require_number(args[2], "window_seconds")
  if type(window_seconds_n) == "table" then return window_seconds_n end
  local rate_limit_n      = require_number(args[3], "rate_limit")
  if type(rate_limit_n) == "table" then return rate_limit_n end
  local concurrency_cap_n = require_number(args[4], "concurrency_cap")
  if type(concurrency_cap_n) == "table" then return concurrency_cap_n end

  local A = {
    now_ms          = now_ms_n,
    window_seconds  = window_seconds_n,
    rate_limit      = rate_limit_n,
    concurrency_cap = concurrency_cap_n,
    execution_id    = args[5],
    jitter_ms       = tonumber(args[6] or "0"),
  }

  local window_ms = A.window_seconds * 1000

  -- 1. Idempotency guard: already admitted in this window?
  if redis.call("EXISTS", K.admitted_guard_key) == 1 then
    return { "ALREADY_ADMITTED" }
  end

  -- 2. Sliding window: remove expired entries
  redis.call("ZREMRANGEBYSCORE", K.window_zset, "-inf", A.now_ms - window_ms)

  -- 3. Check rate limit
  if A.rate_limit > 0 then
    local current_count = redis.call("ZCARD", K.window_zset)
    if current_count >= A.rate_limit then
      -- Compute retry_after from oldest entry
      local oldest = redis.call("ZRANGE", K.window_zset, 0, 0, "WITHSCORES")
      local retry_after_ms = 0
      if #oldest >= 2 then
        retry_after_ms = tonumber(oldest[2]) + window_ms - A.now_ms
        if retry_after_ms < 0 then retry_after_ms = 0 end
      end
      local jitter = 0
      if A.jitter_ms > 0 then
        jitter = math.random(0, A.jitter_ms)
      end
      return { "RATE_EXCEEDED", tostring(retry_after_ms + jitter) }
    end
  end

  -- 4. Check concurrency cap
  if A.concurrency_cap > 0 then
    local active = tonumber(redis.call("GET", K.concurrency_key) or "0")
    if active >= A.concurrency_cap then
      return { "CONCURRENCY_EXCEEDED" }
    end
  end

  -- 5. Admit: record in sliding window (execution_id as member — idempotent)
  redis.call("ZADD", K.window_zset, A.now_ms, A.execution_id)

  -- 6. Set admitted guard key with TTL = window size
  -- Guard: PX 0 or PX <0 causes Valkey error inside Lua (after ZADD committed).
  if window_ms > 0 then
    redis.call("SET", K.admitted_guard_key, "1", "PX", window_ms, "NX")
  end

  -- 7. Increment concurrency counter if cap is set
  if A.concurrency_cap > 0 then
    redis.call("INCR", K.concurrency_key)
  end

  -- 8. Track in admitted set (for cluster-safe reconciler — replaces SCAN)
  redis.call("SADD", K.admitted_set, A.execution_id)

  return { "ADMITTED" }
end)

---------------------------------------------------------------------------
-- ff_release_admission  (on {q:K})
--
-- Release a previously-recorded admission slot. Called when a claim grant
-- fails after admission was recorded, preventing leaked concurrency slots.
--
-- KEYS (3): admitted_guard_key, admitted_set, concurrency_counter
-- ARGV (1): execution_id
---------------------------------------------------------------------------
redis.register_function('ff_release_admission', function(keys, args)
  local K = {
    admitted_guard_key = keys[1],
    admitted_set       = keys[2],
    concurrency_key    = keys[3],
  }

  local execution_id = args[1]

  -- 1. Delete the guard key (idempotent — DEL returns 0 if absent)
  redis.call("DEL", K.admitted_guard_key)

  -- 2. Remove from admitted set
  redis.call("SREM", K.admitted_set, execution_id)

  -- 3. Decrement concurrency counter (floor at 0)
  local current = tonumber(redis.call("GET", K.concurrency_key) or "0")
  if current > 0 then
    redis.call("DECR", K.concurrency_key)
  end

  return {1, "OK", "released"}
end)


-- source: lua/flow.lua
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

