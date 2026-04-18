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
            return Err(SdkError::Config(
                "bearer token is empty or all-whitespace; use \
                 FlowFabricAdminClient::new for unauthenticated access"
                    .into(),
            ));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth_value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token_str)).map_err(
                |_| {
                    SdkError::Config(
                        "bearer token contains characters not valid in an HTTP header".into(),
                    )
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
    /// Partition indices where another rotation was already in
    /// progress (per-partition `SETNX` lock held). These will be
    /// rotated by the concurrent request; NOT a fault.
    #[serde(default)]
    pub in_progress: Vec<u16>,
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
        assert!(matches!(err, SdkError::Config(_)), "got: {err:?}");
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
                matches!(&err, SdkError::Config(msg) if msg.contains("empty")),
                "token {s:?} should return Config(empty/whitespace); got: {err:?}"
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
        // Exact shape the server emits (ff-server server.rs:2636).
        let raw = r#"{
            "rotated": 3,
            "failed": [4, 5],
            "in_progress": [6],
            "new_kid": "kid-2026-04-18"
        }"#;
        let r: RotateWaitpointSecretResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.rotated, 3);
        assert_eq!(r.failed, vec![4, 5]);
        assert_eq!(r.in_progress, vec![6]);
        assert_eq!(r.new_kid, "kid-2026-04-18");
    }

    #[test]
    fn rotate_response_handles_missing_in_progress() {
        // Older server versions may omit in_progress. Default to
        // empty so the client stays forward-compatible.
        let raw = r#"{"rotated": 1, "failed": [], "new_kid": "k1"}"#;
        let r: RotateWaitpointSecretResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.in_progress, Vec::<u16>::new());
    }
}
