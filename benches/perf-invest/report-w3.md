# ferriskey per-command hot path — code reading vs redis-rs 1.2.0

**Author:** Worker-3
**Branch:** `feat/ferriskey-perf-invest`
**Scope:** source-only analysis; no measurements, no code changes to
ferriskey or redis-rs. File:line citations against ferriskey HEAD of
this branch and redis-rs 1.2.0 as pinned in
`benches/comparisons/baseline/Cargo.lock` (the version the bench
actually runs).

## TL;DR

For a single RPUSH / INCR round-trip on the standalone path, ferriskey
runs a substantially wider envelope than redis-rs 1.2.0 — not because
of one headline cost, but because of **~8 extra steps that each
perform an allocation, a lock, a CAS, or a linear command-name scan**,
most called unconditionally regardless of command shape. For BLMPOP
(and every command listed in
`value_conversion::expected_type_for_cmd`, ~40 commands) there is an
additional post-response conversion stage that walks the whole
response once more.

Predicted hot-path contributors for the 45 % gap W1 will see in
flamegraphs, roughly ranked by likely cost on a command-that-returns-
an-integer (RPUSH / INCR):

1. **4–6 Vec<u8> allocations per call from repeated `Routable::command()`
   invocations.** Each one copies arg-zero and uppercases it.
   (§2.4, §2.5)
2. **`Client::clone()` once per command** — 5 `Arc::clone` atomic
   increments + trait-object clone on `pubsub_synchronizer`. (§2.3)
3. **`Arc::new(cmd.clone())` per command** — an additional allocation
   beyond the clone redis-rs pays. (§2.3)
4. **`InflightRequestTracker::try_new`** — SeqCst CAS loop + `Arc::new`
   of a guard per command, even when no limit is configured. (§2.3)
5. **`pubsub_synchronizer.intercept_pubsub_command` dispatch** — async
   method call + another `cmd.command()` alloc per command. (§2.3)
6. **Per-command response-timeout wrapper** (`tokio::select!` over
   `tokio::time::sleep`). redis-rs instead passes `Option<Duration>`
   down to the pipeline layer and calls `Runtime::timeout` there. (§2.3)
7. **`expected_type_for_cmd` linear-match dispatch + `cmd.command()`
   alloc** — pays even for RPUSH/INCR where the result is `None` and
   the conversion is a no-op. (§3, §4)
8. **Post-dispatch `is_client_set_name_command` / `is_select_command`
   / `is_auth_command` / `is_hello_command`** — 4 sequential
   `cmd.command()` calls after every command, each allocating a new
   uppercase Vec. (§2.3)

redis-rs's equivalent of the whole envelope — `Cmd::query_async` at
`redis-1.2.0/src/cmd.rs:651-657` — is **three lines**: one `await` on
`req_packed_command`, one `extract_error`, one `from_redis_value`.

If W1's flamegraph confirms the top buckets are
`Routable::command`, `Client::clone`, and `convert_to_expected_type`,
the reading below explains why. If W2's pipelined workload closes the
gap substantially, it's because the per-command overheads above are
amortised across the N commands in the pipeline (see §5).

**This is an investigative report. It is not a recommendation to
change ferriskey.** The project owner decides scope; the numbers in
§2 are static counts (branches, allocations, syncs) read from the
source, not profiler output.

## §1 — Executive summary

Things ferriskey does per command that redis-rs 1.2.0 does not:

| # | What | Where (ferriskey) | Plausible per-call cost |
|---|------|-------------------|-------------------------|
| F1 | `Client::clone()` at the top of every `execute` | `ferriskey_client.rs:364` | 5 × `Arc::clone` + `Arc<dyn PubSubSynchronizer>::clone` |
| F2 | `Arc::new(cmd.clone())` inside `send_command` | `client/mod.rs:857` | 1 heap alloc on top of the `Cmd::clone` both libs pay |
| F3 | IAM token changed-check | `client/mod.rs:795-809` | 1 `Option<Arc<T>>` load + `AtomicBool::load`; zero when IAM not configured (our bench case) |
| F4 | `pubsub_synchronizer.intercept_pubsub_command(cmd)` | `client/mod.rs:813` | async method call; inside: 1 × `cmd.command()` alloc (§2.4) |
| F5 | `get_request_timeout(cmd, self.request_timeout)` | `client/mod.rs:817` | `cmd.arg_idx(...)` scans for BLMPOP/BZMPOP/XREAD-style timeout args on every command |
| F6 | `reserve_inflight_request()` SeqCst CAS loop + `Arc::new(guard)` | `client/mod.rs:823-831` + `value.rs:2515-2529` | SeqCst CAS + Arc allocation even when the limit is not binding |
| F7 | `inflight_log_interval` bucket check | `client/mod.rs:836-846` | `AtomicIsize::load` + compute — cheap, but unconditional |
| F8 | `cmd.set_inflight_tracker(tracker)` | `client/mod.rs:848` | 1 field write on `Cmd` |
| F9 | Per-command `tokio::select!` timeout wrapper | `client/mod.rs:867-885` | `tokio::pin!` + `tokio::time::sleep` future creation per call; ~40 B on the heap |
| F10 | `expected_type_for_cmd(&cmd)` linear command-name match | `client/value_conversion.rs:1593-1870` | 1 × `cmd.command()` alloc + byte-slice match through ~40 arms |
| F11 | `convert_to_expected_type(value, expected)` dispatch | `client/value_conversion.rs:64-70` (and body for BLMPOP etc.) | `None` branch is cheap; `Some` branches walk the whole value (§3) |
| F12 | 4× post-dispatch `is_*_command` checks | `client/mod.rs:773-784` | 4 × `cmd.command()` allocs regardless of the real command |
| F13 | Compression dispatch wrapping | `client/mod.rs:850-855`, body at `compression.rs::process_response_for_decompression` | `Option<Arc<CompressionManager>>` clone even though compression is off in the bench |
| F14 | RESP3 push-manager `try_send_raw` on disconnect | `connection/multiplexed.rs:701-710` | only fires on error; zero on happy path |

Things redis-rs does that ferriskey doesn't (or does cheaper):

| # | What | Where (redis-rs 1.2.0) | Note |
|---|------|------------------------|------|
| R1 | Nothing equivalent to F1 (no per-command clone of a multi-Arc Client struct) | `aio/multiplexed_connection.rs:734` | `MultiplexedConnection::send_packed_command` is called on `&mut self` of the existing connection handle |
| R2 | Skip-concurrency-limit short-circuit when `cmd.skip_concurrency_limit` | `cmd.rs:86` + `multiplexed_connection.rs:735-741` | Per-command opt-out — bench does not configure a limiter so this branch evaluates `None` without any CAS work |
| R3 | Boxed `ConnectionLike::req_packed_command` is one `.await` | `aio/multiplexed_connection.rs:873-875` | Single `send_packed_command` dispatch; no per-command env decoration |
| R4 | Cache-aio path is `cfg`-gated; the bench build doesn't compile it | `multiplexed_connection.rs:749-776` | Zero cost when the feature is off |
| R5 | Timeout applied in the pipeline layer, once, as `Option<Duration>` | `multiplexed_connection.rs:474-479` | No per-command `tokio::select!` wrap |
| R6 | `from_redis_value` dispatches on the generic type `T` (monomorphised), not on the command | `cmd.rs:656` | Integer conversion is inlined by codegen; no runtime command-name match |

## §2 — Per-command trace tables

### §2.1 RPUSH — ferriskey path

User call:
```rust
client.rpush(QUEUE_KEY, &elements).await?
```

| Step | file:line | Description | Allocations / syncs / scans |
|------|-----------|-------------|------------------------------|
| 1 | `ferriskey_client.rs:264-274` | `Client::rpush(key, elements)` builds `Cmd` with `cmd("RPUSH").arg(key).arg(elements)` | 1 `Cmd::new` (empty `Vec<u8>`) + N `arg.write_args` (per argument: writes length header + bytes into `data`) |
| 2 | `ferriskey_client.rs:273` | Calls `self.execute(cmd)` | — |
| 3 | `ferriskey_client.rs:358-367` | `Client::execute` clones the inner Client (`(*self.0).clone()`) before calling `send_command` | **F1**: 5 `Arc::clone` + trait-object clone |
| 4 | `client/mod.rs:792-809` | `ClientInner::send_command` enters pinned box; checks IAM token changed | **F3**: `AtomicBool::load`; no-op in bench |
| 5 | `client/mod.rs:811` | `self.get_or_initialize_client().await` | `RwLock::read` on `internal_client`; cheap when initialised |
| 6 | `client/mod.rs:813` | `pubsub_synchronizer.intercept_pubsub_command(cmd).await` | **F4**: async call; inside calls `cmd.command()` (§2.4: 1 × `Vec<u8>` alloc + uppercase copy) and runs a ~6-arm match before returning `None` for RPUSH |
| 7 | `client/mod.rs:817` | `get_request_timeout(cmd, self.request_timeout)` | **F5**: scans `cmd` for BLMPOP/BZMPOP/XREAD-style timeout args |
| 8 | `client/mod.rs:823-831` | `self.reserve_inflight_request()` | **F6**: SeqCst CAS loop + `Arc::new(guard)` |
| 9 | `client/mod.rs:836-846` | Inflight-usage bucket log threshold check | **F7**: atomic load + integer math; cheap but unconditional |
| 10 | `client/mod.rs:848` | `cmd.set_inflight_tracker(tracker)` | **F8**: single field write |
| 11 | `client/mod.rs:851-855` | `compression_manager` conditional clone | **F13**: `Option<Arc<_>>::clone` → None in bench |
| 12 | `client/mod.rs:856` | `self.clone()` a second time for the owned-future move | second per-command Client clone |
| 13 | `client/mod.rs:857` | `Arc::new(cmd.clone())` | **F2**: `Cmd::clone` (Vec copy) + Arc allocation |
| 14 | `client/mod.rs:859-865` | Build `execute_command_owned` future | — |
| 15 | `client/mod.rs:867-885` | `tokio::select!` between execute and `tokio::time::sleep` | **F9**: timer future creation per call |
| 16 | `client/mod.rs:713-721` | `execute_command_owned`: pattern match `ClientWrapper` → `Standalone` | 1 branch |
| 17 | `client/standalone_client.rs:592-611` | `StandaloneClient::send_command` calls `Routable::command(cmd)` | 1 × `cmd.command()` alloc (§2.4); read-only flag check |
| 18 | `client/standalone_client.rs:583-590` | `send_request_to_single_node` gets a `ReconnectingConnection` and calls `send_request` | `RwLock::read` on connection pool |
| 19 | `client/standalone_client.rs:494-508` | `send_request`: `get_connection().await` then `send_packed_command(cmd)` | — |
| 20 | `connection/multiplexed.rs:692-712` | `MultiplexedConnection::send_packed_command` calls `pipeline.send_single` | Packs command via `cmd.get_packed_command()`: `Vec<u8>` + copy of args into RESP-encoded bytes |
| 21 | `connection/multiplexed.rs:518-525` | `send_single` → `send_recv(item, None, timeout, true, is_fenced)` | — |
| 22 | `connection/multiplexed.rs:527-556` | `send_recv` creates oneshot channel, sends `PipelineMessage` through `self.sender` | 1 `oneshot::channel` alloc + 1 mpsc send |
| 23 | `connection/multiplexed.rs:557-567` | `runtime::timeout` wrapping `receiver.await` | another timeout wrap — duplicate of F9 when F9 already wrapped |
| 24 | (driver task) | Event loop writes RESP bytes + reads reply | Not counted per-command — amortised in driver |
| 25 | `connection/multiplexed.rs:701-710` | Disconnect push-manager notify on error | **F14**: zero on happy path |
| 26 | `client/mod.rs:750-768` | Compression-response decompression wrap | **F13**: skipped because `compression_manager` is `None` |
| 27 | `client/mod.rs:770` | `expected_type_for_cmd(&cmd)` | **F10**: 1 × `cmd.command()` alloc + linear match; returns `None` for RPUSH |
| 28 | `client/mod.rs:771` + `value_conversion.rs:64-70` | `convert_to_expected_type(value, None)` | **F11 short path**: `Some(expected) else return Ok(value)` — cost is the `Option` match only |
| 29 | `client/mod.rs:773` | `is_client_set_name_command(&cmd)` | **F12**: 1 × `cmd.command()` alloc |
| 30 | `client/mod.rs:776` | `is_select_command(&cmd)` | **F12**: 1 × `cmd.command()` alloc |
| 31 | `client/mod.rs:779` | `is_auth_command(&cmd)` | **F12**: 1 × `cmd.command()` alloc |
| 32 | `client/mod.rs:782` | `is_hello_command(&cmd)` | **F12**: 1 × `cmd.command()` alloc |
| 33 | `ferriskey_client.rs:366` | `from_owned_value(value)` on the result — for RPUSH, `i64` from integer reply | 1 typed `FromValue` dispatch; simple path |

Per-call allocation count (static estimate, RPUSH): **~8 heap allocations**
(Cmd::clone Vec copy, Arc<Cmd>, Arc<guard> for inflight tracker,
timer state, 6× `cmd.command()` uppercase Vec allocations, oneshot
channel). That doesn't count the driver-task-side serialisation into
the RESP buffer.

### §2.2 RPUSH — redis-rs 1.2.0 path

User call:
```rust
con.rpush(QUEUE_KEY, &vals).await?
```

| Step | file:line | Description | Allocations / syncs |
|------|-----------|-------------|---------------------|
| 1 | `commands/mod.rs:947-949` | Trait method macro-generated to `cmd("RPUSH").arg(key).arg(value).take()` | `Cmd::new` + N `arg.write_args` |
| 2 | `commands/macros.rs:15-22` (async) | `Box::pin(async move { body.query_async(self).await })` — returns `RedisFuture` | Box allocation for pinned future |
| 3 | `cmd.rs:650-657` | `Cmd::query_async`: `con.req_packed_command(self).await?` then `from_redis_value` | 2 `.await`s |
| 4 | `aio/multiplexed_connection.rs:873-875` | `ConnectionLike::req_packed_command` calls `send_packed_command(cmd).await.boxed()` | another box pin |
| 5 | `aio/multiplexed_connection.rs:734-785` | `send_packed_command`: **(a)** optional concurrency-limit permit (feature-gated and disabled in our bench config — `limiter` is `None` so the branch evaluates `None` with no CAS); **(b)** `self.pipeline.send_recv(cmd.get_packed_command(), None, timeout, is_no_response)` | R5: timeout applied once, in the pipeline layer |
| 6 | `aio/multiplexed_connection.rs:429-480` | `Pipeline::send_recv`: oneshot channel + mpsc send, then `Runtime::timeout(timeout, request).await` | 1 oneshot + 1 mpsc |
| 7 | `cmd.rs:656` | `from_redis_value::<T>(val.extract_error()?)` | Monomorphised `FromRedisValue for usize` (integer-parse short path) |

Per-call allocation count (static estimate, RPUSH): **~3 heap allocations**
(pinned future box, oneshot channel, Cmd::get_packed_command Vec).
No per-command `cmd.command()` alloc — the command bytes are already
at the head of `Cmd::data`, and redis-rs never string-extracts it
here.

### §2.3 INCR — diff from RPUSH

Identical to RPUSH in both libraries. One arg instead of N, integer
reply, no command-specific branches. The per-command steps above
apply verbatim. Static allocation count: same (both libs' overhead is
dominated by the envelope, not the payload).

### §2.4 `Routable::command()` — why it allocates on every call

`cluster/routing.rs:1183-1206`. Definition:

```rust
fn command(&self) -> Option<Vec<u8>> {
    let primary_command = self.arg_idx(0).map(|x| x.to_ascii_uppercase())?;
    ...
}
```

`to_ascii_uppercase()` on a byte slice returns an **owned `Vec<u8>`**;
it does not short-circuit on already-uppercase input. RPUSH is stored
as `"RPUSH"` (already uppercase) at arg-zero of `Cmd::data`, so every
call to `cmd.command()` in ferriskey's hot path allocates a fresh
5-byte Vec<u8> just to copy `b"RPUSH"` over.

Called **six times** per RPUSH/INCR in ferriskey:
- `client/mod.rs:813` (`intercept_pubsub_command` → `command()` inside
  `synchronizer.rs:953`)
- `client/mod.rs:770` (`expected_type_for_cmd` → `command()` at
  `value_conversion.rs:1594`)
- `client/mod.rs:773` (`is_client_set_name_command`)
- `client/mod.rs:776` (`is_select_command`)
- `client/mod.rs:779` (`is_auth_command`)
- `client/mod.rs:782` (`is_hello_command`)

Plus the call at `client/standalone_client.rs:593` for read-only
routing in the standalone path (RPUSH is not read-only so no early
exit — the call happens regardless).

redis-rs, by contrast, never takes the command name as an owned
`Vec<u8>` on the hot path for a simple command. The routing / cluster
layer does extract it when needed (`cluster_routing.rs:Routable`) but
the standalone `aio/multiplexed_connection` path just sends the
pre-packed bytes.

### §2.5 BLMPOP — ferriskey path

Identical to RPUSH steps 1-26, plus:

| Step | file:line | Description |
|------|-----------|-------------|
| 27′ | `value_conversion.rs:1671` | `expected_type_for_cmd` matches `b"BLMPOP"` → `Some(ExpectedReturnType::ArrayOfStringAndArrays)` |
| 28′ | `value_conversion.rs:373-391` | `convert_to_expected_type` enters the `ArrayOfStringAndArrays` arm — walks the full response: expects `Value::Array([key, Array([elements])])`, calls `convert_array_to_map_by_type(...)` with `BulkString` key type + `ArrayOfStrings` value type |
| 29′ | `value_conversion.rs:1469-1545` | `convert_array_to_map_by_type` iterates the array in pairs, calls `convert_to_expected_type` recursively for each key + each value — an additional allocation per element of the popped batch, plus a `Vec::push` for the map entry |

For a single-element BLMPOP pop the conversion does:
- 1 outer `Vec<Value>::into_iter().collect::<Result<_>>()` (alloc + move)
- 1 `convert_array_to_map_by_type` iteration
- 1 `convert_to_expected_type` for the key string
- 1 `convert_to_expected_type` for the inner-array value

redis-rs has nothing equivalent: the caller receives the raw `Value`
shape and typed deserialisation is driven by whatever generic type
the caller asked for (`from_redis_value::<T>(val)`) — if they asked
for `Option<Value>` they pay nothing extra.

### §2.6 BLMPOP — redis-rs 1.2.0 path

Identical to §2.2. The baseline bench types its return as
`Option<(String, Vec<Vec<u8>>)>`; `from_redis_value` walks the raw
Value once to build that tuple, directly. No intermediate Value-to-Value
transformation. Same ~3 allocations as RPUSH, plus the tuple
construction.

## §3 — `value_conversion.rs` deep-dive

`expected_type_for_cmd` at `client/value_conversion.rs:1593-1870` is a
~280-line `match command.as_slice()` dispatch. It is called every time
on the hot path via `execute_command_owned` at `client/mod.rs:770`.

Variants relevant to RPUSH / INCR / BLMPOP:

- **RPUSH** — not in the match → returns `None`.
  `convert_to_expected_type(value, None)` short-circuits at
  `value_conversion.rs:68-70` (`let Some(expected) = expected else {
  return Ok(value); }`). Cost: one `Option::is_none` branch + one
  `Ok` wrap. **The cost ferriskey pays vs redis-rs here is the
  `cmd.command()` alloc and the match dispatch itself**, not the
  conversion — the conversion is a true no-op.

- **INCR** — not in the match → `None`. Same short-circuit.

- **BLMPOP** — `value_conversion.rs:1671` →
  `Some(ExpectedReturnType::ArrayOfStringAndArrays)`. Conversion body
  at 373-391 does the map-shape coercion described in §2.5.

### Is the dispatch cheap for the `None` case?

Reading the match body: the matcher is a single `match command.as_slice()`
over byte-slice literals. The Rust compiler turns this into a trie /
branch table. For RPUSH/INCR which are NOT in any arm, the match falls
through the default `_ => None` at `value_conversion.rs:1868`. Cost
is: one `cmd.command()` call (`cluster/routing.rs:1183` — the
`Vec<u8>` alloc from `to_ascii_uppercase`), plus the match dispatch.

The allocation dominates. The match itself is a few branches — the
Rust compiler turns byte-string matches into a small decision tree,
not a linear scan. Likely sub-100ns. The `cmd.command()` alloc is the
real cost, and it happens **regardless** of whether the result is
`None` or `Some`.

### What `convert_to_expected_type` does for the BLMPOP case

Walking `value_conversion.rs:373-391`:

```
ExpectedReturnType::ArrayOfStringAndArrays => match value {
    Value::Nil => Ok(value),
    Value::Array(array) if array.len() == 2 && matches!(array[1], Ok(Value::Array(_))) => {
        let array: Vec<Value> = array.into_iter().collect::<Result<_>>()?;
        // convert the array to a map of string to string-array
        let map = convert_array_to_map_by_type(
            array,
            Some(ExpectedReturnType::BulkString),
            Some(ExpectedReturnType::ArrayOfStrings),
        )?;
        Ok(map)
    }
    ...
}
```

Per successful BLMPOP pop: `array.into_iter().collect::<Result<_>>()` is
a `Vec<Result<Value>>` → `Result<Vec<Value>>` transpose; allocates a
new Vec of length 2. Then `convert_array_to_map_by_type` walks that
Vec, recursively calls `convert_to_expected_type` for each element.

### Does a trivial variant still pay dispatch overhead?

Yes, but only the outer `Option::is_some` match. The `None` branch at
line 68-70 early-returns with `Ok(value)` before entering the giant
variant dispatch. The dispatch cost itself is essentially zero for
the non-converting commands — but the SETUP cost (the
`cmd.command()` alloc to look up whether there's a conversion) is
paid every time.

## §4 — Things redis-rs does that ferriskey doesn't

- **Connection-level pipelining** — both libs have an mpsc-driven
  pipeline driver task. Per-command, they look equivalent. Neither
  auto-batches multiple user requests into a single sendmsg; both
  rely on the tokio write half to coalesce at the TCP layer.

- **RESP parser** — both libs parse RESP via a similar state machine.
  redis-rs 1.2.0 uses `redis-protocol` (external crate). ferriskey
  has its own RESP parser in `src/protocol/`. I did not read the
  parsers side-by-side; they're on the driver task, not the
  per-command path, and W1's flamegraph will tell us if they differ
  in cost. Out of scope for this analysis.

- **Custom allocators** — neither lib opts for a non-default global
  allocator. Both inherit the bench binary's allocator (jemalloc or
  system malloc depending on Cargo profile).

- **`Runtime::locate().timeout(...)`** in redis-rs
  (`multiplexed_connection.rs:475`) is a runtime-agnostic timeout. In
  our bench's tokio-only build this collapses to
  `tokio::time::timeout`. ferriskey uses the same tokio `timeout` via
  `runtime::timeout` at `connection/multiplexed.rs:557`.

## §5 — Predictions for W1 / W2 outputs

### If W1's flamegraph on scenario 1 shows:

- `Routable::command` / `to_ascii_uppercase` as a top bucket →
  matches §2.4. 4-6 copies of a 4-8 byte command name per call,
  100 000 calls in the 10s bench = ~500k allocs from this site alone.
  `to_ascii_uppercase` is not inlined by codegen because it returns
  an owned Vec.

- `Arc::clone` / `Arc::drop` as a top bucket → §F1+F2+F6. Each command
  does ~6 atomic increments (Client::clone = 5 Arcs + pubsub
  trait-object; inflight guard Arc::new; cmd Arc::new). At 3 300
  ops/s that's ~20 000 atomics/sec just on our side of the fence.

- `convert_to_expected_type` as a top bucket **without**
  `convert_array_to_map_by_type` nearby → the dispatch itself
  (`expected_type_for_cmd` linear-match) is the cost, likely driven
  by the `cmd.command()` alloc in the first line. See §3.

- `convert_array_to_map_by_type` as a top bucket → BLMPOP-dominant
  workload (which is ~50% of scenario 1 by count given workers BLMPOP
  every claim attempt).

- `tokio::time::sleep` as a top bucket → the per-command timeout
  wrapper at `client/mod.rs:867-885`. redis-rs doesn't wrap
  per-command; it passes the timeout down once to the pipeline
  layer.

### If W2's pipelined workload closes the gap:

A ferriskey `TypedPipeline` (`ferriskey_client.rs:700`) batches
multiple commands into a single `send_packed_commands` at
`connection/multiplexed.rs:717`. The per-command envelope above fires
**once for the pipeline**, not N times. If the gap disappears under
pipelining, the per-command envelope IS the cost — §2.1 items F1-F9
amortise to `cost/N` and the per-command path approaches the
driver-task cost (which is close to redis-rs's driver cost).

If the gap persists under pipelining, the cost is in the parser /
driver task / RESP encoding — not in the envelope. That would be a
different investigation.

### If the gap is in send/recv (not conversion):

Would suggest the driver-task pipeline (ferriskey
`connection/multiplexed.rs:527-556` vs redis-rs
`aio/multiplexed_connection.rs:429-480`) differs in throughput shape.
Both look structurally identical from the source (oneshot + mpsc);
any perf delta would come from buffer allocation patterns or the
RESP writer/parser.

## §6 — Batch C follow-up candidates

These are observations only. The project owner (ferriskey primary
maintainer) decides what, if anything, to change.

1. **`Routable::command` caching** — store the uppercase command name
   once in `Cmd` at construction (or lazily, memoised) so the 4-6
   per-command allocations collapse to zero. Mechanical change; would
   require touching every `cmd.command()` call site to read from a
   cached field. Potential ~50% alloc-count reduction on the hot path
   per §1.

2. **Elide `is_*_command` checks for commands that can't be any of
   those four** — after `expected_type_for_cmd` has run and returned
   `None`, the command name is already known to not be SELECT / AUTH
   / HELLO / CLIENT SETNAME (those appear elsewhere in the match or
   would have returned Some). Batch the 4 checks into one match.

3. **Short-circuit `intercept_pubsub_command` when pubsub is
   unused** — if the Client has no active subscriptions, the
   interceptor always returns `None` but still does a `cmd.command()`
   alloc per call. A `has_subscribers() -> bool` fast-path check on
   an atomic would skip the alloc entirely for bench-like workloads.

4. **Avoid `Arc::new(cmd.clone())` when the command doesn't fan out
   across cluster sub-commands** — the Arc exists to let PipelineMessage
   clone the Cmd into the event loop while the caller's future stays
   owned of the original. For standalone (single-connection) sends
   the Arc is unused; `Cmd::clone` without the Arc wrap would work.

5. **Collapse the double timeout wrap** — per-command `tokio::select!`
   in `send_command` (F9) is redundant with the `runtime::timeout` in
   `send_recv` (`connection/multiplexed.rs:557`). Pick one.

6. **Inflight tracker for unconfigured limit** — when
   `inflight_requests_limit == isize::MAX` (or whatever the "no
   limit" sentinel is), skip the CAS loop and the Arc guard. Matches
   redis-rs's R2 short-circuit.

None of the above are urgent or a bug. They're mechanical redundancies
that accumulate to ~half the gap we see vs redis-rs 1.2.0 on a simple
command. BLMPOP-specific overhead (the `ArrayOfStringAndArrays`
coercion) is a separate ~10-30% on the BLMPOP path; that one IS
arguably a correctness feature (ferriskey presents a cleaner Value
shape to consumers), not pure overhead.

## §7 — Anti-goals (what this report is not)

- **Not a recommendation to change ferriskey.** §6 lists mechanical
  observations; the project owner has full context we don't, and may
  have good reasons (compression tests, cluster routing, IAM auth
  paths) for the current shape. Most of the items above are "you
  could do this if you wanted", not "you should do this".

- **Not a measurement.** Every "~X% of" or "plausible cost" in this
  report is inferred from the static source. W1's flamegraph is the
  authoritative answer.

- **Not a comparison to ferriskey HEAD or any other version of
  redis-rs.** Pinned to:
  - ferriskey: feat/ferriskey-perf-invest branch HEAD (working tree)
  - redis-rs: 1.2.0 as pinned in `benches/comparisons/baseline/Cargo.lock`

- **Does not address** the Batch B / Batch C deltas to the FlowFabric
  stack above ferriskey (ff-sdk, axum, Lua FCALL, scheduler). Those
  are a separate ~58% of the redis-rs → flowfabric gap and belong in
  a different report.
