# Main final verification (2026-04-23)

## HEAD

`0847f90` — feat(ff-core): UsageDimensions constructor + builders; docs: fix
BackendError match pattern (#162). Pulled from origin/main; fast-forwarded
cleanly from `ffa56bd` (post-#162 is the current tip).

## Build results

- Full workspace + all-features: **PASS** (`cargo build --workspace
  --all-features` finished in 27.80s; warnings only — pre-existing dead-code
  in `ferriskey::cmd::set_fenced`).
- ff-sdk --no-default-features: **PASS** (`cargo build -p ff-sdk
  --no-default-features` finished in 10.97s; agnostic facade compiles without
  ferriskey on ff-sdk's own direct edges).
- Workspace lib tests compile: **PASS** (`cargo test --workspace --lib
  --no-run` produced unittest executables for every crate: ferriskey,
  ff-backend-valkey, ff-core, ff-engine, ff-observability, ff-readiness-tests,
  ff-scheduler, ff-script, ff-sdk, ff-server, ff-test).

## cargo package --workspace

`cargo package --workspace --allow-dirty` initially errored on
`ff-readiness-tests` (no version on its `ff-test` dep — but both crates are
`publish = false`; cargo's workspace-package verifier requires version-pinned
deps even for non-publish crates). Re-ran with `--exclude ff-readiness-tests
--exclude ff-test`; all 9 publishable crates packaged + verify-compiled:

- ferriskey 0.3.4 — OK (65 files, 432.5KiB compressed)
- ff-core 0.3.4 — OK (18 files, 87.7KiB)
- ff-script 0.3.4 — OK (24 files, 118.1KiB)
- ff-backend-valkey 0.3.4 — OK (8 files, 41.5KiB)
- ff-observability 0.3.4 — OK (7 files, 8.7KiB)
- ff-engine 0.3.4 — OK (25 files, 60.3KiB)
- ff-sdk 0.3.4 — OK (11 files, 79.3KiB)
- ff-scheduler 0.3.4 — OK (31.6KiB)
- ff-server 0.3.4 — OK (12 files, 97.1KiB)

All verify-compiles finished in 3m00s.

## Scratch consumer smoke

Scratch at `/tmp/ff-smoke-main-final` (path-dep on `/home/ubuntu/FlowFabric`):

- `ScannerFilter::new().with_namespace("t")` — compiles.
- `BackendConfig::valkey("127.0.0.1", 6379)` — compiles.
- `UsageDimensions::new().with_input_tokens(10).with_dedup_key("x")` —
  compiles (confirms #162 landing: `new()` + builders are on the public
  surface).
- `Frame::new(bytes, FrameKind::Event).with_frame_type("delta")
  .with_correlation_id("c")` — compiles.
- `match err { SdkError::Backend(BackendError::Valkey { kind, .. }) => ... }`
  — compiles, runs, and matches (`kind=Transport`).
- `cargo build` (default features): PASS.
- `cargo build --no-default-features`: PASS.
- `cargo run`: prints `smoke ok: kind=Transport`.
- Scratch deleted; `ls /tmp/ff-smoke-main-final` → No such file or directory.

## Tag-readiness verdict

**ready-to-tag**. Workspace builds + packages cleanly at HEAD `0847f90`; all
publishable crates (ferriskey, ff-core, ff-script, ff-backend-valkey,
ff-observability, ff-engine, ff-sdk, ff-scheduler, ff-server) verify-compile
through `cargo package`; external consumer against the public `ff-core` +
`ff-sdk` surface builds under both feature configurations and the
`BackendError::Valkey { kind, .. }` match pattern advertised in the v0.4.0
migration doc works unchanged.

Advisory (non-blocker): `ff-readiness-tests`'s path-dep on `ff-test` lacks a
`version = ...`. Harmless for publishing since both are `publish = false`,
but it makes workspace-wide `cargo package` require an explicit exclude. If
desired, add `version = "0.3.4"` alongside the path to let plain
`cargo package --workspace` succeed. Not a tag blocker.
