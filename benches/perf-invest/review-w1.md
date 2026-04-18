# W1 cross-review — ferriskey round-3 API/behavior commits

**Reviewer:** Worker-1
**Branch:** `feat/ferriskey-iam-gate` @ `0181f30` (+ follow-up perf reports
`0dccfca`, `50e441d`, `0cb1fb3`, `27a26dc` are out-of-scope for W1)
**Scope:** 8 ferriskey API/behavior commits from round-3, plus the
report-only bench companion `0181f30`. W2 owns `benches/perf-invest/*`
reviews; W3 owns `examples/*`. This review stays on ferriskey.

## Verdict table

| SHA     | Subject                                                  | Verdict |
| ------- | -------------------------------------------------------- | ------- |
| 2a36b46 | remove telemetrylib, replace with tracing                | GREEN (1 YELLOW) |
| 72659cb | re-export ValkeyResult alias                             | GREEN |
| 031be78 | tracing::Instrument on send_command + integration test   | GREEN (1 YELLOW) |
| 025fd4e | introduce LazyClient + build_lazy()                      | GREEN |
| ba73945 | remove ClientWrapper::Lazy + 17 defensive Err arms       | GREEN |
| afaae66 | deprecate ClientBuilder::lazy_connect()                  | GREEN |
| 2111717 | feature-gate IAM behind `iam`                            | GREEN |
| c5e5394 | ff-sdk + ff-server: propagate iam/otel/metrics           | GREEN |
| a722efe | make blocking-cmd timeout extension configurable         | GREEN (1 YELLOW) |
| 0181f30 | bench(blocking-probe): ext-sweep (report-only)           | OUT-OF-SCOPE (W2) |

Overall: **merge-ready**. All 8 API/behavior commits are GREEN. Three
YELLOW items are documentation/naming nits — not blockers, addressable
in a follow-up.

## Known-risks checklist

| Risk                                                            | Status |
| --------------------------------------------------------------- | :----: |
| `ClientWrapper::Lazy` removal leaves no stale `Err(...)` arms   | OK     |
| `Telemetry` stub compiles for external consumers                | OK     |
| `cargo tree --no-default-features` has no AWS / otel / metrics  | OK     |
| `--features iam` brings aws-config + aws-sigv4 + urlencoding    | OK     |
| `--features otel` brings tracing-opentelemetry + opentelemetry  | OK     |
| `--features metrics` brings `metrics` crate                     | OK     |
| ff-sdk / ff-server feature flags propagate via `ferriskey/*`    | OK     |
| BLMPOP safety margin default unchanged at 500ms                 | OK     |
| lazy_connect=true at runtime yields actionable migration error  | OK     |
| `Client::new` has no `ClientWrapper::Lazy` arm left behind      | OK     |

## Per-commit detail

### 2a36b46 — telemetrylib removal + `tracing` events
Q1 claims match: telemetrylib crate removed, `telemetry_compat.rs` stub
exposes the same 23-method Telemetry surface as a deprecated no-op.
Q2 public API: `pub use telemetrylib::Telemetry` → `pub use
telemetry_compat::Telemetry` — 1:1 name match, external consumers that
do `use ferriskey::Telemetry;` still compile (verified with external
probe crate).
Q3 tests: lib-tests pass, tracing events fire at correct sites.
Q4 feature gates: tracing is unconditional (a foundational dep); no
default-feature regression.
Q5 migration: `#[deprecated(since, note)]` points consumers at tracing.
Q6 docs: deprecation note references "ferriskey CHANGELOG
§telemetry-redesign for migration".

**YELLOW — CHANGELOG.md does not exist** at `ferriskey/CHANGELOG.md` or
`CHANGELOG.md`. The deprecation note sends consumers to a file that
isn't in the tree. Either add the CHANGELOG or soften the note to point
at the commit SHA / release notes.

### 72659cb — `ValkeyResult` re-export
Purely additive alias `pub use value::Result as ValkeyResult;` at
`ferriskey/src/lib.rs`. No public API break. Nothing else to say.

### 031be78 — `tracing::Instrument` on `send_command`
Q1 claims match: `Client::send_command` wraps its future in
`debug_span!("ferriskey.send_command", command = %cmd_name, ...)
.in_current_span()` via `Instrument`. The new
`tests/test_tracing_spans.rs` has 3 integration tests (no-subscriber
no-op, subscriber captures events, telemetry stub returns zero).
Q3 tests: the tracing-subscriber capture test exercises the new
`.instrument()` wrap-through end-to-end.

**YELLOW — "zero cost when no subscriber" claim is imprecise.** The
flame confirms `Instrumented<T>::poll` is ~0.06% leaf self-time when no
subscriber is attached (effectively free), but the
`String::from_utf8_lossy(v).into_owned()` at `client/mod.rs:747` that
builds `cmd_name` for the span field **allocates on every call** — it
shows up at 0.04% inclusive on the flame. Small (< 1 µs per call), but
worth wrapping in `if tracing::enabled!(Level::DEBUG) { ... }` so the
alloc truly disappears when the subscriber isn't interested.

### 025fd4e — `LazyClient` + `ClientBuilder::build_lazy`
Q1 claims match: new `pub struct LazyClient { config, inner:
Arc<tokio::sync::OnceCell<Client>> }` with `connect() -> Result<&Client>`
(idempotent, double-checked locking via `OnceCell::get_or_try_init`),
`get() -> Option<&Client>` (non-blocking), and `from_config(
ConnectionRequest)`. Q2 public API: additive; no removal. Q3 tests:
`ValkeyClientForTests for LazyClient` adapter resolves connect-on-
first-use and delegates `send_command` so integration tests using
`assert_connected(&mut lazy_client)` work unchanged.
Design is clean — `OnceCell` eliminates the double-init race the old
`ClientWrapper::Lazy` solved with explicit locking.

### ba73945 — `ClientWrapper::Lazy` removal + 17 defensive `Err` arms
Biggest known risk in the series. Verified:
- Zero code references to `ClientWrapper::Lazy` remain (one doc comment
  in `pubsub/mod.rs` describing the removal — fine).
- 17 `ClientWrapper::Lazy(_) => Err(...)` arms all gone.
- `Client::new` rejects `lazy_connect: true` with
  `InvalidClientConfig` + actionable migration string (`"Use
  ClientBuilder::build_lazy() -> LazyClient to defer connection."`).
- Bootstrap ordering: `pubsub_synchronizer` is constructed with
  `Weak::new()`, then after the real `Arc<RwLock<ClientWrapper>>` is
  built, `pubsub::attach_internal_client(&synchronizer,
  Arc::downgrade(&arc))` wires the weak handle. `attach_internal_client`
  is `Weak::upgrade().is_none() => return`-guarded and feature-gated
  downcasts for `test-util` vs prod `EventDrivenSynchronizer`.
- `create_test_client` unit-test helper gone; `is_select_command`,
  `is_auth_command`, etc. are associated fns on `Client` — tests use
  `Client::fn(&cmd)` directly.
- Integration tests migrated: `Client::new(request_with_lazy_connect,
  None)` → `ferriskey::LazyClient::from_config(request)`. Identical
  deferred-connect semantics.
- `ConnectionRequest.lazy_connect` field kept for one release cycle.

Q3 verification from commit body: 263/263 ferriskey lib + 133/133
ff-test. Spot-checked: `cargo check --workspace` clean against HEAD.
Pre-existing drift in `tests/test_compression.rs:249` (`description()`
method missing on `CommandCompressionBehavior`) confirmed unrelated —
reproduces on `feat/ferriskey-telemetry-redesign` baseline.

### afaae66 — deprecate `ClientBuilder::lazy_connect`
`#[deprecated(since = "0.1.1", note = "Use ClientBuilder::build_lazy()
-> LazyClient instead. ...")]` on the builder method; the
`ConnectionRequest.lazy_connect` flag is kept but `Client::new` rejects
it at runtime (see ba73945). Docs cross-reference `LazyClient::from_
config`. Clean.

### 2111717 — feature-gate IAM behind `iam`
Q1 claims match: `[features] default = []`, `iam = [dep:aws-config,
dep:aws-credential-types, dep:aws-sigv4, dep:http, dep:urlencoding]`.
Extensive `#[cfg(feature = "iam")]` gates across
`client/standalone_client.rs`, `client/reconnecting_connection.rs`,
`client/types.rs`.

Verified via `cargo tree`:
- `cargo tree -p ferriskey --no-default-features` — zero
  `aws-config | aws-sigv4 | aws-credential-types | aws-smithy |
  urlencoding | opentelemetry` matches.
- `cargo tree -p ferriskey --features iam` — `aws-config v1.8.13`,
  `aws-sigv4 v1.3.8`, `aws-credential-types v1.2.11`, `urlencoding
  v2.1.3` all present.

Q2 public API: IAM types (`IAMTokenManager`, `IAMTokenHandle`,
`IamAuthenticationConfig`, `IAMTokenProvider`) only exist with the
feature — this is a breaking change for any consumer that referenced
them by name, but the commit body notes this explicitly and the
default feature-set for downstream builds that weren't using IAM is
unaffected.

### c5e5394 — ff-sdk + ff-server feature propagation
`ff-sdk/Cargo.toml`:
```toml
iam     = ["ferriskey/iam"]
otel    = ["ferriskey/otel"]
metrics = ["ferriskey/metrics"]
```
Same shape in `ff-server/Cargo.toml`. Verified:
- `cargo tree -p ff-sdk --no-default-features` → zero AWS/otel deps.
- `cargo tree -p ff-sdk --features iam` → aws-config + aws-sigv4
  propagate through.
Feature names align 1:1 with the ferriskey-side names.

### a722efe — configurable blocking-cmd timeout extension
Q1 claims match: `ClientBuilder::blocking_cmd_timeout_extension(
Duration)` added; per-`Client` state stored at `client/mod.rs:208`;
`get_request_timeout` / `get_timeout_from_cmd_arg` threaded through
with the extension parameter. Default unchanged at 500ms — confirmed
at `client/mod.rs:253`.
Q3 test: `test_blocking_extension_configurable` exercises ext ∈ {500ms,
1s, 2s} asserting `get_request_timeout = server_timeout + ext`.

**YELLOW — const name mismatch with commit body.** The commit body
names the public const `DEFAULT_BLOCKING_CMD_TIMEOUT_EXTENSION`, but
the actual symbol exported is `DEFAULT_DEFAULT_EXT_SECS` at
`client/mod.rs:253`. Awkward double-`DEFAULT_` + the `_SECS` suffix on
a `Duration` (not seconds). Rename before publishing — consumers will
reference this by name. Proposed: `DEFAULT_BLOCKING_CMD_TIMEOUT_
EXTENSION`, matching the builder method and the commit body.

### 0181f30 — bench(blocking-probe): ext-sweep (report-only)
Out of W1 scope (W2 owns bench/*). Read superficially to confirm it
doesn't modify ferriskey source — it only adds bench harness files
and a `FINDINGS.md`. No ferriskey behavior change.

## Open questions / follow-ups (non-blocking)

1. Create `ferriskey/CHANGELOG.md` with a §telemetry-redesign section,
   OR update the 2a36b46 deprecation note to reference the commit SHA
   instead. Pick one before publishing 0.1.1.
2. Rename `DEFAULT_DEFAULT_EXT_SECS` → `DEFAULT_BLOCKING_CMD_TIMEOUT_
   EXTENSION` to match the commit body and avoid the `_SECS` suffix
   on a `Duration`.
3. Gate the `String::from_utf8_lossy(cmd_name).into_owned()` in
   `Client::send_command`'s span construction behind
   `tracing::enabled!(Level::DEBUG)` so there's truly zero allocation
   when no subscriber is interested.

None of these block merge. (1) and (2) are pre-publish tidy-ups; (3)
is a ~0.04% inclusive perf nit confirmed against the Track-A flame.

## Verification commands

```bash
# Commit list
git log --oneline feat/ferriskey-iam-gate -n 15

# Default-feature dep tree (expect clean)
cargo tree -p ferriskey --no-default-features | \
  grep -E "aws-(config|sigv4|credential-types|smithy)|urlencoding|opentelemetry"

# IAM feature (expect aws-* + urlencoding)
cargo tree -p ferriskey --features iam | \
  grep -E "aws-(config|sigv4|credential-types)|urlencoding"

# ff-sdk propagation (expect clean by default, aws-* with --features iam)
cargo tree -p ff-sdk --no-default-features | grep -E "aws-config|opentelemetry"
cargo tree -p ff-sdk --features iam     | grep -E "aws-config|aws-sigv4"

# Lazy variant fully gone (expect: only one doc-comment match)
grep -rn "ClientWrapper::Lazy" ferriskey/src ferriskey/tests

# Default blocking extension
grep -n "DEFAULT_DEFAULT_EXT_SECS" ferriskey/src/client/mod.rs
```

## Merge readiness

**Recommendation:** merge. The 3 YELLOW items are documentation /
naming / micro-perf cleanups that can land in a follow-up without
gating round-3. All behavior-bearing code is sound; the known-risks
checklist is fully green; the Track-A flame (see
`report-envelope.md`) independently corroborates that the lazy
redesign + telemetry redesign + IAM gating did not regress the
per-command envelope (31 µs p50, parity with redis-rs 1.2.0).

**Artifact:** this file — `benches/perf-invest/review-w1.md`.
