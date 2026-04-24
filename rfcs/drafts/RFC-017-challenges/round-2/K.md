# K — round 2 challenge on RFC-017

Persona: correctness + schema + distributed invariants. Revision target: `rfc/017-consolidated` @ `03004a7`. Round-1 findings: 11 conceded + 1 partial.

## Bottom line

**ACCEPT**, with two narrow new findings on content the revision introduced. Round-1 is substantively resolved; the remaining items are surface-level and do not block merge of the RFC to accepted status.

## Round-1 resolution audit

| Finding | Status | Evidence |
|---|---|---|
| F1 trait-count arithmetic | RESOLVED | §2.3 bucket table sums 5+4+5+3+1+2 = 20; base 31 verified on `origin/main` (`grep -cE '^\s*(async\s+)?fn\s+\w+'` inside `trait EngineBackend` = 31). Summary §, §2.3, §3.1, §5, §15 all say **51 / +20** uniformly. Count is now internally consistent. |
| F2 `#[non_exhaustive]` discipline | RESOLVED | New §5.1.1 enumerates 16 new + 7 existing structs; spot-checked 3 existing (CreateExecutionArgs:21, CancelExecutionArgs:336, RevokeLeaseArgs:364) against `origin/main:crates/ff-core/src/contracts/mod.rs` — all present, all correctly located. Fluent-setter convention called out so reviewers don't block on missing Builders. |
| F3 T/M/H honesty | RESOLVED | §4 row 5 upgraded to H; row 13 upgraded to H with signature change; row 12 (boot) scored H with explicit ordering contract. `get_execution_result` staying M is author's ARGUE-BACK and is defensible (bytea column is mechanical). `create_execution` idempotency_ttl wire impact documented (`Option<Duration>`, `None` default preserves behaviour). |
| F4 `report_usage_admin` surfaced | RESOLVED | Promoted to first-class entry in §5 budget-admin bucket; counted in the 20; feature-flag placement pinned to `admin` with explicit single-binary compile-both note. |
| F5 `PendingWaitpointInfo` schema | RESOLVED | §8 now reframed as "schema rewrite + HMAC redaction"; 6 existing fields retained (`waitpoint_key`, `state`, `required_signal_names`, `activated_at`, `created_at`, `expires_at`); 1 dropped (`waitpoint_token`); 3 added (`execution_id`, `token_kid`, `token_fingerprint`). Matches round-1 option (a). |
| F6 D7 invariant gap | RESOLVED | §3.3 D7 now specifies `connect_with_metrics` idempotency; §4 row 12 documents boot ordering contract (FUNCTION LOAD → HMAC init → lanes SADD) on the trait doc-comment. |
| F7 429 `Retry-After` | RESOLVED | §6 contract mapping shows `ResourceExhausted { pool, max, retry_after_ms: Option<u32> }` + `ServerError::from` maps to `Retry-After` header; backend populates server-side so mapping layer needs no private-state access. |
| F8 `Server::scheduler` dual-dispatch | RESOLVED | §7 pins field-retirement to Stage D and ties it to the §9.0 hard-gate so no Postgres-panic window exists. Debug endpoints moved to `ValkeyBackend::debug_router()`. Q4 closed. |
| F9 silent 501s on Postgres | RESOLVED | New §9.0 `BACKEND_STAGE_READY` hard-gate; see new findings K-R2-N1 below. |
| F10 config ownership | RESOLVED | §11 moves `partition_config` / `lanes` / `waitpoint_hmac_secret` into `BackendConfig::Valkey`; legacy top-level fields validate against new path with `ServerError::ConfigConflict` on mismatch. |
| F11 open-question pruning | RESOLVED | §12 down from 5 → 1 (Q2 only); Q1/Q3/Q5 closed with rationale. |
| F12 `shutdown_prepare` under load | RESOLVED | §4 row 13 now H; §5.4 spec's per-backend contract with `grace: Duration`, no defaults; §14.8 promoted to mandatory Stage B CI test with four explicit assertions. |

Every round-1 finding is either actioned or has an on-the-record argue-back I accept. No smuggled middle paths.

## Per-section verdict (revised doc)

| § | Verdict | Delta from round-1 |
|---|---|---|
| 1 | GREEN | unchanged |
| 2 | GREEN | was YELLOW — count reconciled |
| 3 | GREEN | was YELLOW — D7 contract pinned |
| 4 | GREEN | was RED — scoring fixed, boot + shutdown rows added |
| 5 | GREEN | was RED — §5.1.1 + §5.4 close the discipline + spec gaps |
| 5.1.1 | GREEN | **new section**, correctly scoped |
| 6 | GREEN | was YELLOW — `retry_after_ms` pinned |
| 7 | GREEN | was YELLOW — field-retirement aligned to §9.0 hard-gate |
| 8 | GREEN | was RED — reframed as schema rewrite, 6 fields kept |
| 9 | YELLOW | was YELLOW — §9.0 hard-gate closes the silent-501 window BUT mid-cluster heterogeneous deploy is under-spec'd (K-R2-N1 below) |
| 9.0 | YELLOW | **new section**, see K-R2-N1 |
| 10 | GREEN | unchanged |
| 11 | GREEN | was YELLOW — config ownership specified |
| 12 | GREEN | was RED — Q2 is the one genuinely-owner-scoped tradeoff |
| 13 | GREEN | unchanged |
| 14 | GREEN | was YELLOW — §14.8 mandatory, four explicit assertions |
| 15 | GREEN | updated summary accurate |
| 16 | YELLOW | **new appendix**, see K-R2-N2 |

## New findings surfaced in the revision

### K-R2-N1 — §9.0 `BACKEND_STAGE_READY` gate is per-node, not cluster-aware (YELLOW)

§9.0 says `Server::start_with_metrics` validates `backend.backend_label()` against the `const BACKEND_STAGE_READY` slice at boot. This closes the single-node silent-501 window cleanly. It does **not** address the mid-cluster heterogeneous-deploy case: during a rolling Stage D → Stage E upgrade, an operator can have node A on the Stage-D binary (`BACKEND_STAGE_READY = ["valkey"]`) and node B on the Stage-E binary (`BACKEND_STAGE_READY = ["valkey", "postgres"]`), both pointed at the same Postgres instance (which is legal — Postgres is the shared state). Node B boots and serves `FF_BACKEND=postgres`; node A refuses to boot. A load balancer sitting in front sees 50% of requests land on the old node's "refuses to come up" state. Observable symptom: the deployment controller thinks node A is crashlooping; traffic concentrates on node B; node A's `Err(BackendNotReady)` is indistinguishable from a config error in ops alerting.

Not fatal — the default blast radius is "one node fails to join during the rolling upgrade, operator rolls it back or waits for full fleet promotion." But it is worth one sentence in §9.0 + a release-note callout: **the gate must be lifted fleet-wide before Postgres traffic arrives, not per-node during rollout.** Mixed-version fleets on Postgres are not supported during Stage D→E transition; the operator guide should say so explicitly.

**Minimal change → GREEN:** §9.0 gains one paragraph titled "Fleet-wide cutover requirement": "All `ff-server` instances in a deployment MUST be on the Stage-E binary before `FF_BACKEND=postgres` is set. Rolling-upgrade from Stage D → Stage E with Postgres enabled is unsupported — perform a valkey→valkey Stage D→E rolling first, then flip `FF_BACKEND=postgres` as a second rollout." Release notes + `POSTGRES_PARITY_MATRIX.md` header both carry this callout.

### K-R2-N2 — §16 `cancel_flow` paragraph pre-commits one arm of Q2 (YELLOW)

§12 Q2 is the one remaining owner-scoped question: header-only dispatch vs end-to-end dispatch for `cancel_flow`. §16 introduces "`cancel_flow` dispatch" as an operational-semantics paragraph and describes **both** arms ("Valkey (header-only lean)... Postgres (end-to-end lean)..."). The prose reads as if the two backends will resolve Q2 *differently*, i.e. Valkey keeps the server-side 150-line JoinSet fan-out and Postgres uses `SELECT FOR UPDATE SKIP LOCKED` atomically. That's not what Q2 is asking. Q2 is a single design choice: does the trait method promise header-only semantics (both backends) or end-to-end semantics (both backends)? A trait method cannot mean one thing for one impl and a different thing for another — that's the anti-pattern §13.6 rejects (downcast-style backend coupling hidden behind a trait).

§16 pre-committing to asymmetric per-backend semantics would smuggle the middle path back in. The owner call for Q2 is binary: header-only everywhere, or end-to-end everywhere, with the losing backend either doing server-side reconciliation (header-only) or absorbing the fan-out inside its impl (end-to-end).

**Minimal change → GREEN:** §16 `cancel_flow` paragraph rewrites the two "leans" as two *owner options* pending Q2, not as two *backend-specific outcomes*. The paragraph re-appears after Q2 resolves with the single-answer semantics.

### K-R2-N3 — Q2 framing can be narrowed to a single-answer technical question (advisory, not blocking)

Round-1 F11 successfully closed Q1/Q3/Q5 as not-really-owner-scoped. Q2 is more defensible as owner-scoped, but it can be narrowed further. The real tradeoff is:

- **Header-only** = ships faster, preserves current `Server::cancel_flow_wait` semantics byte-for-byte, leaves the 150-line JoinSet on `Server` (cross-backend — Postgres would have to rebuild the same fan-out logic in server code).
- **End-to-end** = cleaner contract, unblocks Postgres via a single trait method that does atomic cancel + reconcile via transaction, BUT every future backend must replicate that atomicity story or degrade to server-side reconciliation anyway.

Technical observation: the Valkey backend **cannot** do end-to-end atomically (no multi-key transaction across partitions without a cluster-wide lock), so end-to-end everywhere degrades to "trait method calls backend for header, then server iterates members." That is *operationally identical* to header-only with one extra round-trip. The Postgres-only win for end-to-end is a narrow transactional guarantee; the wider system does not get it.

Conclusion: the technically-correct answer is **header-only on the trait**, with Postgres optionally providing a stronger-than-header-only inherent method (not on the trait) that cairn can opt into. This is the pattern §13.6 already endorses for non-trait primitives.

**Not a blocking finding** — if the owner disagrees, end-to-end is still defensible. Flagging it because F11 established a pattern of "close questions that have single technical answers" and Q2 has one on further analysis.

## Scenarios re-checked against the revision

1. **Rolling v0.7.x → v0.8.0 with two `ff-server` replicas** (round-1 scenario 1): §11 `From<LegacyServerConfig>` + `ConfigConflict` validation addresses it. GREEN.
2. **`FF_BACKEND=postgres` during Stage B-C** (round-1 scenario 2): §9.0 hard-gate closes it. GREEN.
3. **Concurrent `rotate_waitpoint_hmac_secret_all` + handler migration** (round-1 scenario 3): §16 `rotate` paragraph + `admin_rotate_semaphore` on `Server` address serialisation; `backend_label()` in audit emit closes forensic correlation. GREEN.
4. **`claim_for_worker` vs `cancel_flow` race under Postgres SKIP LOCKED** (round-1 scenario 4): tied to Q2 resolution. Covered by K-R2-N2/N3.
5. **`MockBackend` in `shutdown_prepare` tests** (round-1 scenario 5): §5.4 + §14.8 mandate per-backend spec; mock is instructed via the spec to implement a real semaphore close, not `Ok(())` no-op. GREEN.
6. **Stage D scratch-project smoke** (round-1 scenario 6): §14.6 pins it, Stage D gate in §9.0. GREEN.
7. **`list_pending_waitpoints` pagination** (round-1 scenario 7): §8 `ListPendingWaitpointsArgs { after: Option<WaitpointId>, limit: Option<u32> }` landed. GREEN.

## Questions (down from 7 to 2)

- **Q-K-R2-1.** Does §9.0's `BACKEND_STAGE_READY` gate apply fleet-wide or per-node? Release-note guidance needs one sentence. (K-R2-N1.)
- **Q-K-R2-2.** §16's `cancel_flow` paragraph describes two per-backend "leans" — is that shipping that way (asymmetric trait semantics, §13.6 anti-pattern), or is §16's description a placeholder pending Q2? (K-R2-N2.)

## Verdict

**ACCEPT.** The revised doc closes 11 of 12 round-1 findings and honours the partial concede on F3. Two new YELLOW items (K-R2-N1 fleet-wide cutover paragraph, K-R2-N2 §16 cancel_flow framing) are surface-level copy + one-paragraph-scope changes, not design-level dissent. K-R2-N3 is advisory, not blocking.

The RFC is ready to proceed per persona K (correctness + schema + distributed invariants). Owner sign-off on Q2 is the only remaining gate.
