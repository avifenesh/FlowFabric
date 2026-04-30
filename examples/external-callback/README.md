# `external-callback` — SC-09 scenario (v0.13 waitpoint surface)

End-to-end demo of the FlowFabric v0.13 waitpoint surface: a workflow
suspends against an HMAC-signed pending waitpoint, an asynchronous
external actor POSTs the token back, a signal-bridge authenticates
the token and dispatches a signal, and the workflow resumes.

This is the **SC-09** scenario from `pre-rfc-use-cases-and-primitives`
— the asynchronous-external-actor contract that cairn's signal-bridge
architecture targets. It is distinct from:

- `deploy-approval` — synchronous operator gate via HTTP `ff-server`.
- `incident-remediation` (SC-10) — lease-reclaim handoff, no external
  actor.

## Scenario

A workflow needs to wait for an async external actor (webhook,
email reply, manual approval, third-party API callback). The flow
cannot know in advance when (or whether) the actor will respond.

```
            ┌─────────────┐       ┌────────────────┐      ┌─────────────┐
            │  workflow   │       │ signal-bridge  │      │  external   │
            │   (SDK)     │       │  (verify+fwd)  │      │   actor     │
            └──────┬──────┘       └────────┬───────┘      └──────┬──────┘
                   │ 1. create_waitpoint   │                     │
                   │───────────┐           │                     │
                   │  (token)  │           │                     │
                   │<──────────┘           │                     │
                   │ 2. log/email token ───────────────────────→ │
                   │ 3. suspend(use_pending, Wildcard)           │
                   │    (lease released)                         │
                   │                                             │
                   │                       │ 4. POST /callback   │
                   │                       │←──── token + body ──│
                   │                       │                     │
                   │           5. read_waitpoint_token (#434)    │
                   │           ←───────── partition,wp_id ───────│
                   │           6. constant-time compare          │
                   │           7. deliver_signal (HMAC re-check) │
                   │                       │                     │
                   │ 8. resume condition satisfied               │
                   │ 9. claim_resumed_execution                  │
                   │ 10. finalize + complete                     │
```

Three workflow steps: `request_approval` → `wait_for_external` →
`finalize`.

1. `request_approval` — claim the execution, `create_waitpoint` with a
   5-minute TTL, log the HMAC token (simulated email / webhook
   registration), then `suspend` bound to the pending waitpoint.
2. `wait_for_external` — engine-side, no worker activity. The
   execution's lease is released; the worker goes back to polling.
3. On signal satisfaction: `claim_resumed_execution` mints a
   resumed-kind handle; `finalize` runs `complete`.

## v0.13 surface exercised

| Surface                                       | Cairn issue |
|-----------------------------------------------|-------------|
| `EngineBackend::create_waitpoint`             | #435        |
| `EngineBackend::read_waitpoint_token`         | #434        |
| `FlowFabricAdminClient::connect_with` (facade)| #432        |
| `WorkerConfig.backend: Option<BackendConfig>` | #448        |
| `FlowFabricWorker::deliver_signal`            | v0.12 PR-3  |

See [`docs/CONSUMER_MIGRATION_0.13.md`](../../docs/CONSUMER_MIGRATION_0.13.md)
for the production migration reference — the ergonomic upgrade path
this example demonstrates on a zero-infra SQLite backend.

## HMAC token lifecycle

1. `create_waitpoint(handle, wp_key, ttl)` mints a `PendingWaitpoint`
   with a `WaitpointHmac { kid, token }`. The digest is HMAC-SHA256
   over the waitpoint identity, keyed by the secret previously seeded
   via `seed_waitpoint_hmac_secret`.
2. The issued token is the only piece the external actor ever sees —
   the workflow logs it (simulating an email / webhook registration).
3. When the actor POSTs the token back, the signal-bridge uses
   `read_waitpoint_token` (#434) to fetch the stored digest off the
   waitpoint row, compares constant-time, and on match forwards to
   `deliver_signal`. The engine re-verifies HMAC server-side on
   delivery as defense-in-depth; the bridge's check is early rejection
   for malformed / adversarial traffic.
4. The example deliberately runs the tamper branch first (flip one
   hex char, preserve the `kid:` prefix) to show the rejection path
   before the happy path.

## Run

### SQLite (zero-infra, end-to-end)

```bash
FF_DEV_MODE=1 cargo run -p external-callback -- --backend sqlite
```

Expected output ordering (abridged):

```
[engine]         SC-09 external-callback starting
[engine]         SQLite dev backend + admin facade ready
[engine]         approval execution filed
[workflow]       step 1/3: request_approval — claimed execution
[workflow]       minted pending waitpoint — token dispatched …
[workflow]       step 2/3: wait_for_external — suspended, lease released
[external-actor] approver thinking…
[external-actor] (adversary attempt — posting with tampered token)
[signal-bridge]  REJECT — token mismatch (HMAC verify failed)
[external-actor] POST /callback with valid token
[signal-bridge]  ACCEPT — token verified, deliver_signal dispatched …
[workflow]       step 3/3: finalize — re-claimed resumed execution
[workflow]       complete — approval workflow finished
[engine]         SC-09 scenario complete
```

### Valkey / Postgres

```bash
# Terminal 1: ff-server against Valkey
cargo run -p ff-server --features valkey

# Terminal 2: the example
cargo run -p external-callback -- --backend valkey
```

The Valkey + Postgres paths require a live `ff-server` and scheduler
(same posture as `incident-remediation`): `create_waitpoint` +
`read_waitpoint_token` on these backends compile and pass integration
tests in-tree, but the end-to-end claim → suspend → resume loop
requires the scheduler/scanner supervisor to drive runnable-state
transitions. The zero-infra SQLite envelope inlines the equivalent
column-level promotion for demo purposes.

Both Valkey + Postgres dispatches will currently print the setup
requirement and exit non-zero. Follow-up work: wire through
`FlowFabricAdminClient::new("http://ff-server:9090")` for a full
cross-backend demo once the SC-09 use case has an `ff-server` HTTP
route for `create_waitpoint` + signal forwarding.

## Why `read_waitpoint_token` is trait-direct, not admin-SDK-direct

v0.13 exposes `read_waitpoint_token` on the `EngineBackend` trait
(#434) rather than on `FlowFabricAdminClient`. The signal-bridge is an
infrastructure component sitting in-process with the engine, not a
remote admin client — it reads the token from the same partition it
will subsequently dispatch a signal against, with no HTTP hop. The
example calls `trait_obj.read_waitpoint_token(...)` directly to
mirror that deployment shape.

If an out-of-process bridge is needed later, the facade can be widened
with a `.read_waitpoint_token` method that routes through HTTP or
trait-dispatch per transport (same pattern as `issue_reclaim_grant`
today).

## What the example deliberately does not do

- **No real HTTP server.** The "external actor" and "signal-bridge"
  are in-process functions. Spinning up a full `ff-server` + reqwest
  client would double the LOC without exercising any v0.13-new
  surface beyond what the in-process path already shows.
- **No retry / idempotency.** The `Signal.idempotency_key` is `None`;
  real consumers should populate it so duplicate webhook POSTs are
  deduped. The `Signal` struct carries it; `deliver_signal` threads it
  through to `DeliverSignalResult::Duplicate`.
- **No timeout-path demo.** `SuspendArgs::timeout_at` is `None`; the
  5-minute waitpoint TTL is there as realism but the demo fires the
  external actor within seconds. `TimeoutBehavior::Fail` +
  `PendingWaitpointExpired` are a natural follow-up scenario.

## Size

~400 LoC of Rust + README, matching the `incident-remediation`
(SC-10) headline example. No external services required for the
SQLite run.
