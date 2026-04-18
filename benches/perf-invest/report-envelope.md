# ferriskey envelope after round 3 — flame comparison (W1, Track A)

**Author:** Worker-1
**Branch:** `feat/ferriskey-iam-gate`
**Capture:** `benches/perf-invest/envelope-probe/20260418-075136/`
**Scope:** Track-A single-client INCR loop. Identify which of the
previously-listed envelope hot frames still dominate after round 3,
which have shrunk, and which should be the next targets.

## TL;DR

On a single-client INCR loop (the tightest per-command-envelope
workload), post-round-3 ferriskey is **at parity** with redis-rs
1.2.0 — p50 31 µs on both (without perf attached). Round 3's work
removed or materially shrank most of the round-1 hot frames. The
remaining outer-envelope cost is a ~3-percentage-point wrapper
thickness at the very top of the call chain (`CommandBuilder::execute`
+ `Client::send_command`), rooted in the 5-async-frame dispatch that
still carries through to `MultiplexedConnection::send_packed_command`.

The 45% BLMPOP gap is **NOT** per-command envelope — on a non-
blocking scalar-reply command the envelope gap is ~3%. Whatever the
BLMPOP benchmark is measuring is workload-shape-specific (mux HOL on
the post-unblock response delivery; ref round-2 probe results), not
carried by the envelope.

## Measured

Host: AMD EPYC 9R14, 16 cores, Linux 6.8.0-1031-aws. Valkey 7.2.12
standalone localhost:6379. Branch HEAD `0181f30`.

Workload: 1 client × 1 worker × 100 000 × `INCR bench:envelope-counter`,
1 000-iter warmup discarded. Identical shape on both sides.

Without perf sampler:

| system    | p50      | p95      | p99      | p99.9    |
| --------- | -------- | -------- | -------- | -------- |
| ferriskey | 0.031 ms | 0.038 ms | 0.045 ms | 0.073 ms |
| redis-rs  | 0.031 ms | 0.039 ms | 0.045 ms | 0.069 ms |

p50 **tied at 31 µs**. Tail (p99.9) ~4 µs wider on ferriskey.

With perf sampler attached (DWARF call-graph adds ~30 µs per call):

| system    | p50      | p95      | p99      | p99.9    |
| --------- | -------- | -------- | -------- | -------- |
| ferriskey | 0.058 ms | 0.077 ms | 0.089 ms | 0.140 ms |
| redis-rs  | 0.056 ms | 0.074 ms | 0.082 ms | 0.123 ms |

The flame analysis below uses the perf-attached captures.

## Top ferriskey frames by inclusive time

Inclusive time = samples whose stack passed through this frame, as %
of total samples (9.84 B samples on ferriskey; 9.2 B on redis-rs).
Only library-relevant frames shown; tokio/libc/unknown noise elided.

| % inclusive | frame                                                                 |
| ----------: | :-------------------------------------------------------------------- |
|      30.94% | `PipelineSink<T> as Sink::start_send` (mux producer side)             |
|      12.25% | `CommandBuilder::execute::closure` (outer user-facing wrapper)        |
|      11.36% | `Instrumented<T>::poll` (tracing span wrapper)                        |
|      11.34% | `Client::send_command::closure` (inner async body)                    |
|      10.82% | `PipelineSink<T>::poll_read` (mux consumer side)                      |
|       9.39% | `Client::send_command::closure::closure`                              |
|       9.06% | `Client::execute_command_owned::closure`                              |
|       8.54% | `StandaloneClient::send_command::closure`                             |
|       8.47% | `StandaloneClient::send_request_to_single_node::closure`              |
|       8.46% | `StandaloneClient::send_request::closure`                             |
|       7.75% | `MultiplexedConnection::send_packed_command::closure`                 |
|       7.27% | `Pipeline::send_single::closure`                                      |
|       7.18% | `Pipeline::send_recv::closure`                                        |
|       4.19% | `PipelineSink<T>::send_result` (response handoff to oneshot)          |
|       1.20% | `connection::runtime::timeout::closure`                               |
|       1.05% | `Pipeline::new::closure`                                              |
|       0.61% | `Arc::<T>::new`                                                       |
|       0.57% | `tokio::time::timeout::Timeout::poll`                                 |
|       0.53% | `<Arc<T,A> as Clone>::clone`                                          |
|       0.50% | `<Arc<T,A> as Drop>::drop`                                            |
|       0.47% | `tokio::time::timeout::timeout`                                       |
|       0.46% | `Cmd::get_packed_command`                                             |
|       0.41% | `Cmd::write_packed_command`                                           |
|       0.41% | `write_command_to_vec`                                                |
|       0.41% | `ReconnectingConnection::get_connection::closure`                     |
|       0.34% | `<Client as Clone>::clone`                                            |
|       0.14% | `Routable::command`                                                   |
|       0.04% | `String::from_utf8_lossy`                                             |

## Top redis-rs frames by inclusive time

| % inclusive | frame                                                                 |
| ----------: | :-------------------------------------------------------------------- |
|      33.37% | `PipelineSink<T> as Sink::start_send` (mux producer side)             |
|      11.10% | `PipelineSink<T>::poll_read` (mux consumer side)                      |
|       9.25% | `Cmd::query_async::closure` (outer user-facing wrapper)               |
|       8.02% | `MultiplexedConnection::req_packed_command::closure`                  |
|       7.95% | `MultiplexedConnection::send_packed_command::closure`                 |
|       7.76% | `Pipeline::send_recv::closure`                                        |
|       7.18% | `Runtime::timeout::closure`                                           |
|       6.15% | `Pipeline::send_recv::closure::closure`                               |
|       3.91% | `PipelineSink<T>::send_result` (response handoff to oneshot)          |
|       1.41% | `ValueCodec::decode` / `decode_stream`                                |
|       1.22% | `combine::parser::Parser::parse_with_state`                           |
|       0.36% | `Cmd::arg` / `<&str as ToRedisArgs>::write_redis_args`                |

## Per-item audit of round-1 envelope frames

### Item 1 — Arc::clone chain inside Client::send_command (round-1 noted "× 5")

**Status:** shrunk dramatically, at the noise floor.

- Round-1 ferriskey.folded: `<Client as Clone>::clone` was one of the
  top allocation-churn frames (round-1 report-w1.md H2 + F1, ~7 Arc
  bumps per command inclusive ~0.5%).
- Post-round-3 ferriskey.folded: `<Client as Clone>::clone` 0.34%,
  `<Arc<T,A> as Clone>::clone` 0.53%. Roughly unchanged in %, but the
  absolute cost dropped because the Client struct is smaller (lazy
  arms removed) and the clone is rarer per call (the round-3 4-way-
  wrapper collapse means fewer Client::clone calls per send_command).
- **Dominant now:** No. It's a lower-tier frame.

### Item 2 — `Arc::new(cmd.clone())` at `client/mod.rs:857`

**Status:** not clearly visible as a hot frame anymore.

- Round-1: `Arc::new` 1.14% + `<Arc as Drop>::drop` 1.40% + `Arc::drop_
  slow` 0.76% with `mimalloc _mi_os_purge_ex → madvise` visible under
  drop_slow.
- Post-round-3: `Arc::new` 0.61%, `<Arc as Drop>::drop` 0.50%,
  `Arc::drop_slow` below 0.25%. No mimalloc madvise syscalls present
  in the hot stacks.
- **Shrunk:** ~50% smaller. The `Arc::new(cmd.clone())` site at
  client/mod.rs likely still exists but its cost is comparable to
  redis-rs's own `Cmd::query_async` wrapper allocation now. Round-3
  didn't remove it per se; the overall drop in the outer-envelope
  cost moved it off the top-5.
- **Dominant now:** No.

### Item 3 — 5-layer dispatch envelope (Client::send_command → … → MultiplexedConnection::send_packed_command)

**Status:** structurally unchanged — STILL 5 layers, still a sizeable
part of inclusive cost.

Current depth-of-stack ferriskey:
- `CommandBuilder::execute` 12.25%  (NEW outer wrapper added by
  round-3's typed Client API — the `client.cmd("INCR").arg(KEY).
  execute()` chain)
- `Client::send_command` 11.34%
- `Client::execute_command_owned` 9.06%
- `StandaloneClient::send_command` 8.54%
- `StandaloneClient::send_request_to_single_node` 8.47%
- `StandaloneClient::send_request` 8.46%
- `MultiplexedConnection::send_packed_command` 7.75%
- `Pipeline::send_single` 7.27%
- `Pipeline::send_recv` 7.18%

Comparable redis-rs depth:
- `Cmd::query_async` 9.25%
- `MultiplexedConnection::req_packed_command` 8.02%
- `MultiplexedConnection::send_packed_command` 7.95%
- `Pipeline::send_recv` 7.76%

ferriskey's outermost frame is 12.25% vs redis-rs 9.25% — a **~3
percentage point thicker envelope**. The delta comes from:

- One additional user-facing layer: `CommandBuilder::execute` (typed
  API added post-round-3). It's a clean builder → thin wrapper that
  calls into `Client::send_command`. Unlike round-1 this isn't
  redundant — it's a new ergonomic layer. But it's still a layer.
- `StandaloneClient::send_request_to_single_node → send_request` —
  two frames that, per source reading, could be collapsed (send_request
  is called from single-node only; it just adds a match on error
  kind). redis-rs doesn't have this split.
- `Pipeline::send_single` wraps `Pipeline::send_recv` with a
  `pipeline_response_count = None` argument. The wrap is a one-line
  function that forwards to send_recv; shouldn't be a separate
  async frame.

**Shrunk:** partially. The 5-layer count is the same, but W2's 4-way-
wrapper collapse work moved the boundary — now it's CommandBuilder
on top (new layer) + Client::send_command + execute_command_owned
+ StandaloneClient layer. The net top-to-bottom percentage is
slightly lower than round-1's 16.9% at the outer.

**Dominant now:** Yes. This is the #1 remaining envelope cost.

### Item 4 — InflightRequestTracker CAS

**Status:** effectively gone.

- Round-1: `InflightRequestTracker::try_new` SeqCst CAS + Arc::new(guard)
  per command; flagged by report-w3.md §2.3 F6 as non-zero cost.
- Post-round-3 folded:
  `drop_in_place<Option<InflightRequestTracker>>` 0.2%,
  `drop_in_place<InflightRequestTracker>` 0.2%.
  Construction paths not visible >0.1%.
- **Shrunk** ~80%. Either the round-3 work inlined the tracker so
  hot paths short-circuit when limit isn't binding, or the per-command
  cost is just lost in noise because the rest of the envelope
  shrunk proportionally. Either way, not a target.

### Item 5 — `run_with_timeout` / `tokio::select!` overhead

**Status:** shrunk.

- Round-1: `run_with_timeout` wrapped every call in
  `tokio::pin!(execute); tokio::select! { result, sleep }` paying
  for a `tokio::time::sleep` future per command.
- Post-round-3: `connection::runtime::timeout::closure` 1.20% inclusive,
  `tokio::time::timeout::Timeout::poll` 0.57%, `tokio::time::timeout`
  0.47%. The `tokio::time::sleep::Sleep::poll_elapsed` + `Clock::now`
  pair at 0.36% + 0.36% self-time are the timeout future's cost.
  Combined: ~2 percentage points of outer-envelope inclusive time is
  timeout machinery.
- Redis-rs does the same thing on its end: `Runtime::timeout::closure`
  7.18% inclusive (wrapping the whole send_recv). Redis-rs applies
  its timeout ONCE at the mux layer with `response_timeout`; ferriskey
  applies it once at `run_with_timeout` (the outer call-site wrapper
  checked at request_timeout resolution time). Rough parity.
- **Shrunk:** Yes, the round-3 W2 work reduced one level of
  timeout-wrapping redundancy. **Dominant now:** No.

### Item 6 — `Routable::command()` 6× + to_ascii_uppercase allocs

**Status:** almost entirely gone.

- Round-1: report-w3.md §F5/F10 identified ~6 per-command
  `Routable::command()` calls allocating a fresh uppercase `Vec<u8>`
  each time (for match scans on BLPOP / EVAL / …).
- Post-round-3: `Routable::command` 0.14% inclusive,
  `Routable::command::closure` 0.13%. One or two remaining call sites.
  No `to_ascii_uppercase` visible.
- **Shrunk** ~93% vs round-1. Round-3 either removed most call sites
  or cached the command-name lookup. **Dominant now:** No.

### NEW item — `Instrumented<T>::poll` (post-round-3 tracing span wrapper)

**Status:** 11.36% inclusive, 0.06% leaf self-time.

This is the tracing span wrapper introduced by the round-3 telemetry
redesign. Every `Client::send_command` call returns
`span.instrument(future)`, so the returned BoxFuture is wrapped in
`tracing::instrument::Instrumented<T>`. Its poll impl is:

```rust
let _guard = self.span.enter();  // no-op when no subscriber is interested
self.inner.poll(cx)
```

- **Inclusive 11.36%** = every ferriskey send_command stack passes
  through this frame. Not extra work by itself; just a wrapping layer.
- **Leaf 0.06%** = the actual self-time of Instrumented's poll body
  (the span enter + re-dispatch). Effectively free when no subscriber
  is attached.

Manager's suspicion about eager span construction — I confirm the
**span IS constructed on every call** (`tracing::debug_span!`
materializes `Span::current()` at the macro site, even when no
subscriber is registered). BUT the construction is cheap because:

- `Span::new` short-circuits via the dispatcher's `would_enabled`
  callsite cache when no interested subscriber exists → returns
  `Span::none()`.
- `debug_span!("ferriskey.send_command", command = %cmd_name, ...)`
  still evaluates the `command = %cmd_name` formatter eagerly.
  `cmd_name = String::from_utf8_lossy(v).into_owned()` allocates
  a String per call.
- That String allocation: `alloc::string::String::from_utf8_lossy`
  is 0.04% inclusive — single-digit microseconds at most.

**Verdict on the span wrapper:** not a hot path. The 11.36%
inclusive is just "this frame appears on every send_command stack" —
it's a wrapper, not a cost carrier. The 0.04% String allocation is
a small correctness wart (allocates even with no subscriber); worth
fixing but tiny.

### Other observations worth calling out

- **PipelineSink::start_send at ~31% and poll_read at ~11%** on both
  clients. These are the mpsc producer and the TCP reader loops.
  Ferriskey's 30.94% vs redis-rs 33.37% means ferriskey's is slightly
  lighter — probably due to fewer value-conversion frames below it.
  This is the real work; not envelope.
- **Parser**: redis-rs uses `combine` (1.4% inclusive on parse). Ferriskey
  hand-rolls its parser (0.48% inclusive on `ValueCodec::decode` +
  0.35% on `parse_int_value`). Ferriskey parses ~67% cheaper on a
  scalar int reply — this is a genuine round-1 advantage that's been
  preserved through all three rounds.

## Ranked list for the next prototype round

Ranked by "inclusive cost ferriskey pays that redis-rs doesn't" —
approximate throughput uplift from eliminating each.

### Tier 1 — outer envelope layer compression

**Target:** `CommandBuilder::execute → Client::send_command →
execute_command_owned` (3 async frames deep before the StandaloneClient
layer).

**Cost:** ferriskey 12.25% inclusive at the top vs redis-rs 9.25%
at the top. Delta ~3 percentage points.

**Mechanism:** each async frame is its own state machine; each
intermediate frame costs a poll() indirection + a small pin stack
setup. Collapsing `CommandBuilder::execute` to be a thin async fn
that inlines into `Client::send_command` (rather than a BoxFuture
returning wrapper) would save one async frame. Collapsing
`execute_command_owned` into `send_command`'s body (which was the
round-1 shape before W2 split it for the typed API) would save
another.

**Estimated uplift:** 2–3% throughput (~1 µs per call at 31 µs p50).
Not huge because each frame is already well-optimized; the win is
the state-machine overhead, not the work inside.

**Complexity:** medium. Round-3 introduced `CommandBuilder` for the
typed API, so this is a "rebase and flatten" job on top of that API.

### Tier 2 — StandaloneClient::send_request_to_single_node + send_request split

**Target:** collapse the two functions into one.

**Cost:** ferriskey has both at 8.47% and 8.46% inclusive — the split
is one poll indirection per call. Redis-rs has a single frame doing
the equivalent.

**Estimated uplift:** 0.5–1% throughput. Small.

**Complexity:** trivial (inline the nested function), modulo the
cluster code which also calls send_request directly.

### Tier 3 — Pipeline::send_single → Pipeline::send_recv wrapper

**Target:** inline `Pipeline::send_single` into `send_packed_command`.

**Cost:** `send_single` 7.27% vs `send_recv` 7.18% — they're back-to-
back forwarder frames.

**Estimated uplift:** 0.5% throughput.

**Complexity:** low (one-line forwarder), but `send_single` might
be used from other entry points in pubsub / cluster.

### Tier 4 — span eager String alloc in send_command

**Target:** `let command_name = cmd.command().map(|v| String::from_utf8_
lossy(&v).into_owned())...` at ferriskey/src/client/mod.rs:747.

**Cost:** 0.04% inclusive on `String::from_utf8_lossy`. Negligible
throughput.

**Estimated uplift:** <0.1% throughput. Listed only because it's a
"do the right thing" correctness issue: the span should be
constructed lazily behind `tracing::enabled!(Level::DEBUG)`. Round-3
work introduced this; minor tidy-up.

**Complexity:** trivial (wrap in `if tracing::enabled!(...) { ... }`
or use `tracing::dispatcher::get_default` to check interest first).

### NOT in the ranked list

- `Arc::new(cmd.clone())` — already shrunk to noise floor post-round-3.
- `Routable::command` × 6 — shrunk to 0.14% inclusive.
- `InflightRequestTracker` CAS — shrunk to 0.4% inclusive combined.
- `run_with_timeout` wrapping — ~1.2% inclusive, but redis-rs pays
  7.18% on its own `Runtime::timeout` wrapper, so ferriskey is
  actually cheaper here. Not a target.

## What we learned about the 45% BLMPOP gap

On this single-client INCR workload, ferriskey is at parity with
redis-rs. The 45% BLMPOP gap is NOT an envelope cost — it's a
BLMPOP-specific cost we already isolated in round-2 (HOL mux
serialization on post-unblock response delivery + client-side timeout
extension tuning).

**Action for that gap**: not envelope optimization. The round-2
BLMPOP findings (BLOCKING_CMD_TIMEOUT_EXTENSION + potential
dedicated-connection-for-blockers) remain the correct targets. Round-3's
`ClientBuilder::blocking_cmd_timeout_extension` made the extension
configurable but didn't change the default. The real fix is the
dedicated-connection architectural change ferriskey hasn't done.

## Proviso

This is a single-workload measurement. Different command shapes
(pipelining, GET with big values, MGET with many keys, XREAD streams)
will stress different parts of the envelope. The INCR probe isolates
the *minimum* envelope — anything workload-specific isn't visible.

Numbers are point-in-time on one host with the perf sampler running.
Run-to-run variance at this resolution (µs deltas) is wide; multiple
captures at different times would harden the conclusions.

## Artifacts

`benches/perf-invest/envelope-probe/20260418-075136/`:

- probe-ferriskey.svg / .folded / .perf.data.gz / .txt
- probe-redis-rs.svg / .folded / .perf.data.gz / .txt
- README.md — host / Valkey / SHA / capture commands
