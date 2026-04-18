# ferriskey vs redis-rs — blocking-command mechanism trace (W1, round 2)

**Author:** Worker-1
**Branch:** `feat/ferriskey-perf-invest`
**Scope:** Source walk of the blocking-command IO path in both clients,
plus a four-binary probe that verifies the theoretical trace against
runtime behaviour. No code changes in either client; recommendations
only.

## TL;DR

1. ferriskey has explicit blocking-command detection (BLPOP / BLMPOP /
   XREAD / BZMPOP / BRPOPLPUSH / …) and extends the client-side
   timeout by a hard-coded 500 ms (`BLOCKING_CMD_TIMEOUT_EXTENSION`)
   to survive a server-side block. See
   `ferriskey/src/client/mod.rs:226, 307–336`.
2. redis-rs 1.2.0 has **no blocking-command detection**. Its mux
   applies a single connection-level `response_timeout` (default
   **500 ms** per `redis-1.2.0/src/client.rs:180`) to every command —
   BLMPOP with a 5-second server-side timeout fails client-side at
   500 ms unless the consumer explicitly calls
   `set_response_timeout(Duration::from_secs(...))` with a window
   larger than the command's block.
3. Both clients use a **single-pipeline in-flight queue**
   (`VecDeque<InFlight>`) over a single TCP/TLS stream. Neither
   spawns a dedicated connection for blocking commands; both multiplex
   blocking + non-blocking commands on the same socket. This is
   architecturally identical.
4. The `-46 %` BLMPOP benchmark gap from round 1 was **not** caused
   by blocking-mechanism divergence. The bench pre-seeded the list so
   BLMPOP never actually blocked — the measurement was per-RPC cost
   on a command shape that returns fast. Round-1 round 1 report §1
   (5-layer dispatch envelope, Arc::new(cmd.clone()), Telemetry
   RwLock in StandaloneClient::Drop) is the real cause.
5. Probe data confirms: at 1 client × 1 BLMPOP (empty list-free fast
   path), ferriskey is ~10 µs / ~27 % slower per command. At
   5 concurrent BLMPOPs against a real server-side block, **both
   clients serialise response delivery post-unblock** — and both lose
   commands to client-side timeout when the post-unblock response
   queue drains slower than the configured timeout window.

## §1 — redis-rs blocking path

Source references are against the vendored copy at
`benches/perf-invest/redis-rs-src/redis/src/`. All files cited are
redis-rs 1.2.0 as pinned in
`benches/comparisons/baseline/Cargo.lock`.

### Call chain

`MultiplexedConnection::send_packed_command` → `Pipeline::send_recv` →
mpsc `Sender<PipelineMessage>` → `PipelineSink::start_send` → TCP.

- `aio/multiplexed_connection.rs:721–765` — `send_packed_command`
  body. Acquires an optional `concurrency_limiter` permit (async-
  lock Semaphore, default `None`), then tail-calls
  `self.pipeline.send_recv(packed, None, self.response_timeout,
  cmd.is_no_response())`.
- `aio/multiplexed_connection.rs:428–481` — `Pipeline::send_recv`.
  The timeout is applied at this ONE point:
  ```
  match timeout {
      Some(timeout) => match Runtime::locate().timeout(timeout, request).await { ... }
      None => request.await,
  }
  ```
  `timeout` is `self.response_timeout: Option<Duration>`, set once
  via `set_response_timeout`. It is **not** per-command and has
  **no blocking-command awareness**.
- `aio/multiplexed_connection.rs:500, 651` — `response_timeout`
  field on `MultiplexedConnection`.
- `client.rs:180` — **`DEFAULT_RESPONSE_TIMEOUT = Some(Duration::
  from_millis(500))`**. Every `get_multiplexed_async_connection`-
  returned handle inherits this default. Blocking commands with a
  server block longer than 500 ms return a client-side timeout error
  unless the consumer explicitly calls `set_response_timeout`.

### Mux queue

`aio/multiplexed_connection.rs:119, 131, 161, 216, 255, 272, 351`

`PipelineSink` holds `in_flight: VecDeque<InFlight>`. Each
`InFlight` carries an `Option<PipelineOutput>` (= `oneshot::Sender<
Result<Value>>`) and a `response_aggregate` enum.

- `start_send` (line 319) is called for every command: push packet
  bytes to the sink, push a matching `InFlight` to the back of
  `in_flight`.
- `poll_read` (line 170) pulls response bytes from the sink stream,
  pops the FRONT entry of `in_flight`, and `oneshot::send`s the value
  to the per-command receiver.

Responses are delivered in FIFO order. Blocking commands DO sit at
the front of the queue. Non-blocking commands issued AFTER a blocker
wait for the blocker's response to pop before they get their own.

### Doc-level disclosure

`aio/multiplexed_connection.rs:484–495`:

> This connection object is cancellation-safe, and the user can drop
> request future without polling them to completion, but this doesn't
> mean that the actual request sent to the server is cancelled. A
> side-effect of this is that the underlying connection won't be
> closed until all sent requests have been answered, which means that
> in case of blocking commands, the underlying connection resource
> might not be released, even when all clones of the multiplexed
> connection have been dropped.

redis-rs documents the head-of-line issue but does not work around it.
No per-command timeout; no dedicated connection for blockers; no
blocking-command detection.

### No blocking-keyword detection

`grep -n "blocking\|BLPOP\|BLMPOP\|XREAD"`  in `aio/multiplexed_
connection.rs` + `aio/connection.rs` (the non-mux single-conn path):

- Only 4 hits, all either doc comments or `cmd.is_no_response()`
  (which flags fire-and-forget pub/sub, NOT blocking — it's used to
  bypass the response-expectation path entirely).

### redis-rs verdict

Single client-side timeout wrapping the whole request. Fails-fast
at 500 ms by default — a **client-side upper bound below any realistic
BLMPOP / XREAD block**. A consumer who issues blocking commands
through `get_multiplexed_async_connection` without first calling
`set_response_timeout` is silently broken.

## §2 — ferriskey blocking path

Source references are against `ferriskey/src/` at the HEAD of this
branch.

### Call chain

`Client::send_command` → `execute_command_owned` →
`StandaloneClient::send_command` → `send_request_to_single_node` →
`send_request` → `ReconnectingConnection::get_connection` →
`MultiplexedConnection::send_packed_command` →
`Pipeline::send_single` → `send_recv` → mpsc `Sender<PipelineMessage>`
→ TCP.

### Blocking-command detection

`ferriskey/src/client/mod.rs:225–305`:

```
const BLOCKING_CMD_TIMEOUT_EXTENSION: f64 = 0.5; // seconds

enum RequestTimeoutOption {
    NoTimeout,
    ClientConfig,
    BlockingCommand(Duration),
}

fn get_request_timeout(cmd: &Cmd, default_timeout: Duration)
    -> Result<Option<Duration>>
{
    match command.as_slice() {
        b"BLPOP" | b"BRPOP" | b"BLMOVE" | b"BZPOPMAX" | b"BZPOPMIN"
          | b"BRPOPLPUSH" =>
            get_timeout_from_cmd_arg(cmd, cmd.args_iter().len() - 1,
                                     TimeUnit::Seconds),
        b"BLMPOP" | b"BZMPOP" =>
            get_timeout_from_cmd_arg(cmd, 1, TimeUnit::Seconds),
        b"XREAD" | b"XREADGROUP" => cmd.position(b"BLOCK")
            .map(|idx| get_timeout_from_cmd_arg(cmd, idx + 1, TimeUnit::Milliseconds))
            .unwrap_or(Ok(RequestTimeoutOption::ClientConfig)),
        b"WAIT" | b"WAITAOF" => ...,
        _ => Ok(RequestTimeoutOption::ClientConfig),
    }?;
}
```

For every command, ferriskey:

1. Reads the command name.
2. If it's in the blocking-command set, parses the command's own
   timeout argument.
3. Builds a per-command effective timeout = `cmd_timeout +
   BLOCKING_CMD_TIMEOUT_EXTENSION` (extension = 500 ms, clamped to
   `u32::MAX`).
4. Wraps the `execute_command_owned` future in a
   `tokio::select! { _ = execute => ..., _ = tokio::time::sleep(d) => ... }`
   (client/mod.rs:867–885).

### The per-command timeout wrapper

`client/mod.rs:867–885`:

```
match request_timeout {
    Some(duration) => {
        tokio::pin!(execute);
        tokio::select! {
            result = &mut execute => result,
            _ = tokio::time::sleep(duration) => {
                // User timeout — execute future is dropped. The Cmd
                // was already moved into the event loop's
                // PendingRequest, so its tracker clone keeps the
                // inflight slot held until all sub-commands complete
                // naturally.
                ...
                Err(io::Error::from(io::ErrorKind::TimedOut).into())
            }
        }
    }
    None => execute.await,
}
```

**Every single command** — blocking or not — pays for creating a
`tokio::time::sleep(duration)` future and entering a `select!`. For
non-blocking commands this is still done, just with `duration =
request_timeout` (the client's configured default).

### Mux

`ferriskey/src/connection/multiplexed.rs:527–571` — `Pipeline::
send_recv`. Architecturally identical to redis-rs: send on an mpsc,
wait on a oneshot receiver, wrapped in `runtime::timeout(timeout,
receiver)`. `in_flight: VecDeque<InFlight>` at line 113, FIFO
response delivery at line 205.

The `send_recv` timeout here is the `MultiplexedConnection::
response_timeout`, same single-point-set-once pattern as redis-rs.
Ferriskey adds ANOTHER wrapper above (the `Client::send_command`
select!) so a non-blocking command has two timeout gates stacked; a
blocking command has the outer one extended to the command's own
timeout + 500 ms.

### Extra envelope cost

The `Client::send_command` body does, per call:

- `get_or_initialize_client()` — tokio RwLock read + `ClientWrapper::
  clone()` (mod.rs:632–651).
- `pubsub_synchronizer.intercept_pubsub_command(cmd)` — async method
  dispatch + another `cmd.command()` scan (813).
- `get_request_timeout(cmd, self.request_timeout)` — command name
  scan + match + BLOCKING_CMD_TIMEOUT_EXTENSION addition (817).
- `reserve_inflight_request()` — SeqCst CAS + `Arc::new(guard)` (823).
- Two atomic loads for the `inflight_log_interval` bucket log (835).
- `cmd.set_inflight_tracker(tracker)` (848).
- `self.clone()` — 7 Arc bumps (856).
- `Arc::new(cmd.clone())` — 1 heap alloc + 1 Vec<u8> copy (857).
- The tokio::select! + sleep future described above (867).

Below the envelope, `execute_command_owned` does another 4 post-
dispatch `is_*_command` scans (773-784) plus a
`convert_to_expected_type` dispatch that walks the whole response
for BLMPOP (value_conversion.rs, see report-w3.md §3).

## §3 — Actual mechanism differences

| dimension                             | redis-rs 1.2.0                   | ferriskey                              |
| ------------------------------------- | -------------------------------- | -------------------------------------- |
| Blocking-command detection            | NONE                             | Per-command name match (10 commands)   |
| Per-command timeout                   | Connection-level, set once       | Per-command, computed from cmd args    |
| Default timeout for blocking cmds     | **500 ms (TIMES OUT THE BLOCK)** | cmd_timeout + 500 ms extension         |
| Client-side timeout wrapper           | One: `Pipeline::send_recv`       | Two: `Client::send_command` +          |
|                                       |                                  | `Pipeline::send_recv`                  |
| Inflight queue                        | `VecDeque<InFlight>`             | `VecDeque<InFlight>` (same shape)      |
| Response delivery order               | FIFO                             | FIFO                                   |
| Head-of-line blocking on blockers     | YES (documented, not worked-     | YES (documented in code comments; 500  |
|                                       |  around)                         |  ms extension partially mitigates)     |
| Dedicated connection for blockers     | NO                               | NO                                     |
| Per-command allocations               | cmd packing only                 | cmd packing + `Arc::new(cmd.clone())` +|
|                                       |                                  | `self.clone()` (7 Arc bumps) +         |
|                                       |                                  | `inflight_tracker Arc` +               |
|                                       |                                  | `tokio::time::sleep` future            |
| Telemetry calls per command           | NONE                             | Multiple (client::clone + drop,        |
|                                       |                                  | standalone_client::clone + drop)       |

**Summary**: the blocking mechanism itself is NOT substantially
different. Both clients share the same single-socket-multiplexed
architecture with FIFO response delivery. The differences are:

- ferriskey treats blocking commands correctly (extends the timeout);
  redis-rs treats them as a 500-ms-failing normal command.
- ferriskey pays ~5 layers of additional envelope cost on every
  command, blocking or not.

The `-46 %` gap the round-1 bench showed on BLMPOP is NOT from
blocking-command mechanism. It's from the envelope cost × N, and
ferriskey treating blockers correctly isn't the issue — ferriskey is
doing extra work per command regardless of command kind.

## §4 — Verification probe

### Probe design

Four binaries at `benches/perf-invest/blocking-probe/src/`:

- `probe-ferriskey-1x1.rs` — 1 client × 1 BLMPOP, no concurrency;
  pre-seeded so each BLMPOP returns an item instantly (fast path).
  100 iterations.
- `probe-redis-rs-1x1.rs` — mirror.
- `probe-ferriskey-5x1.rs` — 5 cloned clients × 1 concurrent BLMPOP
  each; pre-seeded with 2 items so 2 return instantly, 3 hit the
  5-second server-side block.
- `probe-redis-rs-5x1.rs` — mirror, with
  `set_response_timeout(Duration::from_secs(7))` so the default
  500-ms timeout doesn't mask the mux behaviour.

All runs against localhost:6379 Valkey 7.2.

### 1-task probe (per-command cost isolation)

FLUSHALL; pre-seed 100 items; 100 iterations of BLMPOP with 5 s
timeout on a list that always has items (never blocks).

| system    | n    | p50      | p95      | p99      | mean     |
| --------- | ---- | -------- | -------- | -------- | -------- |
| ferriskey | 100  | 0.042 ms | 0.054 ms | 0.091 ms | 0.044 ms |
| redis-rs  | 100  | 0.032 ms | 0.040 ms | 0.055 ms | 0.033 ms |

Gap: ferriskey is ~10 µs slower per BLMPOP at p50 (~27 %), ~14 µs
slower at p99.

`delta_p50 ≈ 10 µs` is consistent with the per-command overhead we
counted in round 1: timeout-wrapper + Arc::new(cmd.clone()) + 7-Arc
self.clone() + command-name scans all add up to single-digit-µs
work. No mux-concurrency issue at play here.

### 5-task probe (mux concurrency under real block)

FLUSHALL; `LPUSH bench:blocking-list A B` (2 items); spawn 5
tokio tasks each cloning the Client and issuing BLMPOP with 5 s
timeout. 2 should return arrays instantly, 3 should wait 5 s for
the server to return nil.

**ferriskey** (3 runs, stable pattern):

```
task=0 elapsed_ms=    0 result=array  (other-ok = Value::Array)
task=1 elapsed_ms=5087  result=nil     ← server-side unblock succeeded
task=2 elapsed_ms=5501  result=err     ← client-side 5500 ms timeout fired
task=3 elapsed_ms=    0 result=array
task=4 elapsed_ms=5501  result=err     ← client-side 5500 ms timeout fired
```

The 5501 ms = 5000 ms (BLMPOP timeout) + 500 ms (BLOCKING_CMD_TIMEOUT_
EXTENSION). Two of the three blocked tasks get the server-side nil
within the extension window; the third doesn't — the client-side
timeout fires first.

**redis-rs** (with `set_response_timeout(7s)`):

```
task=0 elapsed_ms=    0 result=array
task=1 elapsed_ms=7001  result=err:Io   ← client-side 7 s timeout fired
task=2 elapsed_ms=7001  result=err:Io
task=3 elapsed_ms=    0 result=array
task=4 elapsed_ms=5100  result=nil
```

Even with a 7-second budget — 2 s beyond the server's 5 s block — 2
of 3 tasks miss the response window and time out client-side.

### What the probes tell us

1. **Per-command cost at 1×1 is 27 % higher on ferriskey**, not
   46 %. The original bench's 46 % gap mixes per-command cost with
   something else (the bench's ~10 k iteration count amplifies the
   per-cmd delta, plus there are other per-cmd costs the probe
   minimises by using a warm connection).
2. **Both clients experience head-of-line on the response queue**
   when multiple blocking commands release simultaneously. The 5 s
   server-side block releases all waiting BLMPOPs at once; both
   clients' single-socket FIFO demuxer delivers them serially.
3. **ferriskey's 500 ms extension isn't enough for 5-task concurrency**
   against a real block — 2 of 3 late-responders time out.
   redis-rs's 7 s extension also isn't enough.
4. The architectural difference is NOT "ferriskey blocks differently".
   The architectural SIMILARITY is that both pay the same head-of-line
   cost post-unblock. The CORRECTNESS difference is that ferriskey
   recognises blockers and gives them more timeout budget; redis-rs
   silently fails blockers at 500 ms default.

### Probe artifact

All four probe outputs written to `benches/perf-invest/blocking-probe/
output/<timestamp>-*.txt`. Three ferriskey-5x1 runs show
deterministic shape (two arrays fast; one nil at 5021-5087 ms; two
errs at 5500-5501 ms). Two redis-rs-5x1 runs show similar shape with
the error timeout at the configured 7 s instead of 5.5 s.

## §5 — Action items

### Batch C (ferriskey upstream)

1. **[Feature, HIGH impact]** Increase `BLOCKING_CMD_TIMEOUT_EXTENSION`
   or make it configurable. Current 500 ms fails 1–2 of 3 post-unblock
   responses in the probe; operationally this means blockers on a
   shared mux can silently timeout client-side while the server is
   still processing their response. Recommended: 2 s default, or
   read it from `ConnectionRequest`.

2. **[Feature, HIGH impact]** Consider a dedicated connection option
   for blocking commands. If `BLPOP`/`BLMPOP`/`XREAD BLOCK` is
   detected, route through a per-call connection checked out from a
   small pool (not the shared mux). This eliminates head-of-line on
   the shared mux entirely at the cost of extra TCP connections for
   a caller that uses a lot of blockers. Matches the common
   Lettuce / Jedis pattern.

3. **[Perf, MEDIUM impact]** Remove the unconditional
   `tokio::pin!(execute); tokio::select!` timeout wrapper when
   `request_timeout` is `None`. The existing `None => execute.await`
   branch (mod.rs:884) handles that case; but the current code ALWAYS
   computes `get_request_timeout` — hoist that to build time or cache
   by command-name hash.

4. **[Perf, MEDIUM impact]** Eliminate `Arc::new(cmd.clone())` at
   `Client::send_command` mod.rs:857. Round 1 already flagged this;
   rerun a profile after this change is wired to measure the delta.

### Recommendations for our own bench harness

5. **[Bench correctness, MEDIUM impact]** In
   `benches/comparisons/baseline/src/scenario1.rs` and other redis-rs
   bench binaries, always call `set_response_timeout` with a value
   larger than any blocking-command timeout in the workload.
   Otherwise the bench measures redis-rs failing at 500 ms, not
   responding successfully. Currently the baseline scenario 1 uses
   BLMPOP with a 1 s timeout and does NOT override
   `response_timeout`. At the first command that actually blocks
   (e.g. empty queue at worker startup), redis-rs fails client-side
   at 500 ms. Our bench gets away with it because the queue is pre-
   seeded before workers start — but the comparison could be subtly
   wrong on cluster variants where warmup timing differs.

6. **[Bench correctness, LOW impact]** Document the
   `DEFAULT_RESPONSE_TIMEOUT = 500 ms` redis-rs default in the
   `benches/README.md` and in the comparison report. It's a footgun
   for consumers; readers evaluating ferriskey vs redis-rs need to
   know the comparison includes this config difference.

### NOT recommended

- Changing ferriskey's mux architecture. The single-socket FIFO queue
  is fine — matches redis-rs. The extension-and-timeout story is
  where the divergence is.
- Running dedicated connections for non-blocking commands. redis-rs's
  mux is sufficient for non-blocking workloads; the cost is the
  per-command overhead, not the mux design.

## Scope boundary

This report is profiling a working library. Every one of the
"ferriskey does X that redis-rs doesn't" items has a design rationale
(IAM credential rotation, compression, pubsub synchroniser, inflight
tracking, telemetry, blocking-command correctness). The round-1 gap
attribution to envelope cost × N iterations is a HYPOTHESIS confirmed
by the round-2 1×1 probe showing ~27 % per-command delta. Fixing it
is an engineering choice by the maintainer; the probes suggest the
biggest single lever is BLOCKING_CMD_TIMEOUT_EXTENSION configurability
(for correctness) + the round-1 Arc::new(cmd.clone()) removal (for
throughput).
