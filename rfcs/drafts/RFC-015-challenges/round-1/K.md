# RFC-015 Round-1 — K (correctness) challenge

**Verdict:** DISSENT

## Per-section signals

| Section | Signal | Note |
|---|---|---|
| §1 StreamMode enum | GREEN | Non-exhaustive with explicit defaults is sound. |
| §2 Semantics table | YELLOW | "Visible to late tailer" for `BestEffortLive` = "Only if tailer connected before expiry" is fine as policy but glosses over race in §4.3. |
| §3.1 Storage | GREEN | Hash co-located via `{p:N}` hash-tag — atomicity achievable. |
| §3.2 Patch format / null sentinel | **RED** | Null-sentinel design is underspecified — see C1. |
| §3.3 Append path | **RED** | Steps 5/6 ordering + atomicity claim vs. XADD+summary race — see C2. |
| §3.4 XADD-the-delta | YELLOW | Consistency story between Hash `version` and XADD entry IDs is not nailed down — see C3. |
| §3.5 Compaction | YELLOW | `MAXLEN ~ 256` is approximate; a reader that walks "delta chain since version V" has no monotone cursor in stream space — see C3. |
| §4 BestEffortLive | YELLOW | See C4 (terminal marker on best-effort streams). |
| §4.2 Window sizing | GREEN | Bounded [32, 2048]; EMA α open in §10.1 is acknowledged. |
| §4.3 Empty-stream tail | YELLOW | Race between "terminal marker XADD is Durable" + aggressive MAXLEN trim — see C4. |
| §5 Mixed-mode | YELLOW | Caveat is documented but the terminal marker (§7) "Durable" is itself subject to MAXLEN trim in mixed streams — see C4. |
| §6 tail_stream | GREEN | `mode` field on XADD entries + default-fallback for legacy entries is the right shape. |
| §7 Terminal | **RED** | Terminal marker durability vs. best-effort MAXLEN is a latent correctness bug — see C4. |
| §8 Backward compat | GREEN | Defaulting is clean. |
| §9 AppendFrameOutcome | GREEN | Additive. |
| §10 Open questions | YELLOW | Q3 (cross-attempt summary merge) is a spec hole — MUST be answered in-RFC, not deferred. See C5. |

## Concrete correctness objections

### C1. JSON Merge Patch null sentinel — the contract is under-specified

§3.2 reserves the string `" __ff_null__"` (note the leading space in the RFC text — typo? or deliberate?) as the "set to JSON null" sentinel. Problems:

- **Typo risk.** The current text literally has a leading space: `" __ff_null__"`. If that's a copy-paste artifact, the contract is already ambiguous on its first reading. If it's deliberate, it's an obscure gotcha. Either way: REQUIRE the RFC to name the sentinel precisely, at byte-level, with no ambiguity. Propose: `"__ff_null__"` (no leading space) OR use a JSON-object-wrapped sentinel like `{"__ff_null__": true}` which cannot collide with any plausible user string value.
- **Where does the sentinel apply?** The RFC says "in the patch". But Merge Patch is recursive — does the sentinel translate at every depth? In array values? In nested objects used as values? Spec must say: "the sentinel is recognized only as a scalar leaf value in the patch document; at apply time the Lua function replaces occurrences of the sentinel with JSON null in the merged document."
- **Round-trip.** If a consumer reads the summary via `read_summary` and sees `null`, did the author mean "delete" or "set to null"? They're indistinguishable in the output. The RFC must state: "the summary document, as returned by `read_summary`, never contains the sentinel — nulls in the returned document always mean 'the field is explicitly null'." This is an invariant worth calling out.
- **Collision.** What if a user legitimately wants a JSON string value equal to the sentinel? With the current scheme, it's lost. Acceptable tradeoff, but must be documented as a known limitation in §3.2.

**Minimal change to ACCEPT C1:** §3.2 gains a "Null sentinel contract" subsection (byte-exact value, depth semantics, round-trip invariant, collision caveat).

### C2. Append path atomicity — step 6 (XADD) is outside the Hash update window

§3.3 lists:
1. Validate lease
2. Parse
3. HGET document
4. Apply patch
5. HSET new document + HINCRBY version + HSET last_updated_ms
6. XADD the delta

And claims: "All five steps are a single Valkey Function call — atomic for the `{p:N}` slot."

Two issues:
- The text says "all **five** steps" but the list has **six**. Typo or is step 6 genuinely outside the atomic envelope? Must be clarified; if step 6 is outside the atomic envelope, there's a window where a tailer sees a new `version` via `read_summary` but no matching XADD — or vice versa — on partial failure.
- Even if all six are inside one Function call (which is what I believe is intended), the **ordering** matters for observers. A concurrent `read_summary` + `tail_stream` (racing) must see a consistent snapshot: either both the pre-delta state (version=V, no delta entry) or both the post-delta state (version=V+1, delta entry present). Because they're separate Valkey commands from the client, the client-side ordering does not help. Inside the Function, because Valkey runs Functions single-threaded on the shard, this is safe — **as long as the entire sequence is one Function invocation**. The RFC should state this invariant explicitly: "the HGET→HSET→XADD sequence runs in a single Valkey Function call; no interleaving observer can see partial state."

**Minimal change to ACCEPT C2:** (a) fix the five-vs-six count. (b) Add the explicit atomicity invariant sentence in §3.3.

### C3. `summary_version` vs. XADD entry ID — no defined correspondence

§3.4 says XADD the delta AND bump `version`. §9 says `stream_id` is the XADD entry ID and `summary_version` is the Hash version, both returned on `DurableSummary` appends.

But: what's the ordering guarantee **across** appends? Can version V+1 land in the Hash with an XADD entry ID that is less than the entry ID for version V? No — because Function calls are serialized on the shard. Fine. But:

- The RFC does not define: "Entry IDs for `DurableSummary` frames are monotonic in `summary_version`; `summary_version` is recoverable from the XADD entry's fields (store `summary_version` in the XADD fields)."
- Without storing `summary_version` on the XADD entry, a tailer walking the stream has no way to know which Hash version a given delta produced. After compaction trims entries, there is no way to ask "what was the summary as of version V?" (which is also §10.2's question — but the structural answer — store it on the entry — should be in-RFC, not deferred).

**Minimal change to ACCEPT C3:** §3.4 gains a line: "Each `DurableSummary` XADD entry's fields include `summary_version` (u64) and `mode=summary`. Readers can correlate stream entries to Hash version without a separate lookup."

### C4. Terminal marker + mixed-mode MAXLEN = the marker can be trimmed away

§7 says "The terminal marker XADD is `Durable`." §5 says "if a caller mixes `Durable` and `BestEffortLive` in the same stream, the `Durable` frames are subject to the best-effort MAXLEN trim."

Compose them: in a mixed-mode stream with aggressive best-effort MAXLEN (say, K=32), after 32 post-terminal best-effort appends (possible if a misbehaving worker keeps appending after terminal), **the terminal marker is trimmed off**. A late consumer calling `tail_stream` now cannot distinguish "attempt is still live" from "attempt is terminal but marker was trimmed."

The RFC-006 contract is that `tail_stream` sees a terminal marker to stop. If it's trimmable, that contract breaks in mixed mode.

**Minimal change to ACCEPT C4:** Either (a) §7 states "terminal marker is written with a per-entry reservation that MAXLEN will not trim" (Valkey streams don't support this directly — so this option requires a design sub-step: e.g., on terminal, XADD the marker AND set a separate durable flag on metadata Hash that `tail_stream` reads as the authoritative terminal signal), or (b) §5/§7 jointly state "terminal signal is canonically read from the attempt metadata Hash, NOT from the stream marker; the stream marker is a convenience echo only." I prefer (b) — it's simpler and already matches RFC-002's attempt-state model.

### C5. §10.3 cross-attempt summary merge is not a deferrable open question

§10.3 asks: "Cross-attempt summary merging in `read_execution_stream`... Proposed default: latest-attempt only. Confirm."

This is not confirmable-later. `read_execution_stream` is an RFC-006 public surface. Shipping `DurableSummary` with undefined multi-attempt behavior means any consumer that retries (i.e., produces attempt_index > 0 with a `DurableSummary` frame) has undefined behavior on reads. Must be locked in-RFC.

**Minimal change to ACCEPT C5:** §10.3 is resolved in §6.3 text: "`read_summary` is per-(eid, attempt_index). `read_execution_stream` with a `DurableSummary`-mixed attempt returns the latest-attempt's summary document in a dedicated field on the merged view, alongside the unioned frame stream. If no successful attempt exists, returns None."

## Summary

Five RED/YELLOW items, all structural and locally fixable. The RFC is close — patch format choice is fine, hash-tag co-location is fine, phasing is sane — but the correctness contracts around null-sentinel, atomicity, version-to-entry correlation, terminal-marker durability, and cross-attempt merge all need to be nailed down in the text before this is bullet-proof.

**To flip to ACCEPT:** address C1–C5 with the minimal changes listed above.
