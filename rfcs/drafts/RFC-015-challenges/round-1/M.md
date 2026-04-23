# RFC-015 Round-1 — M (implementation) challenge

**Verdict:** DISSENT

## Per-section signals

| Section | Signal | Note |
|---|---|---|
| §3.1 Storage | GREEN | Hash per attempt, co-located via `{p:N}` is the right shape. |
| §3.2 Patch format | YELLOW | "~30-line recursive merge in Lua" claim needs verification — see M1. |
| §3.3 Append path | YELLOW | HGET → merge-in-Lua → HSET of potentially large document on every append. Memory bandwidth cost is not analyzed — see M2. |
| §3.5 Compaction | YELLOW | `MAXLEN ~ 256` default is claimed at 38 KB worst-case, which is fine, but the stream and the summary Hash now live side-by-side; total per-attempt memory claim must add them. See M3. |
| §4.1 Stream-key PEXPIRE | **RED** | `PEXPIRE <stream_key> <ttl_ms * 2>` "refreshed on each append" — in mixed-mode streams, this TTL refresh is a footgun that silently destroys durability of co-resident `Durable` frames — see M4. |
| §4.2 Window sizing | YELLOW | EMA update on the metadata Hash on every append is an extra HSET/HINCRBY inside the hot path — see M5. |
| §9 AppendFrameOutcome | GREEN | Additive field; existing XLEN is already computed. |
| §"Back-of-envelope" 10× claim | **RED** | The "~15 KB" figure for `DurableSummary` excludes the Hash document size itself for non-trivial summaries and ignores patch overhead — see M6. |
| §12 Implementation plan | YELLOW | Phase ordering is reasonable but Phase 2 (Lua patch) and Phase 4 (server-side filter) both touch the same Function — single reviewer per phase may miss interactions. Recommend Phase 4 merges into Phase 1 (types+filter are cheap), leaving Phase 2/3 as the pure Valkey-work phases. |

## Concrete implementation objections

### M1. "~30 lines of Lua" for JSON Merge Patch apply — verify

RFC 7396's algorithm is small on paper (5 lines of pseudocode in the IETF spec), but the **Lua-on-Valkey-Functions** realities add:

- `cjson` is available in Valkey Functions (yes, confirmed — Valkey 7.2+, Lua 5.1). Good.
- `cjson.decode` on the existing document (could be up to 10 KB per §"Back-of-envelope") runs on every append. For 1000 appends per attempt: 10 MB cumulative decode+encode work per attempt per shard. Should be benchmarked in Phase 2, not asserted.
- Recursive merge depth 16 — Lua's default stack is fine at 16, but the check itself needs 3–4 lines for correctness (increment on each recursion, decrement on return, early-exit on cap). Not free.
- "Reject if not a JSON object at root" — 2 lines.
- Null-sentinel swap (K's C1) — 5–10 lines if done right (walk the merged tree; replace sentinels with `cjson.null`).
- `cjson.null` vs. `nil` vs. absent-key serialization footguns — Valkey's cjson has the `cjson.null` singleton AND `cjson.decode_invalid_numbers` / `cjson.encode_sparse_array` knobs. Non-trivial to get right. Worth a 3–4 line config block at function start.

Realistic Lua LOC estimate: **60–80 lines**, not 30. Not a blocker — it still fits in a Valkey Function — but the RFC's claim is off by 2×. More importantly, the **decode/encode cost per append** on a 10 KB document is not a constant-time operation; it's O(document-size), so the per-append cost grows with the summary, not with the patch.

**Minimal change to ACCEPT M1:** §3.2 revises the "~30 lines" to "~60–80 lines plus cjson config" and §3.3 adds a complexity note: "patch apply is O(|document| + |patch|) per append, dominated by JSON decode/encode of the current document." This is honest and matches L2's concern about document growth.

### M2. HGET + HSET of the full document on every append is bandwidth-heavy

In Valkey Functions on a single shard, HGET+HSET of a 10 KB value is ~20 KB of RAM shuffling per append. At 1000 appends/attempt × 100K attempts/day, that's 2 TB/day of intra-shard bandwidth just for the summary read-modify-write. This isn't a correctness issue but it is a scale issue that §"Back-of-envelope" glosses over (the analysis only counted stream bytes, not Hash-update bytes).

**Minimal change to ACCEPT M2:** Add a "Write amplification" paragraph to §"Back-of-envelope": "Each `DurableSummary` append reads and writes the full summary document. For a 10 KB document, the intra-shard write amplification is ~20 KB/append. Callers appending high-frequency deltas against a large summary should batch (see L2)."

### M3. Total per-attempt memory = stream window + summary Hash — must be summed

§3.5 says "256 entries × 150 B ≈ 38 KB" for the delta window. §"Back-of-envelope" says "~15 KB" for the summary. The **total** per-attempt Valkey footprint for `DurableSummary` is **38 KB + 15 KB ≈ 53 KB**, not 15 KB.

That's still 3× better than `durable_full` (~150 KB), not 10×. The 10× claim in the summary needs to be walked back or the compaction window needs to shrink.

**Minimal change to ACCEPT M3:** Either (a) revise the §Motivation claim to "~3× reduction" with the real sum, or (b) compact the delta window harder (MAXLEN ~ 32 or 64) and justify that 32–64 is enough for tailing (it typically is — a tailer only needs ~1 s of back-buffer). Then the sum is ~5 KB + ~15 KB = ~20 KB, 7.5× vs. durable_full. I prefer (b): tighten MAXLEN default to 64, say so.

### M4. Stream-key PEXPIRE refresh in mixed-mode silently destroys Durable frames

§4.1: "The stream key itself gets `PEXPIRE <stream_key> <ttl_ms * 2>` set on first best-effort append; refreshed on each append."

In a mixed-mode stream where a caller appends `Durable` frames interleaved with `BestEffortLive { ttl_ms: 5000 }` frames, once `ttl_ms*2 = 10 s` passes with no more best-effort appends, **the entire stream key expires** — including the `Durable` frames. This is worse than the MAXLEN-trim concern in §5 (which K also flagged as C4 for terminal markers): MAXLEN trim is bounded by K frames; PEXPIRE expiry is wall-clock and takes the whole stream at once.

**Minimal change to ACCEPT M4:** §4.1 must state: "The stream-key PEXPIRE is applied ONLY if the stream has never received a `Durable` or `DurableSummary` frame. If a durable frame has ever been appended, the key is PERSIST'd and the best-effort frames rely solely on MAXLEN trimming. This prevents best-effort TTL from destroying durable content in mixed-mode streams."

Track "has received durable frame" on the metadata Hash as a single flag set on first durable append. Cheap.

### M5. EMA rate update on every append — is it inside the Function call?

§4.2 says EMA is updated "inline on each append." If this is an HINCRBY + HSET + timestamp-diff + EMA-math block inside the Lua Function, that's another 5–10 lines and 2–3 Hash commands per append. Not a blocker but the RFC should call out that the metadata Hash is now touched on every append — previously it was touched only on lease validation.

**Minimal change to ACCEPT M5:** One sentence in §4.2: "EMA update adds 2 HSET + 1 HGET on the metadata Hash per best-effort append. Adjacent to the lease-validation HGET, so amortized cost is small."

### M6. "10× memory reduction" target — is it met?

The §Motivation summary says "Target: ~10× reduction." The §"Back-of-envelope savings" table claims:

- `durable_full` 150 KB
- `durable_summary` ~15 KB

Per M3 above, the real `DurableSummary` number summed across stream+Hash is ~53 KB (or ~20 KB with tightened MAXLEN). The 10× target is met only under (b) with tight MAXLEN — and even then only for streams where the Hash document is small.

**Minimal change to ACCEPT M6:** The §"Back-of-envelope" table must show the SUM: stream-window + summary-hash. Clarify the 10× is achievable with MAXLEN default tuned appropriately + summary-document size ≤ ~5 KB. For larger summaries, savings are ~3–5×. Still worth shipping, but the motivation paragraph must match the arithmetic.

## Summary

Six implementation objections. M4 is the one hard bug — PEXPIRE on mixed-mode streams is a silent-data-loss vector. M1/M2/M3/M6 are all "the numbers in the RFC don't survive contact with reality" — not blockers, but the RFC must be arithmetically honest. M5 is minor.

**To flip to ACCEPT:** fix M1–M6 per the minimal changes above, especially the PEXPIRE-persist rule in M4 and the honest memory arithmetic in M3/M6.
