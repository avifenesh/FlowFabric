# Plan C — IAM integration hot-path audit — for owner evaluation

**Author:** Worker-3
**Branch:** `feat/ferriskey-perf-invest`
**Status:** investigative scoping. No source changes proposed by this
document. All options below are for the project owner to evaluate and
sequence.

## §1 Current state

### 1.1 IAM module surface

`ferriskey/src/iam/mod.rs` is a single file, 1 027 LOC. Public API:

| Item | file:line | Purpose |
|---|---|---|
| `ServiceType` enum | `iam/mod.rs:36-45` | ElastiCache or MemoryDB |
| `IAMTokenManager` struct | `iam/mod.rs:143-157` | Cached token + background refresh task + change flag |
| `IAMTokenManager::new(…)` | `iam/mod.rs:183-210` | Constructor; generates initial SigV4 presigned token, returns manager |
| `IAMTokenManager::get_token() -> String` | `iam/mod.rs:365-368` | Read current cached token (`RwLock::read().await` + `String::clone`) |
| `IAMTokenManager::token_changed() -> bool` | `iam/mod.rs:371-373` | `AtomicBool::load(Ordering::Acquire)` |
| `IAMTokenManager::clear_token_changed()` | `iam/mod.rs:376-378` | `AtomicBool::store(false, Ordering::Release)` |
| `start_refresh_task()` | (at `:1333` call-site) | Spawns a tokio task on a `tokio::time::interval` that generates fresh SigV4 tokens every `refresh_interval_seconds` |

`IAMTokenManager`'s internal state:

```rust
pub struct IAMTokenManager {
    cached_token: Arc<RwLock<String>>,
    token_created_at: Arc<RwLock<tokio::time::Instant>>,
    iam_token_state: IamTokenState,
    refresh_task: Option<JoinHandle<()>>,
    shutdown_notify: Arc<Notify>,
    token_changed: Arc<AtomicBool>,
}
```

AWS dependencies pulled in (from the `use` list at `iam/mod.rs:1-16`):
`aws_config`, `aws_credential_types`, `aws_sigv4`, `aws_sdk_sts` (via
the aws_sigv4 crate-graph). Standalone bench builds against these
regardless of whether IAM is used — see §1.3.

### 1.2 IAM touch points in `ferriskey/src/client/mod.rs`

Per-`grep`, 13 mentions:

| # | file:line | Context | Per-bench-run cost when IAM None |
|---|---|---|---|
| I1 | `:106-110` | Docstring + signature of `get_valkey_connection_info(…, Option<&Arc<IAMTokenManager>>)` | — |
| I2 | `:120` | `if let (Some(_), Some(manager)) = (&info.iam_config, iam_token_manager)` | Connection-time only |
| I3 | `:199` | `iam_token_manager: Option<Arc<crate::iam::IAMTokenManager>>` field on `Client` | Per-command `Client::clone` → `Option::clone` on `None` (zero-cost) |
| I4 | `:658` | `self.iam_token_manager.as_ref()` inside `get_or_initialize_client` Lazy-promotion | Lazy-promotion path only; zero for bench |
| I5 | `:666,676` | Passing `iam_manager_ref` into cluster/standalone client creation | Connection-time only |
| I6 | **`:795-809`** | **Per-command** `if let Some(iam_manager) = &self.iam_token_manager && iam_manager.token_changed() { … }` | **One branch check per command; short-circuits when None** |
| I7 | `:1318-1348` | `create_iam_token_manager(auth_info)` — reads `auth_info.iam_config`; returns `Some(Arc)` only when IAM is configured | Construction-time only |
| I8 | `:1361-1395` | `pub async fn refresh_iam_token(&mut self)` — explicit refresh API; needs `iam_token_manager` Some | Caller-triggered only |

### 1.3 What the per-command check actually does when IAM is None

Per `client/mod.rs:795-809`:

```rust
if let Some(iam_manager) = &self.iam_token_manager
    && iam_manager.token_changed()
{
    let current_token = iam_manager.get_token().await;
    …
    iam_manager.clear_token_changed();
    self.update_connection_password(Some(current_token), false).await?;
}
```

When `self.iam_token_manager == None` (the bench case):

1. `&self.iam_token_manager` produces `&Option::None`.
2. `if let Some(iam_manager) = …` — pattern match on None → branch
   not taken.
3. `iam_manager.token_changed()` — never evaluated (short-circuit).
4. Body is skipped entirely.

**Cost when IAM is None:** one Rust pattern-match branch. The compiler
(opt-level = 3 + LTO on the bench profile) folds this into a single
`cmp + je` at the machine level. Effectively zero.

When `self.iam_token_manager == Some(Arc<_>)` but the background
refresh task hasn't fired `token_changed = true` yet:

1. Pattern match → `iam_manager = &Arc<IAMTokenManager>`.
2. `iam_manager.token_changed()` — `AtomicBool::load(Ordering::Acquire)`.
3. Returns `false` → body skipped.

**Cost when IAM is Some but token hasn't rotated:** one Acquire load
on an atomic. ~1-5 ns uncontended, up to ~30 ns under per-worker
contention. Still cheap but nonzero.

When IAM is Some AND token changed:

1. `get_token()` reads a `tokio::sync::RwLock<String>` + clones the
   cached token string.
2. `clear_token_changed()` writes the atomic.
3. `update_connection_password(...)` propagates the new token to the
   underlying connections — a write lock on `internal_client`, plus
   AUTH commands.

**Cost when token rotates:** unbounded (ish) — but rotation happens
at most every `refresh_interval_seconds` (default 300 s, min 1 s).
On a 10-s bench there is zero rotation.

### 1.4 What else the Option<Arc> costs when None

**Size-of impact:** `Client` struct has
`iam_token_manager: Option<Arc<IAMTokenManager>>`. `Option<Arc<T>>` is
8 bytes on 64-bit (null-pointer optimization). Zero per-field-access
cost when None; Option<Arc> is the standard shape.

**Compile-time cost of the aws_sigv4 dep graph:** `aws_sigv4`,
`aws_credential_types`, `aws_config` etc. bring in substantial transitive
dependencies (aws-smithy-runtime, aws-smithy-http-client, hyper,
rustls). Per the Cargo.lock in
`benches/comparisons/ferriskey-baseline/Cargo.lock`, the aws-* crate
graph is compiled whether the consumer uses IAM or not (there is no
`#[cfg(feature = "iam")]` gate that I can see).

This is a **build-time** cost, not a hot-path cost. Still worth
flagging: pure-Rust consumers pay to compile AWS SigV4 + SDK config
even if they never touch IAM.

### 1.5 Summary of hot-path cost when IAM is None

| Cost | Magnitude (opt level 3 / LTO) |
|---|---|
| Pattern-match branch at `:795` | 1 machine instruction |
| `Option::clone` during per-command `Client::clone` | 0 (None special-case) |
| Atomic load / RwLock / String clone | 0 (branch not taken) |
| Compile time overhead | aws_sigv4 + transitive deps (minutes on cold build) |

**There is no measurable per-command runtime cost when IAM is
unconfigured.** The hot-path audit confirms `report-w3.md` §F3's
scoring.

The gap is in **compile-time + binary size** (aws-\* crates pulled in
regardless) and in **surface area** (IAM API + module is always
compiled into ferriskey). Neither is a throughput bottleneck. The
owner should decide whether the surface/compile cost is acceptable in
a pure-Rust embedding.

## §2 Intent

IAM auth is an AWS ElastiCache / MemoryDB feature. The glide-core
lineage serves AWS customers who need:

1. **Short-lived SigV4 presigned tokens** as the Valkey AUTH
   credential, rotated every 15 minutes (token TTL is 15 min by
   default — see `iam/mod.rs:27`).
2. **Automatic background refresh** so a long-lived client doesn't
   suddenly start seeing AUTH failures mid-workload.
3. **Push-update of the AUTH credential** into every live connection
   — see the `update_connection_password` flow at
   `client/mod.rs:807`.

glide-core has to expose this because a non-trivial fraction of its
users run on AWS-managed Valkey. Pure-Rust ferriskey consumers who
run on non-AWS Valkey have no use for it but still link the module.

## §3 Proposed changes (options — owner decides)

### Option 1 — KEEP as-is (zero runtime cost; accept compile-time dep graph)

Do nothing. The per-command `if let Some` check is free at runtime;
the aws-\* dep graph is the only measurable cost and it's at
compile-time.

**Pros:**
- No code change, no risk.
- AWS users continue to have IAM support out of the box.
- Removes a debate that isn't on the critical path for the 45 %
  throughput gap.

**Cons:**
- Pure-Rust non-AWS users still compile aws_sigv4 + aws_config + their
  transitive ~40 crates. Cold build time impact: measurable (tens of
  seconds on a cold compile).
- Binary size includes the full AWS SDK chunk even if unused. Not a
  showstopper for a library, but worth noting.
- Doesn't address `report-w2.md` / `report-w3.md` observation that
  the IAM module exists solely for AWS consumers.

### Option 2 — Cargo-feature-gate IAM

Add `iam` Cargo feature (default on for back-compat). Every
reference to `IAMTokenManager` wraps in `#[cfg(feature = "iam")]`.
The `Option<Arc<IAMTokenManager>>` field becomes
`#[cfg(feature = "iam")] iam_token_manager: …`.

Non-AWS users: `default-features = false` on their `ferriskey`
dependency skips the entire aws-\* dep graph.

**Pros:**
- Zero compile-time AWS cost for consumers who opt out. Cold-build
  time drops by the aws-smithy-runtime + rustls-native-certs chunk
  (~30-60 s).
- No runtime change for existing users. Default-on keeps the current
  shape.
- Matches the pattern redis-rs 1.2.0 uses for optional features
  (cluster, tokio-native-tls, aio, script, vector-sets, etc.).
- Eliminates the hot-path branch for cfg'd-out consumers (the Option
  field vanishes entirely; the per-command `if let Some` at `:795`
  becomes `#[cfg(feature = "iam")]` no-op).

**Cons:**
- 13 call sites touched in `client/mod.rs`. Each becomes
  `#[cfg(feature = "iam")]` or a method-level cfg. Some match arms
  change shape (`ClientWrapper::Lazy(_) => Err(...)` arms that
  reference iam_manager_ref need the cfg wrapped around the whole
  arm).
- Split CI: `cargo check --no-default-features` becomes a required
  build. `cargo check --features iam` also required.
- Language bindings that always-enable IAM have no change; they
  explicitly depend on the feature anyway.
- Gates the API by feature, not by config — users who want to build
  with IAM capability but leave it unconfigured at runtime see no
  benefit (they still compile aws-\*).

### Option 3 — Extract IAM to a separate crate

Move `ferriskey/src/iam/` + its aws dep graph to a sibling crate
`ferriskey-iam`. ferriskey re-exports it only when a user explicitly
depends on `ferriskey-iam` and passes a manager into the builder.

**Pros:**
- Cleanest separation: `ferriskey` core has zero AWS dependency.
- Opens the door to other auth strategies (HashiCorp Vault, GCP
  secret rotation, custom) without expanding ferriskey's core
  surface.
- Consumers pick their auth story.

**Cons:**
- Biggest refactor of the three options. `IAMTokenManager` is
  instantiated inside `Client::connect` (`client/mod.rs:1318`); moving
  that across a crate boundary requires either a trait at the
  boundary (`trait TokenSource` in ferriskey, impl in ferriskey-iam)
  or a builder callback (`on_token_refresh`).
- Breaks the current API shape: `ClientBuilder::iam_token_manager(…)`
  users would need to depend on both crates and wire the manager
  through the builder differently.
- Language bindings (glide-core's Python/Java) would need the
  separation too, or the bindings depend on both crates.

## §4 Estimated effort per option

| Option | Files touched | Size | Public API break | Per-command cost after |
|---|---|---|---|---|
| 1 (keep) | 0 | none | no | unchanged (already ~0) |
| 2 (feature gate) | `Cargo.toml` + 13 call sites in `client/mod.rs` + `iam/mod.rs` module-level `#[cfg]` | S (1-2 days) | No runtime break; default-on preserves behavior | 0 when feature off; unchanged when on |
| 3 (extract crate) | new `ferriskey-iam/` crate + trait abstraction in `ferriskey/src/iam_trait.rs` + ClientBuilder rework + downstream consumer changes | M-L (3-5 days) | Yes, medium — `IAMTokenManager::new` becomes `ferriskey_iam::TokenManager::new` | 0 for non-IAM users |

## §5 Open questions for owner decision

1. **Is the aws-\* compile-time cost an issue for downstream
   FlowFabric-like pure-Rust consumers?** FlowFabric builds
   ferriskey via a path dep. A `default-features = false` toggle
   saves compile time; a separate crate saves more. If compile-time
   isn't a concern, Option 1 stands.
2. **Which language bindings always enable IAM?** If Python/Java
   always build with it, Option 2's `default-features = true` is
   safe. Option 3 needs binding-side dependency updates.
3. **Is a trait-based abstraction (Option 3) a design direction the
   owner wants to pursue anyway?** If extensible auth strategies are
   on the roadmap, Option 3 is a stepping stone. If not, the cost
   isn't worth it.
4. **Does the IAM module need the full `aws_sigv4` signing path, or
   could SigV4 be vendored + trimmed?** `aws_sigv4` is a ~20 KLOC
   crate; a lot of it handles request signing in general (HTTP,
   body, query strings). IAM auth for Valkey uses presigned URLs with
   a narrow shape; a hand-rolled SigV4 (~300 LOC) could replace
   `aws_sigv4` + most of the aws_config graph. That's a separate
   question from this plan — noted for scope.
5. **What's the current per-command runtime cost when IAM IS
   configured and refreshes fire?** I didn't audit the
   `update_connection_password` path end-to-end. For a user actually
   using IAM in production, that branch's cost matters and deserves
   its own study.

## §6 Cross-reference to Round 1 perf reports

- `report-w3.md` §F3 ("IAM token changed-check; zero when IAM not
  configured") scored this as zero-cost per command. §1.3 here
  confirms: the pattern-match short-circuits at the machine level;
  no atomic load, no RwLock acquire, no String clone when IAM is
  None.
- `report-w1.md` flamegraph will NOT show an IAM bucket for the
  bench workload (non-AWS, no iam_config). If it does, §1.3's
  analysis is wrong and I owe a correction.
- `report-w2.md` wider workload variance (streams +11%, pipeline +6%,
  BLMPOP -46%) is NOT driven by IAM — the variance is in the
  decoder / conversion path. IAM being None short-circuits identically
  across all workloads.

## §7 Anti-goals

- **Not a recommendation.** Option 1 (do nothing) is on the table
  precisely because the runtime cost is already ~zero. §5 lists the
  questions that determine whether Options 2 or 3 are worth the
  engineering cost.
- **Not an indictment of aws_sigv4.** Pulling a reputable SigV4
  implementation is a correct call for production IAM; building one
  by hand is a security-sensitive choice. Question 4 in §5 flags it
  as a follow-up only.
- **Not a claim about the AWS IAM feature's value.** The feature is
  load-bearing for AWS customers; this plan is strictly about
  whether pure-Rust non-AWS consumers should pay its overhead by
  default.
