# FerrisKey Design Debt

Inherited from valkey-glide/redis-rs fork. Tracked for systematic redesign.

## 1. Module Structure — `valkey` submodule is wrong

The entire core lives under `src/valkey/` — a name from when this was a generic Redis library.
FerrisKey is the library. The `valkey` prefix adds nothing.

**Current:** `ferriskey::valkey::aio::ConnectionLike`, `ferriskey::valkey::cmd`
**Target:** `ferriskey::aio::ConnectionLike`, `ferriskey::cmd`

**Status (2026-04-12): DONE.**
- `src/valkey/` is gone
- modules live under `src/cluster`, `src/connection`, `src/protocol`, `src/cmd`, `src/pipeline`, `src/value`
- old `crate::valkey::...` paths have been cleaned from the main source tree

## 2. Public API naming — Exposes internals

| Current | Problem | Target |
|---|---|---|
| `MultiplexedConnection` | Users don't know what "multiplexed" means | `Connection` |
| `ClusterConnection` | It's a client, not a connection | `ClusterClient` |
| `ConnectionLike` | Vague | `Client` trait |
| `req_packed_command(&cmd)` | "packed"? "req"? | `send(&cmd)` or `execute(&cmd)` |
| `get_packed_command()` | Serialization detail | `encode()` (or make internal) |
| `get_packed_pipeline()` | Same | `encode()` (or make internal) |
| `send_packed_bytes()` | Internal optimization | Make `pub(crate)` only |
| `ValueCodec` | RESP codec detail | Internal only |
| `parse_valkey_value()` | RESP parser detail | `parse_resp()` or internal |
| `FromValkeyValue` | Verbose | `FromValue` |
| `ToValkeyArgs` | Verbose | `ToArgs` |
| `ValkeyResult<T>` | Verbose | `Result<T>` (type alias in crate root) |
| `ValkeyError` | Verbose | `Error` |

**Status (2026-04-12): Mostly done.**
- `FromValue`, `ToArgs`, `Error`, and `Result<T>` aliases exist at the crate root
- `connect()` / `connect_cluster()` free functions exist at the crate root
- `ValkeyFuture` — now `pub(crate)`, removed from lib.rs re-exports
- `Arg` — now `pub(crate)`, removed from lib.rs re-exports
- `ValkeyWrite` — removed from lib.rs re-exports (stays `pub` in value.rs: required by public `ToValkeyArgs` trait)
- 19 internal-only methods in cmd.rs (11) and pipeline.rs (8) tightened to `pub(crate)`
- `ConnectionRequest` — still public, tracked in #11
- Remaining: type renames (`ValkeyError`→`Error`, `FromValkeyValue`→`FromValue`, etc.) deferred to avoid churn

## 3. Missing convenience API

Users have to write:
```rust
conn.req_packed_command(&cmd("GET").arg("key")).await?
```

Should be:
```rust
conn.get("key").await?
// or
conn.execute(cmd("GET").arg("key")).await?
```

**Plan:** Add `ferriskey::api` module with ergonomic wrappers:
- `Client` trait with `get()`, `set()`, `del()`, `lpush()`, `rpop()`, etc.
- `connect("valkey://host:port")` factory function
- `Pipeline::new().get("k1").set("k2", "v2").execute(&mut conn).await?`

**Status (2026-04-12): DONE for initial surface.**
- high-level `Client` wrapper exists
- `ClientBuilder` exists
- convenience methods exist for the initial key/hash/list surface
- typed pipeline exists
- crate-root `connect()` / `connect_cluster()` exist

## 4. PubSub — Polling-based synchronizer

**Current:** 800-line synchronizer with 3s polling, two-state diff (desired vs current_by_address),
address-level tracking, pending unsubscribes queue, OnceCell+Weak client reference.

**Target:** Event-driven, ~400 lines:
- `subscriptions` as single source of truth (desired only)
- `on_node_reconnect()` — reapply exact+pattern on reconnected node
- `on_slots_moved()` — targeted sharded resubscribe on new slot owner
- Confirmation tracking via push message callbacks, not polling
- Explicit unsubscribe from old nodes on topology change

**Why not simpler:** Confirmation-based correctness matters (fire-and-forget isn't safe).
Sharded PubSub is slot-routed. Failover blast radius must be minimal.

**Status (2026-04-12): DONE.**
- `EventDrivenSynchronizer` in `src/pubsub/synchronizer.rs` (~1043 lines)
- Event-driven model replaces polling-based synchronizer
- Topology change handling via `handle_topology_changed()`
- Confirmation tracking via push message callbacks

## 5. Connection lifecycle — Too many types

```
ConnectionInfo → Client → MultiplexedConnection → Pipeline<Bytes>
                        ↘ ClusterClient → ClusterConnection → ClusterConnInner → Core → ConnectionsContainer
```

5+ types to go from "I have a URL" to "I can send a command." Redis-rs heritage.

**Target:**
```
Config → Connection (standalone) or ClusterClient (cluster)
```

Both implement `Client` trait. Connection details are internal.

**Status (2026-04-12): DONE.**
- low-level client factory lives in `src/connection/factory.rs`
- `Box::leak` eliminated
- `MultiplexedConnection::new_with_response_timeout` now takes owned `ConnectionInfo`

## 6. Value type — Server errors mixed with data

`Value` enum still has `ServerError(ValkeyError)` as a variant alongside `Int`, `BulkString`, etc.
This means every `Value` consumer must check for errors, even in successful responses.
Top-level parser errors no longer come back as `Value::ServerError(...)`, but nested array elements still can.

**Status (2026-04-12): Partial.**
- Parser now returns `Err(ValkeyError)` for top-level error responses (`parse_error`, `parse_blob_error`)
- `cluster/pipeline.rs` `NodeResponse` changed to `ValkeyResult<Value>` — per-command errors are `Err`
- `is_transaction` field removed from the multiplexed accumulator (dead code)

Remaining for full completion:
- `Value::ServerError` stays — still needed for error elements inside RESP arrays
  (`EXEC` responses, RESP3 inline errors in arrays use `parse_array`, which catches `Err` and stores `ServerError`)
- Full removal requires changing `Value::Array(Vec<Value>)` to `Vec<ValkeyResult<Value>>`
- That cascades into `FromValkeyValue` implementations for tuples, `Pipeline::query_async` semantics, and the public API
- Dedicated session required

**Long-term target:** parser and response model return errors directly without embedding them inside `Value`.

## 7. Command building — Double allocation

`Cmd` stores args in `data: Vec<u8>` + `args: Vec<Arg<usize>>` (offsets).
`get_packed_command()` re-serializes into RESP format (another `Vec<u8>`).
So every command allocates twice: once for building, once for serialization.

**Target:** Build directly in RESP format. `Cmd` writes RESP bytes as args are added.
`get_packed_command()` returns a view, not a new allocation.

## 8. Cluster routing — `connections_container.rs` String cloning

`connection_for_address()` and `connection_for_route()` clone the node address `String`
on every lookup (DashMap returns references, but we clone to return owned values).
At 250K ops/sec, that's 250K String allocations for addresses.

**Target:** Use `Arc<str>` for node addresses. Clone is refcount bump, not heap allocation.

**Status (2026-04-12): DONE.**
- `Arc<str>` migration completed

## 9. Error types — Two parallel hierarchies

**Status (2026-04-12): DONE.**
- ~~`IAMError`~~ — eliminated
- ~~`StandaloneClientConnectionError`~~ — eliminated
- ~~`ConnectionError`~~ — eliminated
- ~~`ServerError` / `ServerErrorKind`~~ — eliminated

**Target:** Single error enum with variants. Use `thiserror` derive consistently.

**Result:** `ValkeyError` is now the single error carrier in the core library. Legacy parallel helper error types have been removed.

## 10. Feature flags — Inconsistent gating

**Status (2026-04-12): DONE.**
- Removed `testing`, `standalone_heartbeat`, `iam_tests`, `mock-pubsub` features
- Merged into: `default=[]`, `test-util=[]`
- Heartbeat always-on; test infrastructure behind `test-util`

## 11. ConnectionRequest exposure — low-level config leaked as public surface

`ConnectionRequest` is still re-exported at the crate root. It is useful today for tests, internal wrappers, and FFI/language-binding layers, but it is not part of the intended end-user API in `API_DESIGN.md`.

**Target:**
- user-facing code should prefer `ClientBuilder`, `connect()`, and `connect_cluster()`
- `ConnectionRequest` should eventually become `pub(crate)` or move behind a clearly internal/bindings-only module
- tests and binding adapters should migrate first before visibility is tightened

---

## Priority

| # | Status | Impact | Effort | When |
|---|---|---|---|---|
| 6 | Partial | Medium (correctness / design) | Large | Dedicated session: remove nested `Value::ServerError(...)` from array semantics |
| 11 | Open | Medium (API hygiene) | Medium | Hide `ConnectionRequest` after tests/bindings migrate |
| 2 | Mostly done | High (public API cleanup) | Small | Type renames deferred; visibility tightening complete |
| 7 | Open | Low (perf) | Medium | Cmd double-allocation — after profiling |
| 1 | ✓ DONE | Medium (structure) | Large | Module migration complete |
| 3 | ✓ DONE | High (user-facing) | Medium | High-level API + typed pipeline complete |
| 4 | ✓ DONE | High (correctness / resiliency) | Large | EventDrivenSynchronizer replaces polling |
| 5 | ✓ DONE | Medium (ownership / lifecycle) | Large | `Box::leak` eliminated in `connection/factory.rs` |
| 9 | ✓ DONE | Medium (clarity) | Large | Legacy parallel error types removed; `ValkeyError` unified |
| 8 | ✓ DONE | Low (perf) | Small | Arc<str> node addresses |
| 10 | ✓ DONE | Low (DX) | Small | Feature flags consolidated |
