# BackendTimeouts / BackendRetry wiring audit (2026-04-22)

Investigation-only, Worker FF. Scope: verify the "placeholder" status flagged
by Worker EE's Stage 1c scope audit so Stage 1c planning has empirical
evidence of what is plumbed-but-ignored today, and whether ferriskey exposes
the knobs Stage 1c will need to thread through.

Checkout: `/home/ubuntu/ff-worker-a-58.6/` (worktree used this session).

## Type definitions

From `crates/ff-core/src/backend.rs:568-587`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendTimeouts {
    pub request: Option<Duration>,      // :572
    pub keepalive: Option<Duration>,    // :574
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendRetry {
    pub max_attempts: Option<u32>,      // :584
    pub base_backoff: Option<Duration>, // :586
}
```

Both `#[non_exhaustive]`, both `Default`. `None` documented as "backend
default" per field doc-comments. Embedded on `BackendConfig` at
`crates/ff-core/src/backend.rs:637-638`.

## Current reference sites

| File:line | Reference | Role |
|-----------|-----------|------|
| `crates/ff-core/src/backend.rs:570` | `pub struct BackendTimeouts` | definition |
| `crates/ff-core/src/backend.rs:581` | `pub struct BackendRetry` | definition |
| `crates/ff-core/src/backend.rs:637-638` | fields on `BackendConfig` | pass-through storage |
| `crates/ff-core/src/backend.rs:647-648` | `BackendTimeouts::default()` / `BackendRetry::default()` in `BackendConfig::valkey` | construction (defaults only) |
| `crates/ff-core/src/backend.rs:1080-1081` | `assert_eq!(..., ::default())` in `backend_config_valkey_ctor` test | asserts defaults, does not exercise behaviour |
| `crates/ff-backend-valkey/src/lib.rs:91` | doc-comment "Stage 1c task" | placeholder marker only |

No **read** of `config.timeouts` or `config.retry` anywhere in the codebase.
`ValkeyBackend::connect` destructures `config.connection` and drops the other
two fields on the floor (`crates/ff-backend-valkey/src/lib.rs:92-154`): dial
goes through URL-based `ferriskey::Client::connect` / `connect_cluster`
which cannot accept timeout or retry overrides.

No test — unit, integration, or e2e — exercises a non-default
`BackendTimeouts` or `BackendRetry`. The single reference in the `#[cfg(test)]`
block at `crates/ff-core/src/backend.rs:1080-1081` only asserts that
`BackendConfig::valkey` produces defaults.

**Classification:** pure placeholder. This is exactly the shape Worker EE
flagged — structurally identical to the 0.3.2 ScannerFilter namespace field
regression (field present on the public API, silently ignored at the
backend layer).

## ferriskey ClientBuilder capabilities

From `ferriskey/src/ferriskey_client.rs` (`ClientBuilder`, lines 28–580ish)
and `ferriskey/src/client/types.rs`:

| Knob | Available? | Shape |
|------|-----------|-------|
| Per-request timeout | Yes | `ClientBuilder::request_timeout(Duration)` at `ferriskey_client.rs:445` → `ConnectionRequest::request_timeout: Option<u32>` ms |
| Connect timeout | Yes | `ClientBuilder::connect_timeout(Duration)` at `ferriskey_client.rs:439` |
| Blocking-command extension | Yes | `ClientBuilder::blocking_cmd_timeout_extension(Duration)` at `ferriskey_client.rs:465` |
| Idle / socket keepalive | **No** | No `keepalive`/`tcp_keepalive` on `ClientBuilder` or `ConnectionRequest` (`grep` empty across `ferriskey/src/`) |
| TCP_NODELAY | Yes | `ClientBuilder::tcp_nodelay()` at `ferriskey_client.rs:519` |
| Max in-flight | Yes | `ClientBuilder::max_inflight(u32)` at `ferriskey_client.rs:471` |
| Connection retry strategy | Yes (reconnect only) | `ClientBuilder::retry_strategy(ConnectionRetryStrategy)` at `ferriskey_client.rs:477`; struct at `client/types.rs:151`: `exponent_base: u32`, `factor: u32`, `number_of_retries: u32`, `jitter_percent: Option<u32>` |
| Per-operation retry loop | **No** | `ConnectionRetryStrategy` only drives **reconnect** after transport loss; no first-class retry loop around user commands (only pipeline-specific `PipelineRetryStrategy` on `send_pipeline`, `client/mod.rs:1071`) |
| Automatic reconnect | Yes | `ReconnectingConnection` (`ferriskey/src/client/reconnecting_connection.rs`); driven by the `ConnectionRetryStrategy` above |

## Gap analysis per field

| Field | ferriskey mapping | Stage 1c action |
|-------|-------------------|-----------------|
| `BackendTimeouts.request` | Direct: `ClientBuilder::request_timeout` | **Mechanical wiring.** Thread through `connect`. Requires migrating `ValkeyBackend::connect` off `Client::connect(url)` onto `ClientBuilder::url(url)?.request_timeout(..).build().await`. |
| `BackendTimeouts.keepalive` | **None** | **Semantic decision + possible upstream change.** Options: (a) drop the field (and un-ship from `#[non_exhaustive]` API — tolerable since field is additive and never read); (b) add `tcp_keepalive` to ferriskey `ClientBuilder` (upstream-ish, owner-auth); (c) clarify semantics — is this intended as OS-level `SO_KEEPALIVE`, application-level PING cadence, or RESP3 push-channel heartbeat? Doc-comment says "idle-connection keepalive interval" which most naturally maps to TCP keepalive not present in ferriskey. **Recommend: clarify semantics first; if TCP keepalive, land on ferriskey ClientBuilder before Stage 1c wiring.** |
| `BackendRetry.max_attempts` | Partial: maps to `ConnectionRetryStrategy.number_of_retries` **but only for reconnect**, not per-operation retry | **Semantic decision.** Two candidate meanings: (1) reconnect attempts on transport loss — maps cleanly to `ConnectionRetryStrategy.number_of_retries`; (2) per-operation retry attempts for transient errors — **ferriskey has no such loop**, Stage 1c would have to BUILD one inside `ff-backend-valkey` (wrap every FCALL; decide which `EngineError` variants are retriable). The `base_backoff` pairing suggests (2). Owner call needed. |
| `BackendRetry.base_backoff` | Partial: maps to `ConnectionRetryStrategy.factor` (ms) if reconnect-semantics; no mapping if per-operation-semantics | Same as above; ferriskey's backoff formula is `factor * exponent_base^attempt + jitter` — richer than a single base `Duration`, so even the reconnect mapping loses fidelity (no exponent/jitter knobs on `BackendRetry`). |

**Summary:** 1 of 4 fields (`timeouts.request`) is a mechanical pass-through.
The other 3 need semantic clarification or behaviour-side implementation
before Stage 1c can wire them without repeating the ScannerFilter-namespace
regression.

## Open questions for Stage 1c planning

1. **`BackendTimeouts.keepalive` semantics.** Is this intended to mean OS TCP
   keepalive (not exposed by ferriskey), application-level PING cadence (not
   implemented), or something else? If ferriskey-upstream work is required,
   does it land before or alongside Stage 1c? Punt-and-remove is an option
   since the field has zero callers.
2. **`BackendRetry` scope: reconnect vs. per-operation retry.** Field names
   (`max_attempts`, `base_backoff`) read as per-operation, but ferriskey's
   only retry primitive (`ConnectionRetryStrategy`) is reconnect-only. Stage
   1c either (a) narrows the docs to "reconnect attempts" and maps to
   ferriskey directly, or (b) commits to building an FF-side retry loop in
   `ff-backend-valkey` and defining which `EngineError` variants are
   retriable (`Transport`? `Unavailable`? not `Conflict`/`NotFound`).
3. **Fidelity loss on backoff.** Even under reconnect-semantics,
   `BackendRetry.base_backoff: Option<Duration>` cannot express ferriskey's
   `factor * exponent_base^attempt + jitter` formula. Does Stage 1c extend
   `BackendRetry` with `exponent_base` / `jitter_percent` fields now (under
   `#[non_exhaustive]` this is additive), or accept the fidelity loss and
   default the other knobs?

Adjacent but out-of-scope flags:

- **`WorkerConfig` forwarding** (RFC-012 §5.1) — migrating `WorkerConfig::host/port/tls/cluster` onto `BackendConfig` is the named Stage 1c job per the type's doc (`backend.rs:626-632`); the timeouts/retry wiring audited here rides that same stage.
- **Cluster dial path.** `ValkeyBackend::connect` at `crates/ff-backend-valkey/src/lib.rs:113-121` uses `Client::connect_cluster(&[url])` which, like `Client::connect`, takes zero timing options. Stage 1c must also migrate this path to a `ClientBuilder`-shaped equivalent, or ferriskey must grow a `connect_cluster_with(request)` entry — worth a ferriskey-API question during Stage 1c planning.
