# publish-list-lint

Cross-checks the publishable crate set across four sources of truth:

1. Workspace `Cargo.toml` members (filtered by `publish != false` AND
   `package.metadata.release.release != false`).
2. `release.toml` — structured `# LINT-PUBLISH-LIST-CRATE:` marker block.
3. `.github/workflows/release.yml` — `cargo publish -p <name>` steps.
4. `docs/RELEASING.md` — "What gets published" numbered list.

Fails CI if any of the four disagree.

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
