# Releasing FlowFabric

This doc covers the operator workflow for cutting a release. Tooling lives in
`release.toml` (cargo-release config) and `.github/workflows/release.yml`
(tag-triggered publish).

## What gets published

Nine crates, all pinned to the same version, published in topological order:

1. `ferriskey`       — Valkey client (reparented fork of glide-core;
   the former `telemetrylib` sub-crate was inlined during the Tier 1
   envelope collapse and is no longer a separate publish target)
2. `ff-core`         — types, keys, errors
3. `ff-script`       — typed FCALL wrappers + Lua library loader
4. `ff-observability` — OTEL metrics registry + typed handles + no-op
   shim. Added to the publish set in v0.3.1 after v0.3.0
   partial-published (ff-engine depends on it as a workspace dep
   and its publish step fails without it being on crates.io).
5. `ff-backend-valkey` — RFC-012 EngineBackend trait Valkey impl,
   separated from ff-sdk in RFC-012 Stage 1a. Added to the publish
   set in v0.3.2 after v0.3.1 partial-published (ff-sdk depends on
   it as a workspace dep and its publish step fails without it
   being on crates.io).
6. `ff-engine`       — cross-partition dispatch + scanners
   (includes the `completion_listener` module for push-based DAG
   promotion; see [`rfc011-operator-runbook.md`](rfc011-operator-runbook.md)
   §"DAG promotion: push listener + safety-net reconciler")
7. `ff-scheduler`    — claim-grant scheduler
8. `ff-sdk`          — worker SDK. `direct-valkey-claim` feature is
   off by default; production deployments use the scheduler-routed
   HTTP path via `FlowFabricWorker::claim_via_server`
9. `ff-server`       — HTTP server library + binary

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
- [ ] Valkey minimum is 8.0+. The server refuses to start against
      Valkey < 8.0 (RFC-011 §13). Ensure all production + CI targets
      are on 8.x before cutting.
- [ ] RESP3 is required for the engine's completion listener.
      `FlowFabricWorker::connect` defaults to RESP3; servers fronted
      by protocol-downgrading proxies must be verified against.
- [ ] `CARGO_REGISTRY_TOKEN` is configured in repo settings (Settings →
      Secrets and variables → Actions). Generate at
      <https://crates.io/me> → API Tokens with scope `publish-new` +
      `publish-update`. The token must have upload rights to all nine
      crate names — claim them on crates.io first if they are new.
- [ ] Working on a release branch (`main` or `release/*`).
- [ ] Working tree is clean.

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
