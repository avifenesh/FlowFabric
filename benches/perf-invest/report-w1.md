# ferriskey vs redis-rs — flame-graph analysis (W1)

**Author:** Worker-1
**Branch:** `feat/ferriskey-perf-invest`
**Capture:** `benches/perf-invest/20260418-024349/`
**Scope:** Phase 1 flame graphs (this report). No ferriskey / redis-rs
code changes in this PR — recommendations only.

## Host and target

- AMD EPYC 9R14, 16 cores, Linux 6.8.0-1031-aws
- Valkey 7.2.12 standalone, localhost:6379
- redis-rs 1.2.0 (pinned in `benches/comparisons/baseline/Cargo.lock`)
- ferriskey HEAD of this branch (vendored in `ferriskey/`)

Both binaries built with `[profile.perf] inherits = "release", debug = 1`
so symbols resolve in the flame graph without changing codegen.

## Measured throughput at capture

| system    | throughput    | p99      |
| --------- | ------------- | -------- |
| redis-rs  | 45 938 ops/s  | 0.39 ms  |
| ferriskey | 37 284 ops/s  | 0.38 ms  |

delta ≈ 19 % on this run. The top-level briefing's 45 % number was a
10-k capture on a cold Valkey; on the 100-k warm capture the throughput
curve sits higher for both and the gap compresses. The analysis below
is independent of the absolute numbers — it's about which functions
appear on ferriskey's stacks that don't appear on redis-rs's.

## Flame graphs

- [redis-rs.svg](20260418-024349/redis-rs.svg)
- [ferriskey.svg](20260418-024349/ferriskey.svg)

Both are in the standard inferno folded-stack format (rectangles wider
= more samples land there; vertical = call depth). Open in a browser;
click a frame to zoom.

## Top divergent hot functions

Methodology: folded stacks → for every function `f` present in a stack,
count the sum of sample weights of stacks that include `f`. That is
inclusive (cumulative) time: a function that calls something expensive
gets the full inclusive cost. I also computed leaf-self time separately
as a sanity check, but leaf self-time is dominated by `[unknown]`
(unwind through inlined tokio) at 63–65 % in BOTH captures, so it is
not useful for comparing the two implementations. Inclusive time is.

Numbers below are `inclusive samples on that frame` divided by
`total samples`. They over-lap by design — a stack that touches five
frames contributes to all five.

### H1 — `Client::send_command` dispatch envelope is 5 async frames deep

| layer                                                                          | ferriskey inclusive | redis-rs counterpart                          |
| ------------------------------------------------------------------------------ | ------------------- | --------------------------------------------- |
| `ferriskey::client::Client::send_command` (outer wrapper)                      | **16.9 %**          | — (no equivalent outer wrapper)               |
| `ferriskey::client::Client::send_command::closure::closure` (inner async body) | 11.8 %              | —                                             |
| `StandaloneClient::send_command`                                               | 8.0 %               | —                                             |
| `StandaloneClient::send_request_to_single_node`                                | 7.5 %               | —                                             |
| `StandaloneClient::send_request`                                               | 7.3 %               | —                                             |
| `MultiplexedConnection::send_packed_command` (ferriskey)                       | 5.8 %               | `redis::aio::multiplexed_connection::Pipeline::send_recv` — **6.6 %** |

redis-rs's equivalent envelope is essentially flat: `Cmd::query_async`
→ `MultiplexedConnection::send_packed_command` (`redis-1.2.0/src/aio/
multiplexed_connection.rs:734`) → `Pipeline::send_recv`. Three frames;
`send_packed_command` body has one small `if let` cache-aio branch
then a single tail call into `pipeline.send_recv`.

ferriskey's equivalent has **five** async frames between `Client::
send_command` and the multiplexed connection write. Each one is a
separate `async fn` = separate state machine = extra poll() invocations
= extra futures scaffolding. The 16.9 % at the outer wrapper is the
union of everything below it; each intermediate layer drops ~1–2 %
of its own state-machine overhead.

This matches W3's F1/F2/F3/F4/F5/F8/F9 independently — the source
reading predicted "envelope is wider"; the flame graph confirms it
costs ~5 % to `MultiplexedConnection::send_packed_command` in
ferriskey vs ~6.6 % to `Pipeline::send_recv` in redis-rs, **with an
additional 11 percentage points above it** (16.9 − 5.8) that's pure
envelope.

### H2 — `get_or_initialize_client` takes a tokio RwLock + clones a wrapper per call

File: `ferriskey/src/client/mod.rs:632-651`

```rust
async fn get_or_initialize_client(&self) -> Result<ClientWrapper> {
    {
        let guard = self.internal_client.read().await;  // (1) tokio RwLock read
        if !matches!(&*guard, ClientWrapper::Lazy(_)) {
            return Ok(guard.clone());                    // (2) enum clone, see below
        }
    }
    // ... lazy-init path, cold after first call ...
}
```

ferriskey inclusive on this path:

| frame                                                              | %       |
| ------------------------------------------------------------------ | ------- |
| `Client::get_or_initialize_client`                                 | 0.46 %  |
| `tokio::sync::rwlock::RwLock<T>::read`                             | 0.32 %  |
| `tokio::sync::rwlock::RwLock<T>::read::closure::closure`           | 0.27 %  |

The lock itself isn't the story — the lock contends on 16 workers
hitting the same `Arc<RwLock<ClientWrapper>>`, and tokio's read
RwLock is cheap in the uncontended case. The interesting bit is the
`guard.clone()` on L636: `ClientWrapper` is `#[derive(Clone)]` so the
enum-body `StandaloneClient` is cloned. `StandaloneClient` is itself
`#[derive(Clone)]` with `inner: Arc<DropWrapper>` (ferriskey/src/
client/standalone_client.rs:57-60) — so the clone is an Arc bump, not
a deep copy. **BUT** `StandaloneClient` has an `impl Drop` that calls
`Telemetry::decr_total_clients(1)` (line 62-70), which takes a
`std::sync::RwLock::write` guard on `telemetrylib::Telemetry`.

redis-rs has no equivalent: you construct a `MultiplexedConnection`
once and hand it a `&mut self` on each `send_packed_command`. Nothing
to clone, nothing to drop, no telemetry write-lock.

ferriskey inclusive on that path:

| frame                                                              | %       |
| ------------------------------------------------------------------ | ------- |
| `StandaloneClient as Drop::drop` (via `get_or_initialize_client`)  | 0.19 %  |
| `std::sync::poison::rwlock::RwLock<T>::write` (Telemetry lock)     | 0.12 %  |
| `Telemetry::decr_total_clients`                                    | 0.21 %  |

The individual numbers are small, but they're unconditional and on
the hot path of every command. Cross-check: W3 flagged
`telemetrylib::Telemetry::decr_total_clients` in §2.3 table row F1
via `Client::clone`, for the same reason — a cloned
`Arc<dyn PubSubSynchronizer>`'s drop takes a Telemetry write lock.

### H3 — `Arc::new(cmd.clone())` inside `send_command`

File: `ferriskey/src/client/mod.rs:857`

```rust
let self_clone = self.clone();
let owned_cmd = Arc::new(cmd.clone());  // (!)
```

ferriskey inclusive:

| frame                                 | %      |
| ------------------------------------- | ------ |
| `alloc::sync::Arc<T>::new`            | 1.14 % |
| `<Arc<T,A> as Drop>::drop`            | 1.40 % |
| `Arc<T,A>::drop_slow`                 | 0.76 % |
| (plus mimalloc's `_mi_os_purge_ex` →  |        |
|  `madvise` syscalls under drop_slow)  | 0.9 %  |

The fact that `Arc::drop_slow` fires at a measurable rate tells us the
refcount reaches zero on most commands — so the Arc wrapper is NOT
shared across threads; it's created, used once, dropped. A fresh heap
allocation and deallocation per command — including the 4 KiB
payload's `Vec<u8>` being held inside the `Cmd` clone.

redis-rs doesn't do this. `Cmd::query_async` calls `self.get_packed_
command()` which packs the command into a `Vec<u8>` ONCE, then the
`Cmd` itself is borrowed (`&Cmd`) into `Pipeline::send_recv`. No
`Arc`, no extra clone, no per-command heap churn.

Cross-check: W3 flagged this identically in §2.3 row F2.

### H4 — Command packing + encoding: similar shape, but an extra hop

| frame (ferriskey)                                                         | %      |
| ------------------------------------------------------------------------- | ------ |
| `Cmd::get_packed_command`                                                 | 1.58 % |
| `Cmd::write_packed_command`                                               | 1.60 % |
| `write_command_to_vec`                                                    | 1.51 % |
| `write_command`                                                           | 1.39 % |

| frame (redis-rs)                                                          | %      |
| ------------------------------------------------------------------------- | ------ |
| `redis::cmd::Cmd::get_packed_command`                                     | 1.92 % |

`Cmd::get_packed_command` is comparable in cost (1.58 % vs 1.92 %) —
that part of the pipeline is solid. The extra ferriskey hops
`write_packed_command` / `write_command_to_vec` / `write_command` are
roughly free in absolute terms but still add poll-frame overhead. Not
a root cause; the difference at this layer is within the noise band.

### H5 — Response parser: equal-weight, different crate

| frame (redis-rs)                                                          | %      |
| ------------------------------------------------------------------------- | ------ |
| `combine::parser::sequence::ThenPartial::parse_mode_impl`                 | 6.4 %  |
| `Pipeline::send_recv::closure`                                            | 6.6 %  |

| frame (ferriskey)                                                         | %      |
| ------------------------------------------------------------------------- | ------ |
| `MultiplexedConnection::send_packed_command::closure`                     | 5.8 %  |
| (parser frames don't aggregate cleanly via `combine::`; ferriskey hand-   | —      |
|  rolls its own RESP parser)                                               |        |

The parser is not the differentiator. Both libraries spend ~6 % of
inclusive time in response parsing; ferriskey does it slightly faster
in absolute terms but the difference is not on the ~19 % throughput
gap's critical path.

## Actionable optimisations — not implemented; recommendations only

Ranked by "expected throughput uplift / complexity to change":

1. **Eliminate `Arc::new(cmd.clone())` on every `send_command`.**
   File: `ferriskey/src/client/mod.rs:857`. Either (a) borrow the
   `Cmd` through to the event-loop's pending-request queue
   (complicated — the Arc was added for timeout-drop semantics), or
   (b) keep the Arc but hand a `&Arc<Cmd>` down so only the first
   call pays, subsequent clones are pure refcount bumps. Highest
   impact: removes ~2.5 % inclusive + the mimalloc `madvise` syscalls.

2. **Flatten the dispatch envelope.**
   Merge `Client::send_command` → `execute_command_owned` →
   `StandaloneClient::send_command` → `send_request_to_single_node`
   → `send_request` → `MultiplexedConnection::send_packed_command`
   into fewer `async fn`s. Each layer is a separate state machine; 5
   deep is at least 4 too many. redis-rs runs on 2. Expected ~5 %
   uplift from fewer poll() calls, lower cache pressure on the state
   machine futures.

3. **Move telemetry counter out of the clone/drop path.**
   File: `ferriskey/src/client/standalone_client.rs:62-70`. Each
   `StandaloneClient::Drop` takes a std::RwLock::write on
   `telemetrylib::Telemetry`. Either (a) replace the RwLock with an
   `AtomicI64` + hash-table-keyed-by-client-id slow path, or (b)
   only register/deregister at `StandaloneClient::create_client` /
   the final `DropWrapper::drop` (the Arc-owning one). Today every
   cloned-then-dropped `StandaloneClient` touches the lock.

4. **Avoid cloning `ClientWrapper` on every `get_or_initialize_client`
   happy-path return.**
   File: `ferriskey/src/client/mod.rs:636` — `return Ok(guard.clone());`.
   Returning the enum by value forces the `StandaloneClient` inside
   to clone (= Arc bump + subsequent Drop = Telemetry lock touch). A
   different shape — e.g. `async fn with_client<F, T>(&self, f: F) -> Result<T>`
   that invokes `f` while holding the read guard — avoids the clone
   entirely. This composes with recommendation 3: if the Telemetry
   counter lives only in `DropWrapper`, the per-clone Drop cost
   disappears.

5. **Hoist `get_request_timeout` / `is_*_command` command-name scans.**
   Cross-reference W3 §2.3 F5 and the four post-dispatch
   `is_{client_set_name,select,auth,hello}_command` calls. Each scans
   the cmd name again via `Routable::command()` → uppercase Vec<u8>
   alloc. Store the command name once when the Cmd is built; re-use.

Items 1, 2, and 3 are the most load-bearing — each of them stops
the bleeding at a different part of the stack. Item 4 is cheap to
do alongside item 3. Item 5 is lower-impact per-call but adds up.

## Cross-check vs W3's source-level reading

W3's report-w3.md §1 predicted 8 contributors in rough rank:
`Routable::command()` Vec<u8> alloc (1), `Client::clone` (2),
`Arc::new(cmd.clone())` (3), `InflightRequestTracker` CAS+alloc (4),
`pubsub_synchronizer.intercept_pubsub_command` (5), per-command
timeout `tokio::select!` (6), `expected_type_for_cmd` scan (7), four
post-dispatch `is_*_command` scans (8).

What the flame graph confirms: (2) `Client::clone` and (3) `Arc::new
(cmd.clone())` are visible hot frames. (4) `InflightRequestTracker`
doesn't surface as a distinct frame >1 %, but its atomic traffic
contributes to the `core::sync::atomic::atomic_add` leaf-self bucket
at 1.0 %. (6) the per-command timeout future does not appear on a
hot stack — on a localhost workload with no timeout set, tokio's
select!-vs-sleep compiles down to a trivial pin+poll.

What the flame graph adds: the **5-deep async dispatch envelope** is
the single biggest contributor the source reading alone doesn't make
obvious. W3 counted individual costs; the flame graph shows they're
compounded by per-layer future overhead.

## Things W1 did NOT look at

- Per-command `OTel` frames never surface >0.2 % — FerrisKeyOtel is
  cheap when no OTel exporter is configured (our bench case).
- Reconnecting-connection heartbeat: not visible in the profile;
  not triggered under scenario-1-shape (workers never disconnect).
- Value conversion (`convert_to_expected_type`): W3 §3 calls it out
  for BLMPOP specifically. RPUSH/INCR return integers where the
  function is a no-op and doesn't show up hot.

If W2's wider workload set (GET/SET/HGET/etc) shifts these numbers,
re-run this analysis; the methodology transfers directly.

## Proviso

This is a report from profiling a working library whose maintainer
owns the upstream project. Recommendations are engineering
observations, not a critique. Every item listed above traces to a
real-world constraint (telemetry, lazy initialization, timeout
plumbing, inflight tracking) that redis-rs doesn't carry. The gap is
the price of those features; trimming it is a design decision for
the maintainer.
