# RFC-015 Round-2 — K (correctness) challenge

**Verdict:** ACCEPT (with two nitpicks noted as GREEN, not blocking).

## Re-review of round-1 concerns

| Objection | Status |
|---|---|
| C1 null-sentinel contract | **Resolved.** Byte-exact `"__ff_null__"`, scalar-leaf-only, round-trip invariant, collision caveat all land in §3.2 "Null-sentinel contract" subsection. Text is precise. |
| C2 atomicity / 5-vs-6 step count | **Resolved.** §3.3 now says "All six steps execute inside a single Valkey Function invocation" and the atomicity invariant paragraph makes the no-partial-state guarantee explicit. |
| C3 summary_version on XADD entry | **Resolved.** §3.3 step 6 adds `mode=summary` and `summary_version=<new version>` on XADD fields; §3.5 references the carried version as a recoverable cursor. |
| C4 terminal marker trim | **Resolved via K's option (b).** §5 and §7 both state the canonical terminal signal is the attempt metadata Hash; the stream marker is a convenience echo. Consumers MUST check the metadata Hash. Good; this also aligns with RFC-002's attempt-state model. |
| C5 cross-attempt merge spec hole | **Resolved.** §6.3 now specifies "latest-attempt summary in `latest_summary: Option<SummaryDocument>` field on the merged view; `None` if no attempt produced a `DurableSummary`." §10.3 marked resolved. |

## New items I looked for on re-read

1. **PEXPIRE-persist interaction with `has_durable_frame` flag (M4 fix).** The §4.1 text is now: "The stream key gets `PEXPIRE <stream_key> <ttl_ms * 2>` set on first best-effort append **only if** the stream has never received a `Durable` or `DurableSummary` frame. ... If a durable frame has ever been appended ... the stream key is left with no expiry (`PERSIST` is issued if one was previously set, and PEXPIRE is never issued on subsequent best-effort appends)." Correct; the ordering "PEXPIRE-then-durable" is covered by the PERSIST-on-flip clause. GREEN.

2. **Atomicity of the `has_durable_frame` flag.** Setting the flag + PERSIST on the key must happen inside the same Function call as the durable append that first triggers it, otherwise a best-effort append racing with the first durable append could re-PEXPIRE after the PERSIST. §4.1 does not spell this out explicitly. Strong recommendation but not blocking: add a sentence to §4.1 saying "the `has_durable_frame` flag is set atomically with the first `Durable`/`DurableSummary` append; best-effort appends that run after a durable append observe the flag and skip PEXPIRE."

3. **Empty-stream tail and terminal-from-Hash.** §4.3 says an empty best-effort tail returns "empty frame list plus the terminal marker (if the attempt has ended)." Now that §5/§7 say the canonical terminal signal is the metadata Hash, "the terminal marker" in §4.3 should read "the terminal signal (from the attempt metadata Hash, not necessarily a stream-side marker)." Cosmetic. GREEN.

4. **`AppendFrameOutcome.stream_id` under BestEffortLive.** §9 still says "may vanish under MAXLEN" for the stream_id under best-effort. Fine; the caller who needs cursor semantics should use `Durable`.

## Per-section final signals

§1 GREEN • §2 GREEN • §3.1 GREEN • §3.2 GREEN (null-sentinel subsection is tight) • §3.3 GREEN (atomicity invariant is explicit) • §3.4 GREEN • §3.5 GREEN (MAXLEN 64, summary_version recoverable cursor) • §4.1 GREEN (with nit #2 above noted) • §4.2 GREEN • §4.3 GREEN (with nit #3 cosmetic) • §5 GREEN • §6.1 GREEN • §6.3 GREEN (cross-attempt resolved) • §7 GREEN • §8 GREEN • §9 GREEN • §10 GREEN (only α-tuning and optional keep-all-deltas remain; both non-blocking) • §11 GREEN • §12 GREEN.

## Verdict

**ACCEPT.** The two nits (atomicity of `has_durable_frame` flag; cosmetic terminal-marker wording in §4.3) are sufficiently minor that I'll flag them for the author to fold in if they want, but I do not dissent on them.
