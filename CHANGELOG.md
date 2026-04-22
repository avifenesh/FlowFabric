# Changelog

All notable changes to FlowFabric are documented here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.3.1] - 2026-04-22

### Changed

- Release workflow now publishes `ff-observability` (8 crates total,
  up from 7). `ff-engine` depends on `ff-observability` as a
  workspace dep; omitting it from the publish set caused the v0.3.0
  release to partial-publish.

### Fixed

- `ff_expire_suspension` now emits on `ff:dag:completions` for the
  terminal branches (fail / cancel / expire) when the execution is
  flow-bound. Previously, flow-bound children unblocked only via the
  15 s `dependency_reconciler` safety net.
- `ff_resolve_dependency` child-skip now emits on
  `ff:dag:completions` with `outcome="skipped"` when the skipped
  child is flow-bound. Same latency fix as above for the
  child-skip's own downstream edges.

### Notes

- v0.3.0 was yanked after partial-publishing (`ferriskey`, `ff-core`,
  `ff-script` only). Operators should re-resolve against 0.3.1.
