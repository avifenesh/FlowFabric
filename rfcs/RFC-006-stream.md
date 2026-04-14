# RFC-006: Stream Model

**Status:** Draft
**Author:** FlowFabric Team (Worker-3)
**Created:** 2026-04-14
**Pre-RFC Reference:** flowfabric_use_cases_and_primitives (2).md — Primitive 5, Finalization item 5

---

## Summary

A **stream** is an append-only ordered sequence of frames associated with one attempt. It is the partial-output channel that lets long-running work emit useful content before terminal completion. The true stream is attempt-scoped — replay, retry, and reclaim each produce their own stream, keeping output cleanly attributed. The execution API may expose a merged stream view across attempts for UX convenience. This RFC defines the stream and frame objects, invariants, lifecycle, durability modes, ownership rules, the merged view, and the Valkey data model using Valkey Streams (XADD/XREAD).

## Motivation

FlowFabric is built for long-running, interruptible work. Without partial output, callers and operators are blind until terminal completion — which may be minutes or hours away.

- **UC-36 (Single LLM call with streamed output):** Tokens arrive incrementally; the client must see them live.
- **UC-45 (Streaming artifacts, not just tokens):** Logs, tool events, structured progress, and intermediate results must flow alongside tokens.
- **UC-42 (Tool-calling agent step):** Tool invocations and results are stream events the operator needs to observe in real time.
- **UC-56 (Live output feed):** Clients subscribe to execution streams for live UX.
- **UC-57 (Snapshot and diagnosis):** Operators inspect what an attempt has produced so far to diagnose stalls or unexpected behavior.
- **UC-37 (Token / cost accounting):** Usage update frames provide incremental cost visibility before the attempt terminates.

Attempt-scoping is required because retry, reclaim, and replay produce distinct concrete runs (RFC-002). If the stream were execution-scoped, a failed attempt's partial output would mix with the successful retry's output, making debugging and cost attribution unreliable.

---

## Detailed Design

### Stream Object Definition

A stream is a durable or semi-durable ordered output channel associated with exactly one attempt. Each attempt may have at most one stream.

#### Stream Metadata

| Field | Type | Required | Description |
|---|---|---|---|
| `stream_id` | `String` | Yes | Unique identifier for this stream. Derived deterministically: `stream:{execution_id}:{attempt_index}`. |
| `execution_id` | `UUID` | Yes | Parent execution (RFC-001). |
| `attempt_id` | `String` | Yes | Parent attempt (RFC-002). For audit correlation. |
| `attempt_index` | `u32` | Yes | Attempt index within the execution (RFC-002). Primary binding key. |
| `created_at` | `i64` (ms epoch) | Yes | When the stream was created (on first append or eagerly at attempt start). |
| `last_sequence` | `String` | No | Valkey Stream ID of the most recently appended frame. `None` if no frames appended. |
| `frame_count` | `u64` | No | Total number of frames appended. Maintained for fast inspection. |
| `closed_at` | `i64` (ms epoch) | No | When the stream was closed (attempt reached terminal state). `None` if still open. |
| `closed_reason` | `String` | No | Why the stream was closed: `attempt_success`, `attempt_failure`, `attempt_cancelled`, `attempt_interrupted`, `retention_trim`. |
| `retention_policy` | `RetentionPolicy` | Yes | How long and how much stream data to keep. |
| `durability_mode` | `DurabilityMode` | Yes | Durability guarantee for this stream's frames. |

#### Stream Identity

The stream is identified by `(execution_id, attempt_index)`. This is the same key space used by RFC-002 for attempt records and RFC-003 for lease binding. The `stream_id` field is a convenience string; the composite key is authoritative.

### Frame Definition

A frame is one unit of output appended to a stream. Frames are immutable once appended.

| Field | Type | Required | Description |
|---|---|---|---|
| `sequence` | `String` | Yes | Valkey Stream entry ID (e.g., `1713100800150-0`). Provides total order and approximate timestamp. Assigned by Valkey on XADD. |
| `frame_type` | `FrameType` enum | Yes | Category of this frame (see Frame Types). |
| `timestamp` | `i64` (ms epoch) | Yes | Producer-supplied timestamp. May differ slightly from Valkey-assigned sequence timestamp due to clock skew. |
| `payload` | `Bytes` | Yes | Frame content. Interpretation depends on `frame_type` and `payload_encoding`. |
| `payload_encoding` | `String` | No | Encoding hint: `utf8`, `json`, `msgpack`, `binary`. Default: `utf8`. |
| `correlation_id` | `String` | No | Optional correlation identifier linking this frame to a tool call, agent step, or external request. |
| `source` | `String` | No | Who appended this frame: `worker` (default), `system`, `operator`. Used to distinguish control-plane annotations from worker output. |

### Frame Types

| Frame Type | Description | Typical Payload |
|---|---|---|
| `token` | Single token or small token batch from model inference. | Raw token text or token IDs. |
| `text_chunk` | Larger text fragment (sentence, paragraph). | UTF-8 text. |
| `log` | Structured log entry from the worker. | JSON log record. |
| `progress` | Progress update (percentage, step description). | JSON: `{"pct": 45, "message": "Processing batch 3/7"}` |
| `usage_update` | Incremental usage report (tokens consumed so far). | JSON: `{"input_tokens": 500, "output_tokens": 120}` |
| `artifact_reference` | Pointer to a produced artifact (file, image, document). | JSON: `{"artifact_id": "...", "type": "image/png", "size_bytes": 42000}` |
| `tool_event` | Tool invocation or result in an agent loop. | JSON: `{"tool": "web_search", "action": "invoke", "input": {...}}` |
| `structured_json` | Arbitrary structured output from the worker. | JSON document. |
| `warning` | Non-fatal warning from the worker or engine. | UTF-8 text or JSON. |
| `debug` | Debug-level output, typically filtered in production views. | UTF-8 text or JSON. |

Frame types are an open enum — new types may be added without breaking existing consumers. Consumers must ignore unknown frame types gracefully.

**Maximum frame payload size:** 64KB (65,536 bytes) per frame in v1. This is generous for tokens, structured events, and log entries. Large artifacts (images, documents, model weights) must use `artifact_reference` frames with a pointer to external storage, not inline payloads. The limit is enforced in the `ff_append_frame` function.

**Known v1 limitation — no frame-level idempotency:** `XADD` uses auto-generated entry IDs (`*`). If the client connection drops after a successful XADD but before the client receives the entry ID, a retry produces a duplicate frame. For token streaming, this means consumers may see duplicate tokens. This is not data loss but data duplication. Consumers should be resilient to occasional duplicates. Future mitigation: client-side sequence tracking where the client assigns a monotonic frame sequence number and the `append_frame` script rejects frames with sequence <= last seen.

### Invariants

#### Invariant T1 — Per-attempt order

Frames for one attempt must have a stable total order. The Valkey Stream entry ID provides this order. Two frames within the same stream are ordered by their entry IDs. This order is immutable once assigned.

**Rationale:** Consumers replay streams from offsets and expect deterministic ordering. Token streaming in particular requires strict order for coherent text reconstruction.

#### Invariant T2 — Append-only

Frames are appended, not mutated or deleted in place. Any redaction or truncation policy operates via explicit trim operations that remove frames from the head (oldest first) and are auditable via stream metadata changes.

**Rationale:** Append-only semantics simplify consumer logic, enable reliable offset-based replay, and prevent subtle bugs from in-place mutation.

#### Invariant T3 — Separate from terminal result

The stream carries intermediate output. The final result payload on the execution record (RFC-001 `result_payload`) is separate. A stream may contain thousands of token frames; the final result is a single consolidated value. Consumers must not assume the stream contains the final result or vice versa.

**Rationale:** Terminal result has different durability, visibility, and access patterns than streamed output. Conflating them makes retention policy, access control, and API design harder.

#### Invariant T4 — Live and replayable

Consumers can:
- Read from the beginning (full replay).
- Read from a specific offset (resume after disconnect).
- Tail the live stream (blocking read for new frames as they arrive).

All three access patterns must work according to the stream's retention policy. Once frames are trimmed, they are no longer readable.

**Rationale:** Live tailing is required for real-time UX (UC-56). Replay from offset is required for reconnection resilience and debugging (UC-57). Full replay is required for post-hoc analysis.

#### Invariant T5 — Multi-type frames

A single stream may contain frames of different types interleaved. A token frame may be followed by a tool_event frame, then more tokens, then a usage_update. Consumers filter by frame type as needed.

**Rationale:** Real-world AI execution produces heterogeneous output. A single-type stream would require multiple parallel streams per attempt, complicating the model without benefit.

### Stream Lifecycle

```
  ┌──────┐   first append   ┌────────┐   attempt ends   ┌────────┐
  │ none ├──────────────────►│  open  ├─────────────────►│ closed │
  └──────┘    (lazy create)  └───┬────┘                  └───┬────┘
                                 │                            │
                                 │  append frames             │  queryable but
                                 │  tail/subscribe            │  not appendable
                                 │  read from offset          │  read from offset
                                 │                            │  retain / trim
                                 ▼                            ▼
```

#### Open

A stream is created lazily on first `append_frame` call. There is no separate `open_attempt_stream` step required by the worker — the first append implicitly creates the stream metadata and Valkey Stream key.

The engine may also create streams eagerly at attempt start if the execution's `stream_policy` specifies eager creation. This is useful for pre-allocating consumer subscriptions before any frames arrive.

#### Append

The active lease owner appends frames in order. Each `append_frame` call adds one entry to the Valkey Stream via `XADD`. The Valkey-assigned entry ID becomes the frame's `sequence`.

Append is only valid while the stream is open (attempt not terminal).

**MAXLEN trimming is applied on every append, including during active streams.** If the stream's `retention_maxlen` is 10,000 and the worker has appended 10,001 frames, the oldest frames are silently trimmed. Workers and consumers must not assume frames beyond MAXLEN are available for re-reading. For agent loops that need to reference their own earlier output (e.g., for context or continuation), either increase MAXLEN to cover the expected output volume, or maintain a local copy of critical frames. MAXLEN is a memory guardrail applied continuously, not a post-close cleanup mechanism.

#### Tail / Subscribe

Consumers use `XREAD BLOCK` on the Valkey Stream to tail new frames as they arrive. Multiple consumers may tail the same stream concurrently. Consumers track their own read position (no consumer groups needed for this use case — each consumer maintains its own cursor).

#### Suspension

When the parent attempt transitions to `suspended` (RFC-002), the stream remains **open but not appendable**. No lease exists during suspension (RFC-003 invariant L5), so no worker can append frames. Consumers may still read existing frames and tail the stream (they will see no new frames until resume).

On resume, when a worker re-claims the execution and the same attempt transitions back to `started` with a new lease, the original stream accepts appends again under the new `lease_epoch`. The stream is not closed and reopened — it is the same continuous stream. This preserves frame ordering across the suspension gap.

The stream metadata is not modified during suspension. The gap in frame timestamps between the last pre-suspension frame and the first post-resume frame is the natural record of the suspension duration.

#### Close

The stream closes automatically when the parent attempt reaches a terminal state (`ended_success`, `ended_failure`, `ended_cancelled`, `interrupted_reclaimed`). Closing sets `closed_at` and `closed_reason` on the stream metadata atomically with the attempt state transition (within RFC-002's `ff_complete_execution` / `ff_fail_execution` or `ff_reclaim_execution` functions). After close, no new frames may be appended (except by privileged system context for annotations).

Consumers tailing a closed stream receive the remaining unread frames and then see the stream end.

#### Retain / Trim

Retention policy determines how long stream data persists after the stream closes:

- **MAXLEN-based trim:** Valkey Stream `XTRIM MAXLEN` caps the number of frames retained. Applied on each append or periodically.
- **Time-based trim:** A background scanner removes streams (entire keys) when they exceed the retention window.
- **Size-based trim:** If total payload size exceeds a threshold, oldest frames are trimmed.

Trimmed frames are gone — there is no soft-delete or archive in v1.

### Execution-Level Merged Stream View

The execution API exposes a merged stream view that aggregates frames across all attempts for one execution.

**Semantics:**
- Frames are grouped by attempt, ordered by `attempt_index` ascending.
- Within each attempt group, frames are ordered by their `sequence` (Valkey entry ID).
- Each frame in the merged view carries `attempt_index` attribution so consumers know which attempt produced it.
- If the consumer specifies a cursor, the cursor encodes both `attempt_index` and `sequence` to enable resume.

**Merge modes (v1):**

| Mode | Description |
|---|---|
| `all_attempts` | Include frames from all attempts, in attempt order. Default. |
| `latest_attempt_only` | Include frames only from the current/latest attempt. Useful for simple "watch this execution" UX where prior failed attempts are noise. |

**Implementation:** The merged view is computed client-side or by a server-side function that iterates attempt streams in order. It is a convenience surface, not stored separately.

### Durability Modes

Stream durability is a policy decision, not a law of nature. Different execution classes have different durability needs.

| Mode | Description | Use Case |
|---|---|---|
| `durable_full` | All frames are persisted to Valkey with AOF/RDB durability. Full replay available until retention trim. | Production AI inference where output must be auditable. |
| `durable_summary` | Only summary frames (progress, usage_update, warning) are durably retained. Token-level frames may be trimmed aggressively or not persisted at all. | High-throughput batch inference where token-by-token replay is not needed. |
| `best_effort_live` | Frames are available for live tailing but may not survive Valkey restart. Minimal retention after stream close. | Development, debugging, or low-value background work. |

**V1 default:** `durable_full`. This is the safest default for a v1 product. Operators can opt into lighter modes per lane or execution class.

**Policy resolution:** Stream durability is set from the execution's `stream_policy` field (RFC-001 ExecutionPolicySnapshot), which inherits from lane defaults and may be overridden at submission time.

### Ownership and Append Rules

#### Normal append path

Appending a frame requires a valid active lease for the parent attempt. The engine validates:

1. The attempt is in `started` state.
2. The caller provides `execution_id`, `attempt_index`, `lease_id`, and `lease_epoch`.
3. The `lease_epoch` matches the current lease on the execution core record.
4. The stream is not closed.

If validation fails, the append is rejected. This prevents stale workers (whose lease expired and was reclaimed) from continuing to emit output into a stream that now belongs to a new attempt.

**Atomicity class:** B (durable append). Append does not need to be atomic with major lifecycle transitions, but the lease validation must be checked before the XADD.

#### Privileged append path

Control-plane or operator annotations may append frames outside normal worker ownership. These frames must carry `source = "system"` or `source = "operator"` to distinguish them from worker-produced output.

Use cases:
- Engine appends a `warning` frame when a lease is about to expire.
- Operator appends a `debug` annotation explaining why they intervened.
- System appends a `progress` frame summarizing post-reclaim state.

Privileged appends do not require a lease but do require system-level authorization. They are not part of the normal worker API.

#### Post-close append

After the stream is closed (attempt terminal), no further appends are allowed — even from privileged context. The only exception is a narrow window for the engine to append a final `usage_update` or `warning` frame as part of the atomic attempt-end transition. This is handled within the `ff_complete_execution` / `ff_fail_execution` functions (RFC-002), not as a separate append.

### Relationship to Engine Events

Stream frames and engine lifecycle events are distinct:

| Concern | Stream Frames | Engine Events |
|---|---|---|
| **What** | Execution-produced content: tokens, logs, tool events, progress, artifacts. | System facts: state transitions, lease operations, scheduling decisions, control actions. |
| **Who produces** | Worker (via append_frame) or system annotations. | Engine (automatically on every state change). |
| **Where stored** | Valkey Stream per attempt. | Lease history stream (RFC-003), execution event log (RFC-001). |
| **Retention** | Stream retention policy (may be aggressive). | Audit retention policy (typically longer). |
| **Consumers** | Clients, dashboards, live UX. | Operators, audit systems, debugging tools. |

Mixing them in one stream would force consumers to filter constantly, complicate retention (audit events need longer retention than token frames), and blur the boundary between "what the execution produced" and "what the engine did."

### Operations

#### append_frame

Appends a frame to the current attempt's stream.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `lease_id: String`
- `lease_epoch: u64`
- `frame: Frame` — frame_type, timestamp, payload, payload_encoding, correlation_id

**Semantics:**
- Validates lease ownership (lease_id + lease_epoch match execution core).
- If stream does not exist, creates it lazily (stream metadata + Valkey Stream key).
- Appends frame via XADD. Valkey assigns the sequence.
- If MAXLEN retention is configured, applies `XTRIM MAXLEN ~{limit}` (approximate trim for performance).
- Updates stream metadata: `last_sequence`, `frame_count`.
- **Atomicity class: B** (durable append with lease pre-check).

**Errors:**
- `stale_owner_cannot_append` — lease_id or lease_epoch mismatch.
- `stream_closed` — attempt is terminal; no more appends.
- `invalid_frame_type` — unrecognized frame type (warning, not hard error if open enum).
- `retention_limit_exceeded` — payload exceeds maximum frame size.

#### append_system_frame

Appends a frame from privileged system context (no lease required).

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `system_auth: SystemAuth` — proof of privileged access
- `frame: Frame` — must have `source = "system"` or `source = "operator"`

**Semantics:**
- Validates system authorization.
- Validates stream is open.
- Appends frame via XADD with source marker.
- **Atomicity class: B**

**Errors:**
- `stream_closed`
- `unauthorized`

#### read_attempt_stream

Reads frames from a specific attempt's stream.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `from_sequence: Option<String>` — exclusive start position. `None` = read from beginning.
- `count: Option<u64>` — max frames to return. Default: 100.
- `frame_type_filter: Option<Vec<FrameType>>` — return only matching types.

**Semantics:**
- Uses `XRANGE` on the Valkey Stream.
- If `from_sequence` is provided, reads entries after that sequence (exclusive).
- Applies frame_type_filter client-side (Valkey Streams do not support field-level filtering natively).
- **Atomicity class: C** (read-only).

**Returns:** `Vec<Frame>` with sequence IDs, plus a `has_more: bool` flag.

**Errors:**
- `stream_not_found` — no stream exists for this attempt (no frames were ever appended).
- `invalid_offset` — `from_sequence` is not a valid Valkey Stream ID.

#### tail_attempt_stream

Blocking read for new frames on a live stream.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `last_sequence: Option<String>` — read frames after this position. `None` = start from now.
- `block_ms: u64` — how long to block waiting for new frames. `0` = non-blocking.
- `count: Option<u64>` — max frames per batch.

**Semantics:**
- Uses `XREAD BLOCK` on the Valkey Stream.
- Returns new frames as they arrive, up to `count`.
- If the stream closes while blocking, returns available frames and signals stream end.
- **Atomicity class: C** (read-only, blocking).

**Returns:** `Vec<Frame>`, `stream_ended: bool`.

**Implementation guidance — stream close detection:** Valkey's XREAD BLOCK does not natively signal "stream closed." The implementation must use a poll-and-check pattern:
1. `XREAD BLOCK <short_timeout_ms>` (recommended: 1-5 seconds) on the stream key.
2. If new frames arrive: return them with `stream_ended = false`.
3. If XREAD times out (no new frames): check `ff:stream:{p:N}:{eid}:{idx}:meta` for `closed_at`.
4. If `closed_at` is set: perform a final `XRANGE` from `last_sequence` to `+` to drain any remaining unread frames. Return them with `stream_ended = true`.
5. If `closed_at` is not set: the stream is still open but idle. Return empty with `stream_ended = false`. The consumer loops back to step 1.

Frames between the consumer's last read and stream closure are never lost — they remain in the Valkey Stream until read or trimmed by MAXLEN. A consumer that is behind (e.g., 500 frames behind head) naturally drains the backlog via successive XREAD calls before discovering the closure at the head.

**Errors:**
- `stream_not_found`

#### read_execution_stream

Reads the merged stream view across all attempts for one execution.

**Parameters:**
- `execution_id: UUID`
- `merge_mode: MergeMode` — `all_attempts` or `latest_attempt_only`.
- `cursor: Option<MergeCursor>` — resume position encoding `(attempt_index, sequence)`.
- `count: Option<u64>` — max frames to return across all attempts.
- `frame_type_filter: Option<Vec<FrameType>>`

**Semantics:**
- Reads the attempt index sorted set (RFC-002) to discover all attempts.
- For each attempt (in attempt_index order), reads from the attempt's Valkey Stream.
- If `merge_mode = latest_attempt_only`, reads only the latest attempt's stream.
- Annotates each frame with `attempt_index` in the response.
- **Atomicity class: C** (read-only; eventual consistency is acceptable).

**Returns:** `Vec<(attempt_index, Frame)>`, `cursor: Option<MergeCursor>`, `has_more: bool`.

**Errors:**
- `execution_not_found`

#### close_attempt_stream

Closes a stream, marking it as no longer appendable.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`
- `reason: String` — why the stream is closing.

**Semantics:**
- Sets `closed_at` and `closed_reason` on stream metadata.
- Called automatically by the engine when the attempt reaches a terminal state (within the `ff_complete_execution` / `ff_fail_execution` or `ff_reclaim_execution` functions from RFC-002).
- Not normally called by workers directly.
- **Atomicity class: A** (atomic with attempt state transition).

**Errors:**
- `stream_not_found`
- `stream_already_closed`

#### get_stream_info

Returns stream metadata without reading frames.

**Parameters:**
- `execution_id: UUID`
- `attempt_index: u32`

**Semantics:**
- Returns stream metadata: created_at, last_sequence, frame_count, closed_at, closed_reason, retention_policy, durability_mode.
- If no stream exists (no frames ever appended), returns a sentinel indicating no stream.
- **Atomicity class: C**

**Returns:** `StreamInfo` or `stream_not_found`.

### Valkey Data Model

#### Partitioning

All stream keys use the same `{p:N}` partition hash tag as RFC-001/RFC-002/RFC-003. This collocates the stream with its parent execution, attempt, and lease records on the same Valkey Cluster shard.

#### Key Schema

**Stream data (Valkey Stream per attempt):**
```
ff:stream:{p:N}:{execution_id}:{attempt_index}  →  STREAM
  Each entry (via XADD):
    frame_type       → token
    ts               → 1713100800150
    payload          → "Hello,"
    encoding         → utf8
    correlation_id   → (empty)
    source           → worker
```

Valkey Streams provide:
- Automatic entry ID assignment (millisecond timestamp + sequence number) → total order.
- `XADD` for append, `XRANGE` for range reads, `XREAD BLOCK` for live tailing.
- `XTRIM MAXLEN` for retention-based trimming.
- `XLEN` for frame count.
- `XINFO STREAM` for metadata (first/last entry, length).

**Stream metadata (hash per attempt):**
```
ff:stream:{p:N}:{execution_id}:{attempt_index}:meta  →  HASH
  stream_id         → stream:exec_01HYX:0
  execution_id      → exec_01HYX...
  attempt_id        → att_01HYX...
  attempt_index     → 0
  created_at        → 1713100800000
  closed_at         → (empty)
  closed_reason     → (empty)
  durability_mode   → durable_full
  retention_maxlen  → 10000
  retention_ttl_ms  → 86400000
```

The metadata hash is created atomically with the first frame append. It is separate from the Valkey Stream itself because Valkey Streams do not support arbitrary metadata fields.

#### Append Operation (Valkey Function)

`ff_append_frame` — registered in the `flowfabric` library. Validates lease and appends atomically. Invoked via `FCALL ff_append_frame <numkeys> <keys...> <args...>`.

```lua
-- KEYS (all share {p:N} hash tag):
--   [1] exec_core  = ff:exec:{p:N}:{execution_id}:core
--   [2] stream_key = ff:stream:{p:N}:{execution_id}:{attempt_index}
--   [3] stream_meta = ff:stream:{p:N}:{execution_id}:{attempt_index}:meta
-- ARGV:
--   [1] attempt_index
--   [2] lease_id
--   [3] lease_epoch
--   [4] frame_type
--   [5] ts (producer timestamp, ms)
--   [6] payload
--   [7] encoding
--   [8] correlation_id
--   [9] source (worker|system|operator)
--   [10] retention_maxlen (0 = no trim)
--   [11] execution_id (for lazy metadata creation)
--   [12] durability_mode (for lazy metadata creation)
--   [13] max_payload_bytes (64KB default)
--   [14] attempt_id (for lazy metadata creation)

local now = redis.call("TIME")
local now_ms = now[1] * 1000 + math.floor(now[2] / 1000)

-- Validate payload size (v1 default: 65536 bytes)
if #ARGV[6] > tonumber(ARGV[13]) then
  return redis.error_reply("RETENTION_LIMIT_EXCEEDED")
end

local core = redis.call("HMGET", KEYS[1],
  "current_attempt_index", "current_lease_id",
  "current_lease_epoch", "lease_expires_at",
  "lifecycle_phase", "ownership_state")

-- Validate lease
if core[5] ~= "active" then
  return redis.error_reply("STREAM_CLOSED")
end
if core[6] == "lease_expired_reclaimable" or core[6] == "lease_revoked" then
  return redis.error_reply("STALE_OWNER_CANNOT_APPEND")
end
if tonumber(core[4]) <= now_ms then
  return redis.error_reply("STALE_OWNER_CANNOT_APPEND")
end
if tostring(core[1]) ~= ARGV[1] then
  return redis.error_reply("STALE_OWNER_CANNOT_APPEND")
end
if core[2] ~= ARGV[2] or tostring(core[3]) ~= ARGV[3] then
  return redis.error_reply("STALE_OWNER_CANNOT_APPEND")
end

-- Lazy-create stream metadata if needed
if redis.call("EXISTS", KEYS[3]) == 0 then
  redis.call("HSET", KEYS[3],
    "stream_id", ARGV[11] .. ":" .. ARGV[1],
    "execution_id", ARGV[11],
    "attempt_id", ARGV[14],
    "attempt_index", ARGV[1],
    "created_at", now_ms,
    "closed_at", "",
    "closed_reason", "",
    "durability_mode", ARGV[12],
    "retention_maxlen", ARGV[10],
    "last_sequence", "",
    "frame_count", 0)
end

-- Check stream not closed
local closed = redis.call("HGET", KEYS[3], "closed_at")
if closed ~= nil and closed ~= "" and closed ~= false then
  return redis.error_reply("STREAM_CLOSED")
end

-- Append frame
local entry_id = redis.call("XADD", KEYS[2], "*",
  "frame_type", ARGV[4],
  "ts", ARGV[5],
  "payload", ARGV[6],
  "encoding", ARGV[7],
  "correlation_id", ARGV[8],
  "source", ARGV[9])

-- Update stream metadata atomically with append
redis.call("HSET", KEYS[3],
  "last_sequence", entry_id,
  "frame_count", redis.call("XLEN", KEYS[2]))

-- Apply retention trim (approximate for performance)
local maxlen = tonumber(ARGV[10])
if maxlen > 0 then
  redis.call("XTRIM", KEYS[2], "MAXLEN", "~", maxlen)
end

return entry_id
```

#### Close Operation

Stream close is inline within the attempt-end functions (`ff_complete_execution`, `ff_fail_execution`, `ff_reclaim_execution`). Within those functions, after setting the attempt to a terminal state:

```lua
-- Close stream metadata
local stream_meta_key = KEYS.stream_meta
if redis.call("EXISTS", stream_meta_key) == 1 then
  redis.call("HSET", stream_meta_key,
    "closed_at", now_ms,
    "closed_reason", ARGV.close_reason)
end
```

This ensures stream close is atomic with attempt termination — no window where a stale worker could append to a closed stream.

#### Retention and Cleanup

**Per-stream MAXLEN:** Applied on each append via `XTRIM MAXLEN ~{limit}`. The `~` (approximate) flag allows Valkey to batch trim operations for better performance.

**Time-based cleanup:** A background scanner checks `ff:stream:{p:N}:{execution_id}:{attempt_index}:meta` for streams where `closed_at + retention_ttl_ms < now`. Expired streams are deleted entirely:
- `DEL ff:stream:{p:N}:{execution_id}:{attempt_index}`
- `DEL ff:stream:{p:N}:{execution_id}:{attempt_index}:meta`

**Execution-level cleanup:** When an execution is purged (RFC-002 retention), all its streams are purged. The cleanup process iterates the attempt sorted set and deletes each attempt's stream keys.

#### Memory Considerations

Valkey Streams use a radix tree + listpack encoding that is memory-efficient for sequential appends. However, high-throughput token streaming (thousands of frames per second) can accumulate significant memory.

Mitigations:
- `MAXLEN` caps per stream (configurable per lane/execution class).
- `durable_summary` mode aggressively trims token-level frames.
- `best_effort_live` mode uses short TTL on stream keys.
- Large artifacts should be stored externally and referenced via `artifact_reference` frames, not inlined.

### Error Model

| Error | When |
|---|---|
| `stream_not_found` | No stream exists for this attempt (no frames ever appended). |
| `stream_closed` | Attempted to append to a stream whose parent attempt is terminal. |
| `stream_already_closed` | Attempted to close a stream that is already closed. |
| `stale_owner_cannot_append` | Lease validation failed: lease_id/lease_epoch mismatch, lease expired, or attempt_index mismatch. Stale worker. |
| `invalid_frame_type` | Frame type is not recognized. Soft error if open enum — may log and accept. |
| `invalid_offset` | Provided sequence ID is not a valid Valkey Stream entry ID. |
| `retention_limit_exceeded` | Single frame payload exceeds maximum allowed size (protects against abuse). |
| `execution_not_found` | Parent execution does not exist (for merged view). |
| `unauthorized` | System/operator append without valid system authorization. |

---

## Interactions with Other Primitives

### RFC-001 — Execution

- The execution's `stream_policy` field (in ExecutionPolicySnapshot) controls stream durability mode and retention parameters.
- The execution summary may include stream presence indicators (e.g., `has_stream: bool`, `stream_frame_count: u64`) for dashboard display.
- The merged execution stream view reads from the execution's attempt index to discover attempt streams.
- Stream frames are execution-produced content; engine lifecycle events are separate (execution event log).

### RFC-002 — Attempt

- Streams are attempt-scoped. The primary key includes `attempt_index`.
- Stream creation is lazy on first append during an active attempt.
- Stream close is atomic with attempt termination (within RFC-002 functions).
- The attempt record may cache `has_stream: bool` for fast inspection.

### RFC-003 — Lease

- `append_frame` validates the caller's `lease_id` and `lease_epoch` against the execution core record before allowing the append.
- Lease expiry or revocation makes subsequent appends fail with `stale_owner_cannot_append`.
- Lease renewal does not affect stream state — the stream remains open as long as the attempt is active.

---

## V1 Scope

### V1 must-have

- Attempt-scoped Valkey Streams with ordered frame append.
- Lazy stream creation on first append.
- Lease-validated append (lease_id + lease_epoch).
- All 10 frame types.
- `read_attempt_stream` with offset and count.
- `tail_attempt_stream` with XREAD BLOCK.
- `read_execution_stream` with `all_attempts` and `latest_attempt_only` merge modes.
- `get_stream_info` for metadata inspection.
- Stream close atomic with attempt termination.
- MAXLEN-based retention per stream.
- `durable_full` as default durability mode.
- `ff_append_frame` function with lease validation.
- Privileged system/operator append path with source marker.
- Separation from engine lifecycle events.

### Designed-for but deferred

- `durable_summary` and `best_effort_live` durability modes (v1 ships with `durable_full` only; lighter modes require frame-type-aware trim logic).
- Consumer groups for multi-consumer coordination (v1 uses independent cursors).
- Server-side frame_type filtering (v1 filters client-side).
- Stream compression or deduplication.
- Cross-execution stream search (find all streams containing a specific correlation_id).
- Stream archival to external storage (S3, etc.) before Valkey-side deletion.
- Frame-level access control (v1 inherits from execution visibility).

---

## Open Questions

1. **Merged view pagination:** Should the merged cursor be opaque (server chooses encoding) or structured (client can construct `(attempt_index, sequence)` tuples)? Opaque is simpler; structured enables more flexible client-side navigation.

**Resolved:** Eager vs. lazy stream creation — both modes supported. Default is lazy (on first append). Eager creation is opt-in via `stream_policy.eager_create = true` on the execution or lane config. Eager mode creates the stream key and metadata hash at attempt start, before any frames are appended. This allows clients to start `XREAD BLOCK` immediately without polling for stream existence — critical for real-time token streaming UX.

---

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md — Primitive 5, Finalization item 5
- RFC-001: Execution Object and State Model (worker-2)
- RFC-002: Attempt Model and Execution Lineage (worker-3)
- RFC-003: Lease and Fencing Semantics (worker-1)
