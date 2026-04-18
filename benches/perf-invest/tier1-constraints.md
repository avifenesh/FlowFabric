# Tier 1 envelope collapse — constraint inventory

Branch: `feat/tier1-envelope-collapse` (off main `fcfb98c`).
Target: inline `Client::execute_command_owned` (`ferriskey/src/client/mod.rs:666`)
into `Client::send_command` (`ferriskey/src/client/mod.rs:737`).

This document is the safety net around W1's change. Each section is
read-only constraint capture — what MUST still hold after the
collapse. Written against the pre-collapse baseline so a reviewer can
diff against it.

> **Note — post-merge reader**: line numbers and the `execute_command_owned`
> name refer to the pre-collapse tree (`main` @ `fcfb98c`). After this PR
> merges, `execute_command_owned` no longer exists as a separate function —
> its body is inlined into `Client::send_command`. Treat the citations in
> this doc as a historical reference for what invariants the collapse had
> to preserve, not as a current map of the code.

## §2 — Public signature invariants

```rust
pub fn send_command<'a>(
    &'a mut self,
    cmd: &'a mut Cmd,
    routing: Option<RoutingInfo>,
) -> BoxFuture<'a, Result<Value>>
```
`ferriskey/src/client/mod.rs:737-741`

Every token in that signature is load-bearing. In order:

- **`pub`** — public API. Any change is a breaking change for every
  external caller listed in §1.
- **`fn` (not `async fn`)** — the body is `Box::pin(async move { … })`.
  The non-async outer function captures the tracing span before the
  async body begins, so span entry is synchronous with the call site
  (`:750` `let span = …`, `:757` `Box::pin(async move { … }.instrument(span))`).
  Converting to `async fn` would move the span creation into the
  future's first poll and lose correlation for immediate-error paths
  (e.g. `intercept_pubsub_command` returning synchronously).
  It would also change the return type (`impl Future`), breaking the
  `ValkeyClientForTests` trait at `:1934` — which is dyn-dispatched
  in tests/benches.
- **`<'a>`** — single caller-supplied lifetime; the function is
  generic over it. Pins the lifetime of the returned future to the
  caller's borrow of self + cmd. Callers can assume the future does
  NOT outlive either borrow.
- **`&'a mut self`** — exclusive borrow on the Client. Required by:
  - `self.pubsub_synchronizer.intercept_pubsub_command(cmd)` at
    `:781` (may mutate subscription state).
  - `self.update_connection_password(...)` in the IAM branch at
    `:775` (requires `&mut self` by way of
    `update_stored_password` → `internal_client.write().await`).
  - **All four handle_\* post-calls** (`:723-732` in
    `execute_command_owned`) take `&mut self`.
  - Inflight slot logging at `:809-818` reads `self.inflight_*` via
    `&self`, but the field access pattern requires the overall
    `&mut self` to already be held.
- **`cmd: &'a mut Cmd`** — mutable borrow, NOT owned. Required by
  `cmd.set_inflight_tracker(tracker)` at `:820`. Clippy would allow
  `&mut Cmd` to be tightened to `&Cmd` if and only if set_inflight_tracker
  were removed; it cannot be, so `&mut` must stay.
  Shares lifetime `'a` with self — a single borrow region covers both.
- **`routing: Option<RoutingInfo>`** — owned. Consumed when passed to
  `Self::execute_command_owned(...)` at `:834`. Non-Copy enum; passing
  it by value lets the cluster arm move it into `CmdArg::MultiCmd`.
- **`-> BoxFuture<'a, Result<Value>>`** — `futures_util::future::BoxFuture<'a, T>`
  is `Pin<Box<dyn Future<Output = T> + Send + 'a>>`. Three things
  are pinned by this return type:
  1. **`Box<…>` heap-alloc** — required so the return type is
     sized. `async fn` returns an unboxed `impl Future`, which would
     defeat the `ValkeyClientForTests` trait's dyn dispatch at
     `:1934-1940`.
  2. **`Send`** — callers spawn the future (`tokio::spawn`). Verified
     at `ferriskey/tests/test_pubsub.rs:861-873` (spawned send_command
     in pubsub concurrency test) and in user code that crosses task
     boundaries via `ff-sdk` worker loops. Removing Send is a silent
     break: tests that `.await` without spawn still pass, but
     downstream consumers that spawn would fail to compile.
  3. **`'a`** — future borrows self + cmd for `'a`. Not `'static`. The
     INNER `execute` future (currently `Self::execute_command_owned`)
     is designed to be `Send + 'static` (see comment at `:664`) so
     that `tokio::pin! + tokio::select!` at `:841-843` can hold it
     across a sleep timeout. When W1 inlines, the resulting pinned
     future will be `'a` rather than `'static` — that's FINE for the
     select, because the awaiting outer future is also `'a`-scoped.
     What's NOT fine: inadvertently making the future `!Send` by
     capturing something non-Send across an await point.

### Implied invariants

- `self.clone()` at `:828` produces a second `Client` handle for the
  owned-dispatch path; `Client: #[derive(Clone)]` at `:201`. All
  fields are Arc or Copy, so clone is cheap; if W1's collapse
  eliminates this, the `handle_*` post-calls still need `&mut self`
  access — and since the *same* `&mut self` is alive on the outer
  future, the compiler must see that path clearly.
- `cmd` must stay borrowable for the tracing span attribute
  (`command = %command_name` at `:754` is captured BEFORE the async
  block; this one is safe because the String is materialized up
  front).

## §3 — `ClientWrapper` match-arm behavior spec

`ClientWrapper` at `:195-199`:
```rust
#[derive(Clone)]
pub enum ClientWrapper {
    Standalone(StandaloneClient),
    Cluster { client: ClusterConnection },
}
```

Both variants hold internally-Arc-backed state; `.clone()` is cheap.
`execute_command_owned` consumes an owned `ClientWrapper` value
produced by `get_or_initialize_client` (`:659-662`, which does
`guard.clone()` under a RwLock read). That cloned wrapper is what
gets pattern-matched.

### Standalone arm (`:674`)

```rust
ClientWrapper::Standalone(mut client) => client.send_command(&cmd).await,
```

- Inner type: `StandaloneClient` (`Arc<DropWrapper>`-backed; `.clone()` is
  Arc bump).
- Method consumed: `StandaloneClient::send_command(&mut self, cmd: &Cmd) → Result<Value>`
  at `standalone_client.rs:599`.
- Mutates: nothing visible to the caller. Internally reaches into one of
  the `ReconnectingConnection`s and calls `send_packed_command`. The
  Cmd itself is NOT held past the await — the connection copies out
  packed bytes via `cmd.get_packed_command()`.
- **Post-dispatch state persisted**: none beyond the wire response.
- **Requires**: just `&Cmd` — the `Arc<Cmd>` deref in the current
  code (`&cmd` where `cmd: Arc<Cmd>`) is used to produce a `&Cmd`
  via `Deref`. Collapsing to `&mut Cmd → &Cmd` is safe.

### Cluster arm (`:675-693`)

```rust
ClientWrapper::Cluster { mut client } => {
    let final_routing = /* disambiguate SingleNode::Random for writes */;
    client.route_command(&cmd, final_routing).await
}
```

- Inner type: `ClusterConnection`.
- Method consumed: `ClusterConnection::route_command(&mut self, cmd: &Cmd, routing: RoutingInfo)`
  at `cluster/mod.rs:492-496`.
- Mutates: sends a `Message` onto an `mpsc` owned by the cluster task.
- **Post-dispatch state persisted**: The message body may clone the
  Cmd. Two subcases in `cluster/mod.rs:499-509`:
  - `RoutingInfo::MultiNode(_)` → wraps in `CmdArg::MultiCmd { cmd: Arc::new(cmd.clone()), … }`.
    **This Arc<Cmd> is owned by the cluster event loop and outlives the
    `route_command` await** — hence the whole `execute_command_owned`
    "owned data so future is Send + 'static" rationale at `:664`
    comment. The inflight tracker attached to the Cmd stays alive via
    this Arc until the event loop drains the MultiCmd.
  - Single-node routing (`_ =>` arm) → `CmdArg::Cmd { packed: cmd.get_packed_command(), … }`.
    The Cmd is packed into bytes BEFORE the message is sent. The
    original Cmd (and its tracker) is free to drop as soon as
    `route_command` returns.
- **Requires**: `&Cmd` only. Same as standalone.

### Call order after dispatch

The pre-collapse order is:
1. Dispatch → `raw_value` (match arm).
2. Decompression + type-conversion on `raw_value`.
3. Post-dispatch handlers (§4) fire in fixed order: SETNAME → SELECT
   → AUTH → HELLO.
4. Return `Ok(value)`.

**Dispatch must happen before decompression** (needs raw bytes to
inspect). **Decompression must happen before handle_\***, because if
the command was e.g. a compressed HELLO payload (hypothetical —
valkey auth responses are never compressed, but the code is
pessimistic), the handle_\* methods don't inspect the response.
Actually `handle_select_command` et al. only inspect `cmd`, not the
response — so handle_\* ordering vs decompression is inert in
practice, but the current source clearly sequences decompression
first; preserving that order avoids any future coupling.

### Order-of-drop question (the one manager flagged)

Current (pre-collapse): `owned_cmd: Arc<Cmd>` is moved into
`execute_command_owned`, which consumes it through the match. The
match arm may consume it further (`Arc::new(cmd.clone())` in MultiCmd
— a SECOND Arc wrapping a NEW Cmd clone). After the function
returns, the original `owned_cmd` Arc drops (refcount -1). But the
user's `&mut Cmd` at the top-level still has its own tracker because
`set_inflight_tracker` was called directly on the user's Cmd at
`:820`.

If W1's inlining eliminates `Arc::new(cmd.clone())` at `:829` and
instead dispatches `cmd: &mut Cmd` straight into the match arm, the
tracker refcount starts at 1 (user's Cmd) rather than 2 (user's +
intermediate Arc). The MultiCmd arm still performs its own
`Arc::new(cmd.clone())` at `cluster/mod.rs:501`, so the cluster
event-loop retention is unchanged. Single-node cluster and standalone
see no intermediate Arc in either the pre- or post-collapse world —
identical behavior.

Net: **the refcount trajectory changes (smaller peak, same end) but
the slot-release timing for the USER-visible contract is identical.**
See §5 for the detailed tracker analysis.

## §4 — Post-dispatch command handlers

Four `handle_*_command` methods fire AFTER the dispatch returns Ok.
All four are `async fn (&mut self, cmd: &Cmd) -> Result<()>`. They
fire in this order (currently `execute_command_owned:722-733`):

1. `handle_client_set_name_command` (`:455-459`) if `CLIENT SETNAME`.
2. `handle_select_command` (`:416-423`) if `SELECT`.
3. `handle_auth_command` (`:502-512`) if `AUTH`.
4. `handle_hello_command` (`:608-632`) if `HELLO`.

### handle_select_command

- Extracts database id from `cmd.arg_idx(1)`.
- `self.update_stored_database_id(database_id)` → acquires
  `self.internal_client.write().await` and mutates the inner
  connection's stored database id.
- Mutates `self.otel_metadata.db_namespace`.
- **Error surface**: extract can return `Err(Response/InvalidIntArg)`;
  write-guard acquisition can't fail; inner call can return
  connection errors.

### handle_client_set_name_command

- Extracts client name from `cmd.arg_idx(2)` (Option<String>).
- `self.update_stored_client_name(client_name)` → write-guard on
  `internal_client`, mutates Standalone or Cluster client's stored
  name.
- **No otel_metadata mutation.**
- **Error surface**: Only the inner update call can fail.

### handle_auth_command

- Extracts `(username, password)` from `cmd`.
- Conditionally calls `update_stored_username` and
  `update_stored_password`. Each re-acquires `internal_client.write()`.
- **Error surface**: username update fails → password update is
  SKIPPED (bail on first `?`). password update fails → function
  returns Err. Subsequent handlers (HELLO, etc.) do NOT fire — the
  early `?` in `execute_command_owned` propagates.

### handle_hello_command

- Parses protocol, username, password, client_name from HELLO's args.
- Calls up to four update_stored_\* methods, each one re-acquiring
  `internal_client.write()`. **Each write-guard acquisition is
  INDEPENDENT** — guard is dropped between calls.
- **Error surface**: first failing update_stored_\* short-circuits
  via `?`. Remaining updates for this HELLO do NOT fire.

### Ordering / fire-or-skip-on-error semantics

All four handlers use `?`. The ORDERING of the `if` blocks in
`execute_command_owned:722-733` means:
- CLIENT SETNAME and SELECT and AUTH and HELLO are mutually exclusive
  commands (the `is_*` checks look at `cmd.command()`). So
  at most ONE handler fires per dispatch.
- The "order" is therefore inert for normal cases. The order matters
  only if someone constructs a Cmd whose command() returns multiple
  of these — not possible with the public API.

### What W1 must preserve

- `&mut self` reborrow for the handlers. Currently via
  `self_clone.handle_*()` where `self_clone: Client` is owned (line
  `:667`). Post-collapse, W1 will likely use the outer `&mut self`
  directly — fine, since the match-arm completed its consumption of
  `client: ClientWrapper` and returned.
- Handlers must NOT run on the error path. If the dispatch returns
  Err, the `?` propagates and no handler fires. Same in the
  decompression/convert step.
- Handlers must NOT run on the user-timeout branch of the `select!`
  either (`:842-856`). Currently the select drops the `execute`
  future; the handlers are PART of that future and never execute.
  W1's collapse must keep the handlers inside the timeout-governed
  future, NOT hoist them past the select.

## §5 — Known risk matrix

A grep-driven audit of the five candidate risk categories. Each entry
lists the risk, what to verify in W1's diff, and the ground-truth file
reference for the check.

### R1 — Arc::clone on the path

Non-`self.clone()`, non-`Arc::new(cmd.clone())` clone sites visible in
the pre-collapse send_command body (`:737-863`):

- `let self_clone = self.clone()` @ `:828` — Client is Arc-backed
  (internal_client + compression_manager + iam + pubsub_synchronizer
  are Arc). Derived `#[derive(Clone)]` at `:201`.
- `let owned_cmd = Arc::new(cmd.clone())` @ `:829` — Arc-wraps a Cmd
  clone. Cmd is `#[derive(Clone)]` at `cmd.rs:26`; its
  `inflight_tracker: Option<InflightRequestTracker>` clones too
  (Arc bump).
- `self.pubsub_synchronizer` access @ `:781` — no clone, just `&dyn` method call.
- `self.compression_manager.clone()` @ `:824` — Option<Arc<CompressionManager>>
  clone. Cheap Arc bump.
- `get_or_initialize_client` @ `:779` does `guard.clone()` internally
  (`:661`) — clones `ClientWrapper`.

**Verify in W1 diff**: post-collapse, the only clones that should
still be present are `self.compression_manager.clone()` and
`get_or_initialize_client`. The `self.clone()` and `Arc::new(cmd.clone())`
SHOULD both disappear, because dispatch now borrows `&mut self` and
`&mut cmd` directly. If either persists, W1 probably kept a vestigial
owned path — flag it.

### R2 — InflightRequestTracker kept-alive-via-Drop

Source of truth: `ferriskey/src/value.rs:2484-2570`.

- `InflightSlotGuard` holds `Arc<AtomicIsize>`; its `Drop`
  `fetch_add(1)` to release the slot.
- `InflightRequestTracker { _guard: Arc<InflightSlotGuard> }`.
  Cloneable — clone is Arc bump.
- `try_new` at `:2515` does **exactly one** `compare_exchange`
  `current - 1` — reserves EXACTLY ONE slot per tracker.
- Tracker clones share the same `Arc<InflightSlotGuard>`. Slot
  released when the `Arc` refcount hits zero (last clone drops) —
  verified by unit test `cloned_tracker_releases_only_when_last_clone_drops`
  at `:2552-2569`.

**Implication for manager's tracker-count question**: Collapsing
`Arc<Cmd>` (one Arc over a clone of Cmd) into `&mut Cmd` (direct
borrow) changes the number of TRACKER CLONES that exist during the
dispatch — but NOT the number of SLOTS RESERVED. One slot is reserved
per call to `reserve_inflight_request` (`:1161-1163`), and that call
happens once per `send_command` invocation at `:795`, NOT per clone.

Refcount trajectory, single-node / standalone:
- Pre-collapse: reserve → `cmd.set_inflight_tracker(tracker)` (refcount
  on Arc<InflightSlotGuard> = 1). `cmd.clone()` inside `Arc::new(…)`
  at `:829` → Cmd clone bumps tracker refcount to 2. Dispatch
  consumes the Arc<Cmd>. On return, that Arc drops (refcount 1).
  User's `&mut cmd` still holds the tracker (refcount 1). User
  eventually drops their cmd → 0 → slot released.
- Post-collapse: reserve → `cmd.set_inflight_tracker` (refcount 1).
  No `Arc::new(cmd.clone())`. Dispatch borrows &mut cmd. On return,
  user's cmd still has tracker (refcount 1). User drops → 0 →
  released.

**Net slot-release timing: identical.** Both paths release when the
user drops their Cmd.

Refcount trajectory, cluster MultiNode:
- The MultiNode arm at `cluster/mod.rs:501` does
  `cmd: Arc::new(cmd.clone())`. That's a separate Arc<Cmd> stashed
  inside `CmdArg::MultiCmd`, which gets sent via mpsc to the cluster
  event loop.
- Pre-collapse: reserve (1) → outer clone (2) → MultiCmd Arc<Cmd>
  (3). Cluster event loop holds the MultiCmd until all sub-commands
  complete. Outer future completes first → outer Arc drops (2).
  User drops their cmd (1). Cluster event loop finally drops
  MultiCmd → 0 → released.
- Post-collapse: reserve (1) → MultiCmd Arc<Cmd> (2). Outer future
  completes → refcount still at 2 (cluster loop holds it). User
  drops their cmd (1). Cluster event loop drops MultiCmd → 0 →
  released.

**Net slot-release timing (MultiNode): identical.** The slot is held
until the LAST reference goes away — cluster event loop in both
worlds.

**Verify in W1 diff**: `cmd.set_inflight_tracker(tracker)` still
fires exactly once per invocation. `reserve_inflight_request` called
exactly once. No accidental second call to either. If W1 inlines the
reservation out of the `Box::pin(async move { … })` block, ensure it
still fires before the dispatch.

### R3 — BoxFuture `Send + 'a` boundary

Callers that REQUIRE the future be `Send`:
- `ferriskey/tests/test_pubsub.rs:861-873` — `tokio::spawn(async move
  { let _ = client_clone.send_command(&mut sub, None).await; })`.
  The spawn requires the inner future to be `Send + 'static`. Here
  `client_clone` is an owned `Client` (via `client.clone()`) and
  `sub` is an owned `Cmd` in the async move — the `'a` becomes
  `'static` because the borrows are of owned values inside the
  spawned task. **Send IS load-bearing**; `'static` IS load-bearing.
- Production code in `ff-sdk` worker loops spawns futures across task
  boundaries. Same constraint.

**Verify in W1 diff**: the `BoxFuture<'a, Result<Value>>` return type
MUST remain `Send + 'a` (which `futures_util::future::BoxFuture`
guarantees). If W1 accidentally captures a non-Send type across an
await in the inlined body — e.g. a raw pointer or an `Rc` — the
return type stays `BoxFuture<'a, …>` syntactically but the Box::pin
won't compile. That's the good kind of failure (loud).

The subtler risk: W1 captures `&mut self` inside the async block
where it was previously `self_clone: Client` (owned). `&mut Client`
is Send (because Client itself is Send). So Send is preserved.

### R4 — Cluster `route_command(&Cmd, …)`

`ClusterConnection::route_command` at `cluster/mod.rs:492-496`:
```rust
pub async fn route_command(
    &mut self,
    cmd: &Cmd,
    routing: RoutingInfo,
) -> Result<Value>
```

Takes `&Cmd`. The current code at `:693` dispatches `client.route_command(&cmd, final_routing).await`
where `cmd: Arc<Cmd>` — the `&cmd` coerces through `Deref<Target = Cmd>`.

**If W1 collapses to `&mut Cmd`, the call becomes
`client.route_command(cmd, final_routing)` or `client.route_command(&*cmd, …)`.**
Both work: `&mut Cmd` reborrows as `&Cmd` for the argument. This is
safe.

**Where Arc<Cmd> is still required**: `CmdArg::MultiCmd { cmd: Arc<Cmd>, … }`
at `cluster/mod.rs:947` stashes an Arc<Cmd> inside the message sent
to the event loop. The `Arc::new(cmd.clone())` at `:501` is done
INSIDE `route_command`, so `route_command` produces that Arc — W1's
caller does not need to construct one.

**Verify in W1 diff**: the call site passes `cmd` or `&*cmd` (not
`&Arc::new(cmd.clone())`). `RoutingInfo::for_routable(cmd as &Cmd)`
at `:690` should collapse to `RoutingInfo::for_routable(cmd)` or
`RoutingInfo::for_routable(&*cmd)`.

### R5 — `#[cfg(feature = "iam")]` divergence

IAM cfg sites inside the send_command path (pre-collapse):
- `:762-777` — IAM token-changed check BEFORE the dispatch. This
  block lives directly in `send_command`'s body, NOT in
  `execute_command_owned`. It takes `&mut self` via
  `self.update_connection_password(...)`. **Inlining does not move
  this block.** The cfg-off variant compiles to a no-op.
- No IAM cfg inside `execute_command_owned` itself — the dispatch,
  decompression, and handlers are all feature-independent.

**Verify in W1 diff**: the `#[cfg(feature = "iam")]` block at `:762`
must still fire BEFORE `get_or_initialize_client` at `:779`,
`get_request_timeout` at `:785`, `reserve_inflight_request` at `:795`,
and `cmd.set_inflight_tracker` at `:820`. Swapping the IAM block
past any of those would change timing: the IAM password update
should be applied on the internal_client BEFORE the dispatch
acquires its connection.

## §1 — Test inventory

All call sites that exercise the public `Client::send_command` or the
`ValkeyClientForTests` trait are enumerated below. Each entry lists
the file:line, the exact argument shape, and which contract the test
leans on (routing variant, cmd mutation pattern, post-call
inspection).

### `ferriskey/tests/test_client.rs`

All `test_basics.client` here is the public `Client` (struct at
`tests/test_client.rs:60-63`, NOT the StandaloneClient in
`utilities/mod.rs:652`).

| Line     | Shape                                                | Contract leaned on                                                                 |
|----------|------------------------------------------------------|------------------------------------------------------------------------------------|
| :176     | `(&mut cmd, Some(RoutingInfo::MultiNode((AllNodes,None))))` | Routing MultiNode dispatch; return Ok on CONFIG RESETSTAT across all nodes.        |
| :205     | `(&mut cmd, Some(RoutingInfo::MultiNode((AllNodes,None))))` | Same.                                                                              |
| :268     | `(&mut ferriskey::cmd("HELLO"), None)`               | HELLO post-dispatch handler (`handle_hello_command`) fires and updates stored protocol. |
| :279,:286,:294 | `(&mut cmd, None)`                              | Default routing (None → `for_routable` fallback). Std operation.                  |
| :687     | `(&mut ferriskey::cmd("PING"), None)`                | Default routing, PING result Ok check.                                            |
| :697-700 | `(ferriskey::cmd("SET").arg(&key).arg("test_value"), None)` | **Temporary `&mut Cmd` via `.arg()` returning `&mut Cmd`.** Relies on the public signature accepting `&mut Cmd`. |
| :708     | `(ferriskey::cmd("GET").arg(&key), None)`            | Same temporary pattern.                                                            |
| :789,:799-802,:810 | same shapes as :687/:697/:708              | Same contracts.                                                                   |
| :1015,:1024,:1046 | `(ferriskey::cmd(...).arg(...), None)`      | Temporary-chain pattern.                                                          |
| :1289    | `(&mut client_info_cmd, None)`                       | Re-used cmd variable (caller mutates between calls).                              |
| :1309    | `(ferriskey::cmd("SET").arg(&test_key).arg(test_value), None)` | Temporary chain.                                                                  |
| :1323    | `(&mut client_info_cmd, None)`                       | Reused cmd.                                                                       |
| :1339    | `client_clone.send_command(&mut cmd, None)` inside `retry()`→`async`→`retry` | Retry loop: each iteration clones `client` and constructs a fresh `cmd`. Does NOT cross await boundary in spawn, but uses `&mut self` on the clone. |
| :1382    | `(ferriskey::cmd("GET").arg(&test_key), None)`       | Temporary chain.                                                                  |
| :1430, :1459, :1484, :1510 | `(&mut cmd, None)`                   | Error-path tests; inspect the `Result` shape on failure (Ok vs Err — no panics on dropped tracker). |
| :1539-1543 | `(*cmd*, Some(RoutingInfo::MultiNode((AllNodes,None))))` | MultiNode routing.                                                                |
| :1584-1588 | `(*cmd*, Some(RoutingInfo::MultiNode((AllNodes,None))))` | MultiNode routing.                                                                |
| :1653    | `(&mut ferriskey::cmd("HELLO"), None)`               | HELLO handler.                                                                    |
| :2503    | `(&mut ferriskey::cmd("MSET"), None)`                | Default routing.                                                                  |
| :2603    | `(&mut client_info_cmd, None)`                       | Reused cmd.                                                                       |
| :2624    | `(&mut select_cmd, None)`                            | SELECT post-dispatch handler (`handle_select_command`) fires, updates database id. |

### `ferriskey/tests/test_cluster_client.rs`

| Line | Shape                                               | Contract                                                                  |
|------|-----------------------------------------------------|---------------------------------------------------------------------------|
| :67  | `(&mut cmd, None)`                                  | Default routing; `for_routable` cluster fallback.                         |
| :92-96, :123-127, :154-158, :185-189, :219-223, :256-260 | `(&mut cmd, Some(RoutingInfo::MultiNode((AllMasters or AllNodes, None))))` | MultiNode dispatch path (cluster/mod.rs:500 MultiCmd Arc<Cmd>). |
| :325 | `(&mut cmd, Some(routing_info))`                    | Explicit routing variant.                                                |
| :403, :421 | `(&mut client_info_cmd, None)`                | Reused cmd.                                                               |
| :435 | `(&mut cmd, None)`                                  | Inside `retry()` closure — same as test_client.rs:1339 pattern.           |
| :549 | `(&mut ferriskey::cmd("PING"), None)`               | Default routing.                                                          |
| :759 | `(&mut cmd, Some(routing))`                         | Explicit routing.                                                         |

### `ferriskey/tests/test_pubsub.rs`

| Line | Shape                                  | Contract                                                                                 |
|------|----------------------------------------|------------------------------------------------------------------------------------------|
| :793 | `(&mut sub_cmd, None)`                 | SUBSCRIBE: requires `intercept_pubsub_command(cmd)` at `:781` to run and mutate state.   |
| :801 | `(&mut pub_cmd, None)`                 | PUBLISH.                                                                                 |
| :815,:825,:835,:848 | `(&mut *_cmd, None)`          | SUBSCRIBE/UNSUBSCRIBE interleaved.                                                       |
| :866, :872 | `send_command(&mut sub, None)` inside `tokio::spawn(async move { ... })` | **Requires `BoxFuture<'a, …>: Send`.** client_clone is owned in the spawn; sub is constructed inside `async move`. If W1's collapse accidentally captured a non-Send value, this test fails to compile. |
| :885 | `(&mut get_subs, None)`                | PUBSUB CHANNELS; inspect return value for channel list.                                 |

### `ferriskey/tests/utilities/mod.rs`

| Line | Shape                                 | Contract                                                                                |
|------|---------------------------------------|-----------------------------------------------------------------------------------------|
| :632 | `(&mut get_command, None)`            | `send_set_and_get` helper — baseline GET after SET. Used by many tests as a smoke.      |
| :641, :644 | `(&mut set_command, None)` / `(&mut get_command, None)` | Same helper, with unwrap.                                                        |
| :868 | `(*cmd*, Some(*routing*))`            | Explicit routing helper.                                                                |
| :887 | `(&mut client_kill_cmd, Some(route))` | CLIENT KILL with explicit route.                                                        |
| :904 | `(&mut info_cmd, None)`               | INFO dispatch.                                                                          |
| :981 | `(&mut ping_cmd, None)`               | PING ping.                                                                              |

### `ferriskey/tests/test_standalone_client.rs` — EXCLUDED

Lines 98 and 115 pass `(&info_clients_cmd)` with a SINGLE argument —
these call `StandaloneClient::send_command(&mut self, cmd: &Cmd)` at
`standalone_client.rs:599`, NOT the public `Client::send_command`.
Collapse of `Client::send_command` does NOT affect these tests.
Listed here to prevent false-alarm grep hits.

### Trait surface — `ValkeyClientForTests`

`client/mod.rs:1934-1940`:
```rust
pub trait ValkeyClientForTests {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a mut Cmd,
        routing: Option<RoutingInfo>,
    ) -> BoxFuture<'a, Result<Value>>;
}
```

Three impls: Client (`:1942`), StandaloneClient (`:1952`),
ClusterConnection (`:1963`). Used inside pubsub tests for dyn
dispatch. **Trait signature is frozen by the public signature**;
any change to `Client::send_command`'s signature that doesn't match
this trait will fail the `impl ValkeyClientForTests for Client` at
`:1942`.

### Bench surface

`ferriskey/benches/connections_benchmark.rs` — grep returns 0 hits
for `.send_command(` in the benches directory. Tier 1 collapse is
invisible to benchmarks (they use the higher-level ClientBuilder
fluent API that delegates to `send_command` internally via
`CommandBuilder::execute`).

### Non-test consumers inside the crate

| Site                                            | Shape                                    | Contract                                                                     |
|-------------------------------------------------|------------------------------------------|------------------------------------------------------------------------------|
| `client/mod.rs:1143`                            | `(&mut eval, routing.clone())`           | Script/FCALL path (`invoke_script`).                                         |
| `client/mod.rs:1152-1153`                       | `(&mut load, None)`, `(&mut eval, routing)` | Script load-then-eval fallback.                                              |
| `client/mod.rs:1245`                            | `(&mut cmd, Some(routing))`              | IAM refresh-token internal path.                                             |
| `client/mod.rs:1948` (trait impl)               | `self.send_command(cmd, routing)`        | ValkeyClientForTests::send_command for Client.                               |
| `ferriskey_client.rs:365`                       | `inner.send_command(&mut cmd, None)`     | `Client::execute` from the fluent API. Every CommandBuilder.execute() hits here. |
| `ferriskey_client.rs:623`                       | `inner.send_command(cmd, routing)`       | CommandBuilder with explicit routing.                                        |
| `ferriskey_client.rs:697`                       | `inner.send_command(&mut cmd, None)`     | Second wrapper-level call (cmd(...).execute()).                             |

Three crate-internal call sites + one trait impl + three fluent-API
call sites. All match the public signature. Tier 1 collapse is a
drop-in replacement for them as long as §2's invariants hold.

---

## Cross-review harness for W1's PR

When W1's diff lands I will do the line-by-line review against this
doc and report a single consolidated message to manager. Specifically
I will verify:

1. `pub fn send_command<'a>(&'a mut self, cmd: &'a mut Cmd, routing: Option<RoutingInfo>) -> BoxFuture<'a, Result<Value>>` unchanged.
2. `ValkeyClientForTests::send_command` impl at `client/mod.rs:1942`
   still compiles against the trait at `:1934-1940`.
3. `cmd.set_inflight_tracker(tracker)` still called exactly once
   (trace the `reserve_inflight_request` → `set_inflight_tracker`
   chain in the inlined body).
4. `#[cfg(feature = "iam")]` block at pre-collapse `:762-777` still
   fires BEFORE `get_or_initialize_client`, `get_request_timeout`,
   and the tracker reservation.
5. MultiNode cluster dispatch — the `Arc::new(cmd.clone())` in
   `cluster/mod.rs:501` is untouched (not W1's scope).
6. No `self.clone()` or `Arc::new(cmd.clone())` in the inlined body
   (R1 + R2).
7. All four `handle_*_command` calls fire in-order AFTER the dispatch
   result is Ok — and are INSIDE the timeout-governed future, not
   hoisted past the select.
8. Grep confirms no new `#[allow(…)]`, no `unsafe`, no new `Box::pin`
   calls beyond the single return-shaping one.
9. `cargo test -p ferriskey --lib` and the full integration test
   suite green on no-default-features AND `--features iam`.
10. `cargo tree -p ferriskey --no-default-features` unchanged (Tier 1
    should NOT shift the dep graph).
