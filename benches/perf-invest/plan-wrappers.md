# Plan B — 4-way client wrapper simplification — for owner evaluation

**Author:** Worker-3
**Branch:** `feat/ferriskey-perf-invest`
**Status:** investigative scoping. No source changes proposed by this
document. All options below are for the project owner to evaluate and
sequence.

## §1 Current state

### 1.1 The outer struct

`ferriskey::Client` at `ferriskey/src/client/mod.rs:191-204`:

```rust
#[derive(Clone)]
pub struct Client {
    internal_client: Arc<RwLock<ClientWrapper>>,
    request_timeout: Duration,
    inflight_requests_allowed: Arc<AtomicIsize>,
    inflight_requests_limit: isize,
    inflight_log_interval: isize,
    iam_token_manager: Option<Arc<crate::iam::IAMTokenManager>>,
    compression_manager: Option<Arc<CompressionManager>>,
    pubsub_synchronizer: Arc<dyn PubSubSynchronizer>,
    otel_metadata: types::OTelMetadata,
}
```

Every command path clones this (`ferriskey_client.rs:364`
`(*self.0).clone()` inside `Client::execute`). That's:

- `Arc::clone` × 4 counters (internal_client, inflight_requests_allowed,
  iam_token_manager if Some, compression_manager if Some,
  pubsub_synchronizer)
- `Duration::clone` (trivial, 16 bytes)
- `isize::clone` × 2 (trivial)
- `OTelMetadata::clone` — need to inspect
- One `Arc<dyn PubSubSynchronizer>::clone` (trait-object vtable bump)

Per-command Arc-bump count when IAM + compression are absent
(bench case): 3 (internal_client, inflight_requests_allowed,
pubsub_synchronizer). See `report-w3.md` §2.1 step 3 + §F1.

### 1.2 The inner enum

`ClientWrapper` at `ferriskey/src/client/mod.rs:177-182`:

```rust
#[derive(Clone)]
pub enum ClientWrapper {
    Standalone(StandaloneClient),
    Cluster { client: ClusterConnection },
    Lazy(Box<LazyClient>),
}
```

`LazyClient` at lines 185-189:

```rust
#[derive(Clone)]
pub struct LazyClient {
    config: ConnectionRequest,
    push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
}
```

### 1.3 What "Lazy" means

`ClientWrapper::Lazy(_)` is the "not-yet-connected" state the client
sits in when the builder is invoked with `.lazy_connect()`. The
connection is deferred until the first command. Construction path:

- `Client::connect(...)` at `ferriskey/src/client/mod.rs:1810-1905`:
  - line 1814 constructs `ClientWrapper::Lazy(...)` as the *initial*
    state unconditionally (for pubsub-synchronizer wiring).
  - line 1880-1905 reads `request.lazy_connect`:
    - if `lazy_connect` → stays `Lazy`.
    - else if cluster → `ClientWrapper::Cluster { client }`.
    - else → `ClientWrapper::Standalone(client)`.
  - line 1908 replaces the Lazy wrapper with the real one under a
    write lock.

So the lifetime of `ClientWrapper::Lazy` is either:

1. **Transient** (normal path): from line 1814 to line 1908 — a few
   microseconds during construction. Then the wrapper is replaced
   with Standalone or Cluster and never sees Lazy again.
2. **Persistent** (opt-in): when `request.lazy_connect == true`, the
   Lazy variant survives construction and only gets promoted when the
   first command arrives via `get_or_initialize_client` at line 632.

### 1.4 The `Lazy(_) => Err(...)` match arms

`ClientWrapper::Lazy(_)` appears in 17 match arms across
`ferriskey/src/client/mod.rs` (lines 397, 437, 493, 511, 625, 742,
933, 1077, 1151, 1232, 1308, 1420; plus 5 non-error-returning sites
handling initialization). **Every non-construction arm returns
`Err(ErrorKind::ClientError, "Client not yet initialized")`.**

In other words: nearly every public method on `Client` that accepts a
`ClientWrapper` needs to handle an unreachable-once-construction-is-
complete case, because *in principle* a caller could invoke the
method between construction and first-command when lazy_connect is
enabled.

Only **5 arms** do substantive work with Lazy:

- line 635 — `get_or_initialize_client` read-path (matches `Lazy`, does the
  promotion)
- line 643 — `get_or_initialize_client` write-path (does the promotion)
- line 659 — second write-phase promotion check
- line 1880-1905 — initial construction branch (wraps request into Lazy)
- line 2280 — tests

The other **12 arms** are "defensive" error paths that exist solely
because Lazy is a valid state of the enum.

### 1.5 `get_or_initialize_client` is per-command

`client/mod.rs:632-709`:

```rust
async fn get_or_initialize_client(&self) -> Result<ClientWrapper> {
    {
        let guard = self.internal_client.read().await;
        if !matches!(&*guard, ClientWrapper::Lazy(_)) {
            return Ok(guard.clone());  // happy path — already initialized
        }
    }
    // …lazy-init path — tens of lines…
}
```

Called on every `send_command` invocation
(`ferriskey/src/client/mod.rs:811`). The happy-path cost:

1. `self.internal_client.read().await` — tokio `RwLock` read guard
   acquire
2. `matches!(&*guard, ClientWrapper::Lazy(_))` — one discriminant
   check
3. `guard.clone()` — `ClientWrapper::Clone` which, for
   `Standalone(StandaloneClient)`, does `Arc::clone` on
   `StandaloneClient.inner` (cheap bump) — BUT see Plan A §1.2: the
   clone then drops at the end of the command's async block,
   triggering `StandaloneClient::drop` and a global
   `Telemetry::decr_total_clients` call.

Two interactions make this particularly costly:

1. The `Arc<RwLock<ClientWrapper>>` read is a tokio async lock acquire
   per command. Uncontended: fast (one atomic CAS). Contended across
   16 workers on the same client: serialisation point.
2. The `guard.clone()` + drop round-trip wakes the T5 telemetry
   hot-path in Plan A.

### 1.6 `Arc<RwLock<ClientWrapper>>` — why the RwLock?

The RwLock exists because:

- `ClientWrapper::Lazy → Standalone/Cluster` promotion is a **write**
  to the enum discriminant, hence a write lock at
  `get_or_initialize_client` lines 642 + 657.
- IAM token rotation updates `StandaloneClient` state — see the write
  guard acquisition at line 1876 for `iam_token_manager = ...`.
  That's a ClientBuilder-time write though, not per-command.

When no lazy_connect and no IAM token rotation mid-connection: the
RwLock is **write-acquired exactly twice** (initial construction at
1814 and replacement at 1908) and then only read-acquired. Even so,
the read-acquisition itself still has a cost (tokio lock
state-machine).

### 1.7 Cluster vs standalone shape

`ClusterConnection` at `ferriskey/src/cluster/` is substantial —
`cluster/mod.rs` alone is ~2600 LOC, with moved/asking redirect
handling, cluster-slot cache, per-node reconnecting connections, and
routing logic. The ClientWrapper::Cluster { client: ClusterConnection }
variant is large and internally has its own mutable state.

`StandaloneClient` at `ferriskey/src/client/standalone_client.rs:57-60`
is much smaller:

```rust
#[derive(Clone, Debug)]
pub struct StandaloneClient {
    inner: Arc<DropWrapper>,
}

struct DropWrapper {
    primary_index: usize,
    nodes: Vec<ReconnectingConnection>,
    read_from: ReadFrom,
    read_only: bool,
}
```

A concrete, immutable-after-construction struct. The `Arc<DropWrapper>`
is the sharing primitive; the RwLock around it (inside `ClientWrapper`)
adds no concurrent-mutation value for the standalone case.

## §2 Intent

### 2.1 Why the 4-way shape exists in glide-core

glide-core is a single codebase serving **multiple language bindings**
(Python, Java, Node, Go). Each binding's FFI surface needs:

1. **Async cluster awareness** — Python's asyncio + Java's CompletableFuture
   etc. both need to talk to the same underlying cluster client type.
2. **Deferred connection** — binding callers create a Client handle from
   their language's synchronous constructor and only pay the connect
   cost on first use (matches expectations in languages where "open
   a connection" is a discrete action from "start using it").
3. **Runtime reconfiguration** — some language bindings support
   "reconnect to different cluster nodes without replacing the Client
   handle", which requires the enum to change discriminant
   mid-lifetime.

The 4-way (cluster × standalone) × (lazy × non-lazy) product gives
glide-core a single `ClientWrapper` enum that handles all the
combinations a language binding might throw at it.

### 2.2 What pure-Rust ferriskey doesn't need

A pure-Rust `ferriskey::Client` consumer (FlowFabric included) holds
the Client by ownership or by Arc and never needs to:

- Upgrade from Lazy to Standalone/Cluster (they construct
  with `.build()` which always connects).
- Downgrade or swap cluster type mid-lifetime (they'd drop the Client
  and build a new one).
- Share the Client with a foreign runtime (there's one tokio runtime).

A pure-Rust shape could be:

```rust
pub enum Client {
    Standalone(Arc<StandaloneClient>),
    Cluster(Arc<ClusterConnection>),
}
```

No RwLock, no Lazy variant, no per-command `get_or_initialize_client`
hop. The `Arc` handles sharing; the enum handles the two flavours.

## §3 Proposed changes (options — owner decides)

### Option 1 — REMOVE the Lazy variant; keep the RwLock

Make the constructor always eagerly connect. Delete
`ClientWrapper::Lazy` and the 17 match arms that handle it. The
outer `Arc<RwLock<ClientWrapper>>` stays, but `get_or_initialize_client`
becomes:

```rust
async fn get_client(&self) -> ClientWrapper {
    self.internal_client.read().await.clone()
}
```

or, if IAM rotation still needs mutation, a read-then-clone.

**Pros:**
- Removes 12 defensive error arms — ~60 LOC simplification.
- Removes the 5 lazy-promotion sites — ~50 LOC simplification.
- Removes one error return path from every command-adjacent public
  API, which is a user-visible simplification of the error type.
- No hot-path change: per-command `RwLock::read` + clone still
  happens.
- No breaking change for users who never set `.lazy_connect()`.
- Language bindings that use lazy_connect would break — **scope the
  blast radius by grepping non-ferriskey use cases first.**

**Cons:**
- Language bindings that *do* use lazy_connect lose a feature. If
  Python/Java bindings materialize clients synchronously and only
  connect lazily, this removes that affordance.
- Doesn't reduce per-command RwLock-read cost.

### Option 2 — COLLAPSE Client to an enum with no RwLock

Replace `Client { internal_client: Arc<RwLock<ClientWrapper>>, … }`
with:

```rust
#[derive(Clone)]
pub enum Client {
    Standalone {
        inner: Arc<StandaloneClient>,
        /* the rest of Client's fields: request_timeout, inflight_*, … */
    },
    Cluster {
        inner: Arc<ClusterConnection>,
        /* same */
    },
}
```

(or lift the shared fields into an `Arc<ClientInner>` and have the
enum only differentiate the connection type.)

**Pros:**
- Removes the `RwLock::read().await` per command.
- Removes the `ClientWrapper::clone()` + drop round-trip that wakes
  the Telemetry T5 hot-path — the `Client` enum's `clone` is just
  `Arc::clone`s.
- Eliminates Lazy as a design concept (Option 1's benefits carry
  over).
- Matches the shape pure-Rust callers actually want.

**Cons:**
- Bigger rewrite than Option 1. Every `ClientWrapper` match site
  would be re-shaped. Estimated 200-300 LOC touched.
- IAM token rotation currently relies on the `Arc<RwLock<>>` for
  mid-lifetime mutation of `iam_token_manager`. Rotation has to move
  to interior mutability in `StandaloneClient` / `ClusterConnection`
  or to a `tokio::sync::watch` channel pattern.
- Breaks any language-binding consumer that relies on the
  mid-lifetime reconfiguration capability (swapping cluster
  endpoints on a live Client).
- Lazy connect could still be re-offered as a separate constructor
  function (`Client::connect_lazy(...)` that spawns a task to
  complete the connection in the background), but it becomes a
  different API shape, not a variant of the enum.

### Option 3 — KEEP the 4-way shape; add a fast-path for Standalone

Introduce a `Client::Standalone` top-level variant that bypasses
`get_or_initialize_client`'s RwLock read when the wrapper is known
non-Lazy + non-Cluster. One way: store a `OnceLock<StandaloneClient>`
alongside the `Arc<RwLock<ClientWrapper>>`; first successful
initialization populates the OnceLock; subsequent commands hit the
OnceLock and skip the RwLock.

**Pros:**
- Minimal invasive change — no public API break.
- Language bindings retain the 4-way shape.
- Hot path gets faster: `OnceLock::get()` is an atomic load (zero
  cost when populated).

**Cons:**
- Two ways to store the same state is a footgun — the OnceLock and
  the RwLock need to agree, and if IAM rotation updates the RwLock
  version, the OnceLock becomes stale.
- Adds complexity instead of removing it. Owner explicitly flagged
  "inherited glide-core baggage" as the thing to remove, not layer
  over.

## §4 Estimated effort per option

| Option | Files touched | Size | Public API break | Per-command cost after |
|---|---|---|---|---|
| 1 (remove Lazy) | `client/mod.rs` body + `LazyClient` struct deletion + call-sites | S (1-2 days) | Yes (`.lazy_connect()` on ClientBuilder becomes a no-op or gets removed) | Unchanged |
| 2 (collapse to enum w/o RwLock) | `client/mod.rs` full rework + `ferriskey_client.rs:358-367` + downstream | M-L (3-5 days) | Yes (Client shape changes) | Skips RwLock + one Arc<Client> clone on hot path |
| 3 (OnceLock fast-path) | `client/mod.rs` additions only | S (1 day) | No | Skips RwLock on hot path |

## §5 Open questions for owner decision

1. **Which language bindings currently rely on `.lazy_connect()`?**
   If Python/Java/Node consumers use it, Options 1 and 2 break them;
   Option 3 preserves it. If no binding uses it, it's vestigial and
   Option 1 is low-risk.
2. **Does any binding swap cluster endpoints on a live Client (i.e.
   rely on the `Arc<RwLock<ClientWrapper>>` being mutable
   mid-lifetime)?** If no, Option 2's removal of the RwLock is
   straight-up better. If yes, Option 2 needs an alternative story
   for runtime reconfig.
3. **Would `Client::connect_lazy(...)` -> spawned-background-task
   be an acceptable replacement for `.lazy_connect()` if Option 2
   drops the Lazy variant?** The call-site shape changes but the
   lazy-connect intent (cheap synchronous-ish construction) survives.
4. **Is `IAMTokenManager` mid-lifetime rotation a real requirement?**
   See `client/mod.rs:1876`:
   `client_guard.iam_token_manager = iam_token_manager.clone()` —
   this is a write under the RwLock. If IAM tokens are only set at
   construction, the RwLock isn't needed for IAM. If they can be
   replaced mid-lifetime, Option 2 needs a `tokio::sync::watch` or
   equivalent for the rotation path.
5. **Is the 4-way shape needed for TLS mode switching or other
   connection-level reconfig?** Glide-core doesn't do this; worth
   confirming ferriskey doesn't expose an API that does either.

## §6 Cross-reference to Round 1 perf reports

- `report-w3.md` §F1 ("Client::clone at top of every execute = 5
  Arc::clone + trait-object clone") quantifies the per-command Client
  clone cost. Option 2 directly reduces this by removing the
  `Arc<RwLock<>>` indirection.
- `report-w3.md` §2.1 step 3 ("Client::execute clones the inner
  Client (`(*self.0).clone()`)") is the same observation.
- Plan A §1.2 (T5 `Telemetry::decr_total_clients` in
  `StandaloneClient::drop`) is partially amplified by the
  `ClientWrapper::Standalone(client)` move-into-async-block pattern
  at `client/mod.rs:721`. Option 2's enum-over-Arc layout would
  clone `Arc<StandaloneClient>` instead of moving an owned
  StandaloneClient into the async block, eliminating the drop-based
  Telemetry fire at the end of each command. **Option 2 and Plan A
  interact: doing either one alone reduces the cost; doing both
  compounds.**

## §7 Anti-goals

- **Not a recommendation.** The 4-way shape carries real utility for
  language bindings the owner has visibility into that I don't. §5
  enumerates the questions that gate which option is right.
- **Not a breakdown of runtime reconfiguration requirements.** I
  inferred from the source that the `Arc<RwLock<>>` is mostly
  there for Lazy→real promotion + IAM rotation. If bindings rely
  on other mid-lifetime mutations, my analysis is incomplete.
- **Not advocacy for dropping `.lazy_connect()`.** Several of the
  options (1, 2) drop it; option 3 keeps it. I'm listing the tradeoff,
  not picking.
