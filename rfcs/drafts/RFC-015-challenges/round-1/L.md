# RFC-015 Round-1 — L (ergonomics) challenge

**Verdict:** DISSENT

## Per-section signals

| Section | Signal | Note |
|---|---|---|
| §1 StreamMode enum | YELLOW | Enum is clear, but `DurableSummary { patch_kind: PatchKind }` with a single-variant `PatchKind` is ceremonial — see L1. |
| §2 Semantics table | GREEN | Clear at-a-glance table. |
| §3.2 Patch format choice | YELLOW | JSON Merge Patch is fine for additive updates but cairn's LLM pattern has **append-to-end-of-string** for `output.content`, not field-set — see L2. |
| §3.3 Append path | GREEN | Ordered steps are readable. |
| §4.1 TTL model | YELLOW | `ttl_ms: u32` with max ~49 days is fine, but the RFC does not call out a reasonable default or typical range, leaving the caller to guess — see L3. |
| §5 Mixed mode caveat | **RED** | The footgun in "mixing Durable + BestEffortLive in one stream trims Durable frames" is documented but not surfaced in the type system. A caller reading the SDK docs on `StreamMode::Durable` does not see this caveat — see L4. |
| §6 TailVisibility | YELLOW | `TailVisibility::DurableOnly` includes both `Durable` and `DurableSummary` frames — naming suggests otherwise. See L5. |
| §6.3 `read_summary` | GREEN | Signature is clean; returning `SummaryDocument` wrapping `serde_json::Value` is the right level. |
| §7 Terminal | GREEN | Invariant "final summary always readable via `read_summary`" is a good user-facing guarantee. |
| §9 AppendFrameOutcome | GREEN | Additive field is fine. |
| §10 Open questions | YELLOW | §10.1 (EMA α) and §10.2 (keep-all-deltas) should not block v1 but §10.3 (cross-attempt) is a user-visible hole (see K). |

## Concrete ergonomics objections

### L1. `DurableSummary { patch_kind: PatchKind }` with a one-variant enum is ceremony

Today the only `PatchKind` is `JsonMergePatch`. Every call site writes:

```rust
append_frame(..., StreamMode::DurableSummary { patch_kind: PatchKind::JsonMergePatch });
```

That's 40 characters of type-name to express "make it a summary." The RFC is right to make `PatchKind` an enum (future-proof for RFC 6902 or CRDTs) but the ergonomic default needs a shortcut:

**Minimal change to ACCEPT L1:** Add a constructor `StreamMode::durable_summary()` (or `StreamMode::summary()`) that constructs `DurableSummary { patch_kind: PatchKind::JsonMergePatch }`. The struct variant stays for forward compatibility; the helper carries the default. This is project-consistent with the `non_exhaustive_needs_constructor` memory — a non-exhaustive enum variant with a non-exhaustive inner enum is exactly the unbuildable-API pattern that feedback flagged, in reverse: buildable, but only via verbose path.

### L2. JSON Merge Patch maps poorly onto token-by-token LLM append

Cairn's actual LLM workload appends **tokens to a growing string**. In Merge Patch terms, every token append is:

```json
{"output": {"content": "<FULL STRING SO FAR>"}}
```

— because Merge Patch cannot express "concatenate to this string", only "replace this field's value." So for a 2000-token response where each token is ~4 chars, the **cumulative bytes on the wire** are O(N²) in token count: frame 1 is 4 B, frame 2 is 8 B, frame N is 4N B, summed ≈ 2·N²·4 B = 32 MB for N=2000.

The RFC's "~150 KB → ~15 KB" savings claim (§"Back-of-envelope savings") assumes the summary is small and patches are small. For token-append workloads — which §"Motivation" explicitly names as the target — the patches **grow unboundedly** because you're re-sending the whole `output.content` on every token.

Two ways to resolve:

- **(a) Clarify the workload.** If cairn's actual usage is "emit every 50 tokens, not every 1 token", the problem shrinks (40 patches × ~4 KB = 160 KB, back under budget). The RFC should state the assumed batch size.
- **(b) Add a string-append variant.** A second `PatchKind::StringAppend { path: String }` that interprets the frame payload as a string to concatenate at `path`. Tiny in Lua (~10 lines). Handles the token-stream workload natively.

I lean toward (a) in v1 — defer (b) to a follow-up — but the RFC **must** state what batching cairn is expected to do, or the memory claim is wrong.

**Minimal change to ACCEPT L2:** §"Back-of-envelope savings" gains a "Patch batching assumption" subsection that says "DurableSummary savings assume callers batch updates (e.g., every 50 tokens); per-token patches cause O(N²) patch bytes. Cairn's token-emit policy must be >= 50 tokens/frame for the projected savings. This is a SDK usage guidance, not a protocol constraint."

Alternatively, commit to (b) as a v1.1 follow-up in §11 with a concrete re-open trigger.

### L3. `BestEffortLive { ttl_ms: u32 }` — no guidance on reasonable values

A developer seeing `BestEffortLive { ttl_ms: u32 }` for the first time has to invent a value. 1000? 60000? The RFC gives no hint. Add in §4.1:

> Typical values: 5000–30000 ms. Below ~1000 ms the tailer may not connect in time; above ~60000 ms the memory argument for best-effort weakens versus just using `Durable`.

**Minimal change to ACCEPT L3:** §4.1 gains one-sentence guidance on typical `ttl_ms`.

### L4. Mixed-mode Durable-trim footgun is not discoverable

§5 documents that `Durable` frames in a mixed-mode stream are trimmed. A developer writing:

```rust
append_frame(..., StreamMode::Durable);  // expecting durability
append_frame(..., StreamMode::BestEffortLive { ttl_ms: 5000 });
append_frame(..., StreamMode::Durable);  // also expecting durability
```

— reasonably expects both `Durable` frames to survive. They won't, per §5. The SDK must surface this:

**Minimal change to ACCEPT L4:** One of:
- (a) The `append_frame` function panics or returns `Error::MixedModeDurabilityViolation` if a caller mixes modes on the same stream without opting in via a `mixed_mode: bool` flag. Loud failure beats silent trim.
- (b) The doc comment on `StreamMode::Durable` says, verbatim: "If this stream also receives `BestEffortLive` frames, `Durable` frames are subject to the best-effort MAXLEN trim. See §5."
- (c) Add a sibling-stream primitive now (explicitly deferred in §5 — but if we're defending correctness, defer is the wrong answer).

(b) is cheapest. (a) is most correct. I'll accept (b) for v1.

### L5. `TailVisibility::DurableOnly` is misleadingly named

The RFC says `DurableOnly` "Returns only frames appended under `StreamMode::Durable` or `DurableSummary`." But the variant name reads "Durable — singular." A reader passing `DurableOnly` could reasonably expect only `StreamMode::Durable` frames and be surprised to also get `DurableSummary` deltas.

**Minimal change to ACCEPT L5:** Rename to `TailVisibility::PersistedOnly` or `TailVisibility::ExcludeBestEffort`. The latter is self-describing. Update §6.1 accordingly.

## Summary

Five items, mostly naming/docs/constructors. L2 (the O(N²) patch growth for naive token-append) is the one structural concern — the memory-savings claim in §Motivation only holds under an unstated batching assumption. That assumption must be written down or the claim must be softened.

**To flip to ACCEPT:** address L1–L5; in particular, answer L2 with either an explicit batching contract or a `StringAppend` follow-up commitment.
