# FerrisKey Design Debt

Inherited from valkey-glide/redis-rs fork. Tracked for systematic redesign.

## 1. Module Structure — `valkey` submodule is wrong

The entire core lives under `src/valkey/` — a name from when this was a generic Redis library.
FerrisKey is the library. The `valkey` prefix adds nothing.

**Current:** `ferriskey::valkey::aio::ConnectionLike`, `ferriskey::valkey::cmd`
**Target:** `ferriskey::aio::ConnectionLike`, `ferriskey::cmd`

**Plan:** Flatten `src/valkey/` into `src/`. Move `valkey/mod.rs` exports to `lib.rs`.
This is a large but mechanical refactor (every `use crate::valkey::` becomes `use crate::`).

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

## 6. Value type — Server errors mixed with data

`Value` enum has `ServerError(ServerError)` as a variant alongside `Int`, `BulkString`, etc.
This means every `Value` consumer must check for errors, even in successful responses.
The parser wraps errors inside `Ok(Value::ServerError(...))` instead of `Err(...)`.

**Current:** `Ok(Value::ServerError(e))` — successful parse of an error response
**Problem:** Callers must call `.extract_error()` to get the actual error
**Target:** Parser returns `Err(ValkeyError::Server(e))` for error responses.
`Value` only contains data variants.

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

## 9. Error types — Two parallel hierarchies

**Status (2026-04-12): Partially done.**
- ~~`IAMError`~~ — From<IAMError> for ValkeyError added; IAMError being eliminated
- ~~`StandaloneClientConnectionError`~~ — deleted (47 lines); create_client returns ValkeyError
- ~~`ConnectionError`~~ — being deleted (~40 lines); Client::new returns ValkeyError
- `ServerError` / `ServerErrorKind` — **deferred**. 16 direct construction sites (9 in cluster/pipeline alone). Performance-critical redirect logic. Needs benchmarking when touched. Can collapse to `ValkeyError(ErrorKind::Moved/Ask, ...)` but large effort.

Remaining:
- `ValkeyError` (value.rs) — the main error type
- `ServerError` / `ServerErrorKind` — server-specific errors (MOVED/ASK, inline error responses). Deferred: large effort, hot path.

**Target:** Single error enum with variants. Use `thiserror` derive consistently.

## 10. Feature flags — Inconsistent gating

**Status (2026-04-12): DONE.**
- Removed `testing`, `standalone_heartbeat`, `iam_tests`, `mock-pubsub` features
- Merged into: `default=[]`, `test-util=[]`
- Heartbeat always-on; test infrastructure behind `test-util`

---

## Priority

| # | Status | Impact | Effort | When |
|---|---|---|---|---|
| 1 | ✓ DONE | Medium (structure) | Large | Module migration complete |
| 2 | Open | High (user-facing) | Medium | Next: rename public types |
| 3 | ✓ DONE (partial) | High (user-facing) | Medium | Client + convenience methods + pipeline |
| 4 | Open | Medium (correctness) | Medium | PubSub rewrite — dedicated session |
| 5 | Open | Medium (simplicity) | Large | Box::leak in connection/factory.rs — after lifecycle work |
| 6 | Open | Medium (correctness) | Large | Value::ServerError variant — with ServerError collapse |
| 7 | Open | Low (perf) | Medium | Cmd double-allocation — after profiling |
| 8 | ✓ DONE | Low (perf) | Small | Arc<str> node addresses |
| 9 | Partial | Low (clarity) | Mixed | IAMError+Standalone+Connection done; ServerError deferred |
| 10 | ✓ DONE | Low (DX) | Small | Feature flags consolidated |
