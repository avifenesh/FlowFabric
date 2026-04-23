# RFC-015 Round-2 — L (ergonomics) challenge

**Verdict:** ACCEPT.

## Re-review of round-1 concerns

| Objection | Status |
|---|---|
| L1 ceremonial struct-variant construction | **Resolved.** `StreamMode::durable_summary()` helper lands in §1. Common case is now a 30-character call site. |
| L2 JSON Merge Patch vs. token-append workload | **Resolved (a).** "Patch batching assumption" subsection documents the ≥ 50 tokens/frame SDK guidance and explains the O(N²) wire-byte trap for naive per-token cadence. §11 adds explicit re-open trigger for `PatchKind::StringAppend` tied to cairn production measurement. Honest framing. |
| L3 ttl_ms typical-range guidance | **Resolved.** §4.1 adds the "5000–30000 ms" typical-range sentence with the too-small / too-large reasoning. |
| L4 mixed-mode Durable-trim footgun | **Resolved (b).** Doc-comment contract on `StreamMode::Durable` (in §1) names the hazard at the call site, where SDK users see it. §5 caveat also references the doc comment for discoverability. |
| L5 `TailVisibility::DurableOnly` naming | **Resolved.** Renamed to `TailVisibility::ExcludeBestEffort`. Extra doc-comment clarifies the "DurableSummary deltas are included because they have a durable backing (the summary Hash)" nuance — nice touch, prevents the inverse confusion. |

## New ergonomics check

1. **`summary_version` in `AppendFrameOutcome`.** Optional field, populated only for `DurableSummary`. The Rust idiom is `Option<u64>`; callers consuming other modes just ignore it. Fine. GREEN.

2. **`read_summary` return type.** `Result<Option<SummaryDocument>, Error>` — the `None` case is "no `DurableSummary` frame ever appended." Discoverable; matches the idiom used elsewhere in the SDK. GREEN.

3. **`read_execution_stream` added field.** §6.3 now specifies `latest_summary: Option<SummaryDocument>` on the merged view. `Option` means legacy consumers that don't care ignore it; new consumers pick it up. Non-breaking. GREEN.

4. **Doc discoverability for the batching assumption (L2 follow-through).** The batching guidance lives in §"Back-of-envelope savings" — in the RFC body, not in the SDK doc comments. For the SDK, `StreamMode::durable_summary()`'s doc comment (or `PatchKind::JsonMergePatch`'s doc comment) should include a one-liner: "If you're streaming tokens, batch updates at ≥ 50 tokens/frame — see RFC-015 §Back-of-envelope." This is a follow-up docs PR, not a RFC revision. Non-blocking.

5. **Name of the `ExcludeBestEffort` variant is slightly longer than the alternative (`Persisted`), but self-describing wins over brevity here.** Fine.

## Per-section final signals

§1 GREEN • §2 GREEN • §3.2 GREEN • §4.1 GREEN • §5 GREEN • §6.1 GREEN • §6.3 GREEN • §9 GREEN • §11 GREEN.

## Verdict

**ACCEPT.** All five ergonomics objections addressed with the minimum necessary changes. The follow-up doc-comment polish (L2 point 4 above) is SDK-docs work, not RFC work.
