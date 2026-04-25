# umbrella-quickstart

The canonical v0.9 FlowFabric bring-up example. One execution, one
claim, one complete — demonstrating every v0.9 consumer-facing
addition in under 260 lines of a single binary.

If you're asking "how do I integrate FlowFabric?", start here. The
other examples in `examples/` (`retry-and-cancel`, `deploy-approval`,
`coding-agent`, `llm-race`, `media-pipeline`) build richer scenarios
on top of this shape.

## What this demonstrates

| v0.9 addition                             | Issue  | Exercised by                                                     |
|-------------------------------------------|--------|------------------------------------------------------------------|
| `flowfabric` umbrella crate               | #279   | `Cargo.toml` — the ONLY FlowFabric pin — plus `flowfabric::…` imports everywhere |
| `EngineBackend::prepare()` boot prep      | #281   | `backend.prepare()` at boot — `Applied { description }` on Valkey |
| `seed_waitpoint_hmac_secret` trait method | #280   | `backend.seed_waitpoint_hmac_secret(..)` — idempotent, returns `AlreadySeeded { same_secret: true }` against an ff-server-seeded partition |
| `ClaimGrant` at `flowfabric::sdk`         | #283   | `ClaimGrant` / `ClaimForWorkerArgs` / `ClaimForWorkerOutcome` named through the umbrella — no `ff-scheduler` dep |
| `LeaseSummary` new fields                 | #278   | `snapshot.current_lease.{lease_id, attempt_index, last_heartbeat_at}` all read post-claim, pre-complete |

## Prereqs

* `valkey-server` reachable at `FF_HOST:FF_PORT` (defaults `localhost:6379`).
* `ff-server` reachable at `FF_SERVER_URL` (default `http://localhost:9090`), started with
  `FF_WAITPOINT_HMAC_SECRET` set. Matching `FF_WAITPOINT_HMAC_SECRET` /
  `FF_WAITPOINT_HMAC_KID` at the example side (defaults `k1` + the
  v0.7 reference secret) yields `AlreadySeeded { same_secret: true }`.

## Run

```sh
cargo run -p umbrella-quickstart-example
```

## Expected transcript

Against a freshly-booted Valkey + ff-server:

```text
INFO umbrella_quickstart: backend connected backend_label="valkey"
INFO umbrella_quickstart: prepare: Applied description=FUNCTION LOAD (flowfabric lib v26)
INFO umbrella_quickstart: seed: AlreadySeeded kid=k1 same_secret=true
INFO umbrella_quickstart: submitted execution execution_id={fp:86}:77531a91-aa0f-4c42-889b-3627f2644944
INFO umbrella_quickstart: claim_for_worker: Unavailable on consumer-built ValkeyBackend (v0.9 surface gap) — falling back to claim_via_server
INFO umbrella_quickstart: claim acquired execution_id={fp:86}:77531a91-aa0f-4c42-889b-3627f2644944 attempt_index=0
INFO umbrella_quickstart: LeaseSummary (v0.9 fields: lease_id / attempt_index / last_heartbeat_at) lease_id=60ed12ca-… attempt_index=0 last_heartbeat_at=Some(TimestampMs(1777138140062)) lease_epoch=1
INFO umbrella_quickstart: task completed
INFO umbrella_quickstart: final execution state public_state=Completed total_attempt_count=1
INFO umbrella_quickstart: umbrella-quickstart complete
```

## v0.9 surface gap surfaced by this example

`EngineBackend::claim_for_worker` is in the public trait (RFC-017 Stage C)
but the in-tree `ValkeyBackend` only wires a scheduler when constructed
through the server-internal builder. Every public entry point —
`ValkeyBackend::connect(..)`, `from_client_and_partitions(..)`,
`from_client_partitions_and_connection(..)` — sets `scheduler: None`, so
`backend.claim_for_worker(..)` on a consumer-built `Arc<dyn EngineBackend>`
returns

```text
EngineError::Unavailable { op: "claim_for_worker (scheduler not wired on this ValkeyBackend)" }
```

The production claim path is `FlowFabricWorker::claim_via_server`, which
POSTs the server's scheduler route and threads the returned `ClaimGrant`
into `claim_from_grant`. This example exercises the trait call once to
demonstrate the shape, catches `Unavailable`, and falls back to
`claim_via_server`.

**This is a real v0.9 gap between the trait's advertised surface and
what a consumer can actually invoke.** Tracked as a follow-up issue; do
not treat the `claim_for_worker` fall-through as example boilerplate
past v0.9.

## Upgrading to the published umbrella crate

Today `Cargo.toml` pins the in-tree path:

```toml
flowfabric = { path = "../../crates/flowfabric", default-features = false, features = ["valkey"] }
```

After `flowfabric 0.9.0` ships to crates.io, flip this to:

```toml
flowfabric = { version = "0.9", default-features = false, features = ["valkey"] }
```

Nothing else in this crate changes — the umbrella's re-export map is
stable across feature flags.

## Switching to the Postgres backend

Change exactly the feature flag:

```toml
flowfabric = { version = "0.9", default-features = false, features = ["postgres"] }
```

and replace `flowfabric::valkey::ValkeyBackend` with
`flowfabric::postgres::PostgresBackend`. `prepare()` returns `NoOp` on
Postgres (migrations run out-of-band); the rest of the flow is
identical.
