# llm-race: UC-38 model/provider fallback

A FlowFabric v0.6 example that races **three free OpenRouter LLM
providers** against the same prompt and streams the winner's response
back to the caller as a durable summary. Demonstrates the v0.6
headline primitives:

| RFC | Primitive                                             | Where used                            |
|-----|-------------------------------------------------------|---------------------------------------|
| 016 | `AnyOf { on_satisfied: CancelRemaining }` edge group   | submit wires it on the aggregator     |
| 016 | Stage C sibling-cancel dispatcher                      | visible as `provider_cancelled` lines |
| 015 | `DurableSummary { JsonMergePatch }` stream mode        | aggregator `append_frame_with_mode`   |
| 014 | `Composite(Count { 2, DistinctSources })` resume      | `--review` HITL gate (optional)       |
| 013 | Typed `ResumeCondition` + `SuspendArgs`                | aggregator suspend path               |
| 007 | Flow DAG + dependency edges                            | `stage_dependency_edge` for each racer |

## Build status

Scaffolded against ff 0.6.x. The task brief had this example gated on
the in-flight 0.6.1 `read_summary` decoder hotfix; that hotfix landed
on `main` as PR #225 while this example was being scaffolded, so the
read-back blocker is resolved on tip-of-main. The example was
nevertheless only **build-checked, never executed**, per the task
instructions:

```bash
cargo build --release -p llm-race-example
```

Owner should execute end-to-end against a live server + Valkey before
merging. Expect rough edges on first run (details below).

## Architecture

```
              submit CLI
                 |
                 | 1. POST /v1/flows
                 | 2. GET openrouter.ai/api/v1/models  (filter free)
                 | 3. POST /v1/executions  x3 (provider_X, caps=role=provider)
                 | 4. POST /v1/executions      (aggregator,   caps=role=aggregate)
                 | 5. SDK.set_edge_group_policy(AnyOf{CancelRemaining})
                 | 6. POST /v1/flows/{f}/edges         x3
                 | 7. POST /v1/flows/{f}/edges/apply   x3
                 v
  +------------------------------------------+
  |  provider worker(s) on lane "provider"   |
  |  claim_via_server -> OpenRouter -> complete
  +------------------------------------------+
          \    |    /
           \   |   /   (AnyOf: first to complete satisfies;
            \  |  /     Stage C dispatcher cancels stragglers)
             v v v
  +------------------------------------------+
  |  aggregator worker on lane "aggregator"  |
  |  list_incoming_edges -> find winner      |
  |  append_frame_with_mode(DurableSummary,  |
  |                        JsonMergePatch)   |
  |  [--review: suspend on Count(2, DistinctSources)]
  |  complete(WinnerRecord)                  |
  +------------------------------------------+
                 ^
                 | (optional)
                 | POST /v1/executions/{agg}/signal   x2 (distinct reviewers)
                 |
            approve CLI (run twice for 2-of-3)
```

## Why the free-model list is discovered at runtime

OpenRouter's free tier churns. Hardcoding models means an example that
quietly rots; instead the submit CLI hits `GET
https://openrouter.ai/api/v1/models` at launch and filters to entries
with `pricing.prompt == "0"` AND `pricing.completion == "0"`. Up to
three are raced.

**Graceful degradation:** If OpenRouter reports fewer than 2 free
models at runtime, `submit` exits with `"AnyOf race needs >= 2"` —
racing a single provider defeats the point of the primitive. With
exactly 2 free models, the flow runs as a 2-way race.

## Prerequisites

1. **Valkey** on `localhost:6379` (or set `FF_HOST` / `FF_PORT`).
2. **ff-server** from this repo running with default configuration:
   ```bash
   cargo run -p ff-server
   ```
3. **OpenRouter API key** — [get one][orkey] and export it; the
   example never reads a `.env` file to avoid collision with the
   `coding-agent` example's key.
   ```bash
   export OPENROUTER_API_KEY=sk-or-...
   ```
4. Rust stable toolchain.

[orkey]: https://openrouter.ai/keys

## Run steps (post-0.6.1)

Four terminals. All commands from the repo root.

### Terminal 1 — ff-server
```bash
cargo run -p ff-server
```

### Terminals 2, 3, 4 — provider workers
Each worker advertises `role=provider` capability. Running three
simultaneously gives the race maximum parallelism, though one worker
that picks up all three provider executions in sequence works too (the
race is the *providers*, not the workers).
```bash
cd examples/llm-race
cargo run --bin provider
```

### Terminal 5 — aggregator worker
```bash
cd examples/llm-race
cargo run --bin aggregator
```

### Terminal 6 — submit the race
```bash
cd examples/llm-race
cargo run --bin submit -- --prompt "Explain RFC 7396 JSON Merge Patch in two sentences."
```

### With the HITL review gate
```bash
cargo run --bin submit -- --prompt "..." --review
# aggregator will suspend after streaming the deltas; deliver 2 approvals
# (distinct reviewers) to satisfy Count(2, DistinctSources):
cargo run --bin approve -- --execution-id <agg-eid> --reviewer alice
cargo run --bin approve -- --execution-id <agg-eid> --reviewer bob
```

## What to expect in the timeline

With three free providers racing, the submit CLI will print something
like:

```
Racing 3 free model(s):
  - provider-a/free-1 (Free 1)
  - provider-b/free-2 (Free 2)
  - provider-c/free-3 (Free 3)
Flow created: <flow-id>
Provider execution created: <eid-a> model=provider-a/free-1
Provider execution created: <eid-b> model=provider-b/free-2
Provider execution created: <eid-c> model=provider-c/free-3
Aggregator execution created: <eid-agg>
Edge-group policy set: AnyOf { CancelRemaining }
Edge staged+applied: <eid-a> -> <eid-agg>
Edge staged+applied: <eid-b> -> <eid-agg>
Edge staged+applied: <eid-c> -> <eid-agg>
[aggregator state] blocked
[aggregator state] waiting
[aggregator state] running
[aggregator state] completed
```

Meanwhile the provider terminals print interleaved lines:
```
[timeline] provider_claimed   execution_id=<a> model=provider-a/free-1
[timeline] provider_claimed   execution_id=<b> model=provider-b/free-2
[timeline] provider_claimed   execution_id=<c> model=provider-c/free-3
[timeline] provider_completed execution_id=<b> model=provider-b/free-2 tokens=183
[timeline] provider_cancelled execution_id=<a> model=provider-a/free-1 reason=<...>
[timeline] provider_cancelled execution_id=<c> model=provider-c/free-3 reason=<...>
```

And the aggregator terminal:
```
[timeline] aggregator_claimed          execution_id=<agg>
[timeline] aggregator_received_winner  execution_id=<agg> model=provider-b/free-2
[timeline] aggregator_streamed frames=4 execution_id=<agg>
[timeline] aggregator_published        execution_id=<agg>
```

## Known design question (deferred to owner)

- **No REST route for `set_edge_group_policy`.** The submit CLI
  currently opens a direct SDK backend connection for that one call.
  A consumer that only has HTTP reach to ff-server cannot run this
  example as-is. Owner may want to add
  `POST /v1/flows/{id}/edge-group-policy` before cutting 0.6.1.

## Constraints honoured

- No OpenRouter key in source or `.env`.
- No `dotenv` auto-load (the coding-agent example's `.env` must stay
  unreachable from here).
- Minimum-2 graceful-degradation check on the free-model list.
- `cargo build --release` only; the scaffold has never been executed.
