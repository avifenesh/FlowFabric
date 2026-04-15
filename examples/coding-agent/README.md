# Coding Agent Example

An LLM-powered coding agent built on FlowFabric with human-in-the-loop review.

## What it demonstrates

- **Execution lifecycle**: create, claim, progress updates, complete/fail
- **Lease management**: worker holds a lease while processing; auto-renewed by ff-sdk
- **Streaming output**: each agent reasoning step is appended as a stream frame
- **Suspend/signal (human-in-the-loop)**: worker suspends after producing a patch, waits for a human review signal before completing
- **Polling**: submit CLI polls execution state transitions in real time
- **Priority scheduling**: tasks are scheduled by priority in the FlowFabric lane

## Architecture

```
submit CLI                FlowFabric Server            Worker
    |                     (Valkey + REST API)              |
    |--- POST /executions ------>|                         |
    |    (creates task)          |                         |
    |                            |<--- claim_next() ------|
    |                            |---- task payload ------>|
    |<-- poll state ------------>|                         |
    |    [waiting]               |<--- progress(5%) ------|
    |    [running]               |<--- append_frame() ----|  (LLM loop)
    |                            |<--- progress(90%) -----|
    |                            |<--- suspend() ---------|  (patch ready)
    |    [suspended]             |                         |
    |                            |                         |
approve CLI                      |                         |
    |--- POST signal ----------->|                         |
    |    (approved: true)        |---- resume ------------>|
    |                            |<--- claim_next() ------|
    |                            |<--- complete() --------|
    |    [completed]             |                         |
```

## Prerequisites

1. **Valkey** running on `localhost:6379`

   ```bash
   # Docker
   docker run -d --name valkey -p 6379:6379 valkey/valkey:7.2

   # Or install locally: https://valkey.io/download/
   ```

2. **OpenRouter API key** (for LLM access)

   Get one at https://openrouter.ai/keys

3. **Rust toolchain** (stable)

## Setup

```bash
# From the FlowFabric project root:
cargo build -p ff-server
cd examples/coding-agent && cargo build
```

## Running

You need 3 terminals. All commands assume you start from the FlowFabric project root.

### Terminal 1: Start the FlowFabric server

```bash
FF_LISTEN_ADDR=0.0.0.0:9090 cargo run -p ff-server
```

The server connects to Valkey, loads the Lua library, starts 14 background scanners, and listens for HTTP requests on port 9090.

### Terminal 2: Start the worker

```bash
cd examples/coding-agent
OPENROUTER_API_KEY=sk-or-... cargo run --bin worker
```

The worker connects directly to Valkey via ff-sdk, polls for eligible tasks, and processes them with an LLM reasoning loop.

### Terminal 3: Submit a task

```bash
cd examples/coding-agent
cargo run --bin submit -- --issue "Write a Rust function that checks if a string is a palindrome"
```

The submit CLI creates an execution via the REST API and polls for state changes. You'll see output like:

```
Created execution: a1b2c3d4-...
[state] waiting
[state] running
[state] suspended
Awaiting review. Use:
  cargo run --bin approve -- --execution-id a1b2c3d4-... --waitpoint-id e5f6g7h8-... --approve
```

### Approve or reject

Approve the patch:

```bash
cargo run --bin approve -- \
  --execution-id a1b2c3d4-... \
  --waitpoint-id e5f6g7h8-... \
  --approve
```

Or reject with feedback:

```bash
cargo run --bin approve -- \
  --execution-id a1b2c3d4-... \
  --waitpoint-id e5f6g7h8-... \
  --reject --feedback "needs edge case handling for empty strings"
```

After approval, the worker re-claims the execution and completes it. The submit CLI shows:

```
[state] completed
Task completed!
```

## Environment variables

| Variable | Used by | Default | Description |
|----------|---------|---------|-------------|
| `FF_HOST` | worker | `localhost` | Valkey host |
| `FF_PORT` | worker | `6379` | Valkey port |
| `OPENROUTER_API_KEY` | worker | (required) | OpenRouter API key |
| `OPENROUTER_MODEL` | worker | `minimax/minimax-m2.7` | LLM model name |

### CLI flags

**submit**: `--server`, `--issue`, `--language`, `--context`, `--max-turns`, `--namespace`, `--lane`, `--priority`, `--no-wait`

**worker**: `--host`, `--port`, `--api-key`, `--model`, `--namespace`, `--lane`

**approve**: `--server`, `--execution-id`, `--waitpoint-id`, `--approve`, `--reject`, `--feedback`

Run any binary with `--help` for full details.
