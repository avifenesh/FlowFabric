# RFC-012 — Round-6 amendment (DRAFT v5): namespace prefix on `BackendConfig`

**Status:** DRAFT v5. Three rounds of challenger review (K, L, M) applied. Pre-1.0 posture locked by owner: CHANGELOG-only communication, no BC shims, no migration tool.
**Tracks:** issue #122 (namespace prefix hook).
**Shape:** additive to trait surface (`BackendConfig.namespace` field); breaking to `ff-core::keys` function signatures; breaking to FCALL ARGV layout (append + pop — see §R6.2.4).

**v4 → v5 changelog (addresses round-3 M findings):**
- **#M1 fix:** ARGV-append mechanism now specifies `local NS = table.remove(args)` at function entry, not `ARGV[#ARGV]` read-in-place. Three Lua functions (`ff_set_execution_tags`, `ff_set_flow_tags`, `ff_resolve_dependency`) iterate `#args` and `unpack(args)` variadically; leaving the prefix on the tail silently shifts their parity counts. `table.remove` restores `#args` before the existing bodies run.
- **#M2 fix:** Inventory extended — 79 additional `format!("ff:…")` sites outside `ff-core::keys` across 15 files (all 13 `ff-engine/src/scanner/*`, `ff-engine/budget.rs`, `ff-sdk/worker.rs`). These are **readers**; without threading, scanners iterate bare `ff:` while FCALLs write `<ns>:ff:`, producing silent split-brain on namespaced deploys. Stage 1c scope extends accordingly.
- **#M3 fix:** Dropped the `CLIENT SETNAME` follow-up note in §R6.6 (owner: not currently needed, not on backlog until requested).
- **#MD1:** Rustdoc on `Namespace::new` now explicitly documents the `""→EMPTY` vs `"   "→InvalidChar` asymmetry.
- **#MD2:** §R6.5 commits to a two-backend-same-Valkey isolation property test as Stage 1c acceptance gate.
- **#MD3:** §R6.1 now explicitly calls out the cairn zero-migration path (`EMPTY` default = byte-identical = no config change required).

---

## §R6.1 Motivation

Multiple logical FF instances sharing one Valkey backend observe each other's state. Mechanisms:

- `ff_core::keys` emits every key as bare `ff:…`. No prefix hook. 83 format sites in `keys.rs`.
- `flowfabric.lua` and `ff-script::functions::lease` construct keys/channels inline; 13 Lua-literal key sites + 4 Lua `PUBLISH` sites + 4 Rust-literal sites in `ff-script`.
- Subscribers iterate `SCAN MATCH ff:idx:*` — matches every tenant.
- Valkey Function library (`FUNCTION LOAD ff`) is server-global — stateless code; not a leak surface (§R6.7 #4).

Cairn ships a read-layer `cairn.instance_id` exec-tag filter as immediate mitigation. This amendment is the storage-layer complement. We do not coordinate cairn's removal of the filter (peer-team boundary).

**Cairn zero-migration path.** `BackendConfig.namespace` defaults to `Namespace::EMPTY`. Deploys that do not set the field produce byte-identical keys, channels, and wire bytes to today — including cairn's current deploy. Cairn opts into isolation only when they explicitly set a non-empty namespace in their next release; until then their existing data and in-flight executions are undisturbed. No forced migration event.

**Stability posture.** Pre-1.0. Cairn is the only external consumer. Breaking changes land freely; CHANGELOG entry is the migration contract. No BC shims, no deprecation windows, no in-place migration tooling.

Out of scope: env-var parsing, config-file schema, online data migration, backward compatibility of any kind.

## §R6.2 Contract

### §R6.2.1 Invariant

Every `EngineBackend` implementation carries `namespace: Namespace` at construction. For two backend instances `A` and `B` with `A.namespace != B.namespace`:

- Every persistent key/row/object `A` writes is disjoint from `B`'s.
- Every pubsub/notification channel `A` publishes on is disjoint from `B`'s.
- Every SCAN/range pattern `A` uses is scoped to `A`'s namespace.

Empty namespace = today's behaviour (no prefix). `EMPTY` is a valid, supported value.

### §R6.2.2 Covered surfaces (grep-verified against `origin/main` `69de6a5`)

Verification: `grep -c 'format!("ff:' crates/ff-core/src/keys.rs` = 83.

| Surface | Location | Count |
|---------|----------|-------|
| Key contexts (methods on these types) | `ExecKeyContext` (`keys.rs:25`; `new` at `:26`, incl. `signal_dedup` `:156`, `waitpoint_hmac_secrets` `:275`, and all other `&self` methods), `IndexKeys` (`:210`; `new` `:211`), `FlowKeyContext` (`:323`; `new` `:324`), `FlowIndexKeys` (`:404`; `new` `:405`), `BudgetKeyContext` (`:437`; `new` `:438`), `QuotaKeyContext` (`:494`; `new` `:495`) | 6 structs |
| Free functions in `ff-core::keys` | `budget_attach_key` (471), `budget_resets_key` (476), `budget_policies_index` (482), `quota_policies_index` (535), `quota_attach_key` (540), `lane_config_key` (547), `lane_counts_key` (552), `worker_key` (557), `worker_caps_key` (568), `workers_index_key` (578), `workers_capability_key` (583), `lanes_index_key` (588), `global_config_partitions` (594), `namespace_executions_key` (601), `idempotency_key` (609), `noop_key` (618), `tag_index_key` (623), `waitpoint_key_resolution` (628), `usage_dedup_key` (644) | 19 fns |
| `format!("ff:…")` sites in `keys.rs` | — | 83 |
| Rust-literal `ff:` in `ff-script` | `ff-script::functions::lease:97,121,138-139` | 4 sites |
| Rust-literal `ff:` reader sites (scanner + SDK) | All 13 `ff-engine/src/scanner/*.rs`, `ff-engine/src/budget.rs`, `ff-sdk/src/worker.rs` (grep: `grep -rn 'format!("ff:' crates/ff-engine crates/ff-sdk crates/ff-server`) | 79 sites / 15 files |
| Lua-literal `ff:…` key sites | `flowfabric.lua:476,1663,1994-1995,2669,2678,2714,2794,2894,2942-2958` | 13 sites |
| Lua-literal `ff:dag:completions` PUBLISH | `flowfabric.lua:1920,2128,2558,3014` | 4 sites |
| Pre-fix scratchpads (`ff:noop:` sentinel) | `flowfabric.lua:1299,1522` | 2 sites (string-contains check; prefix-aware audit needed) |
| SCAN patterns | audit via grep at Stage 1c start | — |

### §R6.2.3 Prefix encoding

**Chosen form: `<namespace>:ff:…`** (namespace prepended before the `ff:` sentinel).

Rationale: single-tenant inspection scan is unambiguous as `SCAN MATCH <ns>:ff:*`. The alternative `ff:<ns>:…` collides with existing segment-2 names (`idx`, `sec`, `idem`, etc.) and forces brittle SCAN patterns. Both forms are correctness-equivalent.

**Empty namespace** produces no leading colon — byte-identical to today's keys.

**Brace-bearing keys (`{fp:N}` etc.) preserve their hash-tag.** Valkey extracts bytes between first `{` and first `}`; the validation regex forbids `{`/`}` in the namespace; prepending preserves the extraction. Cluster slot routing for partitioned keys is unchanged.

**No-hash-tag keys change Cluster slot.** The following keys have no brace and are routed by CRC16 of the whole key:

- `ff:idx:workers` (`keys.rs:578`)
- `ff:idx:lanes` (`keys.rs:588`)
- `ff:config:partitions` (`keys.rs:594`)

Under the prefix, `<ns>:ff:idx:workers` hashes to a different slot than the unprefixed form. This is **correct** — per-namespace globals should route independently — but operators doing ad-hoc `CLIENT GETKEYS` or SCAN slot assumptions should know. No migration path is offered (greenfield posture).

### §R6.2.4 Lua-side propagation — append, don't prepend

Rust-side keygen owns the prefix for keys passed via `KEYS[]`. Lua inline key constructions and `PUBLISH` channel names construct strings locally and must be threaded.

**Mechanism:** each FCALL gains a **trailing** ARGV slot carrying the namespace prefix. Lua functions **pop** the prefix before consuming remaining args:

```lua
-- At function entry, BEFORE any args[i] / #args read:
local NS = table.remove(args)   -- pops the tail; #args is now back to pre-amendment value
-- ... existing args[1..N] consumption unchanged ...
redis.call("PUBLISH", NS .. "ff:dag:completions", payload)
local att_key = NS .. "ff:attempt:" .. tag .. ":" .. eid .. ":" .. idx
```

**Pop, not read-in-place (round-3 M fix).** `ARGV[#ARGV]` read-in-place would silently break FCALLs that iterate variadic trailing args:

- `ff_set_execution_tags` (`flowfabric.lua:3042`) — `n = #args` parity check + `unpack(args)` into HSET
- `ff_set_flow_tags` (`flowfabric.lua:7033`) — same pattern
- `ff_resolve_dependency` (`flowfabric.lua:6919`) — `num_edges = #args - 2`

For these, leaving `namespace_prefix` on the tail poisons `#args` and `unpack`. `table.remove(args)` pops the tail and restores `#args` to its pre-amendment value; the existing variadic-consumer bodies then run unchanged.

**Append, not prepend.** Prepending at ARGV[1] would shift every positional index in every registered function — 47 `redis.register_function` sites and every Rust caller. Appending + popping leaves existing positional indexing untouched.

**Rust-side:** every FCALL caller in `ff-script` appends `namespace.as_str()` as the final ARGV element. A thin helper (`fcall_with_namespace(cmd, keys, args, &namespace)`) centralises the append to reduce per-site error surface.

**Alternative rejected:** moving Lua-literal keys into Rust-built `KEYS[]`. Explodes `KEYS[]` cardinality; Valkey Cluster requires all `KEYS[]` entries to hash to one slot; multi-partition FCALLs would break.

### §R6.2.5 Exclusions

- **Valkey Function library (`FUNCTION LOAD ff`):** server-global; library is stateless code; per-namespace loads rejected in §R6.7 #4.
- **FCALL latency / CPU contention:** not a correctness boundary.
- **Intra-namespace multi-tenancy:** application-level concern.

### §R6.2.6 Naming collisions to resolve

Three `ff-core::keys` free functions take an argument literally named `namespace`, for execution-domain idempotency / tag-scoping purposes unrelated to the `Namespace` type we're adding:

- `idempotency_key(tag, namespace, idem_key)` (`keys.rs:609`)
- `namespace_executions_key(namespace)` (`keys.rs:601`)
- `tag_index_key(namespace, key, value)` (`keys.rs:623`)

Rename these parameters in Stage 1c (not as a prep PR — just do it as the first commit in the Stage 1c PR, alongside the signature threading). Proposed:

- `idempotency_key(tag, idem_domain, idem_key)`
- `namespace_executions_key(exec_domain)` — or rename the whole fn since "namespace" is now ambiguous
- `tag_index_key(tag_domain, key, value)`

Final names set at Stage 1c review; this amendment just flags that the renames must happen in the same commit that introduces the `Namespace` arg, to avoid reviewer conflation.

## §R6.3 Type surface

`Namespace` and `NamespaceError` live in **`ff-core`**, alongside `BackendConfig` (`crates/ff-core/src/backend.rs`). `ff-core` already depends on `thiserror` via workspace (`Cargo.toml:18`); no new crate deps.

```rust
/// Opaque namespace identifier threaded through every backend
/// key-derivation and pubsub-channel site. Empty = no prefix.
///
/// Validation: `[A-Za-z0-9_-]{1,64}`, no leading `-`.
///   - `-` allowed: UUID/ULID-shaped tenant IDs are common.
///   - No leading `-`: Postgres identifier convention (backend
///     double-quotes on the SQL side).
///   - No `:`: parse ambiguity with key segments.
///   - No `{`/`}`: Valkey hash-tag collision.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct Namespace(String);

impl Namespace {
    /// Empty namespace — no prefix. Supported, valid value.
    pub const EMPTY: Namespace = Namespace(String::new());

    /// Construct and validate.
    ///
    /// - `new("")` → `Ok(EMPTY)`. Empty is a supported production
    ///   config (no-prefix deploy), not a caller bug.
    /// - `new("   ")` → `Err(InvalidChar(' '))`. Whitespace-only
    ///   input is NOT normalised to empty — callers are responsible
    ///   for trimming their own config input. This asymmetry is
    ///   intentional: an operator who explicitly sets
    ///   `NAMESPACE=" "` has made a config error, not opted into
    ///   no-prefix.
    /// - Any other input is validated against `[A-Za-z0-9_-]{1,64}`
    ///   with no leading `-`.
    pub fn new(s: impl Into<String>) -> Result<Self, NamespaceError> {
        let s = s.into();
        if s.is_empty() { return Ok(Self::EMPTY); }
        // ... length + char validation ...
    }

    pub fn as_str(&self) -> &str { &self.0 }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
}

impl fmt::Display for Namespace { /* emits as_str, log/debug only */ }

#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    #[error("namespace too long (max 64, got {0})")]
    TooLong(usize),
    #[error("namespace contains disallowed character: {0:?} (allowed: [A-Za-z0-9_-])")]
    InvalidChar(char),
    #[error("namespace must not start with '-'")]
    LeadingDash,
}
```

**Single constructor, permissive on empty.** No `new_or_empty` helper (v3's attempt had a whitespace footgun). `new("")` → `Ok(EMPTY)`. Whitespace-only input (`"   "`) errors via `InvalidChar(' ')` — callers trim their own config input.

`BackendConfig` gains one field (additive; `#[non_exhaustive]` already in place):

```rust
pub struct BackendConfig {
    pub connection: BackendConnection,
    pub timeouts: BackendTimeouts,
    pub retry: BackendRetry,
    pub namespace: Namespace,          // NEW; default Namespace::EMPTY
}

impl BackendConfig {
    pub fn with_namespace(mut self, ns: Namespace) -> Self {
        self.namespace = ns;
        self
    }
}
```

`BackendConfig::valkey(host, port)` signature unchanged; default namespace `EMPTY`. The builder idiom `BackendConfig::valkey(...).with_namespace(...)` becomes the canonical construction path; future `BackendConfig::postgres(...)` will mirror.

## §R6.4 Stage mapping

| Stage | State | Scope |
|-------|-------|-------|
| Stage 1a (merged `81e08dc`) | done | unchanged |
| Stage 1b (merged `6f54f9b`) | done | unchanged |
| **Stage 1c** (pending) | planning | **Absorbs all namespace work in one landing.** Scope: `Namespace` + `NamespaceError` type in `ff-core::backend`; `BackendConfig.namespace` field + `with_namespace` builder; rename the three `namespace`-named parameters + `namespace_executions_key` fn (§R6.2.6); thread `namespace: &Namespace` through 6 key contexts + 19 free functions + 83 `format!` sites in `keys.rs`; 4 Rust-literal sites in `ff-script::functions::lease`; **79 `format!("ff:…")` reader sites in `ff-engine` scanners + `ff-engine/budget.rs` + `ff-sdk/worker.rs`** (#M2 — scanners read what FCALLs write; skipping these produces silent split-brain); 13 Lua-literal key sites + 4 PUBLISH sites in `flowfabric.lua`; ARGV-append + `table.remove(args)` pop in every FCALL caller (`fcall_with_namespace` helper); SCAN-pattern grep audit; isolation property test (§R6.5). Scope is breaking to `ff-core::keys` signatures (acceptable per posture). Wall-time estimate set at Stage 1c planning — prior 25-35h was for keygen-only. |
| Stage 1d (pending) | unchanged | snapshot + `.client()` removal do not touch keys |
| Stage 2 (Postgres PoC) | future | Namespace maps to schema. `PostgresBackend::connect` verifies pre-existing via `SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1` (privilege-independent, unlike `information_schema.schemata`). Does NOT auto-CREATE. Clear error on missing schema. |
| Stage 4 (seal `ff_core::keys`) | unchanged | |

**Interaction with #90 (CompletionBackend, in flight):** #90 hides the `ff:dag:completions` channel behind a `CompletionBackend` trait. Lua PUBLISH sites persist; namespace threading applies to them regardless of merge order. #90 and this amendment are concurrent, not sequenced.

**Interaction with #91 (PartitionKey, in flight):** disjoint. #91 touches `contracts.rs` + `partition.rs` + DTOs; this amendment touches `keys.rs` + Lua + `ff-script`.

**Interaction with #92 (StreamCursor, in flight):** disjoint. #92 changes XRANGE/XREAD arg shape, not keys or channels.

## §R6.5 Migration, rollback, and acceptance testing

**Migration: not provided.** Pre-1.0 posture: breaking changes land freely, CHANGELOG documents them, cairn (sole external consumer) rebases on their next deploy. No migration tool, no online conversion, no rollback support.

Cairn's default path requires no action: `EMPTY` namespace = byte-identical keys to today. Cairn opts into isolation by setting a non-empty namespace in a future release; any operator who wants to switch a running deploy does a FLUSHDB-equivalent and restarts (same as any other schema-changing pre-1.0 release).

**Isolation acceptance test (Stage 1c gate, per #MD2).** Stage 1c does not merge until the following property test passes, added in `crates/ff-test/tests/namespace_isolation.rs`:

- Spin two `ValkeyBackend` instances `A` and `B` against the same Valkey, with `A.namespace = "tenant_a"` and `B.namespace = "tenant_b"`.
- Exercise a representative slice of the API through both: claim-to-complete, suspend-resume, budget report, waitpoint HMAC resolve, stream read, cancel_flow.
- Assert after each step:
  - `A.describe_execution(eid_b)` returns `NotFound` (A cannot see B's executions).
  - Subscribers to `A`'s completion stream do not observe `B`'s completions.
  - A SCAN under `A`'s prefix does not match any `B`-originated keys, and vice versa.
- Additionally, run a third backend `C` with `namespace = EMPTY` and assert its SCAN/DESCRIBE surface is disjoint from both `A` and `B` (empty-prefix behaves as its own namespace, not as a catch-all view).

The test is the acceptance gate for the full invariant in §R6.2.1. Stage 1c PR body must link to the test output.

**CHANGELOG entry** accompanying the Stage 1c release must state:

- `BackendConfig.namespace` field added; defaults to `EMPTY`.
- `ff-core::keys` free-function signatures changed (19 functions; 3 parameter renames).
- FCALL ARGV tail now carries `namespace_prefix`; Rust callers using the bundled helpers are unaffected; callers that construct ARGV manually must append.
- Existing deploys without a namespace config continue producing the same keys (byte-identical).
- Switching a live deploy to use a non-EMPTY namespace is a keyspace reset — no in-place migration.

## §R6.6 What this amendment does NOT do

- Does NOT define how operators source the namespace. Caller-side.
- Does NOT add per-call namespace override. Construction-time only.
- Does NOT change trait method signatures.
- Does NOT address intra-namespace multi-tenancy.
- Does NOT namespace the Valkey Function library.
- Does NOT provide migration tooling.
- Does NOT support rollback across the Stage 1c boundary.
- Does NOT coordinate cairn's removal of the read-layer filter.

## §R6.7 Alternatives considered

1. **Env-var read inside `ff-core::keys`.** Rejected: violates construction-time-not-global; couples ff-core to process environment.
2. **Namespace as a per-call arg on `EngineBackend`.** Rejected: every op carries an arg that never varies within a backend's lifetime.
3. **Namespace inside the hash-tag (`{ns:fp:5}`).** Rejected: breaks Cluster routing (hash-tag == partition identifier assumption).
4. **Per-namespace Valkey Function libraries (`FUNCTION LOAD ff_<ns>`).** Rejected: operational surface (who cleans up stale libs?); library is stateless code; KEYS/ARGV isolation is sufficient.
5. **Separate Valkey databases (`SELECT N`).** Rejected: Cluster mode supports only DB 0.
6. **New RFC-013 instead of amending.** Rejected: Stage 1c re-touches every keygen site and ARGV caller; co-threading namespace avoids a second sweep of the same ~83 sites + 47 FCALL sites. Same-pass fusion; cheaper than sequenced.
7. **Hash-prefix instead of text prefix.** Rejected: loses human-readable/greppable property.
8. **`ff:<namespace>:…` instead of `<namespace>:ff:…`.** Rejected: inspection SCAN collides with existing segment-2 names.
9. **Move Lua-literal keys into Rust-built `KEYS[]`.** Rejected: explodes `KEYS[]` cardinality; Cluster requires single-slot.
10. **`ARGV[1] = namespace_prefix` (prepend).** Rejected: shifts every positional index in every function; wire-breaking across all 47 FCALLs. Appending + popping at ARGV-tail is strictly better (§R6.2.4).
11. **`new_or_empty` permissive helper.** Rejected: whitespace-leak bug; single constructor that maps empty→EMPTY is simpler.
12. **`ARGV[#ARGV]` read-in-place (no pop).** Rejected: breaks `ff_set_execution_tags`, `ff_set_flow_tags`, `ff_resolve_dependency` which iterate `#args` / `unpack(args)` variadically. `table.remove(args)` pops the tail and restores `#args` for variadic consumers (§R6.2.4).

## §R6.8 Open questions for owner

**Locked (v3 + v4 adjudication):**

1. Validation regex = `[A-Za-z0-9_-]{1,64}`, no leading `-`.
2. Field name = `namespace`.
3. `Namespace::EMPTY` const + single constructor (`new("")` → Ok(EMPTY), no helper).
4. No migration tool.
5. Postgres schema check = `pg_catalog.pg_namespace` (privilege-independent); require pre-existing; no auto-CREATE.
6. Cairn coordination = none.
7. Stage 1c is atomic — no Stage 1a.0 / 1a.1 split.
8. Pubsub channels namespaced alongside keys.
9. Valkey Function library stays server-global.
10. Key-length recommendation: ≤16 chars operationally; 64 cap. Operators with index-heavy deploys (200M+ members) should stay short — a 65-byte prefix on 200M index members is ~13GB of key memory.
11. Prefix placement = `<namespace>:ff:…`.
12. FCALL ARGV plumbing = append-and-pop (`table.remove(args)` at function entry; not `ARGV[#ARGV]` read-in-place).
13. Naming renames (`idempotency_key`, `namespace_executions_key`, `tag_index_key`) happen in Stage 1c's first commit.

**Remaining open (v4, new):**

14. **`namespace_executions_key` rename.** Locked: rename function to `exec_domain_key`; param to `exec_domain`. Same pass covers `tag_index_key(namespace, …)` → `tag_index_key(tag_domain, …)` and `idempotency_key(tag, namespace, …)` → `idempotency_key(tag, idem_domain, …)`. Grep-verified: no existing symbol collisions with any of the proposed `*_domain` names.

## §R6.9 Landing

- Amend RFC-012 in-place:
  - Insert round-6 summary at top (after round-5).
  - Extend §3.3.0 with `Namespace` row.
  - Add Stage 1c scope note referencing §R6.2/§R6.4.
  - Add §R6.2.4 ARGV-append mechanism as sub-note under §3.4.
- Single PR titled `rfc: RFC-012 amendment — namespace prefix on BackendConfig (#122)`. Doc-only.
- Co-author trailer: `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.
- Stage 1c planning resumes against the amended RFC. No interim "type-plumb" PR.
