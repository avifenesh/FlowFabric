# RFC-015 — Unanimous ACCEPT (Round 2)

**Date:** 2026-04-23
**Rounds to consensus:** 2
**Branch:** `rfc/015-stream-durability`
**PR:** #209

## Verdicts

- **K (correctness):** ACCEPT. Two cosmetic nits noted (atomicity of `has_durable_frame` flag set; terminal-marker wording in §4.3); both explicitly non-blocking.
- **L (ergonomics):** ACCEPT. One follow-up flagged (SDK doc-comment cross-ref to batching guidance); SDK-docs work, not RFC work.
- **M (implementation):** ACCEPT. Three Phase-2/3 smoke-gate suggestions noted; belong in the implementation plan, not RFC text.

## Round-1 concerns resolved

**Correctness (K):**
- Null-sentinel contract: byte-exact `"__ff_null__"`, scalar-leaf-only, round-trip invariant, collision caveat — §3.2.
- Atomicity invariant: all six steps of `DurableSummary` append execute in one Valkey Function call — §3.3.
- `summary_version` carried on XADD entry fields — §3.3 step 6; recoverable cursor in §3.5.
- Terminal signal canonical from attempt metadata Hash; stream marker is a convenience echo — §5, §7.
- Cross-attempt merge: `latest_summary: Option<SummaryDocument>` on merged view — §6.3.

**Ergonomics (L):**
- `StreamMode::durable_summary()` constructor — §1.
- Patch-batching assumption (≥ 50 tokens/frame) documented — §"Back-of-envelope savings".
- `ttl_ms` typical-range guidance (5000–30000 ms) — §4.1.
- Mixed-mode Durable-trim footgun surfaced in `StreamMode::Durable` doc comment — §1.
- `TailVisibility::DurableOnly` → `ExcludeBestEffort` — §6.1.

**Implementation (M):**
- Lua LOC estimate honest (~60–80 LOC + cjson config); O(|document| + |patch|) per append — §3.2, §3.3.
- Write amplification paragraph — §"Back-of-envelope savings".
- Stream+Hash summed total (~15–25 KB); 10× → 5–10× — §Motivation, §"Back-of-envelope savings".
- MAXLEN default 256 → 64 — §3.5.
- PEXPIRE gated on `has_durable_frame` flag; PERSIST on flip — §4.1.
- EMA cost note — §4.2.

**Alternatives §11:**
- `PatchKind::StringAppend` listed with explicit re-open trigger (cairn production measurement of per-token cadence).

## Adjudication notes

- Delta/patch semantics: locked by owner; not relitigated.
- JSON Merge Patch as default PatchKind: no challenger cited cairn-specific evidence to overturn. Patch-batching assumption (L2) captures the one edge case where Merge Patch underperforms.

## Owner adjudication pending

Design is settled. The remaining in-RFC items are:
- §10.1: EMA α decay constant to be measured against real cairn append-rate distribution before v1 lock.
- §10.2: `read_summary_history` (keep-all-deltas) variant — deferred, not rejected.

Both are acknowledged open questions the owner may accept as-is or require closing before escalation to implementation.

## Transcript

- `round-1/K.md`, `round-1/L.md`, `round-1/M.md` — dissent challenges.
- `round-1/author-response.md` — author concessions + revisions list.
- Round-1 revisions commit: `a83ce3d rfc(015): round-1 revisions`.
- `round-2/K.md`, `round-2/L.md`, `round-2/M.md` — ACCEPT.
