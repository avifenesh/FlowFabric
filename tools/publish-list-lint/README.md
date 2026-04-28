# publish-list-lint

Cross-checks the publishable crate set across five sources of truth:

1. Workspace `Cargo.toml` members (filtered by `publish != false` — both
   boolean `false` and the empty-array form `publish = []` count as
   unpublishable — AND `package.metadata.release.release != false`).
2. `release.toml` — structured `# LINT-PUBLISH-LIST-CRATE:` marker block.
3. `.github/workflows/release.yml` — `cargo publish -p <name>` steps.
4. `docs/RELEASING.md` — "What gets published" numbered list.
5. `.github/workflows/matrix.yml` — `feature-propagation` matrix
   `crate:` list.

Fails CI if any of the five disagree as a set. Additionally asserts
that `release.toml` and `release.yml` agree on *publish order* — the
two author the same topological sequence, so order drift between them
means the doc lies about what the workflow actually does. `docs/RELEASING.md`
and the feature-propagation matrix are order-free (narrative + parallel
sharding respectively).

### Follow-up (v0.13 target)

Replace the hardcoded `crate:` list in the feature-propagation matrix
with a setup step that generates it from the `# LINT-PUBLISH-LIST-CRATE:`
markers at runtime, so there is a single source of truth instead of a
lint-asserted mirror. The current design trades a lint extension for
faster v0.12 ship; a dynamic matrix is the correct endgame.

Motivation: `feedback_release_publish_list_drift.md`. Adding a new publishable
workspace crate requires updating all four locations in the same PR; v0.3.0
and v0.3.1 each partial-published because one of them was missed.

## Run locally

```sh
cargo run -p publish-list-lint
```

## Reproduce a failure

Run from the repo root:

```sh
# 1) Rename one entry in release.toml's LINT-PUBLISH-LIST block:
sed -i 's/LINT-PUBLISH-LIST-CRATE: flowfabric/LINT-PUBLISH-LIST-CRATE: flowfabrik/' release.toml

cargo run -p publish-list-lint
# Expected: exit 1 + diff message identifying `flowfabric` as missing
# from release.toml and `flowfabrik` as present in release.toml only.

git checkout release.toml
```

## Decisions

- **Structured markers over free-text parsing** — fragile-comment regex is
  explicitly rejected. The `# LINT-PUBLISH-LIST-CRATE:` prefix is part of the
  contract; removing or renaming it breaks this lint.
- **Plain-text scan of release.yml** — keeps the tool a single dep. A full
  YAML parse would need `serde_yaml` with its security caveats; the
  `cargo publish -p <name>` pattern is stable across all 13 steps.
