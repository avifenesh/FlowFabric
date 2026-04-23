# RFC-015 Round-1 — Author response

Dissent received from K, L, M. Concede on almost all; push back on L2(b) deferral and M3 target framing.

## K (correctness)

- **C1 null sentinel** — Concede. Fix leading-space typo. Add "Null sentinel contract" subsection: byte-exact `"__ff_null__"`, scalar-leaf-only, round-trip invariant (sentinels never appear in output), collision caveat documented.
- **C2 atomicity / five-vs-six** — Concede. Fix count and add explicit atomicity invariant sentence in §3.3.
- **C3 `summary_version` on XADD entry** — Concede. Fields include `summary_version` + `mode=summary`.
- **C4 terminal marker trim** — Concede; adopt K's option (b). Terminal signal is canonically read from the attempt metadata Hash; stream marker is a convenience echo. Add sentence to §5 and §7.
- **C5 cross-attempt merge** — Concede; move resolution into §6.3. `read_execution_stream` returns latest-attempt summary in a dedicated field.

## L (ergonomics)

- **L1 constructor shortcut** — Concede. Add `StreamMode::durable_summary()` helper.
- **L2 patch batching assumption** — Concede (a). Add "Patch batching assumption" subsection in §"Back-of-envelope savings": per-frame batch size ≥ ~50 tokens for projected savings. Defer `StringAppend` to follow-up; list explicit re-open trigger in §11 ("if cairn measurement shows per-token patches are unavoidable, re-open with `PatchKind::StringAppend`").
- **L3 ttl_ms guidance** — Concede. One-sentence guidance added to §4.1.
- **L4 mixed-mode footgun** — Concede (b). Doc comment on `StreamMode::Durable` warns about mixed-mode MAXLEN trim.
- **L5 naming** — Concede. Rename `TailVisibility::DurableOnly` → `TailVisibility::ExcludeBestEffort`.

## M (implementation)

- **M1 Lua LOC estimate** — Concede. Revise "~30 lines" → "~60–80 lines + cjson config". Add complexity note: "patch apply is O(|document| + |patch|) per append."
- **M2 write amplification** — Concede. Add "Write amplification" paragraph to §"Back-of-envelope".
- **M3 total memory = stream + Hash** — Concede partially. Adopt (b): tighten default `MAXLEN ~ 64` (not 256). Rationale: tailers need ~1 s back-buffer; 64 entries covers that at typical rates. Sum becomes ~10 KB stream + ~15 KB Hash = ~25 KB, ~6× vs. `durable_full`. The 10× number stood on optimistic arithmetic; correct claim is "~5–10× depending on summary size and batch cadence." Update §Motivation.
- **M4 PEXPIRE silent destruction** — Concede, hard bug. Add "has received durable frame" flag on metadata Hash; PEXPIRE applied only when flag unset. PERSIST when flag flips.
- **M5 EMA on metadata Hash** — Concede. Add one-sentence cost note.
- **M6 10× target honesty** — Concede (tied to M3). §"Back-of-envelope" table will show stream-window + Hash sum. §Motivation target softens to "~5–10× depending on configuration."

## Revisions to apply

1. §1: typo fix, constructor helper.
2. §3.2: null-sentinel contract subsection; Lua LOC honest estimate.
3. §3.3: atomicity invariant sentence; 5↔6 step fix; O(|document|+|patch|) complexity note.
4. §3.4: `summary_version` + `mode=summary` stored on XADD fields.
5. §3.5: MAXLEN default 64 (was 256); update arithmetic.
6. §4.1: PEXPIRE-persist rule (M4); ttl_ms guidance; EMA cost note.
7. §5: terminal-signal canonical from metadata Hash; Durable-in-mixed caveat surfaced in doc comment.
8. §6.1: `ExcludeBestEffort` rename.
9. §6.3: cross-attempt merge resolution — latest-attempt summary in dedicated field.
10. §7: terminal marker is echo; canonical signal is metadata Hash.
11. §11: explicit re-open trigger for `StringAppend` PatchKind.
12. §Motivation + §"Back-of-envelope": honest arithmetic, sum of stream+Hash, 5–10× framing, batching assumption.

No dissent on delta/patch semantics or JSON Merge Patch as default PatchKind. Proceed to revise.
