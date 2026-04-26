# v010-read-side-ergonomics

Headline consumer demo for the v0.10.0 read-side API surface.
Exercises the three read-side changes called out in the v0.10
changelog under a single process:

| Feature | Issue / PR | Shown in this example |
|---------|------------|-----------------------|
| Flat `Supports` capability struct (replaces `CapabilityMatrix` / `Capability` / `CapabilityStatus`) | #277 / PR #320 | `backend.capabilities()` → `caps.supports.subscribe_lease_history` dot-access; pre-dispatch `if !caps.supports.<flag>` guard |
| Typed stream event enums (`LeaseHistoryEvent`, `CompletionEvent`, `SignalDeliveryEvent`) | #282 / PR #321 | `match` on `LeaseHistoryEvent::{Acquired, Renewed, Expired, Reclaimed, Revoked}` — no `StreamEvent.payload` byte parsing |
| `ScannerFilter` on `subscribe_lease_history` / `subscribe_completion` / `subscribe_signal_delivery` | #282 / PR #325 | One subscriber wired with `&ScannerFilter::default()` (unfiltered), one with `ScannerFilter::new().with_instance_tag("tenant", "acme")` — tenant isolation is verified at runtime |

## Scenario

Multi-tenant lease-audit console. Two tenants (`acme`, `contoso`) share
one FlowFabric deployment. An operator dashboard opens:

* **Global panel** — subscribes unfiltered and sees every tenant's
  lease transitions.
* **Tenant-scoped panel** — subscribes with `ScannerFilter` pinned to
  `tenant=acme` and only sees the acme-tagged executions.

The example submits one execution per tenant (tagged with
`instance_tag` `tenant=<name>`), runs them to success via an in-process
worker, and at shutdown diffs the two streams:

* Global subscriber must observe both executions.
* Tenant-scoped subscriber must observe acme only. The example exits
  non-zero (`Err` out of `main`) if the tenant-scoped panel leaks a
  contoso event.

## Prereqs

* `valkey-server` reachable at `FF_HOST:FF_PORT` (default
  `localhost:6379`).
* `ff-server` reachable at `FF_SERVER_URL` (default
  `http://localhost:9090`).

Launch `ff-server` the usual way (see repo `README.md` Quick start).

## Run

```bash
cd examples/v010-read-side-ergonomics
cargo run
```

Expected log milestones (logging at `info`):

```
INFO backend.capabilities() — identity family=valkey ...
INFO backend.capabilities() — supports (flat struct, no enum + no map lookup) subscribe_lease_history=true subscribe_completion=true ...
INFO subscriber started label=all filtered=false
INFO subscriber started label=acme-only filtered=true
INFO submitted two executions with tenant tags ...
INFO worker loop started
INFO claimed task ...
INFO completed successfully ...
INFO subscriber yielded typed event label=all variant=Acquired ...
INFO subscriber yielded typed event label=acme-only variant=Acquired ...
INFO terminal execution_id=... state=completed
INFO subscriber drain complete all=... acme=...
INFO demo complete — capabilities() read, typed LeaseHistoryEvent match, ScannerFilter isolation verified
```

## What this example is not

* Does not exercise `subscribe_completion` or
  `subscribe_signal_delivery` individually — their API shape is
  identical to `subscribe_lease_history` (same filter param, same
  flat-bool capability gate, same typed enum), so one well-narrated
  family is enough for the headline doc.
* Does not use the Postgres backend. The shape on Postgres is
  identical (`subscribe_lease_history(cursor, filter)` returns a
  typed `LeaseHistorySubscription` either way); see
  `docs/CONSUMER_MIGRATION_0.10.md` for the Postgres backing.

## Runtime note on event visibility (Valkey Stage A)

On the Valkey backend, `subscribe_lease_history` tails the
partition-aggregate stream `ff:part:{fp:0}:lease_history`
(RFC-019 Stage A). The per-execution lease events this deployment
emits during the demo may not land on that aggregate key until the
partition-level XADD producer is wired (tracked separately as the
Stage-A → Stage-B producer fan-in work). **The point of this
example is not the event volume — it is the compiler-level demo of
the three v0.10 surface changes.** Subscribers connect, drain for
the demo budget, and exit cleanly; the test assertion the example
still enforces is "if the acme-only subscriber observes any event,
none of them carry the contoso execution id." Observing zero events
is a deployment observation, not a consumer-surface bug.

On Postgres the outbox table + `NOTIFY` trigger (migration 0006/7)
populate events directly and the same example reads them back
without change.
