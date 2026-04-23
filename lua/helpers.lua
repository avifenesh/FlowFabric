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

-- RFC-014 Pattern 3 — expand {suspension_id, wp_id, wp_key, wp_tok,
-- extras_table} into the 4 primary fields + N_extra count + N_extra ×
-- (id, key, tok) response tail. `extras` is an array of {waitpoint_id,
-- waitpoint_key, waitpoint_token} tables. Empty array → N_extra=0.
local function ok_extras(susp_id, wp_id, wp_key, wp_tok, extras)
  extras = extras or {}
  local out = {1, "OK", susp_id, wp_id, wp_key, wp_tok, tostring(#extras)}
  for _, e in ipairs(extras) do
    out[#out + 1] = e.waitpoint_id or ""
    out[#out + 1] = e.waitpoint_key or ""
    out[#out + 1] = e.waitpoint_token or ""
  end
  return out
end

local function ok_already_satisfied_extras(susp_id, wp_id, wp_key, wp_tok, extras)
  extras = extras or {}
  local out = {1, "ALREADY_SATISFIED", susp_id, wp_id, wp_key, wp_tok, tostring(#extras)}
  for _, e in ipairs(extras) do
    out[#out + 1] = e.waitpoint_id or ""
    out[#out + 1] = e.waitpoint_key or ""
    out[#out + 1] = e.waitpoint_token or ""
  end
  return out
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

-- RFC #58.5 — resolve the (lease_id, lease_epoch, attempt_id) fence triple.
-- Returns (fence_table, must_check) on success, or (nil, err_table) on a
-- partial triple (programming error — caller passed some but not all three).
--
-- Semantics:
--   * All three present (non-empty) → fence triple, must_check=true.
--     Caller is expected to run validate_lease_and_mark_expired next.
--   * All three empty               → server-resolved from exec_core,
--                                      must_check=false. Caller decides
--                                      whether unfenced mode is allowed
--                                      (terminal ops gate on `source`;
--                                      renew/suspend hard-reject).
--   * Any mix of set/empty          → err("partial_fence_triple").
--
-- @param core  hgetall_to_table(exec_core)
-- @param argv  table with .lease_id, .lease_epoch, .attempt_id (strings)
local function resolve_lease_fence(core, argv)
  local has_id = is_set(argv.lease_id)
  local has_ep = is_set(argv.lease_epoch)
  local has_at = is_set(argv.attempt_id)
  if has_id or has_ep or has_at then
    if not (has_id and has_ep and has_at) then
      return nil, err("partial_fence_triple")
    end
    return {
      lease_id    = argv.lease_id,
      lease_epoch = argv.lease_epoch,
      attempt_id  = argv.attempt_id,
    }, true
  end
  return {
    lease_id    = core.current_lease_id    or "",
    lease_epoch = core.current_lease_epoch or "",
    attempt_id  = core.current_attempt_id  or "",
  }, false
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
--   For composite conditions (RFC-014) the returned table also carries
--   `composite = true` and a parsed `tree` spec; the matcher fields
--   remain populated (empty) so legacy diagnostics that read
--   `satisfied_count` / `total_matchers` on the wp_condition hash still
--   work without crashing.
local function initialize_condition(json)
  local spec = cjson.decode(json)
  -- RFC-014: composite path. The Rust-side serializer sets
  -- `composite = true` on multi-signal conditions. Legacy single/
  -- operator/timeout paths continue to use the matcher array below.
  if spec.composite then
    if spec.v and tonumber(spec.v) ~= 1 then
      -- Future-proofing: unknown version. RFC-014 §7.2 rejects v>1.
      return {
        condition_type       = "composite",
        composite            = true,
        invalid              = "invalid_resume_condition",
        matchers             = {},
        total_matchers       = 0,
        satisfied_count      = 0,
        closed               = false,
      }
    end
    return {
      condition_type       = "composite",
      composite            = true,
      tree                 = spec.tree,
      match_mode           = "composite",
      minimum_signal_count = 1,
      total_matchers       = 0,
      satisfied_count      = 0,
      matchers             = {},
      closed               = false,
    }
  end
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
  -- RFC-014 composite path: write a minimal marker + the parsed tree.
  -- The composite evaluator reads `composite` + `tree_json` at signal
  -- delivery time; legacy `satisfied_count` / `total_matchers` fields
  -- stay zero so operator diagnostics that read them don't blow up.
  if cond.composite then
    redis.call("HSET", key,
      "condition_type", "composite",
      "composite", "1",
      "match_mode", "composite",
      "minimum_signal_count", "1",
      "total_matchers", "0",
      "satisfied_count", "0",
      "closed", cond.closed and "1" or "0",
      "updated_at", tostring(now_ms),
      "tree_json", cond.tree and cjson.encode(cond.tree) or "")
    return
  end
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

---------------------------------------------------------------------------
-- RFC-014 composite condition evaluator
--
-- Storage model (§3.1):
--   satisfied_set_key: SET of satisfier tokens ("wp:<id>" | "sig:<id>" |
--                     "src:<type>:<identity>" | "node:<path>").
--   member_map_key:   HASH of waitpoint_key → node_path (write-once at
--                     suspend-time, informational only under single-
--                     waitpoint scoping — see §3.1).
--
-- Per-signal algorithm (§3.3):
--   1. Walk the tree, identifying nodes this signal may contribute to.
--   2. For each such node, apply the local matcher (step 2.5); reject on
--      mismatch with `signal_ignored_matcher_failed`.
--   3. SADD the per-kind satisfier token to satisfied_set. Dedup returns
--      `appended_to_waitpoint_duplicate`.
--   4. Re-evaluate the tree: if the root is satisfied, emit
--      `resume_condition_satisfied`.
---------------------------------------------------------------------------

-- Does a matcher accept this signal? Matches RFC-013 SignalMatcher shape.
-- ByName compares by signal_name; Wildcard accepts any.
local function matcher_accepts(matcher, signal_name)
  if matcher == nil or type(matcher) ~= "table" then
    return true
  end
  if matcher.kind == "Wildcard" or matcher.kind == nil then
    return true
  end
  if matcher.kind == "ByName" then
    return matcher.name == signal_name
  end
  return false
end

-- Compute the satisfier token for a Count node given CountKind + signal.
local function count_satisfier_token(kind, signal, waitpoint_id)
  if kind == "DistinctSignals" then
    return "sig:" .. (signal.signal_id or "")
  elseif kind == "DistinctSources" then
    return "src:" .. (signal.source_type or "") .. ":" .. (signal.source_identity or "")
  end
  -- DistinctWaitpoints (default)
  return "wp:" .. (waitpoint_id or "")
end

-- Set-membership helper: tokens table keyed-by-name (in-memory snapshot).
local function set_has(tokens, token)
  return tokens[token] == true
end

-- Count how many satisfier tokens a Count node currently has in the set.
-- For DistinctWaitpoints we count tokens of the form `wp:<id>` where <id>
-- corresponds to any waitpoint_key in node.waitpoints. For
-- DistinctSignals / DistinctSources we count tokens of the right prefix
-- that were SADDed while delivering a signal matching this node (scoped
-- correctness relies on the per-node matcher filter at step 2.5).
local function count_node_satisfiers(node, tokens)
  local kind = node.count_kind or "DistinctWaitpoints"
  local count = 0
  -- For single-waitpoint composites (current scope), `tokens` contains
  -- the satisfier tokens of signals that landed on THIS node after
  -- passing the matcher filter. The tokens are already namespaced by
  -- the delivery routine so counting by prefix is sufficient.
  local prefix
  if kind == "DistinctWaitpoints" then
    prefix = "wp:"
  elseif kind == "DistinctSignals" then
    prefix = "sig:"
  else
    prefix = "src:"
  end
  local plen = #prefix
  for token, _ in pairs(tokens) do
    if string.sub(token, 1, plen) == prefix then
      count = count + 1
    end
  end
  return count
end

-- Evaluate a node against the current satisfied-set snapshot.
-- @param node    parsed tree node
-- @param tokens  table<string, true> snapshot of satisfied_set
-- @return true if satisfied
local function composite_evaluate_node(node, tokens)
  if node == nil then return false end
  local kind = node.kind
  if kind == "Single" then
    -- Leaf: the wp:<id> token is the satisfaction marker (RFC §3.2).
    -- We look up by path instead so a same-waitpoint AllOf can
    -- disambiguate by matcher; the delivery routine writes
    -- `leaf:<path>` tokens for satisfied Single-under-AllOf leaves.
    return set_has(tokens, "leaf:" .. (node.path or ""))
  elseif kind == "AllOf" then
    local members = node.members or {}
    for _, child in ipairs(members) do
      if child.kind == "Single" then
        if not set_has(tokens, "leaf:" .. (child.path or "")) then
          return false
        end
      else
        if not set_has(tokens, "node:" .. (child.path or "")) then
          return false
        end
      end
    end
    return true
  elseif kind == "Count" then
    -- Timeout token short-circuits a Count (RFC §6.1).
    if tokens["timeout:*"] == true then return true end
    return count_node_satisfiers(node, tokens) >= (tonumber(node.n) or 1)
  end
  -- NeverBySignal / unknown
  return false
end

-- Load satisfied_set contents into an in-memory table<string, true>.
local function load_satisfied_set(satisfied_set_key)
  local members = redis.call("SMEMBERS", satisfied_set_key)
  local t = {}
  for _, m in ipairs(members) do
    t[m] = true
  end
  return t
end

-- Find nodes in the tree that this signal may contribute to. Returns a
-- list of {node, ancestors} where ancestors is the path from the node
-- upward to the root (exclusive of the node itself).
local function composite_collect_candidate_nodes(tree, waitpoint_key, signal_name)
  -- For single-waitpoint composites, every Single/Count node whose
  -- waitpoint_key (or waitpoints list) contains `waitpoint_key` is a
  -- candidate. Walk the tree DFS, tracking ancestors.
  local results = {}
  local function walk(node, ancestors)
    if node == nil then return end
    local kind = node.kind
    if kind == "Single" then
      if node.waitpoint_key == waitpoint_key then
        results[#results + 1] = { node = node, ancestors = ancestors }
      end
    elseif kind == "Count" then
      local matches_wp = false
      for _, wk in ipairs(node.waitpoints or {}) do
        if wk == waitpoint_key then matches_wp = true; break end
      end
      if matches_wp then
        results[#results + 1] = { node = node, ancestors = ancestors }
      end
    elseif kind == "AllOf" then
      local nested = {}
      for i = 1, #ancestors do nested[i] = ancestors[i] end
      nested[#nested + 1] = node
      for _, child in ipairs(node.members or {}) do
        walk(child, nested)
      end
    end
  end
  walk(tree, {})
  return results
end

-- Emit initial member_map for operator diagnostics (RFC §4.4).
-- Accepts a single waitpoint_key (patterns 1 + 2) or a list of
-- waitpoint_keys (RFC-014 Pattern 3). Every candidate node path for
-- each key maps back to that key. Write-once at suspend-time.
local function seed_composite_member_map(member_map_key, tree, waitpoint_keys)
  if type(waitpoint_keys) == "string" then
    waitpoint_keys = { waitpoint_keys }
  end
  local fields = {}
  for _, wk in ipairs(waitpoint_keys or {}) do
    local cands = composite_collect_candidate_nodes(tree, wk, nil)
    for _, c in ipairs(cands) do
      fields[#fields + 1] = "wp:" .. wk .. ":" .. (c.node.path or "")
      fields[#fields + 1] = c.node.path or ""
    end
  end
  if #fields > 0 then
    redis.call("HSET", member_map_key, unpack(fields))
  end
end

-- Deliver a signal against a composite condition. Returns a table:
--   { effect = <string>, resume = <bool>, closer = <signal_id>,
--     all_satisfiers_json = <string> }
-- satisfied_set_key / member_map_key are RFC-014 §3.1 keys.
local function composite_deliver_signal(
  tree, satisfied_set_key, member_map_key, waitpoint_id, waitpoint_key,
  signal)
  local tokens = load_satisfied_set(satisfied_set_key)
  local candidates = composite_collect_candidate_nodes(tree, waitpoint_key, signal.signal_name)
  if #candidates == 0 then
    return { effect = "signal_ignored_not_in_condition", resume = false }
  end

  local added_any = false
  local matcher_failed_all = true
  -- RFC §3.3 step 2.5: per-node matcher filter. If ANY candidate node's
  -- matcher accepts this signal, we proceed; if all reject, bail with
  -- `signal_ignored_matcher_failed`.
  for _, c in ipairs(candidates) do
    local node = c.node
    local matcher = node.matcher
    if matcher_accepts(matcher, signal.signal_name) then
      matcher_failed_all = false
      local token
      if node.kind == "Single" then
        token = "leaf:" .. (node.path or "")
      else -- Count
        token = count_satisfier_token(node.count_kind, signal, waitpoint_id)
      end
      local added = redis.call("SADD", satisfied_set_key, token)
      if added == 1 then
        added_any = true
        -- Track the signal id for `all_satisfier_signals` emission.
        redis.call("SADD", satisfied_set_key .. ":signals", signal.signal_id)
      end
    end
  end

  if matcher_failed_all then
    return { effect = "signal_ignored_matcher_failed", resume = false }
  end
  if not added_any then
    return { effect = "appended_to_waitpoint_duplicate", resume = false }
  end

  -- Re-load + re-evaluate (cheap: depth ≤ 4).
  tokens = load_satisfied_set(satisfied_set_key)

  -- Propagate non-leaf child satisfaction upward: for each AllOf
  -- ancestor whose child is now satisfied, SADD `node:<child_path>`.
  -- Bounded by depth.
  local function propagate(node)
    if node == nil or node.kind ~= "AllOf" then return end
    for _, child in ipairs(node.members or {}) do
      if child.kind ~= "Single" then
        if composite_evaluate_node(child, tokens) then
          local added2 = redis.call("SADD", satisfied_set_key, "node:" .. (child.path or ""))
          if added2 == 1 then
            tokens["node:" .. (child.path or "")] = true
          end
          propagate(child)
        end
      end
    end
  end
  propagate(tree)

  local root_sat = composite_evaluate_node(tree, tokens)
  if root_sat then
    -- Gather satisfier signals (the ids we SADDed during this and prior
    -- deliveries).
    local sig_ids = redis.call("SMEMBERS", satisfied_set_key .. ":signals")
    return {
      effect = "resume_condition_satisfied",
      resume = true,
      closer = signal.signal_id,
      all_satisfiers_json = cjson.encode(sig_ids),
    }
  end
  return { effect = "appended_to_waitpoint", resume = false }
end

-- Clear composite keys on suspension termination (§3.1.1).
local function composite_cleanup(satisfied_set_key, member_map_key)
  redis.call("DEL", satisfied_set_key)
  redis.call("DEL", satisfied_set_key .. ":signals")
  redis.call("DEL", member_map_key)
end

-- RFC-014 Pattern 3 — close per-extra waitpoint storage on
-- cancel/expire/resume. The extras list is stored in
-- `suspension_current.additional_waitpoints_json` as a JSON array of
-- {waitpoint_id, waitpoint_key} pairs at suspend-time. Cleanup owners
-- reconstruct the wp_hash + wp_condition keys dynamically from the
-- suspension's hash tag (same trick ff_cancel_execution uses for lane
-- keys) so no KEYS arity growth is needed.
--
-- @param suspension_current_key  e.g. "ff:exec:{p:12}:E-...:suspension:current"
-- @param additional_json         value of HGET suspension_current additional_waitpoints_json
-- @param close_state_fields      table of HSET fields to apply to each extra waitpoint hash
--                                (e.g. {"state","closed","close_reason","cancelled","closed_at","<now>"})
-- @param close_cond_fields       table of HSET fields to apply to each extra wp_condition hash
local function close_additional_waitpoints(suspension_current_key, additional_json,
                                           close_state_fields, close_cond_fields)
  if not additional_json or additional_json == "" or additional_json == "[]" then
    return
  end
  local ok_dec, pairs_list = pcall(cjson.decode, additional_json)
  if not ok_dec or type(pairs_list) ~= "table" then return end
  local tag = string.match(suspension_current_key, "(%b{})")
  if not tag then return end
  for _, entry in ipairs(pairs_list) do
    local wp_id = entry.waitpoint_id
    if type(wp_id) == "string" and wp_id ~= "" then
      local wp_hash_key = "ff:wp:" .. tag .. ":" .. wp_id
      local wp_cond_key = "ff:wp:" .. tag .. ":" .. wp_id .. ":condition"
      if redis.call("EXISTS", wp_hash_key) == 1 and close_state_fields and #close_state_fields > 0 then
        redis.call("HSET", wp_hash_key, unpack(close_state_fields))
      end
      if redis.call("EXISTS", wp_cond_key) == 1 and close_cond_fields and #close_cond_fields > 0 then
        redis.call("HSET", wp_cond_key, unpack(close_cond_fields))
      end
    end
  end
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
