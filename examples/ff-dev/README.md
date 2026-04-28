# ff-dev — FlowFabric dev backend in 60 seconds

Headline example for the RFC-023 SQLite dev-only backend (v0.12.0).

**Zero Docker, zero ambient services, zero setup.** Runs FlowFabric
against an in-process `:memory:` SQLite database so contributors and
consumers can try out the engine on a fresh clone without spinning up
Valkey or Postgres first.

## Run it

```bash
FF_DEV_MODE=1 cargo run --bin ff-dev
```

You should see a WARN banner (the production guard — see below), then
an end-to-end demo:

- Construct `SqliteBackend` against `:memory:`
- `create_flow` + three `create_execution` calls
- `claim` → `complete` one execution through the attempt lifecycle
- `change_priority` + `cancel_execution` on the others
- `read_execution_info` audit
- `subscribe_completion` cursor-resume surface (RFC-019)
- Clean `shutdown_prepare`

Total: ~200 lines, ~20 seconds wall-clock on a cold build.

## Why SQLite (not Valkey or Postgres)?

The `ff-dev` example exists for the "try FlowFabric in 60 seconds"
story. Valkey and Postgres both require a running service before the
example can do anything useful; SQLite runs in-process with `:memory:`
storage, so `cargo run` is the whole setup.

**SQLite is dev-only, permanently.** It is not a production option —
single-writer, single-process, no cross-process pub/sub. Production
deployments pick Valkey (the engine's native backend) or Postgres (the
enterprise persistence tier). See `rfcs/RFC-023-sqlite-dev-only-backend.md`
§1, §5, and `docs/dev-harness.md` for the full positioning statement
and production-guard rationale.

## The `FF_DEV_MODE=1` gate

`SqliteBackend::new` refuses to construct without `FF_DEV_MODE=1` in
the process environment (RFC-023 §3.3 + §4.5). The gate lives on the
type, so every path that yields a `SqliteBackend` handle pays it —
embedded consumers, ff-server's `start_sqlite_branch`, and
`FlowFabricWorker::connect_with`.

The example surfaces the gate explicitly: running `cargo run --bin
ff-dev` without `FF_DEV_MODE=1` exits with a pointer to this README
and the RFC. Do not plumb `FF_DEV_MODE=1` into production env; the
WARN banner on construction exists so log aggregators can alert if a
SQLite backend ever reaches an environment it shouldn't.

## Shape — backend trait directly, no scheduler

This example follows the RFC-023 §4.7.1 cairn-canonical shape: drive
the `EngineBackend` trait surface directly. No ff-server, no ff-sdk
worker loop, no ferriskey in the dependency graph. The Cargo.toml
pulls only `ff-backend-sqlite` + `ff-core`.

The full-parity story (`FlowFabricWorker::connect_with(config,
Arc<SqliteBackend>, None)`) is orthogonal; this example stays at the
trait level because that's the minimal surface a consumer needs to
reason about SQLite's lifecycle. The `connect_with` path layers the
SDK's worker-loop helpers on top — useful when a consumer wants a
single SDK API across backends, not needed for the "60 seconds"
story.

## Under the hood — what the promote helper does

RFC-023 §4.7.1 calls out that `create_execution` seeds the row in the
`pending` / `initial` shape, which the claim path does not route. In
production backends the scheduler + scanner supervisor flip this to
`runnable` automatically; under the SQLite dev envelope there is no
scanner spawning happening for this example so the `promote_to_runnable`
helper issues the SQL flip directly. Real consumers using
`SqliteBackend::with_scanners` or driving the full ff-server boot
path do not need this helper.

## See also

- `rfcs/RFC-023-sqlite-dev-only-backend.md` — full design
- `docs/dev-harness.md` — operator-side dev→prod gotchas
- `examples/v011-wave9-postgres/` — the Postgres equivalent of this
  demo at v0.11 (shows Wave-9 ops against a service-container PG)
- `scripts/smoke-sqlite.sh` — the CI smoke wrapper for this example
