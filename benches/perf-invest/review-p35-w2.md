# Cross-Review — P3.5 FlowFabricAdminClient (W2 review of W1's fb506af)

**Branch:** `feat/cairn-gaps-p35` (PR#20)
**Reviewed commit:** `fb506af7bac754452b75cf46f999524c3d81f0cd` (single commit)
**Reviewer:** Worker-2
**Scope:** 5 files / 730 LOC net — `crates/ff-sdk/src/admin.rs` (287 LOC), `crates/ff-sdk/src/lib.rs` (+119 for `Http` + `AdminApi` variants), `crates/ff-sdk/Cargo.toml` (+6), `crates/ff-test/tests/admin_rotate_api.rs` (317), `Cargo.lock`.

Every dimension reviewed against the manager's six-point brief. Findings use GREEN / YELLOW / RED, each with `file:line` citations. **No RED items found.** Five YELLOW items, all minor / non-blocking. Three are pure hardening opportunities; two document forward-looking edge cases the existing docstrings already acknowledge.

---

## Verdict summary

| # | Dimension | Verdict |
|---|-----------|---------|
| 1 | Wire-shape alignment (req/resp/ErrorBody vs. server producers) | **GREEN** |
| 2 | Error variant classification (`is_retryable` correctness) | **YELLOW** (2 items) |
| 3 | Auth plumbing (bearer header + empty-token behaviour) | **YELLOW** (1 item) |
| 4 | Timeout 130s (configurability) | **YELLOW** (1 item, forward-looking) |
| 5 | Test quality — especially `enforces_bearer_token` | **GREEN** |
| 6 | `reqwest` dep addition (feature pin) | **GREEN** |

Overall: merge-ready. The YELLOWs are worth recording in-commit or as follow-up tickets if cairn pushes the API to grow, but none of them gate this commit.

---

## 1. Wire-shape alignment — **GREEN**

### Request — `RotateWaitpointSecretRequest` ≡ `ff_server::api::RotateWaitpointSecretBody`

| Field | Client (`admin.rs:172-178`) | Server (`ff-server/src/api.rs:453-458`) | Match |
|-------|------------------------------|------------------------------------------|:-----:|
| `new_kid` | `pub new_kid: String` | `new_kid: String` | ✓ |
| `new_secret_hex` | `pub new_secret_hex: String` | `new_secret_hex: String` | ✓ |

Both derive `Serialize` / `Deserialize` with default field-naming; no `#[serde(rename)]` drift. The client struct carries no extra or missing fields.

### Response — `RotateWaitpointSecretResponse` ≡ `ff_server::server::RotateWaitpointSecretResult`

| Field | Client (`admin.rs:184-202`) | Server (`ff-server/src/server.rs:2638-2655`) | Match |
|-------|------------------------------|------------------------------------------------|:-----:|
| `rotated: u16` | `pub rotated: u16` | `pub rotated: u16` | ✓ |
| `failed: Vec<u16>` | `pub failed: Vec<u16>` | `pub failed: Vec<u16>` | ✓ |
| `in_progress: Vec<u16>` | `#[serde(default)] pub in_progress: Vec<u16>` (`admin.rs:196-197`) | `#[serde(default)] pub in_progress: Vec<u16>` (`server.rs:2651-2652`) | ✓ (default symmetric on both sides — forward-compat preserved even if the server version flips) |
| `new_kid: String` | `pub new_kid: String` | `pub new_kid: String` | ✓ |

Server emits via `Json(result)` at `ff-server/src/api.rs:524` (and the `rotate_waitpoint_secret` handler returns the result struct as-is through `IntoResponse`), so the wire-level ordering/naming is direct from the derived `Serialize`. Unit test `rotate_response_deserialises_server_shape` (`admin.rs:264-277`) locks the JSON shape in; `rotate_response_handles_missing_in_progress` (`admin.rs:280-286`) locks the forward-compat behaviour. Both look correct.

### Error body — `AdminErrorBody` ≡ `ff_server::api::ErrorBody`

| Field | Client (`admin.rs:207-214`) | Server (`ff-server/src/api.rs:69-82`) | Match |
|-------|------------------------------|------------------------------------------|:-----:|
| `error: String` | `error: String` | `error: String` | ✓ |
| `kind: Option<String>` | `#[serde(default)] kind: Option<String>` | `#[serde(skip_serializing_if = "Option::is_none")] kind: Option<String>` | ✓ (server may omit the key entirely; client's `#[serde(default)]` handles the omit path) |
| `retryable: Option<bool>` | `#[serde(default)] retryable: Option<bool>` | `#[serde(skip_serializing_if = "Option::is_none")] retryable: Option<bool>` | ✓ (same reasoning) |

Unit test `admin_error_body_deserialises_optional_fields` (`admin.rs:246-261`) verifies both present + absent cases. Server-side ErrorBody-producing paths: `ErrorBody::plain(...)` for 4xx / 504 (no kind / no retryable) at `api.rs:79-82`, and `ErrorBody { ..., kind: Some(...), retryable: Some(...) }` for `ServerError::Valkey` / `ValkeyContext` / `LibraryLoad` / others (`api.rs:108-169`). All shapes round-trip into `AdminErrorBody`.

### Non-field notes

* The server returns the ergonomic 504 shape via `ErrorBody::plain(...)` at `api.rs:493-498` — parses into `AdminErrorBody { error: "...", kind: None, retryable: None }` on the client side. Combined with dimension 2's hint-or-fallback logic, a 504 from the server's rotation-timeout path correctly classifies as retryable (fallback set includes `504`).
* There is no other admin endpoint today, so request/response symmetry is only measured for `rotate-waitpoint-secret`. No ambiguity to flag.

---

## 2. Error variant classification — **YELLOW** (2 items)

### 2a. `SdkError::Http::is_retryable` under-counts transient reqwest errors — YELLOW

`lib.rs:208` delegates to `source.is_timeout() || source.is_connect()`. That correctly covers:

* Client-side deadline (`timeout(130s)` tripping → `is_timeout()`).
* TCP connect failure (DNS resolution, connect refused, connect timeout → `is_connect()`).

But `reqwest::Error` also exposes `is_body()` / `is_request()` / `is_decode()`. In practice, a connection drop **mid-response** (e.g. LB idle-timeouts the TCP connection after the status line but before the body completes) surfaces as `is_body()=true, is_timeout()=false, is_connect()=false`. That would be misclassified as **non-retryable** under the current implementation.

Why this is YELLOW, not RED:
* The `rotate_waitpoint_secret` rustdoc (`admin.rs:118-122`) explicitly documents retries are **safe** on the rotation endpoint because rotation is idempotent. A cairn operator reading that docstring already knows they can retry regardless of `is_retryable`.
* The caller has escape-hatch access via `SdkError::Http { source, .. }` destructuring — matching on `source.is_body()` at cairn's side is possible (if cumbersome).
* Body-decode errors on the 200-path are in fact NOT retryable (the server finished fine, the client's deserializer broke), so pure `is_decode() → retryable=true` would be wrong too. The current "only timeout/connect" is erring on the conservative side.

Suggestion (not required to land this PR): add `source.is_request() && !source.is_builder()` handling, or document this lower-level behaviour in the `SdkError::Http` rustdoc so cairn knows to check `source.is_body()` if they hit a mid-stream retry loop.

**File:line**: `crates/ff-sdk/src/lib.rs:208`.

### 2b. `SdkError::AdminApi::is_retryable` fallback omits 502 Bad Gateway — YELLOW

`lib.rs:214-216` falls back to `matches!(*status, 429 | 503 | 504)` when the server hint is absent. 502 Bad Gateway is a standard HTTP transient status emitted by reverse proxies / load balancers when the upstream is briefly unreachable. Under a proxied ff-server deployment (which is the expected production topology for cairn), a 502 would:

1. Have no JSON body (the proxy emits text/html) → `parsed = None` → `retryable = None`.
2. Fall through to the status fallback → not in `{429, 503, 504}` → **classified non-retryable**.
3. Cairn's retry loop would give up on a transient LB fault.

Why this is YELLOW:
* 502 is proxy-specific; the ff-server itself never emits 502.
* Rotation is idempotent — a naive retry is safe regardless of classification.
* Trivially fixable by extending the match arm to `| 502`.

Suggestion: add `502` to the fallback set, or explicitly document "the client's retryable-status fallback is conservative; add 502 at the caller if deploying behind an LB that emits it" in the `AdminApi` rustdoc.

**File:line**: `crates/ff-sdk/src/lib.rs:216`.

### Non-findings (checked and clean)

* Server's `retryable: Some(true)` for `ConcurrencyLimitExceeded` (429) at `api.rs:104-106`: client respects it → retryable. ✓
* Server's `retryable: Some(false)` for `Script` / `Config` / `PartitionMismatch` errors (`api.rs:162-169`): client respects it → non-retryable. ✓
* `Http` decode-path error (e.g. server sent corrupt JSON) wrapped with `context: "decode rotate-waitpoint-secret response body"` at `admin.rs:144-147` → not retryable. Semantically correct (retry won't change a server-side encoding bug). ✓

---

## 3. Auth plumbing — **YELLOW** (1 item)

### 3a. `with_token("")` silently produces a `Bearer ` header that degrades to 401 — YELLOW

`admin.rs:64-80` converts any caller-supplied token into an `HeaderValue` via `from_str(&format!("Bearer {}", token.as_ref()))`. An **empty string** token:

* Produces the literal string `"Bearer "` (note trailing space, no token).
* `HeaderValue::from_str("Bearer ")` is VALID (space is a visible ASCII char; HeaderValue accepts it).
* Construction returns `Ok(client)` silently.
* First request lands at server `auth_middleware` (`ff-server/src/api.rs:235-264`), `strip_prefix("Bearer ")` returns `Some("")`, `constant_time_eq("".as_bytes(), token.as_bytes())` compares lengths → false → 401.

So the caller pays an extra round-trip + decodes a 401 before realising the token was empty. Not a security hole — the request provably cannot authenticate. But it's a footgun: a missing env var read (`std::env::var("FF_API_TOKEN").unwrap_or_default()`) would silently produce the empty-token path instead of crashing at construction.

Why this is YELLOW:
* The server ALWAYS correctly rejects (bearer check is constant-time, 401 exactly).
* The misbehaviour is confined to caller-side misconfiguration; cairn's integration either hits 401 on every rotate (loud in monitoring) or supplies a non-empty token (correct).
* The docstring (`admin.rs:62-63`) says "the SDK only reads it" which is accurate — silently accepting empty is consistent with the SDK's "consumer owns the secret lifecycle" contract.

Suggestion (minor hardening): add a `token.as_ref().is_empty()` check early in `with_token` returning `SdkError::Config("bearer token is empty")`. Covers the silent env-miss footgun. Implement as 3 LOC and reuse the existing test harness — trivial, but not worth bouncing this PR.

**File:line**: `crates/ff-sdk/src/admin.rs:64-80`.

### Non-findings (checked and clean)

* `auth_value.set_sensitive(true)` at `admin.rs:79`: correct — reqwest's `Debug` impl + any log-level formatting of the client / headers masks the token. ✓
* Bad-char handling (`tok\nevil`): `HeaderValue::from_str` errors on invalid ASCII → mapped to `SdkError::Config` at `admin.rs:71-76`. Unit test `with_token_rejects_bad_header_chars` (`admin.rs:238-243`) asserts this at construction. ✓ — the "empty-string footgun" above is the one remaining case that slips through.
* Test `test_rotate_waitpoint_secret_enforces_bearer_token` (`admin_rotate_api.rs:280-317`) does verify the live-server auth handshake (see dimension 5 below). ✓

---

## 4. Timeout 130s configurability — **YELLOW** (forward-looking)

`admin.rs:27` pins `DEFAULT_TIMEOUT = Duration::from_secs(130)`. Rationale at `admin.rs:22-26` is sound for the **rotate** endpoint specifically: server's internal `ROTATE_HTTP_TIMEOUT = 120s` (`ff-server/src/api.rs:473`), extra 10s client-side to observe the structured 504. No way to override from the caller.

Why this is YELLOW (not acceptable-as-is — a flag for growth):

* **Today:** rotate is the only admin endpoint. 130s is exactly right. ✓
* **Tomorrow:** the admin surface is explicitly planned to grow (the rustdoc at `admin.rs:1-14` says "admin surfaces **like** HMAC secret rotation" — plural). Future endpoints will have shorter server-side timeouts (e.g. a force-cancel flow endpoint would plausibly run in <1s). Inheriting a 130s client timeout means a hung backend holds up a cairn operator's CLI session for 2+ minutes.
* Today the only fix would be per-method duplication of the `reqwest::Client` builder, or re-building the client with a custom timeout (the field is private).

Suggestion: when the second admin endpoint lands, expose a per-method `timeout` override (either a `FlowFabricAdminClientBuilder` pattern or a `rotate_waitpoint_secret_with_timeout(req, d)` companion). Don't do it now — YAGNI + adds test surface for a single consumer.

**File:line**: `crates/ff-sdk/src/admin.rs:27` + `admin.rs:47-48, 82-83` (both `.timeout(DEFAULT_TIMEOUT)` call sites).

---

## 5. Test quality — **GREEN** (especially `enforces_bearer_token`)

### `test_rotate_waitpoint_secret_enforces_bearer_token` — CRITICAL PATH VERIFIED

`ff-test/tests/admin_rotate_api.rs:280-317`.

Structure:
1. `TestApi::setup_with(Some("s3cret-admin-token".to_owned()))` (`admin_rotate_api.rs:284`) spins up a **real** `ff-server` in-process — same axum router, same `Server::start(config)`, same `ff_server::api::router(server, &["*"], api_token.clone())` as production. The only difference from a prod deployment is the partition fixture and the bound port.
2. The server's `router()` at `ff-server/src/api.rs:222-228` wraps the entire router (minus `/healthz`) in `auth_middleware`. So any request without the correct bearer **must** get a 401 from the middleware — before the handler runs — regardless of the handler's internal state.
3. The unauthed probe asserts `status == 401` **strictly** at `admin_rotate_api.rs:294-299`. This is the exact concern in the brief:
   > "A 500 would pass 'test expects error' trivially while actually masking a server crash."
   The `assert_eq!(status, 401, ...)` rules out both (a) a panic in the server returning `500`, and (b) a misrouted request falling through to a `404`.
4. The authed probe uses `FlowFabricAdminClient::with_token(&api.base_url, token)` (`admin_rotate_api.rs:302`) and asserts `.expect("bearer-configured client must be able to rotate")` plus `assert_eq!(resp.new_kid, new_kid)`. That proves the `Authorization: Bearer <token>` header is actually attached to outbound requests by `with_token` — a regression (e.g. `with_token` forgetting to call `default_headers`) would produce a 401 on the authed leg and fail the `.expect(...)`.

So: the test demonstrably exercises both halves of the critical contract. `#[serial_test::serial]` (`admin_rotate_api.rs:281`) + the harness's careful "no TestCluster cleanup" policy (`admin_rotate_api.rs:50-53`) prevent cross-test interference. Drop order via `AbortHandle` in `TestApi::Drop` (`admin_rotate_api.rs:40-44`) guarantees the spawned axum task is torn down between tests.

### Other tests — also solid

| Test | What it verifies | Verdict |
|------|-------------------|:-------:|
| `happy_path` | Response shape matches + at least one partition rotated + none failed | ✓ |
| `updates_valkey_state` | Post-rotate Valkey state (`current_kid`, `previous_kid`, `previous_expires_at`, `secret:<new_kid>`) via direct `HGET`s on `ff:sec:{p:0}:waitpoint_hmac` | ✓ |
| `rejects_bad_new_kid` | `new_kid` containing `:` → 400, message mentions `new_kid` + `:`, `retryable.is_none()` | ✓ (the `retryable.is_none()` assertion at `admin_rotate_api.rs:270` is the correct test of "4xxs have no hint") |

Unit tests in `admin.rs` (5) and `lib.rs` (3 new + existing): cover serde round-trips, base-URL normalization, bad-header rejection, and both legs of `is_retryable_admin_api_*` (server hint respected + status-fallback + non-retryable statuses). No dead tests. No trivially-passing assertions.

---

## 6. `reqwest` dep addition — **GREEN**

`crates/ff-sdk/Cargo.toml:35`:
```
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

Cross-referenced against every other `reqwest` consumer in the workspace (`grep reqwest\s*=` on `**/Cargo.toml`):

| Crate | Pin |
|-------|-----|
| `examples/media-pipeline/Cargo.toml:39` | `0.12`, `default-features = false`, `["json", "rustls-tls"]` |
| `examples/coding-agent/Cargo.toml:24` | `0.12`, `default-features = false`, `["json", "rustls-tls"]` |
| `crates/ff-test/Cargo.toml:35` | `0.12`, `default-features = false`, `["json", "rustls-tls"]` |
| `benches/harness/Cargo.toml:53` | `0.12`, `default-features = false`, `["json", "rustls-tls"]` |
| **`crates/ff-sdk/Cargo.toml:35`** | `0.12`, `default-features = false`, `["json", "rustls-tls"]` |

**All five pins identical**, including `default-features = false`. No accidental enabling of `native-tls`, `blocking`, `default-tls`, `gzip`, `cookies`, etc. No new version introduced anywhere in the tree. `Cargo.lock` changes (+2 lines per the diff) are the expected consequence of ff-sdk becoming the fifth consumer of an already-vendored dep.

Always-on vs. feature-gated: the commit message explicitly acknowledges this was a manager-level call. For a consumer SDK that pulls in `ff-script` + `ff-core` + `ferriskey` + tokio-runtime anyway, the marginal ~20s and ~2MB of a reqwest + rustls transitive load is negligible. Gating it behind a `http` feature would only serve consumers who want ff-sdk without HTTP — cairn isn't one of them, and no other known consumer needs that. Pragmatic call. ✓

---

## Recommendation

Merge PR#20 as-is. The five YELLOW items are worth capturing as cairn-follow-up tickets if/when the admin surface grows past one endpoint:

1. Document `is_body()` / `is_decode()` semantics on `SdkError::Http` rustdoc (1h).
2. Add `502` to the `AdminApi` retryable-status fallback (5min + 1 test case).
3. Add empty-string token guard in `with_token` (5min + 1 test case).
4. Introduce per-method timeout override once admin endpoints >1 (4h with builder pattern).
5. Consolidate / document that rotation idempotency is the backstop for all of the above classification gaps (docstring polish — no code change).

None of (1)-(5) gate this commit.

---

**Scope boundary.** Reviewed only the six dimensions in the brief + the specific files listed in the commit. Did not:
* Audit other admin endpoints (none exist yet).
* Re-run the integration test suite (`cargo test -p ff-test --test admin_rotate_api`) — the commit message already records 4/4 pass locally.
* Inspect `serde` workspace pin or `thiserror` derivation quality (pre-existing, out of scope).
* Compare against any cairn-fabric consumer code — W3's job on P3.6 covers the comparison pass once W3 completes.

W2 review of P3.5 / fb506af: **complete. Merge-ready.**
