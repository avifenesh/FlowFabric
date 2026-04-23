# RFC-015 Round-2 — M (implementation) challenge

**Verdict:** ACCEPT.

## Re-review of round-1 concerns

| Objection | Status |
|---|---|
| M1 Lua LOC and apply-cost honesty | **Resolved.** §3.2 "Lua apply-cost estimate" subsection lists the cjson realities (decode/encode cost, recursion-depth guard, root validation, null-sentinel walk, cjson config) and revises to ~60–80 LOC. §3.3 step 4 explicitly annotates O(|document| + |patch|) per append. |
| M2 write amplification | **Resolved.** "Write amplification" paragraph in §"Back-of-envelope" states the ~20 KB/append RAM shuffle cost for a 10 KB document and ties it back to the batching argument. |
| M3 total per-attempt memory (stream + Hash summed) | **Resolved.** §"Back-of-envelope" table now shows "**~15–25 KB total**" for `durable_summary`, with the narrative paragraph explicitly summing "~10 KB delta window + ~5–15 KB Hash." The 10× claim is softened to 5–10× consistent with arithmetic. |
| M4 PEXPIRE silent destruction of Durable frames | **Resolved.** §4.1 now gates PEXPIRE on `has_durable_frame == false` and issues PERSIST when the flag flips. This is the right shape. |
| M5 EMA cost note | **Resolved.** §4.2 "Cost of EMA tracking" paragraph states +2 HSET + 1 HGET per best-effort append, amortized against the lease-validation Hash touch. |
| M6 honest 10× claim | **Resolved.** §Motivation says "~5–10×", consistent with the table. |

## New implementation checks

1. **Phase 2 Lua complexity.** Now that §3.2 says ~60–80 LOC and §3.3 says O(|document| + |patch|), the Phase 2 implementation plan in §12 should have a benchmark gate: "Phase 2 lands with a published-artifact smoke that measures append latency at document sizes 1 KB / 5 KB / 10 KB / 25 KB and confirms ≤ 5 ms/append at 10 KB." Strongly recommended but not blocking — project memory `feedback_smoke_after_publish.md` already mandates smoke per phase, and this RFC §12 already references that memory.

2. **`has_durable_frame` flag storage.** The flag lives on the metadata Hash (already used for EMA). Extra HSET on first durable append; extra HGET on every best-effort append to check the flag. In Valkey, an HGET of a non-existent field returns nil — cheap. Does not add significant load. GREEN.

3. **PEXPIRE ↔ has_durable_frame atomicity.** Like K's nit-2, I want this locked in: the flag set + PERSIST must be in the same Function call as the first durable append. §4.1 implies it ("set on first durable append") but does not explicitly say "atomically." Cosmetic.

4. **MAXLEN 64 tailing window empirical check.** §3.5 claims "64 entries covers ~1 s of back-buffer at typical rates." At 100 Hz append rate that's 640 ms; at 1 kHz it's 64 ms — which is tight for a re-connecting tailer. Not wrong but worth noting in Phase 3 smoke: "Phase 3 validates tailer re-connect latency does not exceed MAXLEN-64-window at target append rates." Non-blocking.

5. **`HGET` of a potentially large (25 KB+) document on every append.** Valkey handles this fine on the hot path (in-memory copy), but network egress to the client is not triggered because the document stays server-side in the Function. Good — confirms Function-only semantics save bandwidth even if write amplification is local. GREEN.

6. **Re-baseline RFC-010 §7.3 (Phase 6 docs PR).** Now that the memory numbers are honest in §"Back-of-envelope," Phase 6 should re-baseline §7.3 using the ~20 KB figure (not 15). The RFC-015 text doesn't repeat the wrong number; fine. GREEN.

## Per-section final signals

§1 GREEN • §2 GREEN • §3.1 GREEN • §3.2 GREEN • §3.3 GREEN • §3.4 GREEN • §3.5 GREEN (MAXLEN 64 + smoke check noted as nit) • §4.1 GREEN (PEXPIRE-has_durable gate correct) • §4.2 GREEN • §5 GREEN • §6.1 GREEN • §7 GREEN • §8 GREEN • §9 GREEN • §11 GREEN • §12 GREEN (with Phase-2/3 smoke-gate suggestions as nits).

## Verdict

**ACCEPT.** Implementation objections all addressed. Remaining items are Phase-2/3 smoke suggestions that belong in the implementation-plan phase, not in the RFC text itself.
