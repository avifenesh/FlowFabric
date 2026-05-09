# Contributing to FlowFabric

Thanks for considering a contribution. This repo is small and
opinionated — read this page once before your first PR so we can keep
iteration tight.

For broader behavioural guidelines (simplicity-first, surgical
changes, release-gate requirements), see [`CLAUDE.md`](CLAUDE.md).
For the code of conduct, see [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
For reporting security issues, see [`SECURITY.md`](SECURITY.md).

## Issue first for non-trivial work

Open an issue before spending time on any of the following:

- New public trait methods on `EngineBackend` or the SDK.
- New `ff-*` crates.
- Changes to the Lua library (`lua/*.lua`) or `flowfabric_lua_version`.
- Changes to the HTTP API surface, error-code taxonomy, or wire
  format.
- New feature flags on `ff-sdk` / `flowfabric`.
- New RFC drafts — accepted RFCs live in the
  `avifenesh/flowfabric-archive` private repo; check there before
  starting a redesign.

One-line fixes, doc typos, dependency bumps, and test additions don't
need an issue. Just open a PR.

## Local development

**Fastest loop — no Docker.** The SQLite dev backend runs in-process:

```bash
FF_DEV_MODE=1 cargo test --workspace
```

`FF_DEV_MODE=1` is mandatory for the SQLite backend; it refuses to
construct without it (RFC-023 §1.0). See
[`docs/dev-harness.md`](docs/dev-harness.md) for the canonical dev
setup.

**Against a real Valkey.** Pull the same Valkey CI exercises:

```bash
docker run -d --name valkey -p 6379:6379 valkey/valkey:8-alpine
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) cargo test --workspace
```

**Against Postgres.** Set `FF_PG_TEST_URL` to a Postgres URL
(`postgres://user:pass@host:5432/db`); the `ff-backend-postgres`
tests use it. Migrations live under
[`docs/MIGRATIONS.md`](docs/MIGRATIONS.md).

## Commit style

Conventional commits, matching the existing `git log`:

```
<type>(<scope>): <subject>

<body — why, not what>
```

Types in use: `feat`, `fix`, `chore`, `ci`, `docs`, `diag`, `refactor`,
`test`, `perf`. Scope is typically a crate (`ff-sdk`, `ff-script`,
`ff-backend-postgres`) or an area (`release`, `readme`, `gitignore`).

If the change closes a tracked issue, reference it in the subject as
`#NNN` (e.g. `feat(ff-sdk): #331 WorkerRuntime`). PRs get a `(#PR)`
suffix appended by GitHub on merge — don't add it yourself.

Never pass `--no-verify`, `--no-gpg-sign`, or skip hooks. If a hook
fails, fix the underlying issue.

## Pull request expectations

**Before pushing:**

- `cargo fmt --all` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` passes locally (SQLite dev harness is
  enough for most paths; see above for Valkey/Postgres).
- New public API has rustdoc. `cargo doc --workspace --no-deps` clean.
- CHANGELOG entry under `[Unreleased]` for any user-visible change.
  Keep it short; the commit body carries the detail.

**CI gates that will block your PR:**

- [`matrix.yml`](.github/workflows/matrix.yml) — linux x86_64 + arm64
  × Valkey 7.2 + 8 × {standalone, cluster}, macOS arm64 standalone.
- [`security-and-quality.yml`](.github/workflows/security-and-quality.yml)
  — `cargo audit`, `cargo deny`, `cargo geiger` ratchet, CodeQL,
  `cargo machete`, `cargo semver-checks`.
- Lua drift guard — if you edited `lua/*.lua`, regenerate:
  `scripts/gen-ff-script-lua.sh` and commit the updated
  `crates/ff-script/src/flowfabric_lua_version`.

**PR body:**

- Summary of the change — what and why.
- Test plan — what you ran, what passes. Include the command line.
- If it changes consumer-visible surface, a note on migration impact
  and which `docs/CONSUMER_MIGRATION_*.md` got updated.

## Releases

You don't need to worry about releases as a contributor — the
maintainer cuts them. The contract is in
[`docs/RELEASING.md`](docs/RELEASING.md) and [`CLAUDE.md §5`](CLAUDE.md);
feel free to skim both so you know what your PR will face at tag time.
Pre-tag sweeps validate: `CHANGELOG` completeness, README freshness,
`POSTGRES_PARITY_MATRIX.md` rows for any new trait method, full
example build + live-run, and the `published_smoke` gate.

## Scope of this project

FlowFabric is a pre-1.0 durable-execution engine targeting Valkey and
Postgres. Out of scope for core (these will be closed politely):

- Redis support beyond the 7.2 fallback already in place.
- Non-Rust SDKs. The `ff-sdk` crate is Rust-only; language bindings
  live outside the core repo.
- Alternative DAG DSLs, YAML workflow definitions, visual builders.
- Kafka / message-bus integrations.
- Production-scale SQLite (RFC-023 permanent non-goal).

Bug reports and quality-of-life improvements on existing surface are
always welcome.

## Getting help

- Questions about the code → GitHub Discussions on
  [avifenesh/FlowFabric](https://github.com/avifenesh/FlowFabric/discussions)
  if enabled, otherwise an issue with the `question` label.
- Security issues → [`SECURITY.md`](SECURITY.md). Do not open a
  public issue.
- Code-of-conduct concerns → see
  [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

By submitting a contribution, you agree that your work will be
licensed under the [Apache License 2.0](LICENSE), the same license
this project ships under.
