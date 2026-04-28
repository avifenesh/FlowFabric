# incident-remediation — SC-10 headline example (RFC-024)

FlowFabric v0.12 headline example for the **lease-reclaim consumer
surface** introduced by [RFC-024]. Dramatises **SC-10** from the
pre-RFC use-case catalogue: a responder claims an incident, dies
mid-flow (pager-death), and a second responder picks up via an
explicit `ReclaimGrant` handoff.

[RFC-024]: ../../docs/CONSUMER_MIGRATION_0.12.md#7-rfc-024--lease-reclaim-consumer-surface

## Scenario — why this matters

Operators page on-call engineers. The engineer claims the incident
execution, starts the multi-step remediation
(`diagnose → apply_fix → verify`), and then their laptop battery
dies / they walk into a tunnel / the pager app crashes. Under a
naive queue the job's lease eventually expires, and either the
queue silently re-delivers (which can double-apply a remediation)
or the execution hangs until manual intervention.

RFC-024 gives operators a **third option**: an explicit reclaim
grant. A supervisor (human or automation) issues a
`ReclaimGrant` via the admin surface; a specific second responder
consumes it via `claim_from_reclaim_grant` and mints a fresh attempt.
This is a **handoff**, not a preemption — the grant names the
intended next worker, and `max_reclaim_count` bounds how many times
a given execution can churn before the system escalates to a human.

## Architectural mapping

| Scenario step | RFC-024 primitive | SDK surface |
|---|---|---|
| Responder A claims incident | `ff_claim` FCALL (standard) | `EngineBackend::claim` |
| Pager-death simulated | `lease_expired_reclaimable` set by scanner (simulated here via column update) | — |
| Supervisor issues reclaim grant | `issue_reclaim_grant` | `EngineBackend::issue_reclaim_grant` ¹ |
| Responder B consumes grant | `ff_reclaim_execution` FCALL | `FlowFabricWorker::claim_from_reclaim_grant` |
| Responder B finishes remediation | standard `complete` | `EngineBackend::complete` |
| Nth reclaim past cap | `ReclaimExecutionOutcome::ReclaimCapExceeded` | same |

¹ **Why trait-direct not SDK-direct for `issue_reclaim_grant`?** The
public SDK equivalent is `FlowFabricAdminClient::issue_reclaim_grant`,
which talks **HTTP to `ff-server`**. The SQLite FF_DEV_MODE path in
this example runs zero external processes, so the admin-HTTP shape
does not apply. Production cairn-fabric consumers use the HTTP admin
client per CONSUMER_MIGRATION_0.12.md §7 "AFTER (v0.12)" snippet;
this example exercises the same underlying primitive via the backend
trait.

## Run

### SQLite — zero-infra (recommended)

```sh
FF_DEV_MODE=1 cargo run -p incident-remediation -- --backend sqlite
```

The whole scenario runs in-process against an ephemeral `:memory:`
SQLite. No Docker, no network, no ambient services. Finishes in under
a second.

### Valkey

```sh
valkey-cli FUNCTION DELETE flowfabric  # ensure fresh Lua library
cargo run -p incident-remediation -- --backend valkey
```

The Valkey path **verifies the SDK connects to a live Valkey** but
does NOT run the full scenario end-to-end — the demo transitions
(`lease_expired_reclaimable` injection, `promote_to_runnable`) require
either SQL column surgery per-backend or a live scheduler + scanner
supervisor. For the headline demo, run SQLite; for a production
reclaim flow exercise, follow the migration pattern in
[CONSUMER_MIGRATION_0.12.md §7][RFC-024] against your own deployment.

### Postgres

```sh
FF_POSTGRES_URL=postgres://user:pass@host:5432/db \
    cargo run -p incident-remediation -- --backend postgres
```

Same caveat as Valkey — connects + stops, doesn't drive the full
loop without a scheduler.

## Expected output

SQLite run produces log lines tagged by role:

```
[engine]      SC-10 incident-remediation starting
[engine]      SQLite dev backend ready
[supervisor]  incident filed — execution runnable
[responder-a] pager fired — claimed incident
[responder-a] step 1/3: diagnose — db-primary disk check
[responder-a] step 2/3: apply_fix — rotating log volumes …
[responder-a] (pager-death — process vanishes mid-flow)
[supervisor]  issued reclaim grant for Responder B
[responder-b] claimed via reclaim grant (HandleKind::Reclaimed)
[responder-b] step 3/3: verify — alert resolved, disk at 45%
[responder-b] complete — incident closed
[supervisor]  second incident filed — running past max_reclaim_count=2
[engine]      simulating lease expiry + reclaim round (round=0)
[responder-b] reclaimed (pager-death simulated again) (round=0)
[engine]      simulating lease expiry + reclaim round (round=1)
[responder-b] reclaimed (pager-death simulated again) (round=1)
[engine]      simulating lease expiry + reclaim round (round=2)
[supervisor]  ReclaimCapExceeded — paging a human, execution moved to terminal_failed
[engine]      SC-10 scenario complete — reclaim happy-path + cap-exceeded both exercised
```

## `max_reclaim_count` tuning

The example pins `DEMO_MAX_RECLAIM_COUNT = 2` so the cap-exceeded
branch fires in three rounds. Production default is **1000** per
[RFC-024 §4.6][RFC-024-cap]; realistic values sit in the 5–50 range
depending on how much churn the operator is willing to tolerate
before an execution is declared permanently stuck. `None` on
`ReclaimExecutionArgs::max_reclaim_count` falls through to the
Rust-surface default of 1000; the Lua pre-RFC fallback of 100 still
applies to call sites that don't pass ARGV[9].

[RFC-024-cap]: ../../docs/CONSUMER_MIGRATION_0.12.md#7-rfc-024--lease-reclaim-consumer-surface

## Production migration reference

For the consumer-facing recipe (HTTP admin client,
capability-aware grant issuance, handle-kind observability), see
**[`docs/CONSUMER_MIGRATION_0.12.md` §7][RFC-024]**. This example
inverts the exposition: instead of the retry-loop → one-shot
migration diff the doc carries, it walks the full two-worker
handoff so you can see where `HandleKind::Reclaimed` arrives on the
successor worker.

## Non-goals

- **Not a scheduler demo.** The `force_lease_expired` helper
  performs the column update that the Valkey/PG scheduler would
  emit via the scanner supervisor. For a scanner-driven demo use
  `examples/v011-wave9-postgres`.
- **Not a multi-tenant demo.** Single namespace, single lane,
  single flow; scales of 1 across every dimension. SC-9/SC-11 in
  the pre-RFC catalogue cover fanout + multi-tenant.
- **Not a capability-routing demo.** The reclaim path supports
  capability matching (RFC-024 §3.2 B-2); this example uses the
  empty capability set because the SC-10 scenario's pager-death
  simulation is orthogonal to routing.
