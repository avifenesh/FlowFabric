# #87/#88 scope carve-out (2026-04-22)

Worker BB, read-only investigation. No code changes. All findings cite `file:line` against `main` at commit `47ef2b0`.

## Issue summary

**#87 — Seal `ferriskey::Client` leak on `FlowFabricWorker` + stream free-fns.**
Body (verbatim): "`FlowFabricWorker::client()` (worker.rs:418) + free fns `read_stream`/`tail_stream` (task.rs:2184,2280) leak raw `ferriskey::Client`. Hide public `Client`; reshape stream fns as methods on worker OR opaque `BackendHandle`. Scope: public API only — `ClaimedTask` internal field stays." Blocking HIGH · Breaking HIGH · Effort M. Audit: Worker F 2026-04-22.

**#88 — Seal `ferriskey::Error` leak on `SdkError` and `ServerError`.**
Body (verbatim): "`SdkError::Valkey(ferriskey::Error)`, `ValkeyContext { source: ferriskey::Error }`, `valkey_kind() -> Option<ErrorKind>` (lib.rs:86-94,175) force consumers onto ferriskey. Wrap in internal `BackendError`; expose `valkey_kind` as method returning None for non-Valkey. Extends `EngineError` pattern from 58.6." Blocking HIGH · Breaking HIGH · Effort M. Audit: Worker F 2026-04-22.

Note: issue **#88 title already names `ServerError`** as a target alongside `SdkError`, but its body only enumerates `SdkError` fields (`lib.rs:86-94,175`). The `ServerError` leak is in-scope per the title and per the memory checkpoint's "second leak surface in ff-server" hypothesis — this report confirms that second surface exists and tabulates it.

## Leak surface inventory

Public-API leaks only (non-pub / internal uses of `ferriskey::*` are fine and are omitted — those close naturally when Stage 1c migrates hot-path callers). "Blocks-on" is per RFC-012 §5 stage map.

| file:line | crate | symbol | public? | blocks-on |
|---|---|---|---|---|
| `crates/ff-sdk/src/worker.rs:524` | ff-sdk | `FlowFabricWorker::client() -> &Client` | pub | Stage 1c (backend-handle method replaces it) |
| `crates/ff-sdk/src/task.rs:1933-1941` | ff-sdk | `pub async fn read_stream(client: &Client, …)` | pub free fn | Stage 1c (reshape as worker method / opaque handle) |
| `crates/ff-sdk/src/task.rs:2036-2044` | ff-sdk | `pub async fn tail_stream(client: &Client, …)` | pub free fn | Stage 1c (same) |
| `crates/ff-sdk/src/lib.rs:105` | ff-sdk | `SdkError::Valkey(#[from] ferriskey::Error)` | pub enum variant | Stage 1c/d (wrap in `BackendError`) |
| `crates/ff-sdk/src/lib.rs:110-113` | ff-sdk | `SdkError::ValkeyContext { source: ferriskey::Error, … }` | pub enum variant | Stage 1c/d |
| `crates/ff-sdk/src/lib.rs:240` | ff-sdk | `SdkError::valkey_kind() -> Option<ferriskey::ErrorKind>` | pub method | Stage 1c/d (keep method, return `None` for non-Valkey) |
| `crates/ff-server/src/server.rs:577` | ff-server | `Server::client() -> &Client` | pub (re-exported `pub use server::Server`) | **INDEPENDENT** — callers are ff-server bin + ff-test + ff-readiness-tests only |
| `crates/ff-server/src/server.rs:219` | ff-server | `ServerError::Valkey(#[from] ferriskey::Error)` | pub enum variant (`pub use server::ServerError`) | **INDEPENDENT** |
| `crates/ff-server/src/server.rs:222-226` | ff-server | `ServerError::ValkeyContext { source: ferriskey::Error, … }` | pub enum variant | **INDEPENDENT** |
| `crates/ff-server/src/server.rs:268` | ff-server | `ServerError::valkey_kind() -> Option<ferriskey::ErrorKind>` | pub method | **INDEPENDENT** |
| `crates/ff-scheduler/src/claim.rs:306` | ff-scheduler | `Scheduler::new(client: ferriskey::Client, …)` | pub fn | **INDEPENDENT** (ff-server-only caller) |
| `crates/ff-scheduler/src/claim.rs:1226` | ff-scheduler | `SchedulerError::Valkey(#[from] ferriskey::Error)` | pub enum variant | **INDEPENDENT** |
| `crates/ff-scheduler/src/claim.rs:1229-1233` | ff-scheduler | `SchedulerError::ValkeyContext { source: ferriskey::Error, … }` | pub enum variant | **INDEPENDENT** |
| `crates/ff-scheduler/src/claim.rs:1244` | ff-scheduler | `SchedulerError::valkey_kind() -> Option<ferriskey::ErrorKind>` | pub method | **INDEPENDENT** |
| `crates/ff-engine/src/lib.rs:160` | ff-engine | `Engine::start(config, client: ferriskey::Client)` | pub fn | **INDEPENDENT** (ff-server-only caller) |
| `crates/ff-engine/src/budget.rs:34` | ff-engine | `BudgetChecker::new(client: ferriskey::Client, …)` | pub fn | **INDEPENDENT** |
| `crates/ff-script/src/error.rs:442` | ff-script | `ScriptError::Valkey(#[from] ferriskey::Error)` | pub enum variant | SEE NOTE |
| `crates/ff-script/src/error.rs:470` | ff-script | `ScriptError::valkey_kind() -> Option<ferriskey::ErrorKind>` | pub method | SEE NOTE |
| `crates/ff-script/src/retry.rs:30,48` | ff-script | `is_retryable_kind(kind: ferriskey::ErrorKind)`, `kind_to_stable_str(…)` | pub fns | SEE NOTE |
| `crates/ff-script/src/engine_error_ext.rs:51` | ff-script | `valkey_kind(err: &EngineError) -> Option<ferriskey::ErrorKind>` | pub fn | SEE NOTE |
| `crates/ff-backend-valkey/src/lib.rs:159` | ff-backend-valkey | `ValkeyBackend::client() -> &ferriskey::Client` | pub fn | Intentional per RFC-012 §1.3: "backend-internal access; external callers go through trait" — this is the **correct** public escape hatch for the Valkey backend impl, **not a leak**. |
| `crates/ff-readiness-tests/src/valkey.rs:11` | ff-readiness-tests | `probe_server_version(client: &ferriskey::Client)` | pub fn | Internal harness — `ff-readiness-tests` is a first-party probe crate, no external consumers. Low priority; could be sealed opportunistically. |

**Note on ff-script.** `ScriptError` and `retry::is_retryable_kind` / `kind_to_stable_str` are the canonical Valkey-error vocabulary for the whole workspace. RFC-012 §5.4 (Stage 4) and `crates/ff-script/src/error.rs:4-15` comments explicitly position `ff-script` as the Valkey-transport-aware layer below `ff-core`. Sealing ferriskey out of `ff-script` means deleting `ff-script` as a concept (merging into `ff-backend-valkey`). Out of scope for #87/#88. `ff-sdk::SdkError::valkey_kind` is meant to abstract over `ff-script`, not duplicate the leak.

## Stage-1c-blocked sites (ff-sdk)

Six sites, exactly matching #87 + #88 issue bodies:

* `crates/ff-sdk/src/worker.rs:524` — `FlowFabricWorker::client()`
* `crates/ff-sdk/src/task.rs:1933` — `pub async fn read_stream(client: &Client, …)`
* `crates/ff-sdk/src/task.rs:2036` — `pub async fn tail_stream(client: &Client, …)`
* `crates/ff-sdk/src/lib.rs:105,110-113,240` — `SdkError::{Valkey,ValkeyContext,valkey_kind}`

**Confirmed blocked.** RFC-012 §5.1 Stage 1 scope (at `rfcs/RFC-012-engine-backend-trait.md:561-580`) explicitly converts `ClaimedTask`/`FlowFabricWorker` inherent impls into a `ValkeyBackend` impl of the `EngineBackend` trait and makes SDK wrappers "thin forwarders to the trait." At that point `client()` is replaceable with a trait-level `BackendHandle`, and `SdkError::Valkey(ferriskey::Error)` naturally narrows to `EngineError::Transport { backend, source: Box<dyn Error> }` per §5.0 bullet 2 (the Stage-0 `Transport` broadening). `worker.rs:515-521` already has the Stage-1b `backend()` getter, confirming the migration path is scaffolded.

## Independent sites (ff-server / ff-engine / ff-scheduler)

**RFC-012 §5 does NOT mention `ServerError` or `SchedulerError`.** §5.0 scopes the error broadening to `EngineError::Transport` (in `ff-core`), and §5.1's migration event is framed entirely around `FlowFabricWorker` + `ClaimedTask` + `BackendConfig`. The HTTP-layer error types in `ff-server` and `ff-scheduler` live outside the worker path and are not naturally closed by Stage 1c/d.

Independent set, ranked by accessibility:

1. **ff-server `ServerError` + `Server::client()` (4 sites, server.rs:219-226, 268, 577).** `pub use server::{Server, ServerError}` in `crates/ff-server/src/lib.rs:11` makes both public to any external consumer that links `ff-server` as a library. Current Rust consumers are first-party only (ff-server bin, ff-test, ff-readiness-tests — verified by `rg 'use ff_server' crates/`), but the surface is published. HTTP consumers (cairn-fabric) don't see `ServerError` directly — `crates/ff-server/src/api.rs:228` converts via `IntoResponse` — so this is a Rust-API leak, not a wire-protocol leak.

2. **ff-scheduler `SchedulerError` + `Scheduler::new(client)` (4 sites, claim.rs:306, 1226, 1229-1233, 1244).** `pub use claim::{…, Scheduler, SchedulerError}` in `crates/ff-scheduler/src/lib.rs:5`. Only ff-server consumes it today.

3. **ff-engine `Engine::start(client)` + `BudgetChecker::new(client)` (lib.rs:160, budget.rs:34).** Both pub, both take `ferriskey::Client` by value. ff-server-only caller.

**Size estimate.** ~10-12 public-API edits across 3 crates. Pattern is mechanical and already established by RFC-012 Stage 0's `EngineError::Transport` broadening: introduce a `BackendError` / reuse `EngineError::Transport { backend: "valkey", source: Box<dyn Error + Send + Sync> }`; retain `valkey_kind()` returning `Option<ferriskey::ErrorKind>` by downcasting (or gate behind `#[cfg(feature = "valkey-default")]` like `ff-sdk` at `lib.rs:239`). The `client()` getters are harder to seal cleanly without a `BackendHandle` abstraction — ff-server's case is the test-fixture convenience path (`crates/ff-test/src/helpers.rs` has ~10 call sites; search hit count in `Server::client()` grep). Sealing `ff-server::Server::client()` cleanly requires either (a) moving test helpers onto a `pub(crate)` accessor + `#[cfg(test)]` export, or (b) waiting for RFC-012 to define the backend-handle trait and reusing it.

**Complexity per site.**
* **Low (error types):** 8 sites (3× enum variants + 1× method, ×2 crates) — pure type wrapping, same pattern as Stage 0's `Transport` broadening. Estimated 3-5h each, mostly mechanical.
* **Medium (client getters):** 3 sites (`Server::client`, `Engine::start`, `Scheduler::new`, `BudgetChecker::new`). Sealing `client()` getters is blocked on having somewhere for callers to go; these effectively wait for Stage 1c's backend-handle shape OR get an interim `pub(crate)` + test-helper refactor.

## Recommendation

**Option A (wait for Stage 1c/d) — recommended.**

Rationale:

1. The **error-type leaks** (8 low-complexity sites across ff-server + ff-scheduler) are structurally identical to the ff-sdk leak RFC-012 §5.0 already schedules. Landing them in a separate PR *before* Stage 0's `EngineError::Transport` broadening means doing the wrapping work twice — once with an ad-hoc `BackendError` or direct `EngineError` threading, then again when Stage 0's canonical shape lands. That violates the "don't defer design debt" memory but more importantly violates "RFC phases vs CI" — splitting the error-broadening work across stages multiplies consumer-migration events for no benefit.

2. The **client-getter leaks** (3 medium sites) cannot be sealed cleanly without the backend-handle abstraction that Stage 1c/d defines. An interim `pub(crate)` seal would force test helpers onto a duplicate accessor that Stage 1c would immediately rewrite.

3. Publishing impact is low: **no external Rust consumer links `ff-server` / `ff-scheduler` / `ff-engine` as a library** (verified `rg 'use ff_server' crates/` → ff-readiness-tests + ff-test + ff-server bin only; no external crates in-tree; no cairn checkout found at `/home/ubuntu/cairn-rs`). Cairn consumes ff-server via HTTP, and the HTTP surface (`ApiError::IntoResponse` at `api.rs:228`) already hides `ferriskey::Error` behind stable JSON shapes. The leak is Rust-API-only, and the Rust consumers are all first-party.

4. Worker F's 2026-04-22 audit (per RFC-012 §1.2) catalogues these as part of the same "`ferriskey::Error` leaking through `SdkError`" class — they are one problem, not two; splitting them weakens the migration event.

**Option B (ship ff-server PR now) — not recommended.**

Would seal 8 error-type sites in ff-server + ff-scheduler via ad-hoc wrapping, leave the 3 client-getter sites open, and pre-empt Stage 0's canonical `EngineError::Transport` design. Violates "Approve against RFC, not plan" (RFC-012 §5.0 is the accepted shape) and produces throwaway code.

**Action item.** Extend RFC-012 §5.0 (Stage 0) or §5.1 (Stage 1) to **explicitly** include `ServerError`, `SchedulerError`, and the `Engine::start` / `Scheduler::new` / `BudgetChecker::new` client-accepting constructors in scope. Currently these are implicit — a reader tracking #87/#88 against §5 alone would miss them. A 1-paragraph amendment naming the 3 crates + 10-ish sites closes the gap without redesign. This is cheaper than a separate PR and keeps the migration event coherent.

## Open questions

1. **Should `ff-scheduler::SchedulerError` fold into `EngineError` entirely?** It has only 3 variants (`Valkey`, `ValkeyContext`, `Config`); two map to `EngineError::Transport` and one to `EngineError::Validation`. The crate exists mostly for ff-server internals — collapsing the error hierarchy would simplify Stage 0. Flag for Worker I to consider in Stage 0 scope.

2. **Is `ff-server::Server::client()` a test-helper convenience or a supported public API?** If the former, `#[cfg(feature = "test-helpers")]` is an immediate Option-B-lite win (gates the leak behind a test-only feature). If the latter, it waits for Stage 1c. `crates/ff-test/src/helpers.rs:153-332` has ~10 call sites, all test-only — leans test-helper. Worth asking the RFC-012 owner before Stage 0 lands.

3. **`ff-backend-valkey::ValkeyBackend::client()` at `lib.rs:159` is documented as intentional** ("backend-internal access … RFC-012 §1.3"). Confirm this is the stable escape hatch post-Stage 1c and not itself a leak to seal. Comment at `lib.rs:158` says so, but worth an explicit call-out in RFC-012.

4. **`ff-readiness-tests::valkey::probe_server_version` takes `&ferriskey::Client` at `crates/ff-readiness-tests/src/valkey.rs:11`.** First-party readiness probe — sealing has no external benefit, but should probably migrate to the backend trait once Stage 1c lands to keep the probe backend-agnostic (needed for Postgres-backend readiness at Stage 2+).
