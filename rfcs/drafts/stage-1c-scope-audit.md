# RFC-012 Stage 1c scope audit (2026-04-22)

Worker EE, pre-planning investigation. Read-only. Cites `file:line` on `main` at `47ef2b0` ("archive RFC-012 namespace-amendment exploration"). No plan, no implementation — this only sizes the scope-surface so manager can decide how to shape the Stage 1c PR.

## Scope summary

Stage 1c is scoped, post the #122 ScannerFilter shipping and the namespace-amendment exploration being archived (`47ef2b0`), to the **original round-4 §5.1 scope**: migrate the `FlowFabricWorker` + free-fn hot paths off the embedded `ferriskey::Client` and behind the `EngineBackend` trait, fold in issue #93 (`BackendConfig` carved out of `WorkerConfig`'s connection fields), and wire `BackendTimeouts` / `BackendRetry` (placeholders landed in Stage 1a at `crates/ff-core/src/backend.rs:570-587`). No namespace / no ScannerFilter work — that landed in #122 via a separate mechanism. The four `ClaimedTask` ops deferred from Stage 1b (`create_pending_waitpoint`, `append_frame`, `suspend`, `report_usage`) are blocked on issue #117's trait-shape amendment and are **out of scope** for 1c unless #117 lands first.

## Hot-path method inventory

Direct `ferriskey::Client` / `self.client.{cmd,fcall,get,hget,...}` usage inside `ff-sdk` method bodies. Stage-1b-migrated sites (route through `self.backend.*`) are **omitted** — they're already done. Row shape: op → current site → migration target.

### `FlowFabricWorker` (crates/ff-sdk/src/worker.rs)

| Op | Site | Current shape | Stage-1c target |
|---|---|---|---|
| `connect` PING check | worker.rs:151-155 | `client.cmd("PING").execute()` | backend has no `ping`; either add or keep as Valkey-internal inside `ValkeyBackend::connect` |
| `connect` duplicate-instance guard | worker.rs:203-215 | `SET NX PX` via `client.cmd("SET")` | worker-guard primitive — candidate for `backend.register_worker_instance(id, ttl)` OR stays in the Valkey backend's connect path |
| `connect` caps advertisement (DEL + SREM) | worker.rs:401-415 | `client.cmd("DEL")` + `cmd("SREM")` | backend-internal (caps index is a Valkey implementation detail); move into `ValkeyBackend::advertise_capabilities` helper |
| `connect` caps advertisement (SET + SADD) | worker.rs:425-444 | `client.cmd("SET")` + `cmd("SADD")` | same as above |
| `read_partition_config` (free fn) | worker.rs:1571-1599 | `client.hgetall(ff:config:partitions)` | `backend.partition_config()` — `ValkeyBackend::connect` already reads this; move the read into the backend and expose via trait |
| `claim_next` ZRANGEBYSCORE scan | worker.rs:616-627 | `client.cmd("ZRANGEBYSCORE")` | `backend.peek_eligible(lane, partition, limit)` — new trait method OR fold into a composite `claim_next` on the trait |
| `issue_claim_grant` | worker.rs:788-792 | `client.fcall("ff_issue_claim_grant", …)` | `backend.issue_claim_grant(…)` — composite trait op OR bundled into `claim_next` |
| `block_route` | worker.rs:832-836 | `client.fcall::<Value>("ff_block_execution_for_admission", …)` | `backend.block_execution(…)` |
| `claim_execution` (HGET total_attempt_count) | worker.rs:884-890 | `client.cmd("HGET")` | backend-internal; absorbed by `backend.claim(lane, partition)` |
| `claim_execution` FCALL | worker.rs:938-942 | `client.fcall("ff_claim_execution", …)` | `backend.claim(…)` — Stage-1a §3.1.1 slot already reserved |
| `claim_resumed_execution` (HGET current_attempt_index) | worker.rs:1250-1256 | `client.cmd("HGET")` | backend-internal; absorbed by `backend.claim_resumed(…)` |
| `claim_resumed_execution` FCALL | worker.rs:1293-1297 | `client.fcall("ff_claim_resumed_execution", …)` | `backend.claim_resumed(…)` |
| `read_execution_context` GET payload | worker.rs:1401-1405 | `client.get(&ctx.payload())` | backend-internal; absorbed by the claim result shape OR `backend.read_execution_context(…)` |
| `read_execution_context` HGET kind | worker.rs:1409-1413 | `client.hget(&ctx.core(), "execution_kind")` | same |
| `read_execution_context` HGETALL tags | worker.rs:1417-1421 | `client.hgetall(&ctx.tags())` | same |
| `deliver_signal` lane pre-read | worker.rs:1447-1451 | `client.hget(&ctx.core(), "lane_id")` | backend-internal; absorbed |
| `deliver_signal` FCALL | worker.rs:1520-1524 | `client.fcall("ff_deliver_signal", …)` | `backend.deliver_signal(…)` — new trait method |

**Subtotal worker.rs: 17 direct-client sites** (plus `ClaimedTask::new(self.client.clone(), …)` at worker.rs:1022 and 1372 — passes the client through to `ClaimedTask`, gone once Stage 1d removes the embedded field).

### `ClaimedTask` — deferred-from-1b ops (crates/ff-sdk/src/task.rs)

These four are direct-client today **and** flagged `TODO(stage-1d-or-rfc-amendment)` / `#117`. Listed for completeness; **gated on #117** (trait-shape amendment), not naturally in Stage 1c:

| Op | Site | FCALL |
|---|---|---|
| `report_usage` | task.rs:662-666 | `ff_report_usage_and_check` |
| `create_pending_waitpoint` | task.rs:718-722 | `ff_create_pending_waitpoint` |
| `append_frame` | task.rs:782-786 | `ff_append_frame` |
| `suspend` | task.rs:905-909 | `ff_suspend_execution` |

### Free functions (crates/ff-sdk/src/task.rs, admin.rs)

| Fn | Site | Client use | Stage-1c target |
|---|---|---|---|
| `read_stream` | task.rs:1933-1964 | takes `client: &Client`, calls `ff_script::functions::stream::ff_read_attempt_stream(client, …)` | reshape as `FlowFabricWorker::read_stream(&self, …)` OR accept `&dyn StreamBackend` (issue #92 already laid the `StreamBackend` ground — see §6.2). **Closes #87.** |
| `tail_stream` | task.rs:2036-2074 | takes `client: &Client`, calls `ff_script::stream_tail::xread_block(client, …)` | same shape; **closes #87**. Head-of-line doc requires a dedicated connection — migration shape must preserve that knob |
| `rotate_waitpoint_hmac_secret_all_partitions` | admin.rs:493-519 | takes `client: &ferriskey::Client`, calls `ff_script::functions::suspension::ff_rotate_waitpoint_hmac_secret(client, …)` | reshape as a method on `FlowFabricAdminClient` OR on a new `AdminBackend` trait (RFC-012 §3.2). Not listed in #87's original site list — **additional leak**, same fix |

### snapshot.rs pipeline uses

| Site | Shape |
|---|---|
| snapshot.rs:77 | `self.client().pipeline()` (calls through `FlowFabricWorker::client()`) |
| snapshot.rs:810 | same |

Both consume the `client()` getter — closing `client()` (worker.rs:524) is blocked on re-shaping these two pipeline callers to go through the backend (pipelines are a Valkey primitive; likely needs a `backend.read_snapshot(…)` trait method OR a `pub(crate)` escape hatch at the Valkey backend boundary).

**Total Stage-1c hot-path migration surface: ~17 `FlowFabricWorker` sites + 2 free-fn reshapes + 1 admin free-fn reshape + 2 snapshot.rs sites = ~22 sites / ~8-10 trait methods.** For comparison Stage 1b migrated 8 `ClaimedTask` methods (per `worker.rs:94-110` comment).

## WorkerConfig field split (for BackendConfig migration, #93)

`WorkerConfig` at `crates/ff-sdk/src/config.rs:1-61`. Stage-1a `BackendConfig` already exists at `crates/ff-core/src/backend.rs:635` with `ValkeyConnection` covering `host/port/tls/cluster`.

| Field | config.rs line | Category |
|---|---|---|
| `host` | 6 | connection → `BackendConfig` |
| `port` | 8 | connection → `BackendConfig` |
| `tls` | 10 | connection → `BackendConfig` |
| `cluster` | 12 | connection → `BackendConfig` |
| `worker_id` | 14 | policy (worker identity) → stays |
| `worker_instance_id` | 16 | policy (process identity) → stays |
| `namespace` | 18 | policy (tenant scope) → stays |
| `lanes` | 20 | policy (routing) → stays |
| `capabilities` | 22 | policy (routing) → stays |
| `lease_ttl_ms` | 24 | policy (timing) → stays |
| `claim_poll_interval_ms` | 26 | policy (timing) → stays |
| `max_concurrent_tasks` | 28 | policy (concurrency) → stays |

**4 fields move, 8 stay.** Exact split as forecast in `backend.rs:625-632` doc comment. No mixed fields.

`WorkerConfig::renewal_interval_ms()` helper (config.rs:58) is pure-policy, stays.

## FCALL caller sites in ff-sdk

`ff_script::functions::*` + direct `.fcall("ff_…", …)` sites in `crates/ff-sdk/src/**`:

| Site | FCALL | Stage |
|---|---|---|
| admin.rs:515 | `ff_script::functions::suspension::ff_rotate_waitpoint_hmac_secret` | 1c (admin path) |
| task.rs:664 | `ff_report_usage_and_check` | deferred (#117) |
| task.rs:720 | `ff_create_pending_waitpoint` | deferred (#117) |
| task.rs:784 | `ff_append_frame` | deferred (#117) |
| task.rs:907 | `ff_suspend_execution` | deferred (#117) |
| task.rs:1961 | `ff_script::functions::stream::ff_read_attempt_stream` (via `read_stream`) | 1c |
| worker.rs:790 | `ff_issue_claim_grant` | 1c |
| worker.rs:834 | `ff_block_execution_for_admission` | 1c |
| worker.rs:940 | `ff_claim_execution` | 1c |
| worker.rs:1295 | `ff_claim_resumed_execution` | 1c |
| worker.rs:1522 | `ff_deliver_signal` | 1c |

**11 FCALL sites total, 7 in scope for Stage 1c proper, 4 deferred behind #117.** Adding in `stream_tail::xread_block` (not an FCALL but a direct transport call) → 8 Stage-1c migrations. This is in the same range as Stage 1b (8 methods) — the stage is not obviously over-large.

## #87/#88 overlap

Cross-reference against Worker BB's `87-88-scope-carveout.md` "Stage-1c-blocked sites" section (lines 46-55):

### Closed by Stage 1c (naturally, in-scope)

| Site | BB's classification | Closed how |
|---|---|---|
| `crates/ff-sdk/src/worker.rs:524` — `FlowFabricWorker::client() -> &Client` | "Stage 1c (backend-handle method replaces it)" | Closes once hot-path migration removes the embedded client; `backend()` getter at worker.rs:519 is the replacement (Stage 1c narrows its return type per the doc comment at worker.rs:516-518) |
| `crates/ff-sdk/src/task.rs:1933` — `pub async fn read_stream(client: &Client, …)` | "Stage 1c (reshape as worker method / opaque handle)" | Reshape as `FlowFabricWorker::read_stream` — same pattern |
| `crates/ff-sdk/src/task.rs:2036` — `pub async fn tail_stream(client: &Client, …)` | "Stage 1c (same)" | Same |

**3 of the 6 BB-listed ff-sdk sites close in Stage 1c.**

### Still open after Stage 1c (needs Stage 1d or separate work)

| Site | BB's classification | Why not 1c |
|---|---|---|
| `crates/ff-sdk/src/lib.rs:105` — `SdkError::Valkey(ferriskey::Error)` | "Stage 1c/d (wrap in BackendError)" | Error-type broadening is a separate axis — Stage 0 `EngineError::Transport` broadening is the canonical home; wrapping at SdkError is mechanical but can land separately |
| `crates/ff-sdk/src/lib.rs:110-113` — `SdkError::ValkeyContext { source: ferriskey::Error, … }` | same | same |
| `crates/ff-sdk/src/lib.rs:240` — `SdkError::valkey_kind()` | "Stage 1c/d (keep method, return None for non-Valkey)" | Same. BB recommends Option A — bundle error-type work across crates |

**Additional leak BB did not list** (worth flagging for manager): `crates/ff-sdk/src/admin.rs:493-519` — `rotate_waitpoint_hmac_secret_all_partitions(client: &ferriskey::Client, …)` — same shape as `read_stream` / `tail_stream`. Should be reshaped alongside them in Stage 1c.

## Risk flags

- **Lua impact: NO.** All Stage-1c migrations preserve the existing FCALL names + KEYS/ARGV. The trait method shape is Rust-side; the Valkey backend impl still calls the same `ff_issue_claim_grant` / `ff_claim_execution` / `ff_deliver_signal` FCALLs. `lua/*.lua` is untouched. (Verified: `rg -n "ARGV\[|KEYS\[" lua/` patterns would need to change for ARGV-shape expansion; none of the 8 in-scope sites requires that — they are 1:1 trait wrappers around the current FCALL.)
- **BackendTimeouts/BackendRetry: placeholder.** Types exist at `crates/ff-core/src/backend.rs:570-587`; `BackendConfig` carries them at line 637-638. `ValkeyBackend::connect` at `crates/ff-backend-valkey/src/lib.rs:92` pattern-matches on `BackendConnection::Valkey(v)` but **ignores `config.timeouts` and `config.retry`** — the comment at `lib.rs:91` explicitly names Stage 1c as the wiring point ("Full `BackendTimeouts`/`BackendRetry` wiring is a Stage 1c task"). **Stage 1c must wire these through to `ClientBuilder::connect_timeout` / `request_timeout`** at `worker.rs:136-139` (today hard-coded at 10s connect / 5s request) or they remain dead code — this is the same class of bug as the original namespace-field plumbing miss.
- **Backend-handle coverage for `client()` getter.** Closing worker.rs:524 needs the `backend()` getter at worker.rs:519 to narrow its return type (already scaffolded) AND the two `snapshot.rs` pipeline callers (lines 77, 810) to stop reaching into `self.client().pipeline()`. Snapshot-read may need a new trait method OR a documented `pub(crate)` escape on `ValkeyBackend` — a small design question for the Stage-1c plan.
- **Snapshot pipelines depend on ferriskey `pipeline()`.** `snapshot.rs:77, 810` use `client().pipeline()` — pipelines are inherently Valkey-shaped. Migration likely needs a `backend.pipeline_snapshot_read(…)` or an out-of-trait `ValkeyBackend::pipeline()` accessor. Flag for manager: this is the trickiest design call in the stage.
- **connect-path primitives (PING, SET-NX alive key, caps index SET/SADD/DEL/SREM) don't fit cleanly into the lifecycle-op trait.** These are worker-registration primitives. Options: (a) bundle into a `backend.register_worker(&config)` trait method; (b) leave inside `ValkeyBackend::connect` (which already loads partition config at `lib.rs:133-148` — precedent exists); (c) expose a small `WorkerLifecycleBackend` sub-trait. Design call for Stage-1c plan.

## Effort estimate

Rough sizing based on site counts + 1a/1b comparisons:

- **Hot-path migration (17 `FlowFabricWorker` sites, 7 FCALL wrappers + connect-path primitives + snapshot pipelines):** 18-24h. Larger than Stage 1b (8 methods, 20-28h actual) only if the backend-handle + snapshot pipeline design problem forces a sub-trait or new pipeline method; smaller if we can bundle.
- **#93 `BackendConfig` carveout:** 3-5h. Mechanical field split; `BackendConfig::valkey(host, port)` already exists. The back-compat `WorkerConfig::new(host, port, …)` shim per RFC §5.1 is **1-2h**; BB's carve-out + pre-1.0 CHANGELOG-only posture likely lets us just break the API and update the 20-ish call sites in-tree. Call-site fanout: 20 struct-literal `WorkerConfig { … }` sites + 1 `WorkerConfig::new` doc reference — all first-party (ff-test, ff-readiness-tests). Breaking the API saves ~1h net and removes a future-removal chore.
- **`BackendTimeouts` / `BackendRetry` wiring:** 2-4h. Thread `config.timeouts.request` → `ClientBuilder::request_timeout`, `config.timeouts.keepalive` → `ClientBuilder::ping_interval` (if ferriskey exposes it; check), `config.retry.*` → a retry wrapper on transient errors. Writing the tests that actually verify the plumbing (lesson learned from the namespace-field dead-code miss) is ~half the time.
- **#87/#88 closure for the 3 in-scope sites (`FlowFabricWorker::client()`, `read_stream`, `tail_stream`):** absorbed into hot-path migration — not incremental.
- **Admin-path `rotate_waitpoint_hmac_secret_all_partitions`:** 2-3h. Reshape as method on `FlowFabricAdminClient` or gate behind an `AdminBackend` trait.

**Total: 25-38h.** Slightly below round-4's §5.1 estimate of 25-35h if we scope down (leave error-type broadening to a separate small PR per BB's Option A), slightly above if we bundle the error work in. Stage 1b landed at ~24h; 1c is comparable-to-slightly-larger.

## Open questions for Stage 1c planning

1. **`WorkerConfig::new(host, port, …)` back-compat shim: keep or break?** RFC-012 §5.1 says keep. BB's carve-out and the pre-1.0 CHANGELOG-only posture (project_ff_release_decisions.md lock at 2026-04-22) suggest break. 20 first-party call sites only; no external consumers of the ff-sdk Rust API. Recommendation: break. Needs owner confirmation.
2. **`BackendTimeouts::keepalive` — what does ferriskey expose?** `ClientBuilder` at worker.rs:136-139 uses `.connect_timeout` + `.request_timeout` only. If ferriskey has no keepalive-interval knob, the field plumbs to nothing — repeat of the namespace miss. Needs an empirical ferriskey API check before 1c lands.
3. **Snapshot `pipeline()` — trait method or Valkey escape hatch?** The two snapshot.rs sites are the hardest design call. Trait method `backend.pipeline_snapshot()` commits us to an awkward abstraction; `ValkeyBackend::pipeline()` leaves a leak, just one the backend crate owns.
4. **Connect-path primitives (PING, alive-key SET-NX, caps-index SET/SADD/DEL/SREM) — absorb into `backend.connect` or expose as trait methods?** Precedent: `ValkeyBackend::connect` already pulls `ff:config:partitions` into itself at `lib.rs:133-148`. Suggest: absorb all of them symmetrically. Needs a decision before planning.
5. **Bundle `#88` error-type wrapping in 1c, or separate PR?** BB recommends separate (Option A); 1c is already sizable. Decision affects the 1c PR size by ~3-5h and the error-type sweep's home (ff-server/ff-scheduler/ff-sdk all land together, or staggered).
6. **Is `admin.rs`'s `rotate_waitpoint_hmac_secret_all_partitions` in 1c scope or separate?** Not listed in BB's #87/#88 carveout but is the same leak class. Cheap to bundle; noting the additional scope now prevents a 1d discovery.
7. **#117 timing.** Four `ClaimedTask` deferred ops (`create_pending_waitpoint`, `append_frame`, `suspend`, `report_usage`) still do direct `client.fcall(…)`. If #117 lands before 1c starts, bundle. If after, leave for 1d. Not a hard blocker either way — just affects 1c size.
