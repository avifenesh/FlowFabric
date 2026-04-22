# Round-6 CHALLENGE against RFC-012 namespace amendment (draft v2)

**Challenger:** Worker K. Style: adversarial, file:line-cited, parallels Round-4 Worker M.
**Scope of review:** `/home/ubuntu/FlowFabric/rfcs/drafts/RFC-012-amendment-namespace.md` v2.
**Tree head:** `origin/main` `69de6a5` (fetched fresh). Stages 0/1a/1b landed (`ca8ee39`, `81e08dc`, `6f54f9b`).

Bottom line: the amendment is **structurally sound but factually contaminated**. Two of its central claims about the code ("Rust-side keygen owns the prefix, Lua transparent" and "migration tool detects workers via `ff:lease:*`") are provably false against the tree. Several contract gaps (idempotency keys, HMAC secrets, bus events, rogue SCAN sites) are not covered by §R6.2's invariant wording. Validation regex deserves deliberate debate, not "locked." Fix the factual errors in-place and surface the debated ones to the owner.

---

## MUST-FIX

### #K1: "FCALL KEYS array propagation" claim is false — Lua has dozens of hardcoded `ff:` literal key constructions
Draft §R6.2 lines 42-43: *"Rust-side keygen owns the prefix, Lua `KEYS[i]` receives already-prefixed keys. The Lua library does NOT receive the namespace as a separate ARGV slot — it is transparent to Lua."*

Evidence (these are **genuine key constructions in Lua**, not debug comments or already-prefixed-argument checks):

- `crates/ff-script/src/flowfabric.lua:476` — `local stream_meta_key = "ff:stream:" .. tag .. ":" .. eid`
- `crates/ff-script/src/flowfabric.lua:1663` — `local att_key = "ff:attempt:" .. tag .. ":" .. A.execution_id .. ":" .. tostring(next_att_idx)`
- `crates/ff-script/src/flowfabric.lua:1920,2128,2558,3014` — `redis.call("PUBLISH", "ff:dag:completions", payload)` (4 sites)
- `crates/ff-script/src/flowfabric.lua:1994-1995` — active_index_key / suspended_key built as `"ff:idx:" .. tag .. ...`
- `crates/ff-script/src/flowfabric.lua:2669,2794,2894` — `"ff:idx:" .. tag_wl .. ":worker:" .. wiid .. ":leases"` (SREM sites)
- `crates/ff-script/src/flowfabric.lua:2678` — `terminal_key = "ff:idx:" .. tag .. ":lane:" .. lane .. ":terminal"`
- `crates/ff-script/src/flowfabric.lua:2714` — per-attempt key like #1663
- `crates/ff-script/src/flowfabric.lua:2942-2958` — 7 ZREM sites building `"ff:idx:" .. tag .. ":lane:" .. lane .. ":<blocked-axis>"`

And on the Rust side, `crates/ff-script/src/functions/lease.rs:97,121,138,139` also `format!("ff:idx:…")` directly — outside `ff-core::keys`. The §R6.4 Stage 1c scope says *"every `format!("ff:…")` call in `crates/ff-core/src/keys.rs`"* — but Stage 1c must also touch `crates/ff-script/src/functions/lease.rs` and the Lua library itself.

**Required fix.**
1. §R6.2 rewrite: do NOT claim Lua is transparent. State plainly: "Lua PUBLISH channel names and any Lua-literal-constructed key strings (currently ~18 sites) must also be threaded. The standard remediation passes the namespace as a dedicated ARGV slot (e.g. `ARGV[1] = namespace_prefix`, empty string for EMPTY namespace) and prepends it in Lua: `local NS_PREFIX = ARGV[1]; redis.call("PUBLISH", NS_PREFIX .. "ff:dag:completions", payload)`." (Alternative: move all literal-constructed keys Rust-side into KEYS, but that explodes KEYS cardinality on suspend/complete FCALLs.)
2. §R6.4 Stage 1c scope row: extend to "keygen in `ff-core::keys` + `ff-script::functions::lease` + `flowfabric.lua` literal-key/channel sites (grep-gated)."
3. §R6.8 Q11 "I have not pre-grepped this" is a red flag. Pre-grep **now** — Worker K has done it; the surface is finite and bounded. Fold the count into the draft.

This is the single biggest hole. An implementer reading v2 would ship Stage 1c with Lua literals still bare-`ff:` and declare the invariant satisfied — subscribers to `ff:dag:completions` would still see every namespace's completions.

### #K2: Migration-tool worker detection uses a key pattern that does not exist
Draft §R6.5 line 133: *"Tool errors if any FF worker is observably connected (SCAN for `ff:lease:*` with recent expiries)."*

Evidence: `grep -rn "ff:lease:" crates/` returns nothing. The actual lease-bearing keys are:
- `crates/ff-core/src/keys.rs:63-64` — `ff:exec:{p:N}:<eid>:lease:current`
- `crates/ff-core/src/keys.rs:68-69` — `ff:exec:{p:N}:<eid>:lease:history`
- `ff:idx:{p:N}:lease_expiry` (index ZSET, referenced in issue #122 body and `crates/ff-engine/src/scanner/unblock.rs`)

There is no `ff:lease:*` top-level prefix. The proposed SCAN pattern matches **zero keys** and will succeed trivially on any live cluster — the tool would proceed even with workers actively connected. That is the opposite of the intended guard.

Additionally, even with the *right* pattern, "recent expiries" is not a reliable liveness signal: a worker that crashed two minutes ago still has a live lease-expiry entry. Conversely, an idle worker between claims has no lease key at all. Lease presence is a proxy for *claims*, not for *connections*.

**Required fix.**
1. Correct the key pattern: use `SCAN MATCH "*ff:idx:*:lease_expiry"` (trailing, with namespace-agnostic leading wildcard since we're *in* the old namespace), and check `ZCARD`/`ZRANGEBYSCORE`.
2. Acknowledge this is a *heuristic*, not a guarantee. Document an operator-side affirmation step: the tool prints expected-idle evidence and requires a `--i-have-stopped-all-workers` acknowledgement flag, or integrates with a `CLIENT LIST` check (`CLIENT LIST TYPE normal` filtered by a well-known `CLIENT SETNAME ff-worker-*` prefix if one exists).
3. Or flip the design: tool does a `CLIENT KILL`-then-`CLIENT PAUSE` window and accepts the risk itself rather than trying to infer from keyspace.

### #K3: `§R6.2 "Persistent-state invariant"` wording omits several cross-namespace leak surfaces enumerated in the tree
Draft §R6.2 line 25: *"every persistent key/row/object `A` writes is disjoint from every persistent key/row/object `B` writes."*

That wording is correct. But §R6.2 then enumerates "what the invariant does NOT cover" (FUNCTION LOAD, contention) and silently drops several **cross-namespace-capable** surfaces that do exist in the tree and have non-obvious prefix requirements:

1. **Idempotency / dedup keys that embed caller-supplied strings.** `keys.rs:609-610`:
   ```
   pub fn idempotency_key(tag: &str, namespace: &str, idem_key: &str) -> String {
       format!("ff:idem:{}:{}:{}", tag, namespace, idem_key)
   }
   ```
   Note: this function already has a parameter *named* `namespace`, which is **not the Namespace type we are adding** — it's an execution-domain idempotency namespace. This is a naming collision waiting to happen. If Stage 1c adds a `ns_prefix` argument and keeps the existing `namespace: &str` parameter, every call site reader will conflate them. Also `usage_dedup_key` (`keys.rs:644`) — no existing `namespace` argument, but needs threading.

2. **HMAC-signed waitpoint tokens.** `keys.rs:275-276`: `pub fn waitpoint_hmac_secrets(&self) -> String { format!("ff:sec:{}:waitpoint_hmac", self.tag) }`. A waitpoint HMAC token issued under namespace A must not validate under namespace B. If the HMAC *secret* key is per-namespace (which it will be, once prefixed), tokens will naturally fail cross-namespace validation — **but only if the token-binding code reads the namespace-prefixed secret key, not the bare one**. This is a Stage 1c audit item; call it out.

3. **SCAN sites that cross partitions.** `crates/ff-engine/src/scanner/unblock.rs:635` documents a `SCAN MATCH ff:worker:*:caps` pattern (though the code today uses a central set). Any SCAN-based code must use `{namespace}:ff:…*` (wildcard scope bounded by prefix) rather than `ff:…*` (wildcards across all namespaces).

4. **Bus / completion events emitted by Lua PUBLISH.** Covered by #K1; restated here because §R6.2's pubsub invariant bullet (line 27) implies channel names are covered but the *implementation path* for Lua-side PUBLISH is not acknowledged.

**Required fix.** Expand §R6.2's "covered" section to enumerate:
- keys in `ff-core::keys` (exec/flow/lane/budget/quota/signal/idem/dedup/hmac/workerlease)
- Lua-literal-constructed keys (per #K1)
- Rust-literal-constructed keys outside `ff-core::keys` (per #K1 — `ff-script::functions::lease`, `ff-sdk::worker` lines 201/233/1572, `ff-sdk::admin:493`)
- pubsub channels (PUBLISH from Lua + SUBSCRIBE in `ff-engine::completion_listener:43`)
- SCAN patterns

Rename `idempotency_key`'s `namespace: &str` argument to `idem_domain: &str` to avoid collision with the new type. Cheap rename PR, should ship alongside Stage 1a.1.

### #K4: Stage 1c scope lists 3 key-context structs but the tree has 6
Draft §R6.4 Stage 1c row: *"thread `namespace` through `ExecKeyContext::new`, `IndexKeys::new`, `FlowIndexKeys::new`, and every `format!("ff:…")` call in `crates/ff-core/src/keys.rs`."*

Evidence — `crates/ff-core/src/keys.rs` has 6 key-context structs with `new(partition: &Partition, …)`:
- `ExecKeyContext::new` (line 26)
- `IndexKeys::new` (line 211)
- `FlowKeyContext::new` (line 324) — MISSING FROM DRAFT
- `FlowIndexKeys::new` (line 405)
- `BudgetKeyContext::new` (line 438) — MISSING FROM DRAFT
- `QuotaKeyContext::new` (line 495) — MISSING FROM DRAFT

Plus the free functions `idempotency_key` (609), `usage_dedup_key` (644), and likely others. Total "format!(\"ff:" hits in keys.rs = 92 (not the 83 figure from issue #122; tree has drifted since #122 was filed on HEAD `3c5828e`).

**Required fix.** §R6.4 Stage 1c scope: list all 6 key-context struct `new` functions and acknowledge free-function threading. Update the "~83 sites" estimate to 92 or re-grep on fused HEAD and cite the current figure.

### #K5: "Interaction with #90" claim mis-describes #90's state
Draft §R6.4 line 122: *"#90 moves the completion channel literal (`ff:dag:completions`) out of `ff-engine` into `ff-backend-valkey`. […] Stage 1c honours the prefix on the channel literal in its new home."*

Evidence — `gh issue view 90`: #90's proposed shape is *"Define trait `CompletionStream: Stream<Item=CompletionPayload>` + symmetric `Backend::publish_completion`. Postgres impl = LISTEN/NOTIFY."* That's a **trait replacing the pubsub channel abstraction**, not a file-move of a string literal. The channel literal ends up *eliminated from the public surface*, not just relocated.

Additionally: #90 explicitly enumerates *"4 Lua PUBLISH sites (flowfabric.lua:1854,2062,2448,2904)"* — these must still publish the completion event somehow. Those Lua PUBLISH sites are EXACTLY the sites flagged in #K1 (lines 1920, 2128, 2558, 3014 in current HEAD; #90's line numbers are pre-HEAD-drift but refer to the same 4 PUBLISH calls). So the "#90 merges first, Stage 1c threads namespace through" claim is slightly backwards: the Lua PUBLISH sites will **still exist** after #90 (Lua doesn't suddenly stop publishing — the Rust side just consumes through a trait); namespace threading applies to them regardless of whether #90 ships first.

**Required fix.** Rewrite §R6.4's #90-interaction paragraph:
- Drop "moves the literal into ff-backend-valkey."
- State correctly: #90 hides the channel behind a trait. Lua PUBLISH sites persist. Namespace prefix applies to the PUBLISH channel name regardless of #90's merge order. #90 and #122 are **concurrent, not sequenced**; whichever merges second rebases over the first.

### #K6: "Hash-tag preservation" encoding is ambiguous and the chosen encoding is not stated crisply
Draft §R6.2 lines 39-41 gives `tenant_a:ff:exec:{fp:5}:1234…`. Issue #122 body explicitly offers two forms: `{namespace}:ff:...` **or** `ff:{namespace}:...`. The amendment picks the first silently without stating rationale.

Both preserve the hash-tag (Valkey rule: bytes between first `{` and first `}` — neither `tenant_a:` nor `ff:tenant_a:` contains `{`). So the choice is a tooling convention, not a correctness question. But:

- `tenant_a:ff:…` (chosen) makes ops `KEYS *ff:*` a cross-namespace scan — useful for inspection, dangerous for accidents.
- `ff:tenant_a:…` (rejected) keeps the `ff:` sentinel first, which is friendlier to Redis/Valkey introspection tools that group by first segment.

Neither is wrong. Pick deliberately.

**Required fix.** §R6.2 add one sentence: *"We prepend the namespace BEFORE the `ff:` sentinel (`<ns>:ff:…`) rather than after (`ff:<ns>:…`) so that a single `FLUSHDB`-equivalent scan of one tenant (`SCAN MATCH <ns>:ff:*`) is unambiguous; the alternative `ff:<ns>:…` would require callers to write `SCAN MATCH ff:<ns>:*` which collides with legitimate segment-2 strings like `ff:idx:<ns>:…` under the current wildcard."* OR reverse the choice if the owner prefers the sentinel-first form. Either is defensible; the draft must declare the rationale.

---

## DESERVES-DEBATE

### #KD1: `[A-Za-z0-9_]{1,64}` character set rules out real-world tenant identifiers
Draft §R6.3 lines 54-55 + §R6.8 Q1 "locked": no dashes, 64-char limit.

**Tradeoff: correctness safety (Postgres unquoted identifier rule) vs. caller ergonomics.**

**Argument A (draft's position): keep locked.**
- Postgres requires double-quoting identifiers that contain dashes (`SET search_path TO "foo-bar"` vs `SET search_path TO foo_bar`). Unquoted is simpler for the Postgres backend.
- 64 chars is adequate for human-chosen names.
- Forcing callers to normalise is a one-time cost.

**Argument B: relax to allow dashes, extend length to 128.**
- UUIDs are 36 chars and are the natural tenant ID for many systems (including cairn, per `cairn.instance_id` which is ULID-shaped — 26 chars, safe — but other consumers may use full UUIDs with dashes).
- Subdomain-style names (`acme-prod`, `eu-west-1`) are idiomatic and will force callers to write lossy transforms (`acme-prod` → `acme_prod`).
- Lossy transforms are dangerous: `acme-prod` and `acme_prod` collapse to the same namespace, silently sharing state.
- 64 → 128 is free (hash-tag untouched, key-length budget #KD2 barely moved).
- Postgres double-quoting is a mechanical concern the backend handles once, not a public-API concern.

**Worker K lean: B.** Draft marks this "locked" via verbal owner answer, but the Postgres-unquoted-safe rationale is a backend implementation convenience that bubbles up into an API constraint that real callers will trip on. The better encoding: allow `[A-Za-z0-9_-]{1,128}`, reject leading `-`, backend double-quotes on the SQL side (one-time cost). Escalate to owner.

### #KD2: Key-length budget at 10M+ keys — real concern or non-issue?
Draft §R6.8 Q10: 80 bytes today + 65 bytes namespace (`<ns>:`) = 145 worst-case.

**Tradeoff: memory cost vs. namespace-length ergonomics.**

**Argument A (non-issue):** Valkey's sds overhead is ~3 bytes/key regardless of length; a 2x key-length increase at 10M keys = +650MB raw key memory. On a cluster sized for 10M executions the dataset is already ~10GB+ (payloads, results, streams); +650MB is ~6% overhead. Acceptable.

**Argument B (concern):** Index ZSETs store keys as members (every exec-id is a key, and every per-exec *index entry* duplicates the hash-tag + ns prefix in its own key name). At 10M execs across 64 partitions with ~20 index sites per exec lifecycle, you hit ~200M ZSET members. 65-byte prefix × 200M = +13GB. That is not 6%.

**Worker K lean:** Argument B is closer to reality for index-heavy deploys. At minimum, Q10's "document a keep-namespace-short operator note" should be tightened to "recommend ≤16 chars; budget 64 for flexibility." And the default validation cap should consider dropping to 32.

Escalate; owner should pick the cap with the 200M-members math in mind.

### #KD3: `Namespace::new("")` returns error — footgun or correct guard?
Draft §R6.3 line 87, §R6.8 Q3 "locked."

**Argument A (draft): catches bugs.** Caller reads `NAMESPACE` env var, forgets to set it, `.unwrap()`s → loud failure at startup. Better than silent unprefixed running.

**Argument B (footgun): legitimate empty-from-config path.** Caller's config file has an optional `namespace` field; unset = use EMPTY = today's behaviour. Natural code: `Namespace::new(config.namespace.unwrap_or_default())` — now panics on empty config. Forces every caller to write `if s.is_empty() { Namespace::EMPTY } else { Namespace::new(s)? }`.

**Worker K lean: draft is right, but ergonomics need a helper.** Add `Namespace::new_or_empty(s: impl Into<String>) -> Result<Self, NamespaceError>` that maps empty to `EMPTY` without error. Callers who want strict-mode use `new`; callers who want permissive-mode use `new_or_empty`. One extra fn, eliminates the footgun, preserves the guard. Amend §R6.3.

### #KD4: Stage 1a.1 as "silently ignored field" — anti-pattern or staged rollout?
Draft §R6.4 Stage 1a.1 row lines 114-115: *"Field is plumbed and silently ignored by all current callers."*

**Argument A (anti-pattern):** Callers set `BackendConfig::with_namespace("tenant_a")` expecting isolation. Stage 1a.1 merges. Callers deploy. Keys are still bare-`ff:`. Cross-tenant leak continues. Callers *believe* they are isolated. This is exactly the class of bug the cairn read-layer filter is defense-in-depth AGAINST.

**Argument B (staged rollout, defensible):** Stage 1a.1 is a small infra PR. No caller should be *using* it before Stage 1c. A prominent `#[doc(hidden)]` or explicit "not yet effective; see Stage 1c tracking issue" doc-comment on `with_namespace` mitigates. Unblocks Stage 1c planning without holding up for full keygen work.

**Worker K lean: B is fine IF `with_namespace` is gated.** Options (pick one):
- `#[doc(hidden)]` until Stage 1c.
- Runtime warning `tracing::warn!("Namespace is plumbed but not yet effective (Stage 1c pending). Do not rely on this for isolation.")` emitted on first non-EMPTY construction.
- Make `with_namespace` a `cfg(any())` stub until Stage 1c (landing as one-line-PR in Stage 1c).

Escalate for owner pick. Status-quo v2 ("silently ignored") is the worst of the three.

### #KD5: Rollback hazard documentation placement
Draft §R6.5 line 139: *"Document prominently in the Stage 1c PR body."*

PR body is ephemeral — anyone reading the code six months later won't see it. Better placements:
- CHANGELOG entry for the Stage 1c release.
- `cargo doc` comment on `Namespace` type: "WARNING: rolling back a namespace-bearing deploy to a namespace-ignoring build re-merges all keys into the unprefixed keyspace."
- Runtime startup warning on any deploy where the running binary's stage is < 1c but `BackendConfig.namespace != EMPTY` — Stage 1a.1 gates this.

Not a correctness question, but documentation-in-PR-body alone is weak.

### #KD6: Round-4 M6 fusion analogy — does it hold?
Draft §R6.7 #6: *"Same fusion rationale as round-4 M6."*

M6 (parent RFC line 569) fused **Stage 1 trait extraction** with **Stage 2 BackendConfig move** because the round-2 split forced consumers to migrate twice: once to the new trait, once to the new config type. The fusion eliminated a *consumer migration event*.

The amendment invokes M6 to justify fusing namespace-keygen into Stage 1c rather than RFC-013. Is this the same shape?

**Worker K analysis: it's similar but not identical.** M6 was about consumer-side migration events (API call site changes). This amendment is about backend-internal keygen changes that consumers mostly don't see (the surface change is one additive field on `BackendConfig`). The "double migration event" cost is lower here because Stage 1c's keygen is mostly opaque to consumers. What this amendment actually avoids by fusing is **two backfill events for operators** (once for Stage 1c's keygen refactor, once for RFC-013's namespace thread) — and since Stage 1c is the first time keygen is materially refactored post-RFC-011, co-threading namespace at that point is cheaper than re-touching 92 sites later.

The rationale is valid but the analogy mis-attributes the savings. **Fix suggested:** rewrite §R6.7 #6 as: *"Stage 1c already re-touches every `format!(\"ff:…\")` site as part of BackendConfig threading; co-threading namespace in the same pass avoids a second 92-site sweep. Similar in shape to round-4 M6's fusion rationale: avoid redundant migration sweeps when a natural co-touchpoint exists."*

---

## NITS

### #KN1: §R6.2 "Called out explicitly because a naive reading" is weak hedging
Line 30. Prefer declarative: *"The Valkey Function library is server-global; per-namespace function loads are explicitly rejected (§R6.7 #4)."* Drop the reader-modelling.

### #KN2: §R6.3 `Display` rationale says "config-file emission" but amendment excludes config schema
Line 88 says "config-file emission" is a `Display` use case. §R6.1 line 21 says config-file format is out-of-scope. Mildly self-contradictory. Prefer: *"`Display` for log/debug only; caller picks serde policy."*

### #KN3: "Scope estimate unchanged in category but wall-time extended"
§R6.4 Stage 1c row. This wording is squishy — either say "scope +N sites, +Mh" once #K4's full inventory lands, or say "scope to be re-estimated at Stage 1c planning" and commit to a number then. The halfway phrasing invites scope creep.

### #KN4: "FCALL library name" naming inconsistency
§R6.1 line 17 says `FUNCTION LOAD … "ff"`. §R6.7 #4 says `FUNCTION LOAD "ff_tenant_a"`. Minor: pick `FUNCTION LOAD ff` (no quotes — Valkey syntax is `FUNCTION LOAD <code>` with the library name inside the code's `#!lua name=ff` shebang, not a string arg). Pedantic but the current wording implies a string literal slot that doesn't exist.

### #KN5: §R6.2 hash-tag rule statement
Line 37: *"Valkey's hash-tag rule extracts bytes between the first `{` and first `}` in the key."* Spec-correct phrasing: *"between the first `{` and the FIRST `}` AFTER it, provided the bracket span is non-empty."* Empty `{}` does not form a hash-tag — if a namespace somehow ended with `{` and the rest started with `}`, the adjacent `{}` would be empty and the whole key would hash to its full byte-sum. Validation regex in the draft prevents this (brace-free), so it's a nit. But the sentence as written misses the empty-span case and is worth tightening.

### #KN6: `NamespaceError` lacks `EmptyInput` constructor helper
Minor ergonomics: `impl From<&str> for Namespace` would be a natural addition for test/const-context construction. Defer to Stage 1a.1 PR review; not RFC-level.

---

## Affirmative no-finding notes

- **Hash-tag preservation mechanics (§R6.2 lines 37-41):** correct. Validation regex forbids braces; prepending a non-brace string preserves the first-`{`/first-`}` extraction. `crates/ff-core/src/keys.rs:10-14` and surrounding hash_tag comments confirm the invariant holds.
- **§R6.7 alternative #3 (hash-tag namespacing):** correctly rejected; well-argued.
- **§R6.7 alternative #5 (`SELECT N`):** correctly rejected on Cluster grounds.
- **§R6.4 disjointness with #91 and #92:** verified disjoint (file sets don't overlap with `keys.rs`).
- **Peer-team boundary with cairn's read-layer filter (§R6.1 final sentence + §R6.8 Q6):** correct and well-framed; matches `feedback_peer_team_boundaries.md` memory.

---

## Summary for owner

Fix #K1–#K5 in-place before landing; each has file:line evidence of a factual error in v2. #K6 is a wording fix; pick an encoding and justify.

Debate #KD1 (validation regex — worth revisiting despite the "locked" marker; dashes + UUID-sized namespaces are real-world) and #KD4 (Stage 1a.1 silent-field posture) before Stage 1a.1 opens as a PR.

Nits are optional polish.

Verdict: **CHALLENGE — amendment is not ready to land as v2.** Expect v3 with #K1–#K5 resolved and #KD1/#KD4 explicitly owner-adjudicated.
