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

Appends a frame to the current attempt's stream. This is the highest-throughput function — called once per token during LLM streaming. Called by `ff-sdk` directly via `ff-script` (`FCALL ff_append_frame`). Single-partition `{p:N}` — no `ff-engine` orchestration needed.

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
- **Atomicity class: B** (durable append with lease pre-check). Single `FCALL`, no cross-partition calls.

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

Reads frames from a specific attempt's stream. Called by `ff-sdk` via direct `XRANGE` (no Valkey Function needed — native Valkey command).

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

**Time-based cleanup:** The stream retention trimmer (`ff-engine::scanner`, RFC-010 §6.8) checks `ff:stream:{p:N}:{execution_id}:{attempt_index}:meta` for streams where `closed_at + retention_ttl_ms < now`. Expired streams are deleted entirely:
- `DEL ff:stream:{p:N}:{execution_id}:{attempt_index}`
- `DEL ff:stream:{p:N}:{execution_id}:{attempt_index}:meta`

**Execution-level cleanup:** When an execution is purged by the terminal retention scanner (`ff-engine::scanner`, RFC-010 §6.12), all its streams are purged as part of the cleanup cascade (§9.3 step 4).

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

## Implementation Notes (Batch B — RFC-006 #2, 2026-04-17)

Read-side primitives landed on `feat/batch-b` to unblock cairn checkpoint
recovery. Two endpoints, two different transports; the split is forced by
Valkey's rule that blocking commands cannot run inside Functions.

**`ff_read_attempt_stream` (XRANGE, inside a Lua Function)**
- `XRANGE` is non-blocking and therefore safe in `redis.register_function`.
- KEYS(2): `stream_data`, `stream_meta` (both share the `{p:N}` hash tag).
- ARGV(3): `from_id`, `to_id`, `count_limit`.
- `count_limit` MUST be in `1..=STREAM_READ_HARD_CAP` (10_000); `0`
  returns `invalid_input` rather than silently reading the entire stream.
- Returns an empty array when the stream key does not exist — a
  never-written attempt is a legitimate state, not an error.
- Also returns `closed_at` and `closed_reason` (read via `HMGET` on
  `stream_meta`) so consumers get the **terminal signal** alongside the
  frames in the same FCALL. A never-written attempt yields empty strings
  for both fields, which the Rust layer maps to `None`/`None`
  (indistinguishable from "still open" at this layer, which is the
  intended semantics).

**`xread_block` (XREAD / XREAD BLOCK, direct client command)**
- Blocking inside a Function is rejected by Valkey, so this lives in
  `crates/ff-script/src/stream_tail.rs` as a plain async helper that
  invokes the client directly.
- `block_ms == 0` → plain `XREAD` (no BLOCK). `XREAD BLOCK 0` blocks
  *indefinitely*, which is never what a web handler wants.
- `block_ms > 0` → `XREAD COUNT <limit> BLOCK <ms> STREAMS <key> <last_id>`.
  Timeout returns `Value::Nil`, which we normalize to `Vec::new()`.
- The REST layer rejects `block_ms > 30_000ms` with HTTP 400. Ceiling
  chosen to sit below common LB idle timeouts (ALB 60s, nginx 60s,
  Cloudflare 100s) so the response can't be cut mid-block.
- After the XREAD settles we fire one follow-up `HMGET stream_meta
  closed_at closed_reason` to attach the terminal signal to the return.
  This is cheap (single RTT, same `{p:N}` slot) and avoids consumers
  having to poll a separate endpoint to learn whether the stream is
  closed. The follow-up HMGET is ALWAYS performed, even on timeout —
  that's the common case where the signal matters most.

**Routing by `(execution_id, attempt_index)` — not `attempt_id`**

The stream key is `ff:stream:{p:N}:<eid>:<idx>`. There is no
`attempt_id → (eid, idx)` reverse index, and the task consumer (cairn)
already has both values at checkpoint time. We route by
`(execution_id, attempt_index)` instead of building a new index.
`attempt_id` remains an audit field inside each frame's write args, not a
routing key. REST paths reflect this:

- `GET /v1/executions/{eid}/attempts/{idx}/stream?from=-&to=+&limit=100`
- `GET /v1/executions/{eid}/attempts/{idx}/stream/tail?after=<id>&block_ms=5000&limit=50`

**REST transport: long-poll in v1**

Tail is exposed as long-poll, not SSE. Rationale:
- Simpler: no SSE framing, works through any HTTP stack, no client state
  beyond "remember the last id I saw".
- Bounded: `block_ms <= 30s` cap means a single request never outlives a
  typical LB idle timeout.
- SSE can be added later without changing the server-side shape — the
  Server method `tail_attempt_stream` already returns `Vec<StreamFrame>`
  suitable for either transport.

**Transport timeout interaction (ferriskey)**

The ferriskey client's default `request_timeout` (5s on ff-server) does
NOT apply to blocking XREAD calls. `ferriskey::client::get_request_timeout`
inspects outgoing commands; for `XREAD`/`XREADGROUP` with a `BLOCK`
argument it returns `BlockingCommand(block_ms + 500ms)` via the
`BLOCKING_CMD_TIMEOUT_EXTENSION` constant (0.5s), overriding the client's
default for that single command.

Practical consequence for **timeouts**: the REST `block_ms` ceiling of
30s is the only knob that bounds tail latency from a transport
perspective. The dedicated tail client (§"Dedicated tail connection"
below) exists for a DIFFERENT reason — head-of-line isolation — not
for request_timeout tuning. Two different concerns, two different
fixes; don't conflate. The regression test
`test_tail_stream_long_block_respects_ferriskey_timeout_extension`
blocks for 7s (above the 5s default) to pin the timeout-extension
behavior.

**Input validation: both bounds reject, neither silently clamps**

Both REST handlers reject out-of-range inputs with HTTP 400 rather than
silently clamping:
- `block_ms > MAX_TAIL_BLOCK_MS (30_000)` → 400.
- `limit == 0` → 400 (`"limit must be >= 1"`).
- `limit > REST_STREAM_LIMIT_CEILING (1_000)` → 400. Lower than the
  internal `STREAM_READ_HARD_CAP (10_000)` because the REST layer
  buffers the full JSON body in axum — a max-payload × max-limit
  response is multi-hundred-MB per call and a clear DoS vector from a
  single client. Internal callers via FCALL or the SDK directly retain
  the 10_000 bound; REST clients paginate through `from`/`to`/`after`
  for larger spans. Chunked-transfer / SSE deferred to v2.
- Malformed `from` / `to` / `after` (anything outside `"-"`, `"+"`, or
  `<ms>[-<seq>]`) → 400 at the REST boundary, never forwarded to Valkey.

Explicit 400 responses surface client misconfiguration; silent clamps
mask it. `STREAM_READ_HARD_CAP` is a single source of truth in
`ff_core::contracts` and is mirrored in the Lua-side `HARD_CAP` literal
in `lua/stream.lua`.

**Dedicated stream-op connection (head-of-line isolation)**

`Server::tail_attempt_stream` AND `Server::read_attempt_stream` both run
on `Server.tail_client` — a dedicated `ferriskey::Client` built alongside
the main client in `Server::start`. The main `Server.client` and the
`Engine`'s scanner client are unaffected by stream-op latency.

The split is about **head-of-line isolation**, not request_timeout
(which ferriskey already handles; see §Transport timeout interaction
above). Two separate classes of stream-op load would otherwise starve
the main mux:

- **Blocking**: `XREAD BLOCK 30_000` holds the socket read side for up
  to 30 seconds.
- **Large reads**: an XRANGE with `COUNT=STREAM_READ_HARD_CAP (10_000)`
  returning ~64 KB-per-frame payloads is a multi-MB reply serialized
  on one TCP socket.

Either load on a shared client would stall every concurrent FCALL
(claim, create, rotate-waitpoint-secret, budget/quota, admin) AND every
engine scanner for the duration. Splitting both reads and tails out
onto the dedicated `tail_client` makes foreground API and
background-scanner cadence independent of stream-op latency.

**Tail serialization on the dedicated client (v1)**

The dedicated `tail_client` is still one multiplexed TCP connection:
Valkey processes commands FIFO on it, and ferriskey's per-call
`request_timeout` starts on the client side at future-poll. Two
`XREAD BLOCK` calls pipelined down one mux therefore DO NOT run
concurrently at the server — the second waits for the first to return
— and the second call's `request_timeout` (auto-extended to
`block_ms + 500ms`) expires BEFORE it ever reaches the server, producing
spurious `timed_out` errors under concurrent tail load.

V1 ships an explicit `tokio::sync::Mutex` (`Server.xread_block_lock`)
that serializes `xread_block` calls against the tail client.
Acquisition order is **semaphore permit first, Mutex second**. The
`max_concurrent_stream_ops` ceiling becomes the queue depth; throughput
on the tail client is 1 BLOCK at a time, but each call gets its full
`block_ms` budget at Valkey instead of racing client-side timeouts.

XRANGE reads (`read_attempt_stream`) are NOT gated by the Mutex. XRANGE
is non-blocking at the server — pipelined XRANGEs on one mux complete
in microseconds each — so they don't trigger the same client-side
timeout race. Keeping reads unserialized preserves read throughput.

**V2 upgrade path**: replace the single `tail_client` + Mutex pair with
a pool of N dedicated `ferriskey::Client` connections. Each tail gets
its own socket; BLOCKs run truly in parallel. Deferred from Batch B
because it requires Server-state changes (pool lifecycle, shutdown
draining of all N clients) that are scope-creep relative to the Mutex
fix. The `xread_block_lock` is labeled with a pointer to this upgrade
so the v2 removal is mechanical.

**Backpressure on concurrent stream ops (`FF_MAX_CONCURRENT_STREAM_OPS`)**

`Server.stream_semaphore` bounds concurrent stream-op calls — reads AND
tails combined (default 64). Read and tail share the same pool because
they share the same `tail_client`; a flood of one can starve the other
unless fairness accounting is unified.

Both `Server::read_attempt_stream` and `Server::tail_attempt_stream`
acquire a permit via `try_acquire_owned` before their Valkey command;
when the pool is exhausted, callers receive
`ServerError::TailUnavailable` which the REST layer surfaces as
**HTTP 429 Too Many Requests** (`retryable=true` in the error body).
Non-blocking acquisition is deliberate: queueing would hold the caller's
HTTP request open with no upper bound, which is exactly the exhaustion
pattern the limit exists to prevent.

Env var precedence: `FF_MAX_CONCURRENT_STREAM_OPS` is the preferred
name; the older `FF_MAX_CONCURRENT_TAIL` is accepted for one release to
keep existing deployments working while they update their config. If
both are set, the new name wins.

Operators tune this based on the server's overall request-concurrency
budget. A single multiplexed tail connection handles arbitrary
pipelining, so the ceiling is about fairness — how many stream ops can
be in flight before new ones must back off — not about
connection-count.

**Permit-hold latency (closed via HSET while tail is blocking)**

`XREAD BLOCK` wakes on `XADD` to the stream, not on `HSET` to
`stream_meta`. When a producer closes a stream via one of the non-XADD
terminal paths — `ff_cancel_execution` / `ff_complete_execution` /
`ff_fail_execution` / `mark_expired` / reclaim, all of which HSET
`closed_at` without appending a frame — any in-flight `tail_stream`
will NOT wake early. It continues to hold its `stream_semaphore`
permit until its configured `block_ms` expires OR a subsequent `XADD`
happens to land (the close paths typically don't append further).

Worst case: a tail with `block_ms=30_000` holds its permit for the
full 30s even though the terminal signal is already on `stream_meta`.
Under load this shrinks effective stream-op capacity for up to
`block_ms` after a close cascade.

V1 accepts this limitation. Operators who see stream-op 429s during
cancellation storms can:
- Tune `FF_MAX_CONCURRENT_STREAM_OPS` up (each permit is cheap — one
  TCP-pipeline slot).
- Recommend callers use smaller `block_ms` values (3–5s) and rely on
  tail loops for responsiveness.

**V2 upgrade path**: have the close paths emit a sentinel
`XADD stream_key * frame_type stream_closed closed_reason=<reason>`
entry. XREAD BLOCK wakes on the sentinel, consumers see a terminal
frame (different from a normal frame via `frame_type`) and exit the
loop immediately. Releases the permit as soon as the close happens,
turning worst-case permit hold into ~RTT. Not done in v1 because it
couples the close paths (execution.lua, signal.lua, reclaim) to the
stream in a way that requires coordinated edits across 8+ Lua
functions.

**Terminal signal contract (closed_at / closed_reason)**

Every read/tail call returns a `StreamFrames { frames, closed_at,
closed_reason }` struct. `closed_at` is `Some` iff the writer has
finalized the stream via one of the close paths below; `closed_reason`
names the specific reason.

**`closed_reason` enumeration (stable, public contract)**

The full set of values `stream_meta.closed_reason` may take. New
entries to this list are a breaking change to consumers and require a
LIBRARY_VERSION bump:

| value | write path | notes |
|---|---|---|
| `attempt_success` | `ff_complete_execution` (lua/execution.lua) | Worker called complete; most common case. |
| `attempt_failure` | `ff_fail_execution` → terminal branch | Retry budget exhausted or unrecoverable fail. |
| `attempt_cancelled` | `ff_cancel_execution` | External cancel while attempt active. |
| `attempt_interrupted` | `ff_fail_execution` → retry-scheduled branch | Will retry; new attempt opens a new stream. |
| `reclaimed` | reclaim path (lua/execution.lua reclaim branches) | Lease reclaimed by a different worker; stream of the reclaimed attempt is closed. |
| `lease_expired` | `mark_expired` (lua/helpers.lua) | Worker died / lease expired without reclaim yet; surfaces the terminal signal so consumers don't wait for reclaim. Overwritten by `reclaimed` if reclaim runs later. |
| `timed_out_auto_resume` | suspension auto-resume timeout path | Attempt's auto-resume suspension timed out. |
| `timed_out_fail` | suspension fail-on-timeout path | Attempt's fail-on-timeout suspension tripped. |
| `retention_trim` | retention trimmer scanner | Entire stream trimmed under a hard MAXLEN; very rare — normally XTRIM prunes entries, not closes streams. Reserved. |

A never-written stream and an in-progress stream both present as
`closed_at=None`; distinguishing them requires reading `exec_core`
separately (out of scope here).

Consumer polling pattern:
```rust
let mut last_id = "0-0".to_string();
loop {
    let page = ff_sdk::tail_stream(&client, &cfg, &eid, idx, &last_id, 5_000, 100).await?;
    for frame in &page.frames {
        handle(frame)?;
        last_id = frame.id.clone();
    }
    if page.is_closed() {
        break; // drained + terminal — exit cleanly, no timeout fallback needed
    }
}
```

The equivalent REST shape surfaces `closed_at` (i64 ms) and
`closed_reason` (string) as optional top-level fields on both the
`/stream` and `/stream/tail` JSON responses; fields are omitted when the
stream is still open.

**Terminal-signal drain pattern (XREAD / HMGET race)**

`xread_block` issues the XREAD and the follow-up HMGET of `stream_meta`
as separate round trips. A close can land between them — `frames`
reflects the stream as of T1, `closed_at`/`closed_reason` reflect
`stream_meta` as of T2. Correctness is preserved because
`ff_append_frame` gates on `closed_at` before every XADD: a stream that
is closed at T2 cannot have gained frames between T1 and T2, so anything
XREAD returned was already the tail.

The only consequence is that frames which landed between a previous
tail's last read and the close may not appear in the tail call that
*observes* the close. Consumers close the race with a final XRANGE
drain:

```rust
let mut last_id = "0-0".to_string();
loop {
    let page = ff_sdk::tail_stream(&client, &cfg, &eid, idx, &last_id, 5_000, 100).await?;
    for frame in &page.frames {
        handle(frame)?;
        last_id = frame.id.clone();
    }
    if page.is_closed() {
        // Final drain: XRANGE from the cursor to '+' picks up any frames
        // that were appended after our last tail read but before the
        // writer called close. `read_stream` also reports closed_at, so
        // the loop is self-terminating.
        let trailing = ff_sdk::read_stream(
            &client, &cfg, &eid, idx, &last_id, "+", 1_000,
        ).await?;
        for frame in trailing.frames.iter().filter(|f| f.id > last_id) {
            handle(frame)?;
        }
        break;
    }
}
```

The SDK `tail_stream` rustdoc carries a short hint; this section is the
normative reference. Implementations MAY skip the final drain when they
can tolerate the edge case (e.g. log-shipping where late frames are
best-effort), but correctness-sensitive consumers (cairn checkpoint
recovery) MUST drain.

**Authorization**

Inherits the global Bearer middleware that protects every other
`/v1/executions/…` route. Namespace-scoped ACLs are a cross-cutting
concern (not stream-specific); when/if we add them, both endpoints pick
them up for free. No per-stream authz is layered in at this time — it
would be redundant until the surrounding ACL story exists.

**Tests** (`crates/ff-test/tests/e2e_lifecycle.rs`, RFC-006 #2 section):
- `test_read_stream_round_trips_append_frame_fields`
- `test_read_stream_empty_returns_empty`
- `test_read_stream_slice_and_resume`
- `test_tail_stream_timeout_returns_empty`
- `test_tail_stream_unblocks_on_write`
- `test_tail_stream_no_block_returns_available`
- `test_tail_stream_long_block_respects_ferriskey_timeout_extension`
  (regression: 7s block does not spuriously transport-timeout at 5s)
- `test_stream_closed_signal_propagates_to_read_and_tail`
  (terminal signal: after complete, both read and tail report
  `closed_at` + `closed_reason=attempt_success`)
- `test_read_stream_rejects_zero_count_limit` (R3 BUG2 regression)
- `test_tail_stream_rejects_zero_count_limit` (R3 BUG2 regression)
- `test_lease_expired_closes_stream_meta` (R4 regression: tail
  consumer observes terminal signal even if no reclaim ever runs)

**Library version** bumped to `"4"` — the initial read path landed at
`"3"`; the R2 change that adds the terminal-signal fields to the return
shape of `ff_read_attempt_stream` (positions 2 and 3:
`closed_at`, `closed_reason`) is a return-arity change, so we bump again
per the library-version policy. Older binaries will hit the version
mismatch on boot and force a `FUNCTION LOAD REPLACE`, which matches the
intent.

---

## References

- Pre-RFC: flowfabric_use_cases_and_primitives (2).md — Primitive 5, Finalization item 5
- RFC-001: Execution Object and State Model (worker-2)
- RFC-002: Attempt Model and Execution Lineage (worker-3)
- RFC-003: Lease and Fencing Semantics (worker-1)
