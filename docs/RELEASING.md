# Releasing FlowFabric

This doc covers the operator workflow for cutting a release. Tooling lives in
`release.toml` (cargo-release config) and `.github/workflows/release.yml`
(tag-triggered publish).

## What gets published

Six crates, all pinned to the same version:

1. `ff-core`
2. `ff-script`
3. `ff-engine`
4. `ff-scheduler`
5. `ff-sdk`
6. `ff-server`

Excluded from publish:

- `ferriskey` — vendored fork, publishability pending (issue #9 item 5).
- `ff-test` — dev-only integration harness.

## Pre-flight checklist

Before cutting a release, verify:

- [ ] All Batch C release blockers (issue #9 item 5, `release-blocker` label)
      are resolved. Specifically: `ferriskey` is publishable — either
      `publish = true` with proper NOTICE/attribution, or path dep replaced
      with a published version.
- [ ] W3's benchmark numbers for the target version exist at
      `benches/results/<new_version>/`. The release workflow does not
      enforce this; it is an operator-level gate.
- [ ] `CARGO_REGISTRY_TOKEN` is configured in repo settings (Settings →
      Secrets and variables → Actions). Generate at
      <https://crates.io/me> → API Tokens with scope `publish-new` +
      `publish-update`.
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

## Why `ferriskey` blocks the first release

`ferriskey` is a vendored fork of glide-core (valkey-glide) currently
set to `publish = false`. Six of our publishable crates depend on it
via a workspace path dep. `cargo publish` rejects path deps at the
index-upload step, so the first real tag push will fail on the first
crate that has a `ferriskey` dep.

Resolution options (tracked in issue #9 item 5):

- Publish `ferriskey` as-is with Apache-2.0 NOTICE + attribution.
- Rename before publishing to avoid any glide-core namespace overlap.
- Retire the fork — upstream changes to valkey-glide and depend on
  the published crate.

Pick one before tagging a real release.
