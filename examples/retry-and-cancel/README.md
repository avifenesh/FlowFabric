# retry-and-cancel

End-to-end example exercising two core FlowFabric control-plane
behaviours against a live `ff-server` + Valkey:

1. **Retry exhaustion → terminal `failed`.** A handler that always
   errors drives an execution through its retry schedule
   (`max_retries = 2`, fixed 50 ms backoff). After the third attempt
   fails, `FailOutcome::TerminalFailed` is returned and the server
   transitions the execution to `failed`.
2. **`cancel_flow` cascade.** A flow with two delayed member
   executions is cancelled with `cancellation_policy = "cancel_all"`
   and `wait=true`. The handler returns synchronously after every
   member reaches `cancelled`.

The demo is single-process: the worker loop runs on a background
tokio task, the orchestrator drives both scenes, and the process
exits once both terminate.

## Prereqs

- Valkey reachable at `FF_HOST:FF_PORT` (default `localhost:6379`).
- `ff-server` reachable at `FF_SERVER_URL` (default
  `http://localhost:9090`). Build it from this workspace:
  `cargo build -p ff-server`.

## Run

```bash
# defaults: valkey at localhost:6379, ff-server at localhost:9090
cargo run -p retry-and-cancel-example

# or point at custom endpoints:
FF_HOST=127.0.0.1 FF_PORT=6399 FF_SERVER_URL=http://localhost:9099 \
    cargo run -p retry-and-cancel-example
```

## Expected transcript

```
INFO retry_and_cancel: ── scene 1: retry exhaustion ──
INFO retry_and_cancel: worker loop started
INFO retry_and_cancel: submitted flaky execution execution_id={fp:86}:…
INFO retry_and_cancel: claimed task execution_id={fp:86}:… attempt=0
INFO retry_and_cancel: retry scheduled execution_id={fp:86}:… delay_until_ms=…
INFO retry_and_cancel: claimed task execution_id={fp:86}:… attempt=1
INFO retry_and_cancel: retry scheduled execution_id={fp:86}:… delay_until_ms=…
INFO retry_and_cancel: claimed task execution_id={fp:86}:… attempt=2
INFO retry_and_cancel: retries exhausted — terminal failed execution_id={fp:86}:…
INFO retry_and_cancel: scene 1 terminal execution_id={fp:86}:… state=failed
INFO retry_and_cancel: ── scene 2: cancel_flow cascade ──
INFO retry_and_cancel: flow created flow_id=…
INFO retry_and_cancel: member added to flow execution_id={fp:103}:…
INFO retry_and_cancel: member added to flow execution_id={fp:103}:…
INFO retry_and_cancel: flow members created (delayed, unclaimable) flow_id=… partition=103
INFO retry_and_cancel: cancel_flow returned flow_id=… response={"Cancelled":{"cancellation_policy":"cancel_all","member_execution_ids":[…,…]}}
INFO retry_and_cancel: member cancelled execution_id={fp:103}:…
INFO retry_and_cancel: member cancelled execution_id={fp:103}:…
INFO retry_and_cancel: demo complete — both scenes terminated
```

(UUIDs and partition indices vary; three attempts for scene 1 and two
cancelled members for scene 2 are fixed.)

## Public APIs exercised

- `ff_core::types::{ExecutionId, FlowId, LaneId, TimestampMs}`
- `ff_core::partition::{PartitionConfig, flow_partition}`
- `ff_core::policy::{ExecutionPolicy, RetryPolicy, BackoffStrategy}`
- `ff_sdk::{FlowFabricAdminClient, FlowFabricWorker, WorkerConfig, FailOutcome}`
- HTTP: `POST /v1/executions`, `POST /v1/flows`, `POST /v1/flows/{id}/members`,
  `POST /v1/flows/{id}/cancel?wait=true`, `GET /v1/executions/{id}/state`.

## SDK gaps noted

- Create-execution and add-member still go through raw HTTP; there is
  no `FlowFabricAdminClient::create_execution` /
  `create_flow` / `add_member` / `cancel_flow` wrapper. Every
  consumer reimplements the body shape. Tracked as a follow-up —
  not blocking this example.
- `poll_until_terminal` is hand-rolled over `GET
  /v1/executions/{id}/state`; a subscribe-style SDK helper would
  remove the polling loop.

## Safety budget

The demo self-terminates after 60 s if either scene stalls (covers
`ff-server` unreachable, worker deadlocked, or HTTP hang). No Ctrl-C
needed in the happy path.
