//! Admin REST client for operator-facing endpoints on `ff-server`.
//!
//! Wraps `POST /v1/admin/*` so downstream consumers (cairn-fabric)
//! don't hand-roll the HTTP call for admin surfaces like HMAC secret
//! rotation. Mirrors the server's wire types exactly — request
//! bodies and response shapes are defined against
//! [`ff_server::api`] + [`ff_server::server`] and kept 1:1 with the
//! producer.
//!
//! Authentication is Bearer token. Callers pick up the token from
//! wherever they hold it (`FF_API_TOKEN` env var is the common
//! pattern, but the SDK does not read env vars on the caller's
//! behalf — [`FlowFabricAdminClient::with_token`] accepts a
//! string-like token value (`&str` or `String`) via
//! `impl AsRef<str>`).

use std::time::Duration;

use ff_core::contracts::{RotateWaitpointHmacSecretArgs, RotateWaitpointHmacSecretOutcome};
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use serde::{Deserialize, Serialize};

use crate::SdkError;

/// Default per-request timeout. The server's own
/// `ROTATE_HTTP_TIMEOUT` is 120s; pick 130s client-side so the
/// client deadline is LATER than the server deadline and
/// operators see the structured 504 GATEWAY_TIMEOUT body rather
/// than a client-side timeout error.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(130);

/// Client for the `ff-server` admin REST surface.
///
/// Construct via [`FlowFabricAdminClient::new`] (no auth) or
/// [`FlowFabricAdminClient::with_token`] (Bearer auth). Both
/// return a ready-to-use client backed by a single pooled
/// `reqwest::Client` — reuse the instance across calls instead of
/// building one per request.
#[derive(Debug, Clone)]
pub struct FlowFabricAdminClient {
    http: reqwest::Client,
    base_url: String,
}

impl FlowFabricAdminClient {
    /// Build a client without auth. Suitable for a dev ff-server
    /// whose `api_token` is unconfigured. Production deployments
    /// should use [`with_token`](Self::with_token).
    pub fn new(base_url: impl Into<String>) -> Result<Self, SdkError> {
        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| SdkError::Http {
                source: e,
                context: "build reqwest::Client".into(),
            })?;
        Ok(Self {
            http,
            base_url: normalize_base_url(base_url.into()),
        })
    }

    /// Build a client that sends `Authorization: Bearer <token>` on
    /// every request. The token is passed by value so the caller
    /// retains ownership policy (e.g. zeroize on drop at the
    /// caller side); the SDK only reads it.
    ///
    /// # Empty-token guard
    ///
    /// An empty or all-whitespace `token` returns
    /// [`SdkError::Config`] instead of silently constructing
    /// `Authorization: Bearer ` (which the server rejects with
    /// 401, leaving the operator chasing a "why is auth broken"
    /// ghost). Common source: `FF_ADMIN_TOKEN=""` in a shell
    /// where the var was meant to be set; the unset-expansion is
    /// the empty string. Prefer an obvious error at construction
    /// over a silent 401 at first request.
    ///
    /// If the caller genuinely wants an unauthenticated client
    /// (dev ff-server without `api_token` configured), use
    /// [`FlowFabricAdminClient::new`] instead.
    pub fn with_token(
        base_url: impl Into<String>,
        token: impl AsRef<str>,
    ) -> Result<Self, SdkError> {
        let token_str = token.as_ref();
        if token_str.trim().is_empty() {
            return Err(SdkError::Config {
                context: "admin_client".into(),
                field: Some("bearer_token".into()),
                message: "is empty or all-whitespace; use \
                          FlowFabricAdminClient::new for unauthenticated access"
                    .into(),
            });
        }
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth_value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token_str)).map_err(
                |_| SdkError::Config {
                    context: "admin_client".into(),
                    field: Some("bearer_token".into()),
                    message: "contains characters not valid in an HTTP header".into(),
                },
            )?;
        // Mark Authorization as sensitive so it doesn't appear in
        // reqwest's Debug output / logs.
        auth_value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth_value);

        let http = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .default_headers(headers)
            .build()
            .map_err(|e| SdkError::Http {
                source: e,
                context: "build reqwest::Client".into(),
            })?;
        Ok(Self {
            http,
            base_url: normalize_base_url(base_url.into()),
        })
    }

    /// POST `/v1/workers/{worker_id}/claim` — scheduler-routed claim.
    ///
    /// Batch C item 2 PR-B. Swaps the SDK's direct-Valkey claim for a
    /// server-side one: the request carries lane + identity +
    /// capabilities + grant TTL; the server runs budget, quota, and
    /// capability admission via `ff_scheduler::Scheduler::claim_for_worker`
    /// and returns a `ClaimGrant` on success.
    ///
    /// Returns `Ok(None)` when the server responds 204 No Content
    /// (no eligible execution on the lane). Callers that want to keep
    /// polling should back off per their claim cadence.
    pub async fn claim_for_worker(
        &self,
        req: ClaimForWorkerRequest,
    ) -> Result<Option<ClaimForWorkerResponse>, SdkError> {
        // Percent-encode `worker_id` in the URL path — `WorkerId` is a
        // free-form string (could contain `/`, spaces, `%`, etc.) and
        // splicing it verbatim would produce malformed URLs or
        // misrouted paths. `Url::path_segments_mut().push` handles the
        // encoding natively.
        let mut url = reqwest::Url::parse(&self.base_url).map_err(|e| SdkError::Config {
            context: "admin_client: claim_for_worker".into(),
            field: Some("base_url".into()),
            message: format!("invalid base_url '{}': {e}", self.base_url),
        })?;
        {
            let mut segs = url.path_segments_mut().map_err(|_| SdkError::Config {
                context: "admin_client: claim_for_worker".into(),
                field: Some("base_url".into()),
                message: format!("base_url cannot be a base URL: '{}'", self.base_url),
            })?;
            segs.extend(&["v1", "workers", &req.worker_id, "claim"]);
        }
        let url = url.to_string();
        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| SdkError::Http {
                source: e,
                context: "POST /v1/workers/{worker_id}/claim".into(),
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if status.is_success() {
            return resp
                .json::<ClaimForWorkerResponse>()
                .await
                .map(Some)
                .map_err(|e| SdkError::Http {
                    source: e,
                    context: "decode claim_for_worker response body".into(),
                });
        }

        // Error path — mirror rotate_waitpoint_secret's ErrorBody decode.
        let status_u16 = status.as_u16();
        let raw = resp.text().await.map_err(|e| SdkError::Http {
            source: e,
            context: format!("read claim_for_worker error body (status {status_u16})"),
        })?;
        let parsed = serde_json::from_str::<AdminErrorBody>(&raw).ok();
        Err(SdkError::AdminApi {
            status: status_u16,
            message: parsed
                .as_ref()
                .map(|b| b.error.clone())
                .unwrap_or_else(|| raw.clone()),
            kind: parsed.as_ref().and_then(|b| b.kind.clone()),
            retryable: parsed.as_ref().and_then(|b| b.retryable),
            raw_body: raw,
        })
    }

    /// Rotate the waitpoint HMAC secret on the server.
    ///
    /// Promotes the currently-installed kid to `previous_kid`
    /// (accepted for the server's configured
    /// `FF_WAITPOINT_HMAC_GRACE_MS` window) and installs
    /// `new_secret_hex` under `new_kid` as the new current. Fans
    /// out across every execution partition. Idempotent: re-running
    /// with the same `(new_kid, new_secret_hex)` converges.
    ///
    /// The server returns 200 if at least one partition rotated OR
    /// at least one partition was already rotating under a
    /// concurrent request. See `RotateWaitpointSecretResponse`
    /// fields for the breakdown.
    ///
    /// # Errors
    ///
    /// * [`SdkError::AdminApi`] — non-2xx response (400 invalid
    ///   input, 401 missing/bad bearer, 429 concurrent rotate,
    ///   500 all partitions failed, 504 server-side timeout).
    /// * [`SdkError::Http`] — transport error (connect, body
    ///   decode, client-side timeout).
    ///
    /// # Retry semantics
    ///
    /// Rotation is idempotent on the same `(new_kid,
    /// new_secret_hex)` so retries are SAFE even on 504s or
    /// partial failures.
    pub async fn rotate_waitpoint_secret(
        &self,
        req: RotateWaitpointSecretRequest,
    ) -> Result<RotateWaitpointSecretResponse, SdkError> {
        let url = format!("{}/v1/admin/rotate-waitpoint-secret", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| SdkError::Http {
                source: e,
                context: "POST /v1/admin/rotate-waitpoint-secret".into(),
            })?;

        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<RotateWaitpointSecretResponse>()
                .await
                .map_err(|e| SdkError::Http {
                    source: e,
                    context: "decode rotate-waitpoint-secret response body".into(),
                });
        }

        // Non-2xx: parse the server's ErrorBody if we can, fall
        // back to a raw body otherwise. Propagate body-read
        // transport errors as Http rather than silently flattening
        // them into `AdminApi { raw_body: "" }` — a connection drop
        // mid-body-read is a transport fault, not an API-layer
        // reject, and misclassifying it strips `is_retryable`'s
        // timeout/connect signal from the caller.
        let status_u16 = status.as_u16();
        let raw = resp.text().await.map_err(|e| SdkError::Http {
            source: e,
            context: format!(
                "read rotate-waitpoint-secret error response body (status {status_u16})"
            ),
        })?;
        let parsed = serde_json::from_str::<AdminErrorBody>(&raw).ok();
        Err(SdkError::AdminApi {
            status: status_u16,
            message: parsed
                .as_ref()
                .map(|b| b.error.clone())
                .unwrap_or_else(|| raw.clone()),
            kind: parsed.as_ref().and_then(|b| b.kind.clone()),
            retryable: parsed.as_ref().and_then(|b| b.retryable),
            raw_body: raw,
        })
    }
}

/// Request body for `POST /v1/admin/rotate-waitpoint-secret`.
///
/// Mirrors `ff_server::api::RotateWaitpointSecretBody` 1:1.
#[derive(Debug, Clone, Serialize)]
pub struct RotateWaitpointSecretRequest {
    /// New key identifier. Non-empty, must not contain `:` (the
    /// server uses `:` as the field separator in the secret hash).
    pub new_kid: String,
    /// Hex-encoded new secret. Even-length, `[0-9a-fA-F]`.
    pub new_secret_hex: String,
}

/// Response body for `POST /v1/admin/rotate-waitpoint-secret`.
///
/// Mirrors `ff_server::server::RotateWaitpointSecretResult` 1:1.
/// The server serializes this struct as-is via `Json(result)`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RotateWaitpointSecretResponse {
    /// Count of partitions that accepted the rotation.
    pub rotated: u16,
    /// Partition indices where the rotation failed — operator
    /// should investigate. Rotation is idempotent on the same
    /// `(new_kid, new_secret_hex)` so a retry after the underlying
    /// fault clears converges.
    pub failed: Vec<u16>,
    /// The `new_kid` that was installed as current on every
    /// rotated partition — echoes the request field back for
    /// confirmation.
    pub new_kid: String,
}

/// Server-side error body shape, as emitted by
/// `ff_server::api::ErrorBody`. Kept internal because consumers
/// match on the flattened fields of [`SdkError::AdminApi`].
#[derive(Debug, Clone, Deserialize)]
struct AdminErrorBody {
    error: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    retryable: Option<bool>,
}

/// Request body for `POST /v1/workers/{worker_id}/claim`.
///
/// Mirrors `ff_server::api::ClaimForWorkerBody` 1:1. `worker_id`
/// goes in the URL path (not the body) but is kept on the struct
/// for ergonomics — callers don't juggle a separate arg.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimForWorkerRequest {
    #[serde(skip)]
    pub worker_id: String,
    pub lane_id: String,
    pub worker_instance_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Grant TTL in milliseconds. Server rejects 0 or anything over
    /// 60s (its `CLAIM_GRANT_TTL_MS_MAX`).
    pub grant_ttl_ms: u64,
}

/// Response body for `POST /v1/workers/{worker_id}/claim`.
///
/// Wire shape of `ff_core::contracts::ClaimGrant`. Carries the opaque
/// [`ff_core::partition::PartitionKey`] directly on the wire (issue
/// #91); the SDK reconstructs the core type via [`Self::into_grant`].
#[derive(Debug, Clone, Deserialize)]
pub struct ClaimForWorkerResponse {
    pub execution_id: String,
    pub partition_key: ff_core::partition::PartitionKey,
    pub grant_key: String,
    pub expires_at_ms: u64,
}

impl ClaimForWorkerResponse {
    /// Convert the wire DTO into a typed
    /// [`ff_core::contracts::ClaimGrant`] for handoff to
    /// [`crate::FlowFabricWorker::claim_from_grant`]. Returns
    /// [`SdkError::AdminApi`] on malformed execution_id — a drift
    /// signal that the server and SDK disagree on the wire shape, so
    /// failing loud prevents routing to a ghost partition.
    ///
    /// The `partition_key` itself is not eagerly parsed here: it is
    /// carried opaquely to the `claim_from_grant` hot path, which
    /// parses it there and surfaces a typed error on malformed keys.
    pub fn into_grant(self) -> Result<ff_core::contracts::ClaimGrant, SdkError> {
        let execution_id = ff_core::types::ExecutionId::parse(&self.execution_id)
            .map_err(|e| SdkError::AdminApi {
                status: 200,
                message: format!(
                    "claim_for_worker: server returned malformed execution_id '{}': {e}",
                    self.execution_id
                ),
                kind: Some("malformed_response".to_owned()),
                retryable: Some(false),
                raw_body: String::new(),
            })?;
        Ok(ff_core::contracts::ClaimGrant {
            execution_id,
            partition_key: self.partition_key,
            grant_key: self.grant_key,
            expires_at_ms: self.expires_at_ms,
        })
    }
}

/// Per-partition outcome of a cluster-wide waitpoint HMAC secret
/// rotation. Returned by [`rotate_waitpoint_hmac_secret_all_partitions`]
/// so operators can audit which partitions rotated vs. no-op'd vs. failed.
///
/// The index is the execution-partition index (`0..num_partitions`),
/// matching `{fp:N}` in the keyspace.
#[derive(Debug)]
pub struct PartitionRotationOutcome {
    /// Execution partition index (`0..num_partitions`).
    pub partition: u16,
    /// FCALL outcome on this partition, or the error it raised.
    pub result: Result<RotateWaitpointHmacSecretOutcome, SdkError>,
}

/// Rotate the waitpoint HMAC secret across every execution partition
/// by fanning out the `ff_rotate_waitpoint_hmac_secret` FCALL.
///
/// This is the canonical Rust-side rotation path for direct-Valkey
/// consumers (e.g. cairn-fabric) that cannot route through the
/// `ff-server` admin REST endpoint. Callers who have an HTTP-reachable
/// `ff-server` should prefer [`FlowFabricAdminClient::rotate_waitpoint_secret`] —
/// that path adds a single-writer admission gate, parallel fan-out,
/// structured audit events, and the server's configured grace window.
///
/// # Production rotation recipe
///
/// Operators MUST coordinate so secret rotation **precedes** any
/// waitpoint resolution that will present the new `kid`. The broad
/// sequence:
///
/// 1. Pick a fresh `new_kid` (must NOT contain `:` — the server uses
///    `:` as the field separator in the secret hash).
/// 2. Call this helper with the previous `kid`'s grace window
///    (`grace_ms` — the duration during which tokens signed by the
///    outgoing secret remain valid).
/// 3. Only after this call returns with all partitions `Ok(_)` (either
///    `Rotated` or `Noop`), begin signing new tokens with `new_kid`.
/// 4. Retain the previous secret in the keystore until the grace
///    window elapses — the FCALL handles GC of expired kids on every
///    rotation, so just don't rotate again before the grace window.
///
/// See RFC-004 §rotation for the full 4-key HSET + `previous_expires_at`
/// dance the FCALL implements server-side.
///
/// # Idempotency
///
/// Each partition FCALL is idempotent on the same `(new_kid,
/// new_secret_hex)` pair: a replay with identical args returns
/// `RotateWaitpointHmacSecretOutcome::Noop`. A same-kid-different-secret
/// replay surfaces as a per-partition `SdkError` (wrapping
/// `ScriptError::RotationConflict`) — pick a fresh `new_kid` to recover.
///
/// # Error semantics
///
/// A per-partition FCALL failure (transport fault, rotation conflict,
/// etc.) is recorded on that partition's [`PartitionRotationOutcome`]
/// and fan-out **continues** — the contract matches the server's
/// `rotate_waitpoint_secret` (partial success is allowed, operators
/// retry on the failed partition subset). Returning `Vec<_>` (not
/// `Result<Vec<_>, _>`) is deliberate: every whole-call invariant is
/// enforced by the underlying FCALL on each partition (kid non-empty,
/// no `:`, even-length hex, etc.), so the aggregate has nothing left
/// to reject at the Rust boundary. Callers decide how to treat partial
/// failures (fail loud / retry the subset / record metrics).
///
/// # Concurrency + performance
///
/// Sequential (one partition at a time) to keep the helper dependency-
/// free: no `futures::stream` / tokio-specific primitives on the caller
/// path. For a cluster with N partitions and per-partition RTT R, the
/// total duration is ~N*R. Consumers needing parallel fan-out should
/// wrap this with `FuturesUnordered` themselves, or use the server
/// admin endpoint (which fans out with bounded concurrency = 16).
///
/// # Test harness
///
/// The `ff-test::fixtures::TestCluster::rotate_waitpoint_hmac_secret`
/// method is a thin wrapper around this helper — integration tests and
/// production code exercise the same code path.
///
/// # Example
///
/// ```rust,ignore
/// use ff_sdk::admin::rotate_waitpoint_hmac_secret_all_partitions;
///
/// let results = rotate_waitpoint_hmac_secret_all_partitions(
///     &client,
///     partition_config.num_flow_partitions,
///     "kid-2026-04-22",
///     "deadbeef...64-hex-chars...",
///     60_000,
/// )
/// .await?;
///
/// for entry in &results {
///     match &entry.result {
///         Ok(outcome) => tracing::info!(partition = entry.partition, ?outcome, "rotated"),
///         Err(e) => tracing::error!(partition = entry.partition, %e, "rotation failed"),
///     }
/// }
/// ```
pub async fn rotate_waitpoint_hmac_secret_all_partitions(
    client: &ferriskey::Client,
    num_partitions: u16,
    new_kid: &str,
    new_secret_hex: &str,
    grace_ms: u64,
) -> Vec<PartitionRotationOutcome> {
    // Hoisted out of the loop — `ff_rotate_waitpoint_hmac_secret` only
    // borrows the args, so every partition can reuse the same struct.
    // Avoids N × 2 string clones on the hot fan-out path.
    let args = RotateWaitpointHmacSecretArgs {
        new_kid: new_kid.to_owned(),
        new_secret_hex: new_secret_hex.to_owned(),
        grace_ms,
    };
    let mut out = Vec::with_capacity(num_partitions as usize);
    for index in 0..num_partitions {
        let partition = Partition {
            family: PartitionFamily::Execution,
            index,
        };
        let idx = IndexKeys::new(&partition);
        let result = ff_script::functions::suspension::ff_rotate_waitpoint_hmac_secret(
            client, &idx, &args,
        )
        .await
        .map_err(SdkError::from);
        out.push(PartitionRotationOutcome {
            partition: index,
            result,
        });
    }
    out
}

/// Trim trailing slashes from a base URL so `format!("{base}/v1/...")`
/// never produces `https://host//v1/...`. Mirror of
/// media-pipeline's pattern.
fn normalize_base_url(mut url: String) -> String {
    while url.ends_with('/') {
        url.pop();
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_strips_trailing_slash() {
        assert_eq!(normalize_base_url("http://x".into()), "http://x");
        assert_eq!(normalize_base_url("http://x/".into()), "http://x");
        assert_eq!(normalize_base_url("http://x///".into()), "http://x");
    }

    #[test]
    fn with_token_rejects_bad_header_chars() {
        // Raw newline in the token would split the Authorization
        // header — must fail loudly at construction.
        let err = FlowFabricAdminClient::with_token("http://x", "tok\nevil").unwrap_err();
        assert!(
            matches!(err, SdkError::Config { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn with_token_rejects_empty_or_whitespace() {
        // Exact shell footgun: FF_ADMIN_TOKEN="" expands to "".
        // Fail loudly at construction instead of shipping a client
        // that silently 401s on first request.
        for s in ["", " ", "\t\n ", "   "] {
            let err = FlowFabricAdminClient::with_token("http://x", s)
                .unwrap_err();
            assert!(
                matches!(&err, SdkError::Config { field: Some(f), .. } if f == "bearer_token"),
                "token {s:?} should return Config with field=bearer_token; got: {err:?}"
            );
        }
    }

    #[test]
    fn admin_error_body_deserialises_optional_fields() {
        // `kind` + `retryable` absent (the usual shape for 400s).
        let b: AdminErrorBody = serde_json::from_str(r#"{"error":"bad new_kid"}"#).unwrap();
        assert_eq!(b.error, "bad new_kid");
        assert!(b.kind.is_none());
        assert!(b.retryable.is_none());

        // `kind` + `retryable` present (500 ValkeyError shape).
        let b: AdminErrorBody = serde_json::from_str(
            r#"{"error":"valkey: timed out","kind":"IoError","retryable":true}"#,
        )
        .unwrap();
        assert_eq!(b.error, "valkey: timed out");
        assert_eq!(b.kind.as_deref(), Some("IoError"));
        assert_eq!(b.retryable, Some(true));
    }

    #[test]
    fn rotate_response_deserialises_server_shape() {
        // Exact shape the server emits.
        let raw = r#"{
            "rotated": 3,
            "failed": [4, 5],
            "new_kid": "kid-2026-04-18"
        }"#;
        let r: RotateWaitpointSecretResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.rotated, 3);
        assert_eq!(r.failed, vec![4, 5]);
        assert_eq!(r.new_kid, "kid-2026-04-18");
    }

    // ── ClaimForWorkerResponse::into_grant ──

    fn sample_claim_response(partition_key: &str) -> ClaimForWorkerResponse {
        ClaimForWorkerResponse {
            execution_id: "{fp:5}:11111111-1111-1111-1111-111111111111".to_owned(),
            partition_key: serde_json::from_str(
                &serde_json::to_string(partition_key).unwrap(),
            )
            .unwrap(),
            grant_key: "ff:exec:{fp:5}:11111111-1111-1111-1111-111111111111:claim_grant".to_owned(),
            expires_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn into_grant_preserves_all_known_partition_key_shapes() {
        // Post-#91: families collapse into opaque PartitionKey literals.
        // Flow and Execution both produce "{fp:N}"; Budget is "{b:N}";
        // Quota is "{q:N}". The DTO preserves the wire string as-is;
        // into_grant hands it opaquely to the core type.
        for key_str in ["{fp:5}", "{b:5}", "{q:5}"] {
            let g = sample_claim_response(key_str).into_grant().unwrap_or_else(|e| {
                panic!("key {key_str} should parse: {e:?}")
            });
            assert_eq!(g.partition_key.as_str(), key_str);
            assert_eq!(g.expires_at_ms, 1_700_000_000_000);
        }
    }

    #[test]
    fn into_grant_preserves_opaque_partition_key() {
        // The SDK does NOT eagerly parse the partition_key on the
        // admin boundary — malformed keys are caught at the
        // claim_from_grant hot path where the typed Partition is
        // actually needed. This test pins the opacity contract.
        let resp = sample_claim_response("{zz:0}");
        let g = resp.into_grant().expect("SDK must not parse partition_key");
        assert_eq!(g.partition_key.as_str(), "{zz:0}");
        // Parsing surfaces the error explicitly.
        assert!(g.partition().is_err());
    }

    #[test]
    fn into_grant_rejects_malformed_execution_id() {
        let mut resp = sample_claim_response("{fp:5}");
        resp.execution_id = "not-a-valid-eid".to_owned();
        let err = resp.into_grant().unwrap_err();
        match err {
            SdkError::AdminApi { message, kind, .. } => {
                assert!(message.contains("malformed execution_id"),
                    "msg: {message}");
                assert_eq!(kind.as_deref(), Some("malformed_response"));
            }
            other => panic!("expected AdminApi, got {other:?}"),
        }
    }

    // ── ClaimForWorkerResponse wire shape (issue #91) ──

    // `rotate_waitpoint_hmac_secret_all_partitions` exercise coverage
    // lives in `ff-test` — the integration test harness in
    // `crates/ff-test/tests/waitpoint_hmac_rotation_fcall.rs` and
    // `waitpoint_tokens.rs` calls through the function via the
    // `TestCluster::rotate_waitpoint_hmac_secret` fixture, which is
    // now a thin delegator. A pure unit test here would require a
    // mock `ferriskey::Client` (ferriskey's `Client` performs a live
    // RESP handshake on `ClientBuilder::build`, so a local TCP
    // listener alone isn't sufficient) — expensive to construct for
    // one-line iteration-count coverage.

    #[test]
    fn claim_for_worker_response_deserialises_opaque_partition_key() {
        // Exact shape the server emits post-#91.
        let raw = r#"{
            "execution_id": "{fp:7}:11111111-1111-1111-1111-111111111111",
            "partition_key": "{fp:7}",
            "grant_key": "ff:exec:{fp:7}:11111111-1111-1111-1111-111111111111:claim_grant",
            "expires_at_ms": 1700000000000
        }"#;
        let r: ClaimForWorkerResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.partition_key.as_str(), "{fp:7}");
        assert_eq!(r.expires_at_ms, 1_700_000_000_000);
    }
}
