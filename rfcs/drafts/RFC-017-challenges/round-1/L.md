# RFC-017 Round-1 Challenger L — cairn operations + migration pragmatics

**Persona:** cairn SRE / consumer migration. Written *before* reading K.
**Target:** `rfcs/drafts/RFC-017-ff-server-backend-abstraction.md` @ `origin/rfc/017-consolidated`.
**Position:** **DISSENT** — the design is defensible; the operator + migration story is thin enough that a cairn-fabric SRE asked to adopt v0.8.0 will push back. Dissent flips to ACCEPT with the specific deltas in §6.

---

## 1. Section scorecard

| Section | Grade | One-line verdict |
|---|---|---|
| §1 Motivation | GREEN | Pain is real; Wave 8 gap framed correctly. |
| §2 Handler inventory | GREEN | 23 call-sites + regional density map is auditable; good. |
| §3 Design shape | GREEN | `Arc<dyn>` single-trait is the right shape. |
| §4 Per-handler table | YELLOW | T/M/H labels are informed by author effort, not consumer-risk. See §3 below. |
| §5 New trait methods + default impls | **RED** | The `Unavailable { op }` default-impl story is the correct trait-level call **and** a migration trap for cairn at Stage A. See §2 below. |
| §6 Valkey-primitive encapsulation | GREEN | Clean. |
| §7 Scheduler location | GREEN | Backend-scoped + `claim_for_worker` on trait is right. |
| §8 HMAC redaction | YELLOW | Security posture correct; v0.7.x "empty-string + deprecation header" is an operational trap. See §4 below. |
| §9 Stage plan cadence | **RED** | Wall-clock 0-9+ weeks with a two-backend CI matrix is aggressive; no guidance on dual-version compat for cairn during the window. See §5 below. |
| §10 Semver | GREEN | v0.8.0 is justified. |
| §11 Back-compat `start` | YELLOW | `start_with_backend` is good; `ServerConfig` shim lifetime "one minor release" under-sells the consumer ask. |
| §12 Open questions | YELLOW | Q2 (cancel_flow dispatch) and Q5 (trait stability commitment) are **operator-visible**, not purely owner-scoped. |
| §13 Alternatives rejected | GREEN | Rejections are load-bearing. |
| §14 Testing | YELLOW | No cairn-migration rehearsal test; scratch smoke is author-side, not consumer-side. |

---

## 2. RED — `EngineError::Unavailable { op }` as default-impl is an operational landmine

RFC §5 states every new data-plane method lands with a default impl returning `EngineError::Unavailable { op: "..." }`, and §5.3 acknowledges Postgres ends Stage A with ~7 `Unavailable` methods, with the "hard block" enforcement deferred to Stage D.

Consumer perspective: **a cairn deployment that pulls v0.8.0 on day one of Stage A's ship sees ff-server happily serve some routes and 501 others, depending on `FF_BACKEND`.** The failure mode is a runtime HTTP error, not a compile error — the trait "works," but the deployment is a minefield.

Two asks:

1. **Document the Postgres parity matrix as a file in-tree, not alongside a Stage D PR.** RFC §5.3 references `POSTGRES_PARITY_MATRIX.md` as a side-artifact. Lift it into the RFC body or commit it at Stage A merge with the exhaustive method-by-method status. Cairn's runbook reads "is method X live on `FF_BACKEND=postgres`?" — that answer must be greppable, not PR-archaeology.

2. **Gate Stage D merge on `Unavailable`-count == 0 for the publicly-HTTP-exposed trait subset.** The RFC already says this ("hard block, not a follow-up") — but the CI enforcement is vague. Concrete proposal: a `test_postgres_parity_no_unavailable` test that iterates every HTTP handler and asserts `backend.<method>(...)` never returns `EngineError::Unavailable`. Mechanical, machine-enforceable.

Without these, the "v0.7.x additive, v0.8.0 cleanup" promise is partially a lie: a Postgres operator on v0.7.x *already* hits `Unavailable` for methods whose trait signature shipped but whose impl didn't.

---

## 3. RED — Stage cadence, wall-clock, and dual-version consumer compat

§9 gives 0-2 / 2-3 / 3-5 / 5-9+ weeks per stage. No guidance on:

- **Can cairn run v0.7.x (pre-refactor) in parallel with early v0.8 adoption?** Practically — cairn-fabric's production is on some v0.x today. A 10-week migration window means cairn must either (a) pin to v0.7.N-pre-RFC and not pick up unrelated fixes, or (b) upgrade to v0.7.N+M during the RFC window and accept the additive trait surface. The RFC is silent on which path they should take.

- **Trait additions across v0.7.x patch releases.** The RFC adds 14 trait methods at Stage A on a v0.7.x tag. Per `feedback_feature_flag_propagation`, any downstream crate with `impl EngineBackend for Mock` breaks on every stage. Cairn has test mocks — do they update 4 times (A/B/C/D) or 1 time (at v0.8.0)?

- **`backend_label()` dashboard rollout.** §5 makes it unconditional. Good. But a cairn deployment mid-migration has a mix: some ff-server instances on v0.7.0 (no `backend_label`), some on v0.7.N+RFC-017-Stage-A (label present, value `"valkey"`), eventually some on `"postgres"`. Dashboards with `ff_backend_kind{backend=...}` selectors break on the first cohort. No doc says "deploy all instances past Stage A before adding the label to selectors."

Ask:

1. **Add §9.3 "Consumer migration cadence."** Explicit table: cairn runs v0.7.0-baseline until Stage A lands, then can adopt as-desired; label-based dashboards MUST wait until every instance is ≥ Stage A; SDK mock impls MUST add the 14 methods once at Stage A (with `Unavailable` defaults fine for their mocks).

2. **Commit to a minimum v0.7.x patch-cadence freeze between Stage A ship and Stage D ship.** e.g. "no further trait additions during Stages B/C." Otherwise cairn's mock-impl churn is a function of our velocity, not their readiness.

3. **Dual-version compat claim should be explicit.** A v0.7.N-Stage-A `ff-server` must serve HTTP identically to v0.7.N+1-Stage-B for every non-migrated handler. This is implied by "zero behaviour change" but not tested. Add a cross-stage HTTP diff test.

---

## 4. YELLOW — `list_pending_waitpoints` v0.7.x "empty string + deprecation header" is operationally worse than a hard removal

§8 proposes: v0.7.x keeps the `hmac_token` wire field, populates it with `""`, emits `Deprecation: ff-017` header. v0.8.0 removes the field.

Cairn SRE read: any consumer parsing the field today gets `""` instead of a token **and no error**. If their code path is "retrieve HMAC, hit resume endpoint with it" — that now fails at the resume step with an authn error, not at the list step. The deprecation is **silently broken for existing consumers** on v0.7.x, not just warned.

Better pattern (from prior rotations):

- v0.7.x: keep the field populated with the real token (status quo) + emit `Deprecation: ff-017` header + log a server-side audit event every time the field is served. Gives cairn a dashboard signal: "our code still reads the legacy field; migrate before v0.8."
- v0.8.0: remove field.

If the security concern is "any list caller sees the token," that's a pre-existing bug to fix *inline*, not a bug to silently half-fix then fully-fix across a semver boundary. Either redact immediately (break v0.7.x consumers — acceptable per `feedback_no_preexisting_excuse` if we own it) or keep functional + warn. The middle path chose in §8 is the worst of both.

Ask: pick one. My lean: **keep functional + warn in v0.7.x; remove in v0.8.0.** If the security boundary can't tolerate one release of continued exposure, redact in v0.7.x and call it a point-release break.

---

## 5. YELLOW — `rotate_waitpoint_hmac_secret` operational semantics cross-backend

Cairn runbook today: the admin rotate runs per-partition with a fan-out concurrency limit, and the ff-server admin semaphore guarantees one rotation at a time cluster-wide. This is the procedure documented on their SRE wiki.

§9 Stage B lists "admin rotate fan-out moves inside Valkey impl; admin semaphore + audit emit stay on `Server`." Fine at the code level. But:

- **Postgres semantics are not specified.** `rotate_waitpoint_hmac_secret_all` on Postgres is presumably a single `UPDATE` over the secrets table (or one `UPDATE` per partition rowset) — not fan-out-over-N-partitions. The trait hides this; cairn's runbook doesn't know. Does the rotation take seconds (Valkey fan-out) or milliseconds (single transaction)? Does partial-failure (N-1 of N partitions rotated) exist in the Postgres path? What does it mean operationally?

- **`backend_label()` visibility in the audit event.** If the audit row doesn't carry backend kind, cairn's post-incident forensics can't tell which rotation semantics ran.

Ask: §7 (scheduler location) spells out per-backend semantics honestly. §9/§11 do not do the same for admin ops. Add a "per-op cross-backend semantics" appendix covering rotate, `cancel_flow` dispatch (§12 Q2), and `replay_execution` (Hard in §4). One paragraph each on failure modes + runbook delta.

---

## 6. DISSENT → ACCEPT deltas (minimal, non-negotiable set)

Flip-to-ACCEPT requires:

1. **§5.3 tightening:** in-tree `POSTGRES_PARITY_MATRIX.md` committed at Stage A merge; `test_postgres_parity_no_unavailable` mechanically enforced at Stage D gate.
2. **§9.3 new subsection:** explicit consumer migration cadence (cairn v0.7.x-baseline → Stage A adopt → dashboard-selector advice → SDK mock churn once, not four times) + trait-stability freeze during B/C.
3. **§8 v0.7.x redaction behaviour:** pick *functional-plus-warn* or *hard-redact*, not *empty-string*.
4. **New appendix on per-op cross-backend operational semantics:** rotate (fan-out vs transactional), cancel_flow dispatch (Q2 once owner decides), replay_execution KEYS variadic behaviour. Each ≤ 1 paragraph.
5. **§12 Q5 owner resolution before Stage A merge, not after.** External `EngineBackend` impl stability is a cairn-facing commitment; it has to be in the RFC before we land the trait surface, not figured out post-hoc.

Everything else in the RFC is defensible. The core design (single trait, `Arc<dyn>`, Valkey-primitive encapsulation, v0.8.0 semver) is correct. The gaps are where cairn's SRE lives, not where the authors' code lives.

---

## 7. Non-issues (pre-empting pushback)

- **14 methods "too many."** No. RFC-012 R7 set precedent; backend drift §1.2 justifies the count. A trait of 45 methods that models a real surface is fine.
- **`start_with_backend` exposure risk.** No; it's gated on the consumer wanting to inject. Fine.
- **Single-trait vs split.** §13.7 rejection is sound.
- **Generic `Server<B>`.** §13.4 rejection is sound.
- **`MockBackend` ergonomics.** §14.5 is the single biggest win for cairn test authors. Ships in Stage A — perfect.

---

**Summary: DISSENT, 5 blocking deltas above, all operational + consumer-facing, none design-invalidating.**
