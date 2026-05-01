# llm-race — racing LLM providers with automatic cancel

End-to-end FlowFabric example: submit one prompt, race it against
multiple free OpenRouter models, automatically cancel the losers when
one wins, stream the winner's response back as it arrives.

**Proved live.** 2026-04-24 against three free OpenRouter models;
8.1 s end-to-end. [Full run report](../../rfcs/drafts/0.6.1-llm-race-example-run.md).

---

## Why this example exists (and why you should read it)

FlowFabric ships primitives that do not exist in apalis/faktory/celery.
This example is the shortest path from "I heard about `AnyOf`
dependencies" to "I understand exactly how to use them." The patterns
below are **copy-pasteable** into your own consumer — the example's
`src/bin/*.rs` are kept short on purpose.

If you're evaluating FlowFabric, read this README and then diff
`submit.rs`, `provider.rs`, `aggregator.rs` against your mental model
of Celery or apalis. The delta is the value prop.

---

## Primitives you'll learn

### 1. `AnyOf { CancelRemaining }` dependency edge (RFC-016)

**The problem.** You want to query N LLM providers in parallel and
proceed on the first response, cancelling the rest so you don't pay
for their tokens. In Celery you'd write an ad-hoc cancellation dance
in application code. In FlowFabric you declare the policy on the edge:

```rust
// submit.rs: after creating provider_a/b/c and the aggregator exec
let policy = EdgeDependencyPolicy::any_of(OnSatisfied::CancelRemaining);
sdk.set_edge_group_policy(&aggregator_eid, policy).await?;

for provider_eid in [provider_a, provider_b, provider_c] {
    sdk.stage_dependency_edge(&provider_eid, &aggregator_eid).await?;
}
sdk.apply_staged_edges(&flow_id).await?;
```

That's it. When the first provider execution transitions to
`Completed`, FlowFabric's dispatcher (running on `ff-engine`'s scanner
loop) cancels the other two with `cancellation_reason =
sibling_quorum_satisfied`. The aggregator becomes eligible.

Related variants:
- `OnSatisfied::LetRun` — satisfies the edge but lets siblings finish
  naturally (audit-friendly workloads).
- `EdgeDependencyPolicy::quorum(k, OnSatisfied::*)` — need k of n
  successes before downstream fires.

### 2. `DurableSummary` stream with JSON Merge Patch (RFC-015)

**The problem.** LLMs emit tokens incrementally. You want the consumer
to see partial output live, AND keep a durable record of the final
response, AND not blow up Valkey with 2,000 individual stream frames
for a long generation.

```rust
// aggregator.rs: stream the winning response
for chunk in openrouter_response.chunks() {
    let patch = serde_json::json!({
        "output": chunk.delta,
        "tokens_used": chunk.cumulative_tokens,
    });
    task.append_frame_with_mode(
        Frame::new(payload_bytes),
        StreamMode::durable_summary(),  // JSON Merge Patch
    ).await?;
}
```

Each `append_frame_with_mode` emits an `XADD` with `mode=summary` fields
AND merges the patch atomically into a server-side rolling summary
Hash. Tailers see deltas; readers calling `read_summary` see the
accumulated document.

```rust
// submit.rs (or any consumer):
let summary = sdk.read_summary(&agg_eid).await?.unwrap();
// summary.document_json is the accumulated doc; summary.version counts patches
```

Best-effort variants (for logs / debug tokens you don't need durable):

```rust
StreamMode::best_effort_live(30_000)  // TTL 30 s, dynamic MAXLEN
```

The server auto-sizes `MAXLEN` based on observed append rate (EMA
α=0.2, 98.3% visibility at TTL on bursty LLM workloads).

### 3. Typed `SuspendArgs` + `suspend` / `try_suspend` split (RFC-013)

**The problem.** A workflow needs human approval before publishing.
You want the worker to release its lease and wait for N distinct
signals without polling.

```rust
// aggregator.rs (the --review variant)
let args = SuspendArgs::new(
    SuspensionId::new(),
    WaitpointBinding::fresh(wp_key, expires_in),
    ResumeCondition::Composite(CompositeBody::Count {
        n: 2,
        kind: CountKind::DistinctSources,
        matcher: None,
        waitpoints: vec![wp_key.to_string()],
    }),
    ResumePolicy::normal(),
    SuspensionReasonCode::HumanReview,
    SuspensionRequester::worker(),
);
let handle = task.suspend(args).await?;  // lease released
```

The `suspend` strict variant returns a `SuspendedHandle`; `try_suspend`
returns `SuspendOutcome::{Suspended, AlreadySatisfied}` and keeps your
`ClaimedTask` alive on `AlreadySatisfied` (classic Rust `foo`/`try_foo`
pattern, like `RwLock::read` / `try_read`).

### 4. Multi-signal resume with `Count(n, DistinctSources)` (RFC-014)

**The problem.** "Two of five reviewers approve." In Celery you'd
build a state machine manually. In FlowFabric the `Count` variant does
it for you — only `n` distinct **sources** count, not `n` signals
total. Duplicate signals from one reviewer don't satisfy the condition.

The approve CLI:

```rust
// approve.rs
sdk.deliver_signal(DeliverSignalArgs {
    waitpoint_key: wp_key,
    source_identity: reviewer.to_string(),  // "alice" or "bob"
    payload: approval_payload,
    now: Utc::now().timestamp_millis(),
    ..Default::default()
}).await?;
```

Other `CountKind` variants:
- `DistinctWaitpoints` — count different waitpoint keys.
- `DistinctSignals` — count different signal_ids (raw dedup).

### 5. Flow DAG with capability routing (RFC-007 + RFC-009)

Providers advertise `role=provider`, aggregator advertises
`role=aggregate`, review worker advertises `role=review`. Workers only
claim from their matching executions. Submit wires everything into one
flow; dependencies are edges; policies live on inbound-edge groups.

---

## Architecture

```
submit CLI
  │
  │ 1. GET https://openrouter.ai/api/v1/models → filter free tier
  │ 2. POST /v1/flows                          (create the race flow)
  │ 3. POST /v1/executions × N                 (provider_i, caps: role=provider)
  │ 4. POST /v1/executions                     (aggregator, caps: role=aggregate)
  │ 5. set_edge_group_policy(aggregator, AnyOf{CancelRemaining})
  │ 6. stage_dependency_edge(provider_i → aggregator) × N + apply_staged_edges
  │
  ▼
provider workers (lane: provider, advertised caps: role=provider)
  │ claim_via_server → POST openrouter.ai/chat/completions → complete(response)
  │
  │ AnyOf: first completer satisfies downstream
  │ ff-engine's Stage-C dispatcher cancels stragglers within ~1s
  ▼
aggregator worker (lane: aggregator, caps: role=aggregate)
  │ list_incoming_edges → find winner
  │ append_frame_with_mode(DurableSummary, JsonMergePatch) × N patches
  │ [--review] suspend(Count{2, DistinctSources}) → wait for 2 approvals
  │ complete(WinnerRecord)
  ▼
submit CLI
  │ tail_stream + read_summary → prints winner incrementally
  │ final state: flow completed
```

---

## Prereqs

1. **Valkey ≥ 7.2** on `localhost:6379`:
   ```bash
   docker run -d --name valkey -p 6379:6379 valkey/valkey:8-alpine
   ```

2. **Rust stable** + `cargo`.

3. **OpenRouter API key** (free; no payment method required for free
   models): <https://openrouter.ai/keys>. Export it:
   ```bash
   export OPENROUTER_API_KEY=sk-or-v1-...
   ```

   The example never reads a `.env` file — it reads `OPENROUTER_API_KEY`
   from the environment only, to avoid collision with sibling examples.

## Run it

Six terminals. All commands from the repo root.

### Terminal 1 — ff-server

```bash
FF_WAITPOINT_HMAC_SECRET=$(openssl rand -hex 32) \
FF_LANES=default,provider,aggregator,review \
  cargo run -p ff-server --release
```

Health check: `curl http://localhost:8080/healthz` returns 200.

### Terminals 2, 3, 4 — provider workers

```bash
cd examples/llm-race
cargo run --release --bin provider
```

Start the command in each terminal. They'll race for the three
provider executions. A single worker claiming all three in sequence
works too (the race is between providers' response times, not
between worker processes); three workers just maximize parallelism.

### Terminal 5 — aggregator worker

```bash
cd examples/llm-race
cargo run --release --bin aggregator
```

### Terminal 6 — submit

```bash
cd examples/llm-race
cargo run --release --bin submit -- \
  --prompt "Write a 3-line haiku about distributed systems."
```

Expected timeline output (times approximate):

```
[0.0s] Racing 3 free model(s): [model-a, model-b, model-c]
[0.1s] Flow created: <flow-id>
[0.2s] Provider exec a, b, c created; aggregator exec created
[0.2s] Edge-group policy: AnyOf{CancelRemaining}
[0.3s] Staged + applied 3 edges
[0.4s] Aggregator state: blocked → waiting
[1.2s] Provider claimed: model-a
[1.3s] Provider claimed: model-b
[1.3s] Provider claimed: model-c
[3.1s] Provider completed: model-b (tokens=183)
[3.3s] Provider cancelled: model-a (reason=sibling_quorum_satisfied)
[3.3s] Provider cancelled: model-c (reason=sibling_quorum_satisfied)
[3.4s] Aggregator state: running
[3.5s] Aggregator streamed frame 1/4
[3.7s] Aggregator streamed frame 2/4
[3.9s] Aggregator streamed frame 3/4
[4.0s] Aggregator streamed frame 4/4 (final)
[4.1s] Aggregator state: completed
[8.1s] Flow completed
```

### Optional — with HITL review gate

```bash
# Terminal 6:
cargo run --release --bin submit -- --prompt "..." --review

# Aggregator will suspend. Watch its log for `waitpoint_id=<wp-id>`;
# the raw HMAC token is fetched via the ff-sdk admin client
# (`FlowFabricAdminClient::read_waitpoint_token`, v0.14) — no
# copy-paste from the worker log:
cargo run --release --bin approve -- \
    --execution-id <aggregator-eid> \
    --waitpoint-id <wp-id> \
    --reviewer alice
cargo run --release --bin approve -- \
    --execution-id <aggregator-eid> \
    --waitpoint-id <wp-id> \
    --reviewer bob
```

After both distinct-source signals arrive, aggregator resumes and
completes. Delivering alice's signal twice does NOT satisfy
`Count(n=2, DistinctSources)` — you need two distinct reviewers.

---

## Reading the code

Source files map to primitives:

| File | Lines | Read this to learn |
|---|---|---|
| `src/bin/submit.rs` | ~200 | Flow creation + edge-group policy + `apply_staged_edges` |
| `src/bin/provider.rs` | ~120 | The classic worker loop with capability routing |
| `src/bin/aggregator.rs` | ~180 | `append_frame_with_mode`, incoming-edge payload retrieval, optional `suspend` with `Count` |
| `src/bin/approve.rs` | ~60 | `deliver_signal` with `source_identity` |
| `src/lib.rs` | ~100 | Shared types, OpenRouter client, free-model discovery |

Each binary is self-contained so you can read one without the others.
Consumers porting their own worker loops typically start from
`provider.rs` + `aggregator.rs`.

---

## What to inspect during / after a run

- **Describe a cancelled provider exec:**
  ```bash
  curl -s http://localhost:8080/v1/executions/<loser-eid> | jq
  ```
  Expect `public_state: cancelled`, `cancellation_reason:
  sibling_quorum_satisfied`, `cancelled_by: operator_override`.

- **Read the aggregator summary:**
  ```bash
  curl -s http://localhost:8080/v1/executions/<agg-eid>/summary | jq
  ```
  Returns `{document_json: {output: "...", tokens_used: N}, version: N}`.

- **Tail the aggregator stream during the run:** `submit.rs` already
  does this internally; see `tail_stream_with_visibility` call.

---

## Free-model discovery

The submit CLI calls `GET https://openrouter.ai/api/v1/models` at
startup and filters to models where both `pricing.prompt == "0"` AND
`pricing.completion == "0"`. It picks up to 3.

**Graceful degradation:** if fewer than 2 free models are listed at
runtime, submit exits with a friendly error — a "race" with one
provider is a no-op. OpenRouter's free tier rotates; check
<https://openrouter.ai/models?q=free> if the CLI complains.

---

## Troubleshooting

- **`401 Unauthorized` from OpenRouter.** `$OPENROUTER_API_KEY` isn't
  set in the shell where you ran `provider`. Export it or put it in
  your shell rc.
- **Dispatcher slow to cancel (>3s).** Set
  `FF_EDGE_CANCEL_DISPATCHER_INTERVAL_S=0.25` on ff-server to tighten
  the scanner cadence for testing.
- **Aggregator never receives winner payload.** Make sure `ff-server`
  is on v0.6.1+ (the `read_summary` RESP3 decoder was broken in 0.6.0;
  fix in PR #225).

---

## Known gaps + design notes

- **No REST route for `set_edge_group_policy` yet.** The submit CLI
  opens a direct SDK Valkey connection for that single call. A future
  PR will add `POST /v1/flows/{id}/edge-groups/{downstream_eid}` so
  HTTP-only consumers can run this example without local Valkey
  access. Tracked; PR not yet filed.

- **Graph-revision bootstrap.** Submit reads `describe_flow` to seed
  its local `graph_revision` counter before staging edges, avoiding a
  `stale_graph_revision` race. Fix landed in PR #227.

- **Stream MAXLEN clamp.** `BestEffortLive` mode dynamically sizes the
  MAXLEN per an EMA of observed append rate (α=0.2). Clamps: floor
  64, ceiling 16 384. Under bursty LLM-token workloads (up to 4 kHz)
  this gives 98.3% visibility at the TTL window. Per the Phase 0
  benchmark report.

---

## Files

```
examples/llm-race/
├── Cargo.toml
├── README.md              # this file
├── .env.example           # OPENROUTER_API_KEY=
└── src/
    ├── lib.rs             # shared types + OpenRouter client + free-model discovery
    └── bin/
        ├── submit.rs      # submit CLI: flow wire-up + edge-group policy + tail
        ├── provider.rs    # provider worker: claim + OpenRouter call + complete
        ├── aggregator.rs  # aggregator: DurableSummary stream + optional HITL suspend
        └── approve.rs     # reviewer CLI: deliver_signal with source_identity
```
