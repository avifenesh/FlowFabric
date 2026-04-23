# RFC-015: Stream durability modes — `durable_summary` and `best_effort_live` for attempt streams

**Status:** Draft
**Author:** FlowFabric Team (Worker-1 drafting)
**Created:** 2026-04-23
**Pre-RFC Reference:** RFC-006 §"Designed-for but deferred" (lines 657–665), RFC-010 §"Stream sizing" (line 2448)
**Locked decision:** `durable_summary` collapse semantics are **delta/patch** (not full-replacement). See §3.

---

## Summary

RFC-006 ships v1 attempt streams with a single durability mode, `durable_full`: every frame is a separate durable XADD entry. This is correct for auditable LLM inference but is wasteful for workloads where intermediate tokens are transient (operator only cares about the final summary plus whatever is on screen right now). This RFC defines the two modes RFC-006 flagged as deferred:

- `durable_summary` — each frame is a **delta/patch** against a server-side rolling summary document. Only the rolling summary (and optionally a compacted delta chain) survives trim. JSON Merge Patch (RFC 7396) is the chosen patch format.
- `best_effort_live` — each frame is a short-TTL XADD entry. Tailer-visible inside the TTL window; not guaranteed to survive past it.

Target: **~5–10× reduction in Valkey memory** for LLM-token workloads relative to `durable_full`, depending on summary-document size and caller batch cadence (see §"Back-of-envelope savings" for the arithmetic that supports this range).

## Motivation

### Workload profile

Cairn — the primary ferriskey consumer — runs LLM attempts that emit **200–2000 tokens per attempt** (RFC-010 §"Stream sizing"). Each token becomes one XADD entry, averaging ~150 B per frame: **30–300 KB per attempt stream**. At 100K active executions this is the dominant Valkey memory consumer (RFC-010 §7.3: "Stream data dominates memory at scale").

### What the operator actually consumes

- Tailing UIs want to display the **current state of the response** (not the full token history).
- Audit and replay want a **concise summary** of what was produced, not every token boundary.
- A small live window of recent tokens is useful for "model-is-alive" liveness signals but does not need durability.

### Back-of-envelope savings

Per-attempt, LLM-token workload:

| Mode | What Valkey holds | Size |
|---|---|---|
| `durable_full` (today) | 1000 XADD entries × 150 B | **~150 KB** |
| `durable_summary` | 1 Hash (final summary ~2–10 KB) + last ~64 delta entries for tailing (~10 KB) | **~15–25 KB total** |
| `best_effort_live` | ~50 frames alive at any moment × 150 B | **~7.5 KB** |

The `durable_summary` figure is the **sum** of the summary Hash and the live delta window; both live in the `{p:N}` slot and count against per-attempt memory. With default MAXLEN 64 (§3.5) the delta-window cost is ~10 KB; summary Hash is ~5–15 KB depending on document shape. Net savings versus `durable_full` are **~5–10×** for summaries in the 2–10 KB band.

For the mixed fleet (60% simple / 30% streaming / 10% agent) in RFC-010 §7.3, switching the 30% streaming slice from `durable_full` (85 KB avg) to `durable_summary` (~20 KB) reduces the weighted average from 47 KB/execution to ~28 KB/execution — **~40% total Valkey memory savings** fleet-wide, and **~5–10× on the streaming slice itself** depending on summary size.

#### Patch batching assumption

The savings above assume callers batch updates: one `DurableSummary` frame per ~50 tokens (or per logical summary revision), **not** per individual token. JSON Merge Patch cannot express "append to this string"; emitting a `DurableSummary` frame on every token re-sends the entire `output.content` field, so per-frame patch bytes grow linearly with response length and cumulative bytes on the wire grow O(N²). SDK usage guidance (v1): batch at ≥ 50 tokens/frame. If measurement under real cairn load shows per-token cadence is unavoidable, re-open with `PatchKind::StringAppend` (§11).

#### Write amplification

Each `DurableSummary` append reads and writes the full summary document. For a 10 KB document, the intra-shard RAM shuffle is ~20 KB/append. High-frequency deltas against a large summary compound this cost — another reason to batch per the assumption above.

## Detailed Design

### §1 `StreamMode` enum on `append_frame`

The durability mode is **per-call**, not per-stream. Rationale: workers routinely mix frame types (tokens + progress + final summary) in a single attempt, and the right durability differs per frame type.

```rust
/// Argument to `append_frame`. Replaces the implicit `durable_full` v1 behavior.
#[non_exhaustive]
pub enum StreamMode {
    /// Current default. Every frame XADDs as a durable entry.
    Durable,

    /// Server-side rolling-summary collapse. Each frame carries a delta
    /// against the previous summary. See §3.
    DurableSummary { patch_kind: PatchKind },

    /// Short-lived frame. XADDed, but expires via per-stream MAXLEN +
    /// per-entry TTL. See §4.
    BestEffortLive { ttl_ms: u32 },
}

#[non_exhaustive]
pub enum PatchKind {
    /// RFC 7396 JSON Merge Patch. Locked choice. See §3.2.
    JsonMergePatch,
}

impl StreamMode {
    /// Ergonomic default for the common case: `DurableSummary` with
    /// `PatchKind::JsonMergePatch`. Shortcut for the verbose struct-variant
    /// spelling.
    pub fn durable_summary() -> Self {
        StreamMode::DurableSummary { patch_kind: PatchKind::JsonMergePatch }
    }
}
```

`append_frame` gains a final optional argument `mode: StreamMode`. Default (absent) = `Durable`, preserving all v1 callers (see §8 / §8.1).

**Doc-comment contract on `StreamMode::Durable`:** If the same stream also receives `BestEffortLive` frames, `Durable` frames are subject to the best-effort MAXLEN trim (see §5). Callers that need strict retention for `Durable` frames alongside best-effort telemetry must not mix modes on one stream.

### §2 `StreamMode` semantics summary

| Mode | Durable across restart | Survives MAXLEN trim | Visible to late tailer | Memory cost |
|---|---|---|---|---|
| `Durable` | Yes (AOF/RDB) | Until explicitly trimmed | Always (within retention) | ~150 B / frame |
| `DurableSummary` | Yes (summary Hash) | Summary always; pre-summary deltas trim on compaction | Latest summary, always | **~O(1)** per attempt |
| `BestEffortLive` | No guarantee | No — TTL-bound | Only if tailer connected before expiry | ~O(TTL · rate) |

## §3 `DurableSummary` — server-side collapse algorithm

### §3.1 Storage

A single Valkey Hash per attempt:

```
Key:    ff:attempt:{p:N}:{eid}:{aidx}:summary
Fields:
  version         → monotonic u64 (starts 1, increments per delta applied)
  document        → JSON string (the rolling summary document)
  patch_kind      → "json-merge-patch"
  last_updated_ms → unix ms
  first_applied_ms→ unix ms
```

The Hash lives in the same `{p:N}` hash-tag slot as the stream data key and metadata key (co-located for atomic application inside the Lua Function — RFC-006 §"Valkey Data Model" conventions).

### §3.2 Patch format: JSON Merge Patch (RFC 7396), locked

**Choice: `JsonMergePatch` (RFC 7396).**

Three formats were on the table. Evaluated below and adjudicated inline (per RFC constraint: no open PatchKind question).

| Candidate | Pros | Cons | Verdict |
|---|---|---|---|
| **JSON Merge Patch (RFC 7396)** | Delta = a JSON document the same shape as the target. Trivially small for "set field X". Apply is a ~30-line recursive merge — fits in Lua. LLM outputs are predominantly additive field-level updates (set `output.content`, bump `tokens_used`, set `stop_reason`). Array replacement is whole-value, which matches LLM usage (no need to patch individual array indices). | Cannot express "delete item at index 3"; array edits are wholesale. Cannot express explicit `null` as "set to null" unambiguously — `null` means "delete key" in 7396. | **Chosen.** |
| JSON Patch (RFC 6902) | Explicit ops (`add`/`remove`/`replace`/`move`/`copy`/`test`). Handles array-index edits. Unambiguous null. | Verbose on the wire (each field update = a JSON object with `op`/`path`/`value`). Apply requires full JSON Pointer engine in Lua (not trivial). Overkill for LLM summaries. | Rejected (complexity/bytes). |
| Plain-text append | Zero apply cost (string concat). | Only works for monolithic text outputs; cannot represent structured summaries (tokens_used, tool_calls, stop_reason, metadata). Cairn's summaries are structured. | Rejected (doesn't fit workload). |

#### Null-sentinel contract

The RFC 7396 "null means delete" ambiguity is handled by reserving a sentinel string for callers that genuinely need to set a field to JSON null.

- **Byte-exact sentinel value:** `"__ff_null__"` (no leading or trailing whitespace; 11 ASCII characters).
- **Depth semantics:** the sentinel is recognized only as a **scalar leaf value** in the patch document. At apply time, the Lua function walks the merged document and replaces occurrences of this exact string (as a scalar string value) with JSON null. The sentinel has no special meaning as a key, inside an array, or as a nested-object value other than a scalar leaf.
- **Round-trip invariant:** the summary document, as returned by `read_summary`, **never contains the sentinel string**. Any `null` observed in the returned document always means "the field is explicitly null."
- **Collision caveat:** if a caller legitimately wants the string value `"__ff_null__"` at a scalar position, that value is lost (replaced with JSON null). Documented limitation; v1 callers must not use that byte string as a real data value.

#### Lua apply-cost estimate

RFC 7396's merge algorithm is ~5 lines of pseudocode in §2 of the IETF spec, but the **Valkey Functions / cjson** realities (decode/encode of the current document per append, recursion-depth guard, root-object validation, null-sentinel walk, cjson config block) drive realistic Lua LOC to **~60–80 lines** (plus a 3–4 line cjson config). Patch apply is **O(|document| + |patch|)** per append, dominated by JSON decode/encode of the current document — not O(|patch|) alone. This is one reason for the batching assumption in §"Back-of-envelope savings".

### §3.3 Append path (delta application)

On `append_frame` with `DurableSummary { patch_kind: JsonMergePatch }`:

1. Validate lease (unchanged from RFC-006).
2. Parse the frame payload as JSON. Reject if not a JSON object (Merge Patch requires an object at the root).
3. `HGET` current `document` from the summary Hash. If absent: initialize with `{}`.
4. Apply the merge patch in Lua (bounded recursion depth: 16). Reject if depth exceeded. Cost: **O(|document| + |patch|)** per append (see §3.2 Lua apply-cost estimate).
5. `HSET` the new `document`, `HINCRBY version 1`, `HSET last_updated_ms`.
6. **Also** `XADD` the delta to the stream (option (a) — see §3.4 for the choice). XADD fields include `mode=summary` and `summary_version=<new version>` so tailers can correlate stream entries to Hash versions without a separate lookup.

**Atomicity invariant.** All six steps execute inside a single Valkey Function invocation. Because Valkey runs Functions single-threaded on the shard owning the `{p:N}` slot, no concurrent observer (`read_summary`, `tail_stream`, or another `append_frame`) can see partial state: any observer sees either the pre-delta state (version `V`, no new XADD entry) or the post-delta state (version `V+1`, XADD entry with `summary_version=V+1` present). Client-side ordering is not required for correctness.

### §3.4 XADD-the-delta vs. summary-only

**Decision: option (a), XADD the delta AND update the summary.**

The two options considered:

- **(a) XADD the delta + update summary** (chosen). `tail_stream` continues to work unchanged for live consumers (they see the delta chain in real time). Late-joining tailers or consumers that want the "current state" read the Hash directly via a new `read_summary` call (§6.3). The pre-summary deltas are compacted (§3.5).
- **(b) Summary-only, no XADD.** Simpler but breaks live tailing — a consumer watching the stream would see nothing, because nothing is written to the stream. Would require a new pub/sub channel to carry deltas for live tailing, which is a second mechanism for the same thing.

Option (a) preserves the RFC-006 tail-stream contract and stays inside one mechanism (Streams). The summary Hash is the "materialized view"; the stream is the change log.

### §3.5 Compaction (trimming pre-summary XADD entries)

Pre-summary XADD entries are only useful to live tailers within a short window. After a tailer has caught up, holding them is waste.

**Policy:**

- Every `DurableSummary` append performs `XADD ... MAXLEN ~ 64 ...`. The `~` (approximate) trim is O(1) amortized.
- `64` entries at ~150 B ≈ 10 KB/stream worst-case — far below the 1.5 MB `Durable` worst case and independent of total frame count. A tailer typically needs only ~1 s of back-buffer; 64 entries covers that at typical append rates.
- The summary Hash is the durable truth; trimmed delta entries are recoverable only via the Hash's `version` field (which is monotonic) and the `summary_version` carried on each XADD entry's fields (§3.3 step 6). Tooling that needs a cursor into the live delta window can pin on `summary_version`.

MAXLEN sizing `64` is a default; operator-tunable in `stream_policy.summary_live_window` (RFC-001 ExecutionPolicySnapshot). Not in v1 scope of this RFC to expose that knob through every SDK surface — that's a follow-up.

## §4 `BestEffortLive` — TTL model

### §4.1 Per-stream MAXLEN, not per-entry TTL

Valkey Streams do not support per-entry TTL (an entry only disappears via XDEL or MAXLEN/MINID trim). So "per-frame EXPIREAT" is not a primitive we have.

**Chosen model: MAXLEN-based rolling window, with an optional stream-level key TTL.**

- Each `BestEffortLive { ttl_ms }` append issues `XADD ... MAXLEN ~ K ...` where `K` is derived from `ttl_ms` and the observed append rate (see §4.2).
- The stream key gets `PEXPIRE <stream_key> <ttl_ms * 2>` set on first best-effort append **only if** the stream has never received a `Durable` or `DurableSummary` frame. If a durable frame has ever been appended (tracked by a boolean flag `has_durable_frame` on the metadata Hash, set on first durable append), the stream key is left with no expiry (`PERSIST` is issued if one was previously set, and PEXPIRE is never issued on subsequent best-effort appends). Rationale: best-effort TTL must never destroy durable content in mixed-mode streams. In pure best-effort streams, PEXPIRE is refreshed on each append; on attempt terminal (§7) the refresh stops and the stream expires naturally.

**Typical `ttl_ms` values:** 5000–30000 ms. Below ~1000 ms the tailer may not connect in time; above ~60000 ms the memory argument for best-effort weakens versus just using `Durable`.

### §4.2 Window sizing

`K` (the MAXLEN) is derived:

```
K = max(32, min(2048, ceil(recent_rate_hz * ttl_ms / 1000) * 2))
```

where `recent_rate_hz` is an EMA of append rate stored on the stream metadata Hash (updated inline on each append). The `* 2` factor gives a visibility buffer. Bounds `[32, 2048]` cap worst-case memory and handle cold starts.

**Cost of EMA tracking.** The EMA update adds 2 HSET + 1 HGET on the metadata Hash per best-effort append. Because the metadata Hash is already touched for lease validation, the amortized cost is a few extra field operations in a Hash already in the working set — small relative to the XADD.

**EMA decay constant α — unset in this RFC, gated on benchmark.** This RFC deliberately does not bake an α value into the spec. See §4.3.

**Guarantee:** "Visible to a tailer that starts before `append_time + ttl_ms`". After `ttl_ms`, the frame may or may not exist (MAXLEN may have rolled it off; key TTL may have fired).

### §4.3 Tuning-before-ship (EMA α gate)

The EMA decay constant α used in §4.2's `recent_rate_hz` update is **not specified in this RFC**. Picking α blindly — e.g., defaulting to α = 0.2 with a ~5-sample span — risks over- or under-sizing `K` for real cairn workloads, which directly drives the memory-vs-visibility tradeoff that motivates `BestEffortLive` in the first place.

**Pre-release gate for v0.6:**

- **Benchmark shape.** Sample append-rate on a production cairn workload over a continuous 24 h window (at minimum; longer preferred if cairn diurnal pattern is pronounced). Record the per-stream append-rate distribution and derive p50 and p99 percentile-based window sizes.
- **α selection criterion.** Choose α such that the EMA-predicted `K` tracks the observed p99 window size within a bounded overshoot (exact bound to be set by the measurement team at gate time, informed by the observed distribution's shape — not guessed here) while not collapsing to the p50 during rate lulls.
- **Gate.** The v0.6 release cannot ship `BestEffortLive` until the α value is selected from this measurement and recorded in the implementation PR + changelog. If the measurement is unavailable by release time, `BestEffortLive` is held back to a later release; `Durable` + `DurableSummary` may ship independently.

This gate closes §11's "EMA α" open question with a decision: deferred-to-measurement, not deferred-indefinitely.

### §4.4 Empty-stream tail

If a consumer calls `tail_stream` after all best-effort frames have expired and no durable frames exist: the read returns an **empty frame list plus the terminal marker** (if the attempt has ended) or an **empty frame list plus the 5 s XREAD BLOCK timeout** (if the attempt is still live but quiet). This is identical to the "attempt produced no output" v1 behavior — no new error kind.

## §5 Mixed-mode streams

**Yes, mixed.** The mode is per-call, not per-stream. Confirmed.

A single attempt stream may interleave:

- N `Durable` frames (e.g., `progress`, `warning`, `artifact_reference`)
- M `DurableSummary` deltas (e.g., rolling `output.content` for the final answer)
- P `BestEffortLive` frames (e.g., token-by-token deltas for live UI)

The stream key holds the union of XADDs. MAXLEN trim is driven by the most aggressive trimmer that touched it (typically best-effort MAXLEN). Durable frames inside the MAXLEN window survive trim only if MAXLEN exceeds the best-effort window.

**Caveat, explicitly documented:** if a caller mixes `Durable` and `BestEffortLive` in the same stream, the `Durable` frames are subject to the best-effort MAXLEN trim. Callers who need strict durable retention alongside best-effort telemetry should not mix modes, or should co-locate the durable frames in a sibling stream (not in v1 scope — this RFC does not add a sibling-stream primitive). This caveat is also surfaced in the doc comment on `StreamMode::Durable` (§1) so SDK users see it at the call site.

**Terminal signal is canonical from the metadata Hash, not from the stream.** Because a `Durable` frame (including a terminal marker XADD) in a mixed-mode stream can be trimmed by best-effort MAXLEN, the authoritative "attempt terminal" signal is the attempt-state transition recorded on the RFC-002 attempt metadata Hash. `tail_stream` and consumers treat a terminal-marker XADD as a **convenience echo** of that signal; if the echo is absent (trimmed or never written), the canonical check is against the attempt metadata Hash. See §7.

The `DurableSummary` Hash is **independent** of the stream key — it survives even if the stream is fully trimmed. The final summary is always retrievable via `read_summary`.

## §6 `tail_stream` updates

### §6.1 New argument

```rust
pub struct TailOptions {
    pub count_limit: u32,
    pub block_ms: u32,
    /// NEW (this RFC). Default = All.
    pub visibility: TailVisibility,
}

#[non_exhaustive]
pub enum TailVisibility {
    /// Default. Returns every XADD entry in the stream regardless of mode.
    All,
    /// Returns only frames appended under `StreamMode::Durable` or
    /// `DurableSummary` (i.e., filters out `BestEffortLive`). Named to be
    /// self-describing: the filter excludes best-effort frames, not "only
    /// Durable" — `DurableSummary` deltas are included because they have a
    /// durable backing (the summary Hash).
    ExcludeBestEffort,
}
```

**Frame-mode marker.** Every XADD entry carries a `mode` field in its fields set (`"mode" → "durable" | "summary" | "best_effort"`). Filtering is a cheap field check in the `ff_read_attempt_stream` function. Client-side filtering is also possible — but server-side is preferred because MAXLEN has already deleted the entries the client would have to filter.

### §6.2 Default = `All`

Consumers that pass no `visibility` see every XADD entry, matching v1. Existing consumers are not broken.

### §6.3 New call: `read_summary`

```rust
pub async fn read_summary(
    client: &ValkeyClient,
    cfg: &Config,
    eid: &ExecutionId,
    attempt_index: u32,
) -> Result<Option<SummaryDocument>, Error>;

pub struct SummaryDocument {
    pub document: serde_json::Value,
    pub version: u64,
    pub patch_kind: PatchKind,
    pub last_updated_ms: u64,
    pub first_applied_ms: u64,
}
```

Returns `None` if no `DurableSummary` frame was ever appended for this attempt. Non-blocking; reads the Hash directly. Safe inside a Lua Function.

**Cross-attempt behavior in `read_execution_stream`.** RFC-006 defines a merged view across all attempts for a given execution. For `DurableSummary`, the merge semantic is: `read_execution_stream` returns the **latest-attempt's summary document** in a dedicated field on the merged view (`latest_summary: Option<SummaryDocument>`), alongside the unioned frame stream. "Latest attempt" = highest `attempt_index` that has any `DurableSummary` frame. If no attempt produced a `DurableSummary`, the field is `None`. This closes RFC-006's cross-attempt-merge semantics for summary mode and resolves the open question that was previously in §10.3.

## §7 Terminal semantics

When an attempt enters terminal state (RFC-002 §"Terminal transitions"):

- **`DurableSummary` Hash:** survives terminal. It is the **final summary of record**. It follows the attempt's normal retention TTL (RFC-006 §"Retention"), not the stream's MAXLEN. A terminal-only archival pass (future work, out of scope here) would pick up the Hash.
- **`BestEffortLive` frames:** no special handling. Existing key TTL / MAXLEN continues; they expire naturally. The stream key TTL refresh (§4.1) stops on terminal, so the key eventually vanishes.
- **Stream close marker:** a terminal marker XADD (mode=`Durable`) is still emitted as a **convenience echo**. The **canonical** terminal signal is the attempt-state transition on the RFC-002 attempt metadata Hash. Consumers MUST treat the metadata-Hash state as authoritative; a missing stream marker (e.g., trimmed in a mixed-mode stream with aggressive best-effort MAXLEN) does not imply the attempt is still live.

Invariant: **a terminal attempt's final summary is always readable via `read_summary` until retention TTL**, regardless of what happened to the stream entries. The terminal signal is always readable via the attempt metadata Hash, regardless of whether the stream-side terminal marker survived trim.

## §8 Interactions with other primitives

### §8.1 Backward compatibility with v1 `durable_full`

**No-op migration.** `append_frame` without a `mode` argument defaults to `StreamMode::Durable` — which is `durable_full` renamed. Existing SDK consumers, including cairn on ff-sdk ≤ 0.5.x, continue to work bit-identically. The `mode` field on XADD entries is added to new writes only; v1 entries without the field are treated as `mode=durable` by the reader (default fallback in the filter).

`stream_policy.durability_mode` on the execution (RFC-001) remains the **fleet default** but is now overridable per-call. Order of precedence: per-call `mode` argument > `stream_policy.durability_mode` > `durable_full`.

### §8.2 RFC-006 (streams)

This RFC extends RFC-006. No invariant of RFC-006 is invalidated. Specifically: lease validation, closed-stream rejection, 64 KB frame-payload cap, and `(execution_id, attempt_index)` keying are all preserved.

### §8.3 RFC-013 (suspend/resume)

Orthogonal. Suspend/resume does not touch stream data; it only blocks the worker. A suspended attempt's `DurableSummary` Hash is frozen until the worker resumes and appends more deltas. No interaction.

### §8.4 RFC-010 (Valkey architecture)

Capacity projections in RFC-010 §7.3 should be re-evaluated once these modes land; the "streaming slice" row collapses from 85 KB to ~15 KB per execution (§"Back-of-envelope savings"). That revision is a follow-up docs PR, not part of this RFC's implementation.

### §8.5 RFC-012 (engine backend trait)

`StreamMode` is part of the backend-trait surface: non-Valkey backends (future Postgres-backed engine) must either implement an equivalent collapse/TTL semantics or reject non-`Durable` modes at the trait boundary with a clear `UnsupportedMode` error. This RFC does not prescribe a Postgres implementation; it does prescribe that the trait carry `StreamMode` as an input so Postgres is at least able to reject explicitly.

## §9 `AppendFrameOutcome` semantics per mode

Unchanged shape (keeps the SDK surface stable). Per-mode meaning of the fields:

| Field | `Durable` | `DurableSummary` | `BestEffortLive` |
|---|---|---|---|
| `stream_id` | XADD entry ID (`<ms>-<seq>`) | XADD entry ID of the delta — the summary `version` is **also** returned in a new optional `summary_version: Option<u64>` field on the outcome | XADD entry ID (may vanish under MAXLEN) |
| `frame_count` | XLEN (total entries, post-trim) | XLEN (post-trim — reflects compacted window, not total deltas ever applied). Consumers that want "total deltas applied" read `summary_version` | XLEN (rolling window size, not lifetime appends) |

Rationale for keeping `stream_id` as the XADD entry ID (not the summary version): the RFC-006 contract says `stream_id` is the stream cursor, and tools already use it for pagination via `XRANGE from_id`. Changing that semantic for `DurableSummary` would break cursor math. The summary version is additive information.

New optional field:

```rust
pub struct AppendFrameOutcome {
    pub stream_id: String,
    pub frame_count: u64,
    /// NEW (this RFC). Populated only for `DurableSummary` appends.
    pub summary_version: Option<u64>,
}
```

## §10 Open questions

All previously-open questions are resolved in this draft. Retained here with resolution pointers for reviewer traceability.

1. **(Resolved — owner adjudication, 2026-04-23.)** Rate-EMA decay constant α for `BestEffortLive` window sizing (§4.2). Decision: **not specified in this RFC; gated on a pre-v0.6-release benchmark.** See §4.3 for the benchmark shape and gate criteria.
2. **(Resolved — owner adjudication, 2026-04-23.)** A `KeepAllDeltas`/`read_summary_history` variant of `DurableSummary` was considered to serve replay/debug. Decision: **dropped from v0.6.** Debug and audit consumers can use raw `Durable` mode for the same need; a second `DurableSummary` variant is not worth the doubled test matrix. See §11 "Alternatives rejected" for the full rationale.
3. **(Resolved, moved to §6.3.)** Cross-attempt summary merging in `read_execution_stream` is locked to "latest-attempt summary in dedicated field." See §6.3.

## §11 Alternatives rejected

- **Full-replacement summaries** (each frame = whole new summary document). Rejected by owner decision, up front. Reasons: wire bandwidth scales with document size × frame count; no inherent advantage over delta when the summary shape is mostly additive.
- **Server-side generic compression** (keep `durable_full`, run LZ4 on stream entries). Rejected: saves ~2–3×, not 10×; Valkey does not expose stream-entry-level compression primitives, would require a custom Module; still holds O(frame_count) entries.
- **Per-entry TTL via XDEL-scanner** (background job that XDELs entries older than `now - ttl`). Rejected: O(n) scan cost per stream; contention with append path; no atomicity guarantee.
- **Separate "summary stream" and "token stream"** (two streams per attempt). Rejected: doubles metadata cost, complicates lease validation, does not fit the "mode is per-call" shape that workers actually want.
- **`DurableSummary::KeepAllDeltas` variant (per-frame keep-all-deltas retention).** Considered and dropped from v0.6 by owner adjudication (2026-04-23). Rationale: debug and audit consumers that want every delta preserved can use raw `Durable` mode to achieve the same retention, and the summary Hash is already the authoritative final-state record. Shipping a second `DurableSummary` variant would double the test matrix (apply path × compaction path × tailer-filter path × cross-attempt merge path) for a use case already served by existing modes. Re-open only if a concrete consumer surfaces that cannot be served by `Durable` (e.g., needs both the collapsed summary Hash and lossless delta history simultaneously). v0.6 `DurableSummary` ships as summary-only tail (summary Hash + ~64-entry live-tail window per §3.5) — no `KeepAllDeltas` variant.
- **JSON Patch (RFC 6902) as PatchKind.** Evaluated in §3.2 and rejected (verbosity + Lua complexity).
- **`PatchKind::StringAppend { path }` for token-by-token append workloads.** Deferred, not rejected. Re-open trigger: if cairn's production measurement shows per-token `DurableSummary` appends against a growing `output.content` field (which drives O(N²) wire bytes under JSON Merge Patch — see §"Patch batching assumption"), add a second `PatchKind` variant that interprets the frame payload as a string to concatenate at a given JSON Pointer path. Estimated Lua cost: ~10–15 lines. Kept out of v1 because the batching guidance (≥ 50 tokens/frame) is believed sufficient for the memory targets, and adding a second PatchKind is a type-surface expansion that is cheaper to do once cairn measurement is in hand.

## §12 Implementation plan

Phased. Each phase lands a PR; phase boundary = CI green workspace-wide (per project reference `feedback_rfc_phases_vs_ci.md`).

1. **Phase 1 — types + backend trait surface.** Add `StreamMode`, `PatchKind`, `summary_version` field, `TailVisibility`. Thread through `append_frame` and the backend trait. Default to `Durable` everywhere. Zero behavior change. Tests: round-trip; existing tests all pass unchanged.
2. **Phase 2 — Valkey-side `DurableSummary`.** JSON-merge-patch apply in the Lua function. Summary Hash key format. Compaction MAXLEN 256. `read_summary` function + SDK call. Tests: delta application correctness, version monotonicity, null-sentinel handling, depth cap.
3. **Phase 3 — Valkey-side `BestEffortLive`.** MAXLEN sizing + stream-key PEXPIRE. EMA rate tracking on metadata Hash. Empty-stream tail smoke. Tests: expiry timing (tolerant), tailer-in-window visibility, tailer-after-window empty return.
4. **Phase 4 — `tail_stream` `TailVisibility` filter.** Server-side mode field filter. Tests: mixed-mode stream returns correct subsets.
5. **Phase 5 — cairn migration guide + docs.** Not code in FlowFabric: a migration note in cairn-rs for switching token-append from `Durable` to `DurableSummary { JsonMergePatch }`. Peer-team artifact only, per `feedback_peer_team_boundaries.md`.
6. **Phase 6 — RFC-010 §7.3 revision.** Docs-only PR; re-baseline scale projections with the streaming-slice reduction.

Watchdog: each phase must land with a published-artifact smoke (per `feedback_smoke_after_publish.md`) before the next phase opens.

## References

- RFC-006 (stream) — extended by this RFC.
- RFC-010 §7.3 — memory-savings baseline.
- RFC-001 — `stream_policy.durability_mode` is the fleet default.
- RFC-012 — `StreamMode` travels through the backend trait; non-Valkey backends reject non-`Durable` explicitly.
- IETF RFC 7396 — JSON Merge Patch (chosen `PatchKind`).
- IETF RFC 6902 — JSON Patch (rejected alternative, §3.2).
- Project memory: `feedback_approve_against_rfc_not_plan.md`, `feedback_rfc_quality.md`.
