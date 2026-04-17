# Releasing FlowFabric

This doc covers the operator workflow for cutting a release. Tooling lives in
`release.toml` (cargo-release config) and `.github/workflows/release.yml`
(tag-triggered publish).

## What gets published

Eight crates, all pinned to the same version, published in topological order:

1. `telemetrylib`    — root of the ferriskey subtree
2. `ferriskey`       — Valkey client (reparented fork of glide-core)
3. `ff-core`         — types, keys, errors
4. `ff-script`       — typed FCALL wrappers + Lua library loader
5. `ff-engine`       — cross-partition dispatch + scanners
6. `ff-scheduler`    — claim-grant scheduler
7. `ff-sdk`          — worker SDK
8. `ff-server`       — HTTP server library + binary

Excluded from publish:

- `ff-test` — dev-only integration harness (`publish = false`).

## Pre-flight checklist

Before cutting a release, verify:

- [ ] W3's benchmark numbers for the target version exist at
      `benches/results/<new_version>/`. The release workflow does not
      enforce this; it is an operator-level gate.
- [ ] `CARGO_REGISTRY_TOKEN` is configured in repo settings (Settings →
      Secrets and variables → Actions). Generate at
      <https://crates.io/me> → API Tokens with scope `publish-new` +
      `publish-update`. The token must have upload rights to all eight
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

1. **verify** (CI gate) — `cargo check --workspace`, clippy -D warnings,
   unit tests + `ff-test` serial e2e against a Valkey service container.
   Same strictness as `.github/workflows/ci.yml`.
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
