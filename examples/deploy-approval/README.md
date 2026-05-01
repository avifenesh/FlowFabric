# deploy-approval — CI/CD pipeline with AllOf, Count{2-of-N}, DurableSummary, and cancel_flow

End-to-end FlowFabric example: a full deploy pipeline that fans out
three test kinds in parallel, gates a canary rollout on two distinct
human approvals, streams build logs as merge-patch frames, and
cascades a `cancel_flow` on verify failure.

**Proved live.** 2026-04-24 against Valkey 6379; full report:
[0.7.0-deploy-approval-example-run.md](../../rfcs/drafts/0.7.0-deploy-approval-example-run.md).

---

## What you'll learn

| RFC | Primitive | File | ~lines |
|---|---|---|---|
| 007 | Flow DAG with `AllOf` edge group (default) | `submit.rs` | 200 |
| 009 | Capability routing (`kind=unit|integration|e2e`) | `test_worker.rs` | 130 |
| 013 | Typed `SuspendArgs` + `suspend()` | `deploy.rs` | 180 |
| 014 | `Composite(Count { n: 2, DistinctSources })` | `deploy.rs` + `approve.rs` | 180 + 100 |
| 015 | `DurableSummary { JsonMergePatch }` stream | `build_worker.rs` | 150 |
| 016 | `cancel_flow { cancel_all }` cascade | `verify.rs` | 150 |

Each binary is self-contained. Read any one to learn the primitive it
exercises; diff against your Celery/apalis workflow for the delta.

---

## Primitives you'll learn

### 1. Flow DAG with `AllOf` edge group (RFC-007)

**The problem.** A deploy must wait for ALL tests to pass (unit +
integration + e2e). In Celery you'd build a chord. In FlowFabric the
default `EdgeDependencyPolicy::AllOf` does it — you just stage the
edges:

```rust
// submit.rs — AllOf is the default; no explicit policy call needed.
for (up, down) in [(build, unit), (build, integ), (build, e2e),
                   (unit, deploy), (integ, deploy), (e2e, deploy),
                   (deploy, verify)] {
    graph_rev = stage_and_apply(&http, &args, &flow_id,
                                up, down, graph_rev).await?;
}
```

The edge reducer (RFC-007 Stage A) maintains `counters.success`
per-downstream; when it hits the inbound fan-in count, the downstream
flips to `eligible_now`. Alternative policies live on the same
surface — `any_of`, `quorum(k, ...)` — set via
`sdk.set_edge_group_policy(...)`.

### 2. Capability-routed tests (RFC-009)

**The problem.** All three tests live on the same lane but run on
different workers. In apalis you'd build three workers each
subscribed to a different queue. In FlowFabric each worker
advertises capabilities and the scheduler matches:

```rust
// test_worker.rs
let capability = match args.kind.as_str() {
    "unit" => CAP_TEST_UNIT,           // "kind=unit"
    "integration" => CAP_TEST_INTEGRATION,  // "kind=integration"
    "e2e" => CAP_TEST_E2E,             // "kind=e2e"
    other => return Err(...),
};
let config = WorkerConfig { capabilities: vec![capability.to_owned()], .. };
```

Submit sets the execution's `required_capabilities` in the policy:

```rust
// submit.rs
"policy": {
    "routing_requirements": { "required_capabilities": [capability] }
}
```

The scheduler (`ff-engine`'s claim loop) only hands a task to a
worker whose advertised caps are a superset of the exec's required
caps. Note: workers must enable the `direct-valkey-claim` ff-sdk
feature (see `Cargo.toml`) — otherwise their caps aren't advertised
to the scheduler's connected-worker index.

### 3. Typed `SuspendArgs` + `suspend()` (RFC-013)

**The problem.** After canary, pause for human review without
polling. In a plain actor system you'd spin or push state into a
side channel. In FlowFabric the worker releases its lease atomically:

```rust
// deploy.rs
let cond = ResumeCondition::Composite(CompositeBody::Count {
    n: 2,
    count_kind: CountKind::DistinctSources,
    matcher: Some(SignalMatcher::ByName("deploy_approval".into())),
    waitpoints: vec![payload.approval_waitpoint_key.clone()],
});
let handle = task.suspend(
    SuspensionReasonCode::WaitingForOperatorReview,
    cond,
    Some((deadline, TimeoutBehavior::Fail)),
    ResumePolicy::normal(),
).await?;
// Lease released. `handle.details.waitpoint_token` is the HMAC the
// approvers need.
```

`suspend()` (strict) vs `try_suspend()` (returns
`SuspendOutcome::{Suspended, AlreadySatisfied}`) follows the
`foo`/`try_foo` Rust convention.

### 4. Multi-signal resume with `Count{2, DistinctSources}` (RFC-014)

**The problem.** "Two reviewers approve." A plain counter trips on
duplicate signals from the same reviewer. `DistinctSources` dedupes
by `source_identity`:

```rust
// approve.rs
let body = serde_json::json!({
    "signal_name": "deploy_approval",
    "source_identity": args.reviewer,  // "alice" / "bob"
    "waitpoint_token": wp.waitpoint_token,
    // ...
});
http.post(&sig_url).json(&body).send().await?;
```

First invocation → `{"effect":"appended_to_waitpoint"}`.
Second distinct-reviewer invocation →
`{"effect":"resume_condition_satisfied"}`.
Second same-reviewer invocation → no-op on the counter.

Other `CountKind` variants:
- `DistinctWaitpoints` — count different waitpoint keys (useful for
  multi-gate flows).
- `DistinctSignals` — raw dedup by `signal_id`.

### 5. `DurableSummary` stream with JSON Merge Patch (RFC-015)

**The problem.** Build logs are incremental; you want live tailing
AND a durable record of the final log state AND bounded stream
memory. The `DurableSummary` mode does all three:

```rust
// build_worker.rs
let mode = StreamMode::DurableSummary {
    patch_kind: PatchKind::JsonMergePatch,
};
for (label, pct) in steps {
    let patch = serde_json::json!({
        "step": label,
        "percent": pct,
        "artifact": payload.artifact,
    });
    task.append_frame_with_mode(
        "summary_delta",
        &serde_json::to_vec(&patch)?,
        None,
        mode,
    ).await?;
}
```

Each `append_frame_with_mode` emits an XADD frame (tailers see
deltas) AND merges the patch server-side into the rolling summary
hash. Consumers read the accumulated document via
`GET /v1/executions/{id}/summary`.

### 6. `cancel_flow { cancel_all }` cascade (RFC-016)

**The problem.** Verify detects a bad deploy; cancel everything
still running in the flow (e.g. a long-tailing integration re-run).
In a queue you'd broadcast. In FlowFabric one REST call walks the
flow's member list and cancels each:

```rust
// verify.rs
let cancel_body = serde_json::json!({
    "flow_id": payload.flow_id,
    "reason": "verify_failed",
    "cancellation_policy": "cancel_all",
    "now": ff_core::types::TimestampMs::now().0,
});
http.post(&format!("{server}/v1/flows/{fid}/cancel?wait=true"))
    .json(&cancel_body).send().await?;
```

`wait=true` makes the handler return only after every member has
reached `cancelled`. The `retry-and-cancel` example has independent
coverage of the same path (scene 2).

---

## Architecture

```
submit CLI
  │  1. POST /v1/flows
  │  2. POST /v1/executions × 6   (build, unit, integ, e2e, deploy, verify)
  │  3. POST /v1/flows/{id}/members × 6
  │  4. describe_flow → seed graph_revision
  │  5. POST /v1/flows/{id}/edges + /edges/apply × 7 edges
  │
  ▼
build worker       (lane=build, caps: role=build)
  │ DurableSummary{JsonMergePatch} frames × 4 + final
  │
  ├──► unit worker        (lane=test, caps: kind=unit)
  ├──► integration worker (lane=test, caps: kind=integration)
  └──► e2e worker         (lane=test, caps: kind=e2e)
            │  AllOf: all three must complete
            ▼
        deploy worker  (lane=deploy, caps: role=deploy)
          │ canary → suspend(Count{2, DistinctSources})
          │   ◄──── approve CLI × 2 (distinct reviewers)
          │ resume → full rollout
          ▼
        verify worker  (lane=verify, caps: role=verify)
          │ healthy → complete
          │ unhealthy → POST /v1/flows/{id}/cancel?wait=true
          ▼
        terminal: completed | cancelled
```

---

## Prereqs

1. **Valkey ≥ 7.2** on `localhost:6379`:
   ```bash
   docker run -d --name valkey -p 6379:6379 valkey/valkey:8-alpine
   ```

2. **Rust stable** + `cargo`.

3. **ff-server** built in this workspace (from repo root):
   ```bash
   cargo build -p ff-server --release
   ```

No external API keys are required — the pipeline is entirely
deterministic (simulated build/test/deploy durations).

## Run it

Eight terminals. All commands from `examples/deploy-approval/`
unless stated.

### Terminal 1 — ff-server

```bash
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
FF_LANES=default,build,test,deploy,verify \
  cargo run -p ff-server --release
```

Health check: `curl http://localhost:9090/healthz` returns
`{"status":"ok"}`.

### Terminals 2-7 — workers

```bash
cargo run --release --bin build-worker
cargo run --release --bin test-worker -- --kind unit
cargo run --release --bin test-worker -- --kind integration
cargo run --release --bin test-worker -- --kind e2e
cargo run --release --bin deploy
cargo run --release --bin verify
```

### Terminal 8 — submit

```bash
cargo run --release --bin submit -- \
  --artifact "app:v1.2.3" --commit abc123
```

Submit prints each primitive interaction as `[timeline] …` lines.
Once deploy suspends, submit will stay polling `[verify state]`
until the flow terminates. Copy the `deploy_created execution_id=…`
line — approvers need it.

### Terminals 9-10 — approvals (after deploy suspends)

When the deploy worker suspends it prints a line like:

```
[timeline] deploy_suspended execution_id=<eid> waitpoint_key=approval waitpoint_id=<wp-id>
```

Copy `waitpoint_id` from that line. The raw HMAC `waitpoint_token` is
fetched automatically via the ff-sdk admin client
(`FlowFabricAdminClient::read_waitpoint_token`) — operators no longer
paste it from the worker log. Set `FF_API_TOKEN` when the server runs
with bearer auth.

```bash
cargo run --release --bin approve -- \
  --flow-id <flow-id-from-submit> \
  --execution-id <deploy-execution-id> \
  --waitpoint-id <wp-id-from-deploy> \
  --reviewer alice

cargo run --release --bin approve -- \
  --flow-id <flow-id-from-submit> \
  --execution-id <deploy-execution-id> \
  --waitpoint-id <wp-id-from-deploy> \
  --reviewer bob
```

Expected signal effects:
- 1st (alice)  → `appended_to_waitpoint`
- 2nd (bob)    → `resume_condition_satisfied`

After the scanner promotes the resumed attempt (tens of seconds on
defaults; tune `FF_UNBLOCK_INTERVAL_S`), deploy picks up the
reclaim grant, finishes the rollout, and verify runs.

### Rollback variant — cancel_flow cascade

```bash
cargo run --release --bin submit -- \
  --artifact "app:v1.2.4" --commit baddeef \
  --fail-verify
```

verify receives `fail=true` in its payload and issues
`POST /v1/flows/{id}/cancel?wait=true` with
`cancellation_policy=cancel_all`.

---

## Troubleshooting

- **Workers start but never claim.** Make sure the ff-sdk dep in
  `Cargo.toml` enables the `direct-valkey-claim` feature — without it
  the worker doesn't advertise its capabilities to the scheduler's
  connected-worker index and the blocking detail shows
  `no connected worker satisfies required_capabilities`.

- **`hmac_secret_not_initialized` from suspend.** The HMAC secret is
  stored per-partition in Valkey on server boot. If you `FLUSHALL`
  while the server is running, restart the server (the secret isn't
  re-seeded lazily).

- **Resume takes >20 s.** The unblock scanner runs at
  `FF_UNBLOCK_INTERVAL_S` (default 5 s) and reclaim grants have
  their own scan. For demo snappiness set
  `FF_UNBLOCK_INTERVAL_S=1 FF_LEASE_EXPIRY_INTERVAL_MS=500` on the
  server.

---

## Known gaps + design notes

- **`--backend postgres` is not live.** ff-server 0.6.1 wires only
  the Valkey backend; there is no server-side config switch to
  select the Postgres backend yet. The Postgres crate ships with
  migrations and trait impls; `scripts/bootstrap-pg.sh` is
  preserved for the follow-up PR that adds `FF_BACKEND=postgres`
  to ff-server's `ServerConfig`. Submit warns and falls back to
  Valkey when `--backend postgres` is passed.

- **Approve CLI requires both `--flow-id` and `--execution-id`.**
  ff-server 0.6.1 does not expose a `GET /v1/flows/{id}/members`
  route, so approve can't derive the deploy execution id from the
  flow id alone. The submit CLI prints `deploy_created
  execution_id=…` for the user to copy. A future server route will
  simplify this to `--flow-id` only.

- **Graph-revision bootstrap via SDK.** Submit opens a
  `FlowFabricWorker::connect` solely to call `describe_flow` and
  seed its local `graph_revision` before staging the first edge
  (matches the llm-race pattern). A future REST route
  (`GET /v1/flows/{id}`) would let HTTP-only consumers drop the
  Valkey dependency entirely at submit time.

---

## Files

```
examples/deploy-approval/
├── Cargo.toml
├── README.md                  # this file
├── scripts/
│   └── bootstrap-pg.sh        # createdb + sqlx migrate run (not yet wired into ff-server)
└── src/
    ├── lib.rs                 # shared payloads + lane/capability constants
    └── bin/
        ├── submit.rs          # flow + execs + AllOf edges
        ├── build_worker.rs    # DurableSummary JsonMergePatch stream
        ├── test_worker.rs     # capability-routed claims (--kind unit|integration|e2e)
        ├── deploy.rs          # canary + suspend(Count{2,DistinctSources}) + full rollout
        ├── verify.rs          # health check + cancel_flow cascade on --fail
        └── approve.rs         # deliver_signal with distinct source_identity
```
