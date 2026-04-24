# RFC-017 Round-2 Challenger L — cairn operations + migration pragmatics

**Persona:** cairn SRE / consumer migration (unchanged from round-1).
**Target:** `rfcs/drafts/RFC-017-ff-server-backend-abstraction.md` @ `origin/rfc/017-consolidated` (commit `03004a7`).
**Position:** **ACCEPT with 3 minor follow-ups** — all 5 round-1 blocking deltas were resolved operationally (not cosmetically). New concerns below are small enough not to block; flag them for the round-2 pass or the implementation PRs.

---

## 1. Round-1 delta resolution — all 5 operationally resolved

| Round-1 delta | Status | Evidence |
|---|---|---|
| 1. `POSTGRES_PARITY_MATRIX.md` in-tree at Stage A + `test_postgres_parity_no_unavailable` CI gate | **RESOLVED** | §9.0 commits matrix at Stage A merge (not Stage D side-artifact); §9.0 + §14.1 wire the mechanical gate at Stage D; §9.0 goes further than I asked and hard-gates `FF_BACKEND=postgres` boot entirely until Stage E (closes the silent-501 window operationally). |
| 2. §9.3 "Consumer migration cadence" + trait-stability freeze B/C | **RESOLVED** | §9.3 landed with the exact table I asked for: cairn mock-impl update cadence = once at Stage A (lines 619), no-action rows for B/C (lines 620-621), dashboard-selector guidance gated on "every instance ≥ Stage A". Dual-version compat test `tests/http_compat_cross_stage.rs` promoted to mandatory CI (line 627). |
| 3. §8 `list_pending_waitpoints` pick functional-plus-warn OR hard-redact (not empty-string) | **RESOLVED** | §8 lines 511-516 chose **functional-plus-warn in v0.7.x → hard-remove in v0.8.0** — exactly my preferred lean. Bonus: server-side audit metric `ff_pending_waitpoint_legacy_token_served_total` gives cairn the dashboard signal to drive the migration. Sanity-checked against round-1 §4 — not the rejected middle path. |
| 4. Per-op cross-backend semantics appendix | **RESOLVED with small gap** | §16 covers rotate, cancel_flow (pending Q2), replay_execution + mandates `backend_label()` in audit emits. Gap detailed in §2.3 below. |
| 5. Q5 owner resolution before Stage A | **RESOLVED** | Closed in F11 of author response: `EngineBackend` is public + semver-protected by default; sealing is out of scope for RFC-017. The cairn-facing commitment is a statement in §10, not an open question. Satisfactory. |

**Summary: flip DISSENT → ACCEPT.** All five round-1 blockers are substantively addressed. The author response doc shows `CONCEDE` on 4/5 and `PARTIAL CONCEDE` on Q5 with reasoning I accept.

---

## 2. New concerns in the revision's new content (all non-blocking)

### 2.1 YELLOW — §9.3 trait-stability freeze has a written-in escape hatch

§9.3 line 625:

> one update at Stage A, one update at v0.8.0 (for signature revisions on Args/Result types if any surfaced during Stages B-D; each such revision is flagged explicitly in the stage PR)

This is the right honest caveat (Args/Result tweaks may surface during Stage B-D as Postgres impl work reveals semantic gaps) but it undermines the "freeze" wording two lines above. If three Args types end up changing during Stage B-D, cairn's mock has to re-chase at each stage regardless of the headline promise.

**Ask (follow-up, not blocker):** tighten the commitment — "trait *method signature* freeze during B/C; Args/Result struct-field additions are allowed behind `#[non_exhaustive]` + new constructors (per §5.1.1) and are guaranteed not to force cairn mock recompile." The §5.1.1 discipline already earns this; the RFC just needs to connect the two.

### 2.2 YELLOW — §9.0 hard-gate has no dev-mode escape

`FF_BACKEND=postgres` refuses to boot on Stages A-D. Correct default. But cairn's roadmap likely wants to **pilot** Postgres internally during Stage C-D — e.g. dev-cluster smoke runs, load-test rehearsal against a throwaway Postgres. Today they literally cannot, until v0.8.0 ships.

**Ask (follow-up):** add an explicit unsupported-dev-override env var, e.g. `FF_BACKEND_ACCEPT_UNREADY=1`, that bypasses `BACKEND_STAGE_READY` with a loud startup log warning (`ff_backend_unready_boot_total` metric increments). Not for prod; documented as dev/CI-only. Otherwise cairn's Postgres readiness feedback to us arrives only *after* v0.8.0 ships, which defeats the point of staging.

### 2.3 YELLOW — §16 cross-backend semantics appendix has two gaps

Covered: rotate, cancel_flow, replay_execution. **Missing:**

- **`shutdown_prepare`.** §5.4 specs it per-backend (Valkey: drain tail sessions + close stream semaphore; Postgres: `UNLISTEN *` + pool drain). That divergence is every bit as operator-visible as rotate — a graceful SIGTERM with `grace=30s` behaves differently on the two backends. Deserves one paragraph in §16: Valkey shuts down the 16 tail sessions of §14.8; Postgres waits for in-flight `NOTIFY`-driven waits to finish + LISTEN connections to close.
- **`list_pending_waitpoints` pagination consistency.** Valkey SSCAN cursor is best-effort-stable under concurrent SADD/SREM (may miss or double-count); Postgres `WHERE waitpoint_id > $after ORDER BY ...` inside a repeatable-read transaction is exactly-once. Reviewer-UI operators will see different churn behaviour during heavy concurrent waitpoint activity. Worth one line in §16.

**Ask (follow-up):** two paragraphs added to §16. ≤ 10 lines of RFC text.

### 2.4 GREEN — §11 relocation does not break existing Valkey consumers

Checked the `ServerConfig` shim path in §11 lines 719-724:

- `ServerConfig.partition_config`/`lanes`/`waitpoint_hmac_secret` stay as `#[deprecated]` top-level fields through v0.7.x.
- `From<LegacyServerConfig> for ServerConfig` populates `BackendConfig::Valkey(...)`.
- Runtime validation on double-set produces `ServerError::ConfigConflict` (no silent drift).
- v0.8.0 removes flat fields — semver-expected break.

This is the correct shape. Cairn's in-tree consumption is HTTP, so zero code delta; the `deploy-approval` example goes via `ff-sdk` and also unaffected. External direct-`ServerConfig`-construction consumers get compile errors at v0.8.0 pinned to a semver-major boundary, which is the deal `feedback_peer_team_boundaries` calls for.

### 2.5 GREEN — §14.8 shutdown test sufficient for cairn deploy pattern

Checked against cairn's known deploy pattern (rolling SIGTERM with grace budget). 16 concurrent tail sessions + 4 assertion classes (clean drain, no panic, 503 not 500 for mid-drain arrivals, `ff_shutdown_timeout_total` correctness) covers the failure modes cairn cares about. Cairn's actual peak concurrency is higher, but the CI gate doesn't need to match prod volume — it needs to exercise the race window, which 16 sessions does.

**One nit (non-blocking):** assertion (c) returns `HTTP 503` for mid-drain arrivals mapped from `EngineError::Unavailable`. That's the same error variant used for unready Postgres backends (§9.0). A shared metric counter cannot distinguish "backend unready at boot" from "backend draining during shutdown" without a label. Not urgent; flag for observability PR.

---

## 3. Method count — `51/+20` is now authoritative; author-response doc is stale

Minor housekeeping: the author-response doc at `round-1/author-response.md` line 15 still says "base 31 + 14 new = 45" as the locked figure. The RFC body (§2.3 line 162, §3.1 line 219, §15 line 848) consistently says **51 / +20**. The RFC body is authoritative; the author-response doc should be updated to match or annotated as superseded. Not a design finding — housekeeping only. Flag for the author.

---

## 4. Non-issues pre-empted

- **Q2 remaining open.** Fine. It's a genuine cross-cutting tradeoff (header-only vs end-to-end on trait) and §16's cancel_flow paragraph calls it out explicitly. Owner-scoped.
- **§9.0 hard-gate blocks Stage A-D Postgres testing entirely.** Partially addressed in §2.2 above; default behaviour is correct.
- **+20 methods "too aggressive for one RFC."** Not reopening. Trait drift §1.2 justifies the count; `#[non_exhaustive]` + default-impl discipline contains blast radius.
- **Shim lifetime "one minor release" too short.** Unchanged from round-1 — I flagged YELLOW there but didn't block. Author stuck with 1 release citing in-tree consumer scarcity. Defensible.

---

## 5. Flip-to-ACCEPT summary

All 5 round-1 blocking deltas resolved operationally. Three new YELLOW follow-ups (§2.1 freeze wording, §2.2 dev-mode override, §2.3 §16 gaps) are non-blocking and appropriate for the implementation PRs or an inline RFC tweak before owner merge.

**ACCEPT.**

---

## 6. Suggested round-2 edits (optional, ≤ 20 lines of RFC text total)

1. §9.3 line 625 — tighten freeze wording as described in §2.1 above.
2. §9.0 — document `FF_BACKEND_ACCEPT_UNREADY=1` dev-override path.
3. §16 — add `shutdown_prepare` + `list_pending_waitpoints` pagination paragraphs.
4. `round-1/author-response.md` line 15 — fix stale 14/45 figure or annotate as superseded.

None of these block further rounds; merge the revision as-is or fold them in. Operator + migration story is now tight enough for cairn to adopt.
