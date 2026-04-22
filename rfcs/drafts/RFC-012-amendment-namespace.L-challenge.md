# Round-7 CHALLENGE against RFC-012 namespace amendment (draft v3)

**Challenger:** Worker L. Independent adversarial review post-K.
**Tree head:** `origin/main` `69de6a5`.
**Scope:** flaws K missed + new flaws v3 introduced. No repetition of K's findings.

Bottom line: v3 resolves K's major factual errors, but the fix for #K1 (ARGV[1] = namespace) was introduced without checking existing ARGV usage in the Lua library. **That claim is wire-breaking.** Several §R6.2.2 counts are also wrong in the other direction from v2 (v3 inflates 83 → 92 with no evidence; miscategorises methods as free functions). Additional cluster-slot, whitespace-input, and Postgres-query realism flaws below.

---

## MUST-FIX

### #L1: v3's `ARGV[1] = namespace_prefix` collides with every existing FCALL's positional ARGV layout
Draft §R6.2.4 line 68: *"FCALL signatures gain a dedicated ARGV slot carrying the namespace prefix: `ARGV[1] = namespace_prefix`."*

Evidence — every registered Lua function in `crates/ff-script/src/flowfabric.lua` already uses `args[1..N]` as the caller-supplied positional argument array:

- `flowfabric.lua:929-952` `ff_renew_lease`: `args[1]` = execution_id, `args[2]` = attempt_index, `args[3]` = attempt_id, `args[4]` = lease_id, `args[5]` = lease_epoch, `args[6]` = lease_ttl_ms, `args[7]` = lease_history_grace_ms.
- Same pattern across all 47 `redis.register_function` calls (lines 908, 929, 1049, 1130, 1256, 1560, 1795, 1945, 2148, 2256, 2366, 2579, 2828, 3036, 3149, 3255, 3314, 3381, 3492, 3605, 3872, 3983, 4081, 4293, 4370, 4490, 4547, 4862, 5006, 5180, 5342, 5408, 5489, 5578, 5633, 5703, 5785, 5838, 5931, 6025, 6094, 6205, 6343, 6369, 6467, 6565, 6750, 6793, 6869, 7027…).
- Rust-side callers in `crates/ff-script/src/functions/*` pack `ARGV` positionally to match. `ff_issue_claim_grant` specifically is documented at `flowfabric.lua:12` as `ARGV[9]`.

Prepending `ARGV[1] = namespace_prefix` shifts **every single positional index** in **every function** AND every Rust caller. That is not "the ARGV-passed prefix" — it is a wire break across the full FCALL surface. The alternative rejected in §R6.2.4 ("move Lua-literal keys into KEYS[]") is correctly rejected, but v3's replacement is under-specified.

**Required fix.** Two options:
- **(Preferred) Append, don't prepend.** Each FCALL gains a trailing ARGV slot `args[#args]` or a documented fixed-name `NS_ARGV_SLOT` sentinel that callers place at a stable position *per function* (e.g. `args[N+1]` where N is the pre-amendment arg count). State this explicitly in §R6.2.4; forbid `ARGV[1]` shift.
- **Alternative.** Adopt a `table.remove(args, 1)` shim at the top of every function that pops the prefix, then keeps the rest positional. But that still requires every Rust caller to inject the prefix at index 0 — mechanical but error-prone, and the "caller forgot" failure mode is silent key-path corruption.

Either way, v3 cannot state `ARGV[1] = namespace_prefix` without qualifying that every existing `args[i]` access shifts to `args[i+1]`. Land the fix Stage-1c-atomically (single PR that updates Lua + every Rust caller) or field a grep-gate CI check.

### #L2: §R6.2.2 count table is wrong in the other direction from v2
Draft §R6.2.2 line 48: *"~92 (was ~83 in issue #122; tree drifted)"*.

Evidence — `grep -c 'format!("ff:' crates/ff-core/src/keys.rs` on HEAD `69de6a5` returns **83**. Not 92. The tree has NOT drifted from the issue's count; v3 invented the 92 figure. K's challenge claimed "92 (not 83)" and v3 adopted that number without re-verifying.

Additionally, §R6.2.2 row 2 lists `waitpoint_hmac_secrets` and `signal_dedup` as **free functions**. They are not:
- `keys.rs:156`: `pub fn signal_dedup(&self, …)` — **method on `ExecKeyContext`** (receiver `&self`).
- `keys.rs:275`: `pub fn waitpoint_hmac_secrets(&self) -> String` — **method on `ExecKeyContext`**.

Actual free functions in `keys.rs` (grep `^pub fn`): `budget_attach_key` (471), `budget_resets_key` (476), `budget_policies_index` (482), `quota_policies_index` (535), `quota_attach_key` (540), `lane_config_key` (547), `lane_counts_key` (552), `worker_key` (557), `worker_caps_key` (568), `workers_index_key` (578), `workers_capability_key` (583), `lanes_index_key` (588), `global_config_partitions` (594), `namespace_executions_key` (601), `idempotency_key` (609), `noop_key` (618), `tag_index_key` (623), `waitpoint_key_resolution` (628), `usage_dedup_key` (644) — **19 free functions**, not "~8". V3 under-counts by more than half.

§R6.2.2 row 1 cites `ExecKeyContext::new` at line 26 etc. — these lines are the `impl` block openers (25/210/323/404/437/494), not the `new` definitions (26/211/324/405/438/495). Minor but cites should be precise since the whole table's credibility hinges on exact line refs.

**Required fix.**
1. Change "~92" to "83" (or re-grep and cite exact, including `grep` incantation).
2. Move `signal_dedup` and `waitpoint_hmac_secrets` out of the "free functions" row into the "key contexts" row (both are `ExecKeyContext` methods).
3. Raise the free-function count to 19 and enumerate them, or at minimum change "~8 fns" to "~19 fns" with a note that Stage 1c re-greps.
4. Fix line cites for `impl` vs `new`.

### #L3: `new_or_empty` permissive claim is leaky on whitespace-only input
Draft §R6.3 lines 111-114:
```
pub fn new_or_empty(s: impl Into<String>) -> Result<Self, NamespaceError> {
    let s = s.into();
    if s.is_empty() { Ok(Self::EMPTY) } else { Self::new(s) }
}
```
Draft §R6.3 line 137: *"callers who want permissive-mode use `new_or_empty`."*

Failure case: `Namespace::new_or_empty("   ")`. `"   ".is_empty()` = false. Falls through to `Namespace::new("   ")`, which per the `[A-Za-z0-9_-]{1,64}` regex rejects space character with `NamespaceError::InvalidChar(' ')`. A caller who wrote `Namespace::new_or_empty(std::env::var("NAMESPACE").unwrap_or_default())` and whose operator set `NAMESPACE=" "` (quoted whitespace, common in container env setups) gets an `Err`, not `EMPTY`. Permissive mode is not permissive.

**Required fix.** Either:
- `new_or_empty` pre-trims: `let s = s.into().trim().to_owned(); if s.is_empty() { EMPTY } else { new(s) }`. Document that whitespace is stripped for permissive mode only.
- OR state explicitly in §R6.3 that `new_or_empty` is *only* permissive for genuinely empty (zero-length) strings and whitespace-only input errors. Caller is responsible for `.trim()`. The rationale in v3 that claims "eliminates the footgun" is partially false without this clarification.

### #L4: Cluster hash-tag interaction not discussed for keys with no brace
Draft §R6.2.3 line 60: *"Both forms are correctness-equivalent (Valkey hash-tag extraction between first `{` and first `}` is unaffected by either prefix shape since the validation regex forbids `{`/`}` in the namespace)."*

That reasoning only holds for keys that **have** hash-tags. Not every FF key has braces:

Evidence — `crates/ff-core/src/keys.rs`:
- `:578-579` `workers_index_key() = "ff:idx:workers"`
- `:588-589` `lanes_index_key() = "ff:idx:lanes"`
- `:594-595` `global_config_partitions() = "ff:config:partitions"`

These are global/config keys with no `{…}` hash-tag. Cluster routes them by CRC16 of the entire key. Prefixing with `<ns>:ff:idx:workers` changes the slot. That is **fine for correctness** (each namespace gets its own global key, as intended) but:

1. Cross-cluster migration tool semantics change: `COPY` between differently-slotted keys on Cluster requires same-slot or cross-shard is rejected (`COPY` fails cross-slot on Cluster). The old key `ff:idx:workers` and the new `<ns>:ff:idx:workers` almost certainly hash to different slots.
2. Any operator SCAN that expected `ff:idx:workers` to be in a known slot breaks.
3. Future backend design (sharded Postgres) needs to know these keys are namespace-scoped, not cluster-global.

V3 says migration uses `COPY`. `COPY` on Valkey Cluster **fails cross-slot by default** (`-CROSSSLOT` error). Unless the operator manually MIGRATEs the slot first or uses a non-cluster Valkey, the `--i-have-stopped-all-workers + COPY + DEL` recipe doesn't work for the no-hash-tag keys.

**Required fix.**
1. §R6.2.3 or §R6.2.5 add a subsection: *"No-hash-tag keys (ff:idx:workers, ff:idx:lanes, ff:config:partitions) are renamed but change slot under the namespace prefix. This is correct (per-namespace globals) but affects migration tooling."*
2. §R6.5 migration-tool section acknowledge that `COPY` on Cluster is slot-bound and DOES NOT work cross-slot for these keys — the tool must either (a) `DUMP`+`RESTORE` through the client or (b) refuse to run against Cluster in v1 and document operator manual-migration for Cluster deploys.
3. Confirm `COPY` is available in the targeted Valkey minimum version (Valkey 7.2+ / Redis 6.2+). State the min version explicitly.

### #L5: Rollback posture — v3 says keys "stranded under old prefix," but in-flight leases/suspensions are worse than stranded
Draft §R6.5 line 191: *"deploys that ran with non-EMPTY namespace have their keys stranded under the old prefix, and the rolled-back binary will create new keys under no prefix — if multiple namespaces were sharing the Valkey, they now all collide in the unprefixed keyspace."*

Failure modes v3 doesn't enumerate:
- **In-flight leases under old prefix.** A worker holding a lease on `<ns>:ff:exec:{fp:5}:EID:lease:current` when the binary is rolled back to a namespace-ignoring build: the rolled-back binary renews/revokes at `ff:exec:{fp:5}:EID:lease:current` (different key). The old lease sits orphaned at TTL (minutes to hours). The new binary sees no lease, potentially re-claims the same execution on another worker — **double-execution risk**, not "stranded".
- **Suspended executions.** Suspension state at `<ns>:ff:exec:…:suspension:current` is invisible to rolled-back binary. On resume-signal delivery the rolled-back binary finds no suspension record; signal either NACKs or silently drops depending on code path. **Stuck executions**, not "stranded keys".
- **HMAC-signed waitpoint tokens.** Tokens minted under `<ns>:ff:sec:{p:N}:waitpoint_hmac` fail HMAC validation under `ff:sec:{p:N}:waitpoint_hmac` (different secret key, even if same bytes — key identity matters for the read). All in-flight waitpoints become un-resolvable. User-visible as "resume never fires."
- **Active-index ZSETs.** Rolled-back binary SCANs `ff:idx:{p:N}:active` and finds nothing (it's under old prefix); workloads that were mid-flight are invisible to operators, and the Valkey memory isn't freed because old keys still have TTL.

**Required fix.** §R6.5 rollback bullet should list these four concrete modes, not just "collide in the unprefixed keyspace." Operators must understand rollback = suspend-all-work + drain + key-migrate, not just "restart."

### #L6: Postgres `information_schema.schemata` claim needs qualification
Draft §R6.4 Stage 2 line 168: *"`PostgresBackend::connect` verifies pre-existing via `SELECT 1 FROM information_schema.schemata WHERE schema_name = $1` (connection-search-path-agnostic)."*

`information_schema.schemata` is NOT unconditionally connection-search-path-agnostic in the sense implied:

1. `information_schema.schemata` filters by **privilege** — it only returns schemas the current role has USAGE or any non-zero privilege on. A role connecting to check a schema it doesn't yet have USAGE on (e.g. a bootstrap/admin role checking a per-tenant schema owned by another role) will see zero rows even if the schema exists. The result depends on `CREATE SCHEMA … AUTHORIZATION …` history and subsequent `GRANT USAGE`. Not schema existence per se.
2. Row-Level Security (RLS) doesn't apply to `information_schema`, so not a concern there.
3. `pg_namespace` (`SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1`) is the privilege-independent form and is what PG tools generally use for existence checks.

**Required fix.** Change §R6.4 Stage 2 to `pg_catalog.pg_namespace`, OR document the privilege-coupling explicitly:
*"Namespace check uses `pg_catalog.pg_namespace` to avoid the `information_schema` privilege filter — we want existence, not 'existence + this role has USAGE'."*

Also state what connection role the check runs as, and whether Stage 2 requires that role to have `pg_read_all_settings`-equivalent visibility.

### #L7: `Namespace` type placement is unspecified
Draft §R6.3 lists the type but does not say what crate owns it.

Evidence — `BackendConfig` lives in `crates/ff-core/src/backend.rs:611`. If `Namespace` is a sibling type (reasonable), it lives in `ff-core`. `crates/ff-core/Cargo.toml:18` confirms `thiserror = { workspace = true }` is already a dep, so the `#[derive(thiserror::Error)]` on `NamespaceError` compiles. Good — but the RFC doesn't say this, and an implementer reading v3 could put `Namespace` in `ff-sdk` or a new `ff-types` crate.

**Required fix.** §R6.3 add one line: *"`Namespace` and `NamespaceError` live in `ff-core` (alongside `BackendConfig`). `ff-core` already depends on `thiserror` via the workspace."*

Also: `BackendConfig::with_namespace` is called out as an added builder, but `BackendConfig` today has no `with_*` methods — the existing constructor is `BackendConfig::valkey(host, port)`. Confirm the builder pattern is new or adopt `BackendConfig::valkey(...).with_namespace(...)` as the new canonical idiom, and document non-Valkey constructors (future `BackendConfig::postgres(...)`).

---

## DESERVES-DEBATE

### #LD1: `CLIENT SETNAME` / `CLIENT LIST` operator visibility not threaded
K's #K2 suggested `CLIENT LIST` as a migration-tool liveness detection path. V3 adopted `--i-have-stopped-all-workers` and moved on, but did not address the broader point: a namespaced deployment benefits from workers identifying themselves via `CLIENT SETNAME ff-worker-<namespace>-<wiid>` so operators running `CLIENT LIST` can see which tenant's workers are connected.

Argument A (add): cheap (~1 line per connection setup), gives operators grep-able output, addresses K's migration-tool concern properly.
Argument B (defer): connection-identification is orthogonal to key namespacing; separate RFC when demand appears.

**Worker L lean: defer, but call out in §R6.6 "does NOT do" so it's a known follow-up and not a silent gap.** Add: *"Does NOT modify CLIENT SETNAME / connection identification. A future RFC may propose per-namespace worker naming for operator CLIENT LIST visibility."*

### #LD2: `usage_dedup_key(hash_tag, dedup_id)` has no `tag` arg — signature change is a break for 4 external call sites
Draft §R6.2.2 lists `usage_dedup_key` as a free function to thread.

Evidence — `keys.rs:644`: `pub fn usage_dedup_key(hash_tag: &str, dedup_id: &str) -> String`. No `tag` param (unlike `idempotency_key`).

Callers (all must be updated):
- `crates/ff-sdk/src/task.rs:11,655`
- `crates/ff-script/src/functions/budget.rs:5,216`
- `crates/ff-server/src/server.rs:22,1324`

Threading namespace requires either (a) a `namespace: &Namespace` prefix arg added to every signature — 4 call-site breaks, or (b) currying via a context object (e.g. `KeysCtx { namespace }.usage_dedup_key(...)`).

Option (b) is more ergonomic long-term but is a bigger refactor than v3 describes. Option (a) is straightforward but the Stage 1a.1 "add Namespace type" PR should then carry a follow-up signature-break PR in Stage 1c, not try to stay API-compatible via a default-namespace overload.

**Required fix.** §R6.4 Stage 1c row acknowledge that threading namespace through free functions in `ff-core::keys` is a **breaking signature change** (not purely internal). ff-core has no external (non-workspace) users today per the project notes, so acceptable, but worth stating to avoid surprise at PR time. Escalate to owner if strong preference on (a) vs (b).

### #LD3: Stage 1a.0 vs Stage 1a.1 ordering — is the micro-PR actually independent?
Draft §R6.4 Stage 1a.0 renames `idempotency_key(tag, namespace, idem_key)` → `(tag, idem_domain, idem_key)` BEFORE Stage 1a.1 adds the `Namespace` type.

Rationale in v3 is "avoid reviewer conflation." Reasonable. But:
- The Stage 1a.0 PR touches every `idempotency_key` caller. Let me grep.

Evidence — `grep -rn "idempotency_key" crates/ --include="*.rs"`: need to check call-site count. V3 says "one-liner cross-crate grep-and-rename" but doesn't cite the number. Implementers should know the count before reviewing "micro-PR" labelling.

**Worker L lean: fine to land as sequenced, but v3 should cite the caller count so the "~1h micro-PR" labelling is verifiable.** Add to §R6.4: *"Stage 1a.0 touches N call sites (grep-verified at Stage 1a.0 planning)."*

---

## NITS

### #LN1: §R6.2.2 "waitpoint_hmac_secrets (:275)" cite is a doc comment, not the fn
Draft §R6.2.2 row 2 cites `waitpoint_hmac_secrets (:275)`. `keys.rs:275` is the `pub fn waitpoint_hmac_secrets(&self)` line — correct — but it's a method on `ExecKeyContext` (impl block at 25), which v3 already claims to enumerate in row 1. Double-counted across rows. Pick one row.

### #LN2: §R6.2.2 estimates "~13 sites" for Lua-literal keys, but K enumerated 13 explicit line numbers and the total of UNIQUE ff: literal sites in the lua file is 14 (incl. the `ff:noop:` sentinel on 1299/1522 which K already flagged). V3 row 4 says "~13 sites" and row 7 separately lists `ff:noop:` as 2 sites. Split-counting is fine but row 4's "~13" should be a hard number (13 or 14) — the tilde invites drift.

### #LN3: §R6.3 `Namespace::EMPTY` as `const` — `String::new()` is `const fn` as of Rust 1.39, so `Namespace(String::new())` compiles in a const context. Good. But v3 doesn't state MSRV, and some internal workspace crates may still be on older toolchains. Confirm MSRV ≥ 1.39 (trivially yes for modern Rust but call it out since the const declaration is load-bearing for the "Namespace::EMPTY" ergonomic claim).

### #LN4: §R6.8 Q13 "as-shown or adapter?" — v3 answers "as-shown is simpler; adapter is over-engineered." Fair. But then reword as a decision, not a question: *"`new_or_empty` lands as a free method on `Namespace` (not behind a NamespacePolicy adapter). Locked."* Questions that are already answered should graduate out of the "Remaining open" block.

### #LN5: §R6.2.2 header "grep-verified against HEAD `69de6a5`" — several cells in the table are NOT grep-verified (see #L2). Either re-run the greps and update the table, or soften the header to "grep-indicated, re-verified at Stage 1c planning."

---

## Affirmative no-finding notes (on K-raised or newly-checked areas)

- **K's #K1 (Lua transparent claim false):** v3 §R6.2.4 now names the ARGV-plumbing remediation. Fix direction is correct (modulo #L1 collision with existing ARGV layout).
- **K's #K2 (migration-tool SCAN pattern broken):** v3 §R6.5 replaces the broken `ff:lease:*` pattern with operator-affirmation + correct `<old>:ff:idx:*:lease_expiry` pattern. Clean fix.
- **K's #K3 (enumerated leak surfaces):** v3 §R6.2.2 adds the inventory table. Structure is correct (modulo #L2 number errors).
- **K's #K4 (6 key contexts, not 3):** v3 §R6.2.2 row 1 lists all 6. Clean fix (modulo impl-vs-new line cites in #L2).
- **K's #K5 (#90 interaction):** v3 §R6.4 rewrite is accurate: Lua PUBLISH sites persist; namespace threading applies regardless of merge order.
- **K's #K6 (encoding rationale):** v3 §R6.2.3 states the rationale explicitly. Clean fix.
- **K's #KD1 (validation regex):** v3 relaxes to allow `-`, stays at 64 chars. Owner-adjudicated per §R6.8 Q1.
- **K's #KD3 (new("") error footgun):** v3 adds `new_or_empty`. Direction correct (modulo #L3 whitespace leakage).
- **K's #KD4 (silent-field anti-pattern):** v3 Stage 1a.1 emits `tracing::warn!`. Clean.
- **K's #KD5 (rollback hazard placement):** v3 adopts triple-placement (CHANGELOG + rustdoc + startup warn). Clean.
- **Hash-tag preservation (brace-bearing keys):** still correct. Validation regex forbids `{`/`}` in namespace; prefix preserves first-`{`/first-`}` extraction for all keys that HAVE braces. (The no-brace case is #L4.)
- **§R6.7 alternatives 1–9:** all correctly framed. Alternative 9 (KEYS[] explosion) is the right rejection.
- **Interaction with #91 and #92:** still disjoint. No file collisions.

---

## Summary for owner

v3 fixed K's findings well, but introduced #L1 (ARGV index collision — wire-breaking as stated), #L3 (whitespace leak), #L4 (no-brace cluster slot + `COPY` cross-slot), #L5 (rollback in-flight state modes), and #L6 (Postgres `information_schema` privilege filter). #L2 is a factual error — 92 sites does not match the tree; the free-function row both undercounts and miscategorises methods.

#L1 and #L6 are the two that can silently break implementation if left in v3. #L4 and #L5 are documentation/scope gaps that invite operator errors. #L2, #L3, #L7 are fixable in minutes.

Verdict: **CHALLENGE — v3 not ready to land.** Expect v4 with #L1–#L7 resolved and #LD1/#LD2/#LD3 owner-adjudicated.
