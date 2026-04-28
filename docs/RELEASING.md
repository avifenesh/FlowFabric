# Releasing FlowFabric

This doc covers the operator workflow for cutting a release. Tooling lives in
`release.toml` (cargo-release config) and `.github/workflows/release.yml`
(tag-triggered publish).

## What gets published

Thirteen crates, all pinned to the same version, published in topological order:

1. `ferriskey`       — Valkey client (reparented fork of glide-core;
   the former `telemetrylib` sub-crate was inlined during the Tier 1
   envelope collapse and is no longer a separate publish target)
2. `ff-core`         — types, keys, errors
3. `ff-script`       — typed FCALL wrappers + Lua library loader
4. `ff-observability` — OTEL metrics registry + typed handles + no-op
   shim. Added to the publish set in v0.3.1 after v0.3.0
   partial-published (ff-engine depends on it as a workspace dep
   and its publish step fails without it being on crates.io).
5. `ff-observability-http` — consumer-side Prometheus `/metrics` HTTP
   endpoint (axum Router + `serve()` convenience). Bridges
   `ff-observability`'s registry to a scrape endpoint for consumers
   embedding `ff-sdk` in library mode without `ff-server`. Added to
   the publish set alongside its introduction; `ff-server` still
   ships its own built-in `/metrics` route (PR #94).
6. `ff-scheduler`    — claim-grant scheduler. Must publish BEFORE
   `ff-backend-valkey` per RFC-017 Stage C (ff-backend-valkey owns
   `Arc<ff_scheduler::Scheduler>` per §7). Reordered in v0.8.0 after
   v0.8.0 partial-published (ff-backend-valkey failed version
   resolution because ff-scheduler ^0.8.0 wasn't yet on crates.io).
7. `ff-backend-valkey` — RFC-012 EngineBackend trait Valkey impl,
   separated from ff-sdk in RFC-012 Stage 1a. Added to the publish
   set in v0.3.2 after v0.3.1 partial-published (ff-sdk depends on
   it as a workspace dep and its publish step fails without it
   being on crates.io).
8. `ff-backend-postgres` — RFC-v0.7 EngineBackend trait Postgres
   impl. Wave 0 scaffold ships the crate as an `Unavailable`-stub
   implementation so `sqlx`/migrations infrastructure is in place
   before Wave 1+ fills in method bodies. Published alongside the
   Valkey backend so dual-backend ff-server builds resolve against
   crates.io.
9. `ff-backend-sqlite` — RFC-023 SQLite dev-only EngineBackend.
   Phase 1a scaffold (v0.12.0) ships the crate as an
   `Unavailable`-stub with the `FF_DEV_MODE=1` guard + registry dedup
   in place; Phase 1b+ fills in the hand-ported SQLite migrations
   and trait bodies. Published so consumers opting into the dev
   harness can `cargo add ff-backend-sqlite` from crates.io.
10. `ff-engine`       — cross-partition dispatch + scanners
    (includes the `completion_listener` module for push-based DAG
    promotion; see [`rfc011-operator-runbook.md`](rfc011-operator-runbook.md)
    §"DAG promotion: push listener + safety-net reconciler")
11. `ff-sdk`         — worker SDK. `direct-valkey-claim` feature is
    off by default; production deployments use the scheduler-routed
    HTTP path via `FlowFabricWorker::claim_via_server`
12. `ff-server`      — HTTP server library + binary
13. `flowfabric`     — umbrella re-export of the ff-* family
    (issue #279). Depends on every other publishable crate; MUST
    publish LAST. Lets consumers pin one crate + feature-flag the
    backend (`flowfabric = { version = "0.8", features = ["valkey"] }`
    or `features = ["postgres"]`) instead of tracking 7–8 separate
    pins in lockstep. Default-features=["valkey"] preserves v0.7–
    v0.8 stability posture.

Excluded from publish:

- `ff-test` — dev-only integration harness (`publish = false`).
- `ff-bench` — internal bench harness at `benches/harness/`
  (`publish = false`).

## Pre-flight checklist

Before cutting a release, verify:

- [ ] Benchmark numbers for the target version have been refreshed.
      Raw JSONs are local (`benches/results/*.json` is gitignored);
      the curated summary at `benches/results/baseline.md` is the
      tracked artifact and must reflect the HEAD that will be tagged.
      The release workflow does not enforce this; it is an
      operator-level gate.
- [ ] Valkey minimum is 7.2+. The server refuses to start against
      Valkey < 7.2 (enforced at `crates/ff-server/src/server.rs`;
      RFC-011 §13 describes the dependency on Functions + RESP3 which
      both stabilised in 7.2). Ensure all production + CI targets
      are on 7.2+ before cutting.
- [ ] SQLite minimum is 3.35+ (dev-only backend, RFC-023 §7.1).
      `ff-backend-sqlite` uses `sqlx` with the **bundled build**,
      which statically links SQLite into the binary — distro SQLite
      version is irrelevant for consumers. Recommended floor 3.38+
      for JSON1 ergonomics; modern distros ship 3.40+. The bundled-
      build choice was locked at v0.12.0 to decouple SQLite version
      from distro and to avoid an Ubuntu-22.04 contributor fork.
- [ ] Postgres `max_locks_per_transaction >= 512` on all target
      deployments (default `64` is insufficient under concurrent
      bench/partition load — see
      [`operator-guide-postgres.md`](operator-guide-postgres.md)).
      `PostgresBackend::connect` emits a `tracing::warn` at boot when
      the value is below `256`.
- [ ] RESP3 is required for the engine's completion listener.
      `FlowFabricWorker::connect` defaults to RESP3; servers fronted
      by protocol-downgrading proxies must be verified against.
- [ ] `CARGO_REGISTRY_TOKEN` is configured in repo settings (Settings →
      Secrets and variables → Actions). Generate at
      <https://crates.io/me> → API Tokens with scope `publish-new` +
      `publish-update`. The token must have upload rights to all thirteen
      crate names — claim them on crates.io first if they are new.
- [ ] Working on a release branch (`main` or `release/*`).
- [ ] Working tree is clean.

## Pre-publish smoke (v0.7+)

**Required release gate.** v0.6.0 shipped with a broken `read_summary`
because the smoke ran *after* `cargo publish`; v0.7 moves the smoke
in front of the tag so correctness regressions block the release
instead of triggering a hotfix point release
(`feedback_smoke_before_release.md`).

Before tagging `v0.7.0` (and every release thereafter):

1. Ensure fixtures are up on `127.0.0.1`:
   - Valkey 8.x on `:6379`
   - Postgres 16 on `:5432` with an `ff_smoke` database.
2. Run:
   ```bash
   scripts/smoke-v0.7.sh
   ```
   The script handles Valkey/Postgres preflight, applies the Postgres
   migrations via `sqlx migrate run` (if the CLI is on `PATH`), boots
   `ff-server` in the background, and runs the scenario binary.
3. All scenarios must pass on **both** backends. Any `Fail` or
   cross-backend parity violation aborts the release. `Skip` counts
   as failure under the default `--strict` mode (pass `FF_SMOKE_STRICT=0`
   only for debugging — the release gate always runs strict).
4. After `git tag` + `cargo publish`, run one more smoke against the
   crates.io artifacts to confirm the publish uploaded cleanly. This
   post-publish pass is a *publication-sanity check only* — it is
   not the correctness gate (the pre-publish smoke is).

## Published-artifact smoke (v0.9+)

**Additional release gate, compile-time.** `scripts/smoke-v0.7.sh`
validates correctness against the *in-tree* workspace. It does not
exercise the crates.io publish path or the external-consumer view
of the API. Past releases (v0.3.2 non-exhaustive-without-constructor,
v0.6.0 read_summary) shipped bugs that only appeared when a scratch
consumer built against the published artifacts
(`feedback_smoke_after_publish.md`).

For each release cut from v0.9.0 onward, `scripts/published-smoke.sh`
runs a scratch consumer against the published crates.io artifacts and
confirms:

- The umbrella `flowfabric` crate + `ff-sdk` + `ff-core` +
  `ff-backend-valkey` + `ff-backend-postgres` all resolve at the
  target version from crates.io.
- The default-feature (Valkey) build compiles.
- The `--no-default-features --features=postgres` build compiles.
- Consumer-facing v0.9 symbols are reachable by name:
  `flowfabric::sdk::ClaimGrant`,
  `flowfabric::core::backend::PrepareOutcome`,
  `flowfabric::core::contracts::SeedWaitpointHmacSecretArgs`,
  `flowfabric::core::contracts::LeaseSummary` fields (`.lease_id`,
  `.attempt_index`, `.last_heartbeat_at`), and the default feature
  `valkey` pulling `ff-backend-valkey`.

Usage:

```bash
scripts/published-smoke.sh 0.9.0
```

Exit codes: `0` all clean; `1` build or symbol resolution failed;
`2` script invocation problem.

**Intended release.yml wiring (follow-up PR):** add a new
`published_smoke` job to `.github/workflows/release.yml` that
`needs: publish` and runs `scripts/published-smoke.sh ${{ env.TAG_VERSION }}`.
The GitHub Release job (`release`) then moves to
`needs: [publish, published_smoke]` so a failed published-artifact
smoke blocks the release-notes-cut step (the publish itself cannot
be rolled back beyond `cargo yank`). This enforces CLAUDE.md §5
item 8 for the external-consumer dimension.

Scenarios covered (see `benches/smoke/README.md` for details):

- `claim_lifecycle` — create → claim → progress → complete
- `flow_anyof` — AnyOf{CancelRemaining} DAG reachability
- `suspend_signal` — suspend + deliver_signal (RFC-013/014)
- `stream_durable_summary` — **the v0.6.0 regression scenario** (read_summary)
- `stream_best_effort` — read_stream / tail_stream probe
- `cancel_cascade` — cancel routing through dispatcher
- `fanout_slo` — 50-way ingress + claim-pump observation

## Cutting a release

```bash
# Dry-run first to see what would change.
cargo release patch --dry-run

# Execute the bump locally. Writes one commit + one tag; does NOT push
# and does NOT publish (see release.toml).
cargo release patch --execute

# Review the commit + tag.
git log -1
git tag -l v*

# Push branch and tag. The tag push fires the GHA workflow.
git push origin HEAD
git push origin v<new_version>
```

Replace `patch` with `minor` or `major` as appropriate.

## Workflow behavior

On tag push matching `v*.*.*`:

1. **verify** (CI gate) — `cargo check --workspace`, clippy -D warnings
   across the workspace AND ferriskey (the vendored fork's test suite
   + benches are gated post-#37), unit tests + `ff-test` serial e2e
   against a Valkey 8.x service container. Same strictness as
   `.github/workflows/matrix.yml`.
2. **publish** (needs verify) — sequential `cargo publish -p <crate>`
   in path→version-dep order with 30s sleeps between each for crates.io
   index propagation. `--no-verify` flag is passed since Job 1 already
   validated the source.
3. **release** (needs publish) — `gh release create` with auto-generated
   notes from merged PRs since the previous tag.

## Dry-rehearsing the workflow

The workflow has a `workflow_dispatch` trigger with a `dry_run` input.
When `dry_run: true`, only the `verify` job runs — the `publish` and
`release` jobs are skipped. Use this to validate the workflow from the
Actions tab without touching crates.io:

1. Actions → Release → Run workflow → select branch → check `dry_run`.
2. Verify `verify` runs green.

## Partial-publish recovery

If `cargo publish` succeeds for crate N but fails for crate N+1, the
workflow fails the job and prints an explicit `::error::` annotation
listing the already-published crates that need yanking.

To recover:

1. For each crate that was published before the failure:
   ```bash
   cargo yank --version <X.Y.Z> <crate_name>
   ```
   (Yanking only marks the version as unavailable for new lockfiles;
   it does not prevent existing users from resolving it.)

2. Fix the root cause on a hotfix branch.

3. Re-tag as `v<X.Y.(Z+1)>`. Crates.io rejects re-publishing a yanked
   version under the same number, so you must bump the patch.

4. Push the new tag; the workflow re-runs from the start.
