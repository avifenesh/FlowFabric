# Plan A — telemetrylib rewrite/removal — for owner evaluation

**Author:** Worker-3
**Branch:** `feat/ferriskey-perf-invest`
**Status:** investigative scoping. No source changes proposed by this
document. All options below are for the project owner to evaluate and
sequence.

## §1 Current state

`ferriskey/telemetry/` is a separate crate (2 485 LOC across 4 files)
wired into `ferriskey/src/` via `use telemetrylib::{Telemetry,
FerrisKeyOtel, FerrisKeySpan}`. It exposes three logical surfaces:

| Surface | File | LOC | What it is |
|---|---|---:|---|
| `Telemetry` struct + static singleton | `telemetry/src/lib.rs:13-209` | 209 | Process-global counters behind a `std::sync::RwLock<Telemetry>` (lazy_static). Counters: total_connections, total_clients, compression bytes in/out, subscription-sync state. |
| `FerrisKeyOtel` + `FerrisKeySpan` | `telemetry/src/open_telemetry.rs:603-1085`, `:407-497` | 1 748 | OpenTelemetry integration — tracer + meter providers, 4 `OnceLock<Counter>` + 1 `OnceLock<Gauge>`, initialise-once config builder, `record_timeout_error` / `record_retry_attempt` / `record_moved_error` / `record_subscription_out_of_sync` fire-and-forget helpers. |
| File exporters | `metrics_exporter_file.rs` (307), `span_exporter_file.rs` (221) | 528 | Periodic JSON-lines exporter writing `Telemetry` snapshots and OTel spans to a rotating file. Used by the Python/Java language bindings' process-external collector. |

### 1.1 Call-site inventory in `ferriskey/src/`

29 lines mention `telemetrylib`. Unique call signatures, with
file:line + classification. Invocation counts are for the whole
`baseline-scenario1` run (16 workers drain 10 000 tasks; 30 000
commands total — see §1.1a for the per-site table and assumptions):

| # | Call site | file:line | Classification | Per-run invocations |
|---|---|---|---|---:|
| T1 | `Telemetry::incr_total_connections(1)` | `client/reconnecting_connection.rs:257` | WARM (per reconnect + initial connect) | 16 (one per worker at connect) |
| T2 | `Telemetry::decr_total_connections(1)` | `client/reconnecting_connection.rs:365` | COLD (teardown path) | 16 (one per worker at disconnect) |
| T3 | `Telemetry::decr_total_connections(1)` | `client/reconnecting_connection.rs:411` | COLD (failed-reconnect teardown) | 0 |
| T4 | `Telemetry::incr_total_connections(1)` | `client/reconnecting_connection.rs:491` | WARM (reconnect) | 0 |
| T5 | **`Telemetry::decr_total_clients(1)` in `StandaloneClient::drop`** | `client/standalone_client.rs:62-70` | **HOT** (see §1.2) | **~30 000 (one per command)** |
| T6 | `Telemetry::incr_total_clients(1)` | `client/standalone_client.rs:324` | COLD (initial client creation) | 16 |
| T7 | `Telemetry::incr_total_values_compressed / incr_total_original_bytes / incr_total_bytes_compressed / incr_compression_skipped_count` | `compression.rs:333-358` | COLD (compression off) | 0 |
| T8 | `Telemetry::incr_total_values_decompressed / incr_total_bytes_decompressed` | `compression.rs:389-390` | COLD (compression off) | 0 |
| T9 | `Telemetry::` (cluster container / cluster mod) | `cluster/container.rs:12`, `cluster/mod.rs:102` | COLD (standalone bench) | 0 |
| T10 | `FerrisKeyOtel::record_timeout_error()` | `client/mod.rs:215`, `:877` | COLD (only on timeout) | 0 on happy path |
| T11 | `FerrisKeyOtel::record_retry_attempt()` | `cluster/pipeline.rs:97`, `cluster/mod.rs:1405` | COLD (cluster retry path) | 0 |
| T12 | `FerrisKeyOtel::record_moved_error()` | `protocol/parser.rs:28` | COLD (only on MOVED redirect) | 0 |
| T13 | `FerrisKeySpan::new / set_attribute / end` | `cmd.rs:34` (field), `pipeline.rs:3`, `cluster/mod.rs:102` | COLD when `Cmd::span = None` | 0 |

Total per-run: **~30 048 Telemetry calls** — 30 000 of them (≈ 99.84 %)
are T5, `decr_total_clients` firing in `StandaloneClient::drop` once
per command. Everything else combined is 48 invocations over the
lifetime of the run.

### 1.1a Invocation frequency at a glance (bench worker shape)

Assumptions: `baseline-scenario1` runs 10 000 tasks total,
submitted once and drained by 16 workers. Each task = 1 RPUSH seed +
1 BLMPOP claim + 1 INCR complete = **30 000 commands total across
the fleet**. 16 physical connections (one per worker). ~1.3 s wall
time at ~8 000 ops/sec. No cluster, no compression, no IAM, no OTel
init.

| Site | Per-command? | Total invocations in a 10 000-task bench | Rate at 8 000 ops/sec |
|---|---|---|---|
| T1 incr_total_connections | No — per connect | 16 | 12 /sec (connect bursts) |
| T2 decr_total_connections (teardown) | No — per disconnect | 16 | 12 /sec (teardown burst) |
| T3 decr_total_connections (reconnect) | No — per reconnect | 0 | 0 |
| T4 incr_total_connections (reconnect) | No — per reconnect | 0 | 0 |
| **T5 decr_total_clients (Drop on clone)** | **Yes — one per command** | **30 000** | **~8 000 /sec** |
| T6 incr_total_clients | No — per client | 16 | 12 /sec (connect burst) |
| T7 compression incr_* | No — compression off | 0 | 0 |
| T8 decompression incr_* | No — compression off | 0 | 0 |
| T9 cluster Telemetry | No — standalone bench | 0 | 0 |
| T10 record_timeout_error | Only on timeout | 0 | 0 |
| T11 record_retry_attempt | Only on cluster retry | 0 | 0 |
| T12 record_moved_error | Only on MOVED redirect | 0 | 0 |
| T13 FerrisKeySpan ops | Only when caller opts in | 0 | 0 |
| **Total Telemetry invocations** | — | **~30 048** | **~8 000 /sec** |

T5 dominates: **99.84 % of Telemetry invocations** in a happy-path
scenario-1 bench are `decr_total_clients` firing in
`StandaloneClient::drop`. T1/T2/T6 combined are 48 invocations over
the whole run (a bursty 16-per-lifecycle-boundary pattern). T10-T12
fire **zero** times on a clean happy-path run.

The critical read: every one of those 30 000 T5 invocations takes a
global `lazy_static::RwLock<Telemetry>::write()` — a process-wide
serialisation point. At 8 000 invocations/sec across 16 writer
tasks, the lock is contended enough to matter. This is the telemetry
call that dominates the telemetry overhead.

### 1.2 Why T5 is hot: the Drop-on-clone pattern

`StandaloneClient` at `client/standalone_client.rs:57-60`:

```rust
#[derive(Clone, Debug)]
pub struct StandaloneClient {
    inner: Arc<DropWrapper>,
}
```

`Drop` impl at lines 62-70:

```rust
impl Drop for StandaloneClient {
    fn drop(&mut self) {
        // StandaloneClient is Clone (shares Arc<DropWrapper>), so this Drop fires
        // on every clone drop, not just the final one.
        Telemetry::decr_total_clients(1);
    }
}
```

The source comment acknowledges Drop fires on every clone drop. In
the per-command path:

1. `Client::execute` at `ferriskey_client.rs:358-367` does `(*self.0).clone()`.
2. `ClientInner::send_command` at `client/mod.rs:811` calls
   `self.get_or_initialize_client().await` which at line 636 does
   `guard.clone()` on the `ClientWrapper`.
3. `ClientWrapper::Standalone(client)` is derived-Clone, so the inner
   `StandaloneClient` is cloned (Arc bump) — and the **return value of
   `get_or_initialize_client` is one per-command owned
   `StandaloneClient`.**
4. `execute_command_owned` at `client/mod.rs:721` pattern-matches
   `ClientWrapper::Standalone(mut client) => client.send_command(&cmd).await`.
   `client` is moved-owned; at async-block exit it's dropped.
5. `Drop` fires → `Telemetry::decr_total_clients(1)` → a global
   `RwLock::write()` acquisition + 2 integer ops + lock release.

**This is one global write-lock acquisition per command**, under
contention across all 16 worker tasks in a standalone-bench scenario.

The counter semantics are also broken: `incr_total_clients` fires once
at actual creation (`StandaloneClient::create_client`
→ `standalone_client.rs:324`), but `decr` fires on every clone drop.
After N commands: `total_clients = N_workers - N_commands`, saturating
at 0. The counter does not reflect reality — see §1.3 for the
aggregate cost.

### 1.3 Aggregate cost in the bench shape

Round-1 ss probe (`report-w1.md`) showed 17 concurrent connections for
redis-rs and 19 for ferriskey, with ferriskey at 7 993 ops/s vs
redis-rs 14 755 ops/s.

Per-command, ferriskey pays:
- 1 × `Telemetry::decr_total_clients` = 1 global `RwLock::write`
  acquisition under 16-task contention
- 5 × `Routable::command()` allocs (not a telemetry concern — see
  `report-w3.md` §1 item F1)
- 1 × `Arc::new(cmd.clone())` (also not telemetry — F2)
- `FerrisKeyOtel::record_timeout_error` / retry / moved: **0 calls
  on happy path** because of `is_initialized()` short-circuit at
  `open_telemetry.rs:973`.

The telemetry cost is concentrated in T5. OTel is cold-path. The
`lazy_static::RwLock` pattern in `Telemetry` is the structural issue.

## §2 Intent

Evidence from the crate structure + naming:

1. **Cross-language binding observability.** The file exporters
   (`metrics_exporter_file.rs`, `span_exporter_file.rs`) write periodic
   snapshots to disk in a format consumable by the glide-core
   Python/Java/Node language bindings' out-of-process collector.
   Language bindings need observability without re-implementing OTel
   in each target runtime; a shared Rust-side telemetry + file
   exporter fits that requirement.
2. **OpenTelemetry counters that survive the binding boundary.** The
   OTel `OnceLock<Counter<u64>>` statics at
   `open_telemetry.rs:605-611` are populated once per process via
   `FerrisKeyOtel::initialise(...)` and then read by the exporter.
   The binding's Python/Java side never has to know how to emit OTel.
3. **Process-global state.** `lazy_static::RwLock<Telemetry>`
   implies "one Telemetry per process, accessible from any Rust call
   site without threading a reference" — which matches a glide-core
   scenario where the Rust client runs inside a long-lived Python
   process and accumulates across every call from every thread.

### 2.1 What pure-Rust ferriskey doesn't need

In a pure-Rust embedding (FlowFabric's case), the same telemetry
shape is already provided by the `tracing` crate:

- Counters → `tracing::counter!` or a `metrics` crate integration.
- Spans → `tracing::span!`.
- Subscribers → the application picks (tracing-subscriber, opentelemetry,
  stdout, none-at-all).
- Zero cost when no subscriber is installed — `tracing` macros compile
  to no-op.

The file-exporter side (PS: snapshot JSON to disk every N seconds) is
an operator tool, not a library concern. A `tracing-subscriber` Layer
can do the same without a crate-internal mutex.

## §3 Proposed changes (options — owner decides)

### Option 1 — REMOVE telemetrylib, replace with `tracing`

Rip out the `Telemetry` struct entirely; replace each
`Telemetry::incr_*/decr_*` call with a `tracing::info!` / `counter!` /
equivalent. Drop the dependency on `telemetrylib` from `ferriskey/src/`.

**Pros:**
- Zero per-command cost when no tracing subscriber is installed.
  `tracing` macros are no-op at the call site (compiles to nothing
  plus a cheap level-filter check).
- Correctness: fixes T5's broken decrement-on-every-clone counter
  semantic — `tracing::info!("client created")` does not need to
  match an `info!("client dropped")`.
- Simpler mental model: "metrics come out of `tracing` subscribers"
  matches Rust ecosystem default.
- Removes ~2 500 LOC of vendored telemetry code.

**Cons:**
- Language bindings (glide-core's Python/Java users) lose the
  pre-packaged file exporter. Someone has to wire up an equivalent
  `tracing-subscriber::Layer` and document the replacement.
- OTel users lose the `FerrisKeyOtel::initialise(...)` convenience;
  they'd configure an `opentelemetry-sdk` pipeline themselves. Some
  downstream library code that calls `FerrisKeyOtel::record_*`
  becomes `counter!("ferriskey.timeouts").increment(1)` — mechanical
  rewrite across ~8 call sites.
- Risk: removing the public `Telemetry` re-export at
  `ferriskey/src/lib.rs:90` is a breaking API change.

### Option 2 — REDESIGN: keep the metrics, drop the hot-path mutex, use atomics

Replace `lazy_static::RwLock<Telemetry>` with a set of top-level
`static AtomicUsize`s. Change every `incr_*/decr_*` method to
`COUNTER.fetch_add(1, Ordering::Relaxed)`. Keep the `FerrisKeyOtel`
initialise/record surface. Keep the file exporter. Keep the public
API of `Telemetry::total_clients()` etc. — just change their backing
store.

**Pros:**
- Preserves the cross-language-binding contract: the public API of
  `Telemetry` does not change. Language bindings continue to work.
- Eliminates the T5 hot-path RwLock. `fetch_add(1, Relaxed)` on a
  per-field atomic is ~1 ns uncontended, 10-30 ns under 16-way
  contention — versus ~hundreds of ns for a contended RwLock write.
- Fixes the semantic broken counter: if we accept that
  `decr_total_clients` fires on every clone drop, we can flip the
  Drop to track that explicitly (e.g. "live_client_clones" instead
  of "total_clients") or use an `Arc<AtomicBool>` inside DropWrapper
  to decrement only on the final drop.

**Cons:**
- Does not address the design question of whether a process-global
  Telemetry state *should* exist in a pure-Rust embedding.
- More LOC change than Option 1 (every `incr_*/decr_*` method is
  touched) but no public API break.
- Language binding file exporter keeps working but still imposes
  memory + disk-IO overhead on processes that don't need it.

### Option 3 — KEEP the current shape; feature-gate per call site

Add a `telemetry` Cargo feature. Every `Telemetry::*` and
`FerrisKeyOtel::*` call site wraps in `#[cfg(feature = "telemetry")]`.
Default OFF for pure-Rust users; language bindings build with the
feature enabled.

**Pros:**
- Minimal risk — existing behavior preserved for users who opt in.
- Zero per-command cost when the feature is off (the Drop impl
  itself vanishes at compile time).
- Easy to roll out: one-line `#[cfg]` per call site, single feature
  flag.

**Cons:**
- Cargo-feature conditional compilation is a known pain point for
  downstream integrators who want a mix. If ferriskey exposes any
  API whose shape depends on the feature (e.g. a `Telemetry`
  re-export), consumers hit "feature unification" footguns where
  depending on two crates that disagree on the feature activates
  it transitively.
- Doesn't fix the broken counter semantic — just hides it behind a
  feature.
- Two code paths to maintain. CI must cover both.

### 3a — Hybrid (not a standalone option, just a note)

Option 1 + a thin `ferriskey-tracing` adapter crate could land the
file exporter as `FileLayer` (a `tracing-subscriber::Layer`). Same
observability shape for language bindings, zero per-command cost for
pure-Rust. Listed here to flag the possibility; owner can mix and
match.

## §4 Estimated effort per option

| Option | Files touched (rough) | Size | Public API break | Hot path after change |
|---|---|---|---|---|
| 1 (remove) | all 29 telemetrylib use sites + telemetry/ crate removal + `pub use telemetrylib::Telemetry` at lib.rs:90 | M (3-5 days) | Yes (Telemetry struct export) | Zero telemetry cost |
| 2 (redesign — atomics) | `telemetry/src/lib.rs` (body rewrite, signatures preserved) + potential StandaloneClient Drop change | S (1-2 days) | No | 1× atomic fetch_add per command |
| 3 (feature-gate) | ~29 `#[cfg]` additions across 8 files | S (half day) | No (feature is additive) | Zero when feature off |
| 3a (hybrid) | Option 1 + ~300 LOC new adapter crate | M (4-6 days) | Yes | Zero |

## §5 Open questions for owner decision

1. **Who currently consumes `telemetrylib`?** Just ferriskey/src itself,
   or do the glide-core Python/Java/Node bindings depend on it via
   ferriskey? The call-site inventory in §1.1 is ferriskey-internal
   only; the cross-binding consumers I can't see from the Rust tree.
2. **Is `Telemetry::total_clients()` etc. a stable public API promise?**
   The `pub use telemetrylib::Telemetry` at `lib.rs:90` re-exports the
   whole struct. Removing the re-export breaks any external code
   reading `ferriskey::Telemetry::total_clients()`.
3. **Is there a telemetrylib consumer in a glide-core language binding
   that reads the counters more often than `FerrisKeyOtel::initialise`
   exports them?** If so, the RwLock semantic around snapshot
   consistency might matter and atomics (Option 2) would need explicit
   ordering (not `Relaxed`).
4. **What's the tracing-subscriber story for pure-Rust consumers today?**
   If no user has ever installed a subscriber for ferriskey spans,
   Option 1's "replace with tracing" has zero existing consumers to
   break.
5. **Regarding the broken counter semantic (T5):** is the current
   "decr per clone-drop" behavior load-bearing for any observability
   dashboard? If dashboards show "live clients" as the `total_clients`
   metric, they're already showing a wrong number. Fixing this is a
   semantic change but arguably a correctness improvement.

## §6 Cross-reference to Round 1 perf reports

- `report-w1.md` flagged `Telemetry::decr_total_clients` in
  `StandaloneClient::Drop` as hot-path-adjacent. **Confirmed: §1.2
  traces Drop-fires-on-every-clone to the source; the call IS hot-path
  at ~1 invocation per command (~30 000 per 10 000-task scenario-1
  run — see §1.1a table).**
- `report-w3.md` §F14 scored `FerrisKeyOtel::record_timeout_error`
  as zero-cost on happy path. **Confirmed: §1.1 T10-T12 are all
  gated by `FerrisKeyOtel::is_initialized()` at
  `open_telemetry.rs:973,990,1007` which short-circuits to a single
  `OnceCell::get().is_some()` check when OTel isn't configured.**
- `report-w2.md` wider-workload results (streams +11%, pipeline +6%,
  BLMPOP -46%) are consistent with T5 being the dominant telemetry
  cost: streams + pipeline paths amortize over fewer command-shaped
  invocations, so the per-command `decr_total_clients` cost matters
  less there.

## §7 Cleanup items (telemetry-adjacent)

**`ValkeyResult` not re-exported from `ferriskey/src/lib.rs`.**
`ferriskey/benches/connections_benchmark.rs:21` uses `ValkeyResult`,
but the `pub use value::{...}` block at `ferriskey/src/lib.rs:48-51`
exports `Result`, not `ValkeyResult`. Compile error blocks the
native bench. Two-line fix: either add `pub use value::Result as
ValkeyResult;` to `lib.rs` or s/ValkeyResult/Result/ in the bench.
Not a telemetry concern per se but it prevents running the native
benchmark that would measure telemetry overhead directly.

## §8 Anti-goals

- **Not a recommendation.** §3 presents three independently-viable
  options; §5 lists the questions that gate which one is right.
- **Not a measurement.** T5's ~30 000 calls-per-run is a static
  count from the source under the assumed worker shape. Actual RwLock
  contention cost is for W1's flamegraph to quantify.
- **Not a breakdown of what Python/Java bindings need.** The §2
  intent inference is from code structure; the owner has ground truth
  from the binding teams that I don't.
