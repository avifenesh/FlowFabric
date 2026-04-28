//! REST API layer — thin axum handlers over Server methods.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, MatchedPath, Path, Query, Request, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put, MethodRouter},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use axum::http::Method;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use ff_core::contracts::*;
use ff_core::state::PublicState;
use ff_core::types::*;

use crate::config::ConfigError;
use crate::server::{Server, ServerError};

// ── Per-route body-size limits (#97) ──
//
// Axum's out-of-the-box `DefaultBodyLimit` is 2 MiB and was applied silently
// with no per-route override, meaning a misrouted multi-GB JSON would still
// be buffered up to 2 MiB before rejection and mega-byte payloads on
// control-plane endpoints (cancel, priority, claim, …) were accepted with
// no protection beyond that global cap.
//
// We apply three categories, ordered by generosity:
//
//   * `BODY_LIMIT_LARGE_PAYLOAD` (1 MiB) — carries an application
//     `input_payload` (`POST /v1/executions`). FlowFabric has no Lua-side
//     cap on `input_payload` (`execution.lua` SETs whatever is passed);
//     1 MiB matches common workflow-engine conventions (e.g. AWS Step
//     Functions input size limit is 256 KB, Temporal's default is 2 MiB)
//     and is an order of magnitude above typical JSON control envelopes.
//
//   * `BODY_LIMIT_MEDIUM_PAYLOAD` (256 KiB) — signal delivery
//     (`POST /v1/executions/{id}/signal`) carries a `DeliverSignalArgs`
//     with an optional `Vec<u8> payload`. Signals are notification-shaped
//     events (webhooks, human approvals), not bulk-data transfers; 256 KiB
//     is the same ceiling Step Functions imposes on task inputs and is
//     comfortably above any real correlation-id / approval-blob.
//
//   * `BODY_LIMIT_CONTROL` (64 KiB) — everything else. Control-plane JSON
//     bodies (cancel args, priority changes, claim requests, flow
//     members, budget/quota definitions, rotate-secret keys) are all tiny
//     fixed-shape records; 64 KiB leaves plenty of headroom for
//     metadata-heavy bodies while capping DoS amplification by ~32× vs
//     the previous axum default.
//
// Cross-checked against `lua/*.lua` — the only payload-size caps in the
// Lua layer are `CAPS_MAX_BYTES=4096` / `CAPS_MAX_TOKENS=256` for
// capability lists (`lua/helpers.lua`). No other Lua path enforces a
// byte-length limit on incoming payloads, so HTTP is the right layer to
// draw the line.
//
// Future admin routes that need larger bodies (e.g. bulk import) should
// introduce a new `BODY_LIMIT_ADMIN_BULK` const and opt in per-route
// rather than bumping `BODY_LIMIT_CONTROL`.

const BODY_LIMIT_LARGE_PAYLOAD: usize = 1024 * 1024;     // 1 MiB
const BODY_LIMIT_MEDIUM_PAYLOAD: usize = 256 * 1024;     // 256 KiB
const BODY_LIMIT_CONTROL: usize = 64 * 1024;             // 64 KiB

/// Limit metadata attached to a request via extension so the 413 error body
/// can report the route's configured cap. The `DefaultBodyLimit::max(bytes)`
/// layer enforces; this struct only exists for response shape.
#[derive(Clone, Copy)]
struct BodyLimit {
    bytes: usize,
}

/// Apply per-route body-size cap + attach [`BodyLimit`] extension so the
/// JSON rejection handler can report `limit_bytes` in the 413 response.
///
/// Three layers, each guarding a different failure mode:
///
///   1. `from_fn(enforce_body_limit)` — inspects `Content-Length` up front
///      and rejects with our structured 413 body BEFORE the handler runs.
///      This is the primary enforcement path and works regardless of
///      whether the handler consumes the body. Also attaches the
///      [`BodyLimit`] extension so `AppJson`'s 413 branch (below, case 2)
///      can report `limit_bytes` even when rejection happens during
///      streaming.
///   2. `DefaultBodyLimit::max(bytes)` — hint consumed by `AppJson`
///      (`Bytes::from_request` internally) for the case where a client
///      lies in `Content-Length` or uses `Transfer-Encoding: chunked`:
///      the body extractor trips on a streaming-length check and we
///      rewrite the rejection into the structured 413 shape in
///      `AppJson::from_request`.
///   3. `tower_http::limit::RequestBodyLimitLayer::new(bytes)` —
///      transport-level body cap that also fires on chunked transfers
///      even when the handler never consumes the body. Returns a plain
///      413 (no JSON body) in that edge case; the structured path in
///      (1) covers the overwhelmingly common case (clients sending
///      Content-Length).
fn with_body_limit(route: MethodRouter<Arc<Server>>, bytes: usize) -> MethodRouter<Arc<Server>> {
    let r: MethodRouter<Arc<Server>> =
        route.layer(tower_http::limit::RequestBodyLimitLayer::new(bytes));
    let r: MethodRouter<Arc<Server>> = r.layer(DefaultBodyLimit::max(bytes));
    r.layer(middleware::from_fn(
        move |req: Request, next: middleware::Next| enforce_body_limit(bytes, req, next),
    ))
}

/// Content-Length-based body-size enforcement + [`BodyLimit`] extension.
///
/// Runs before the handler so routes whose handlers never consume the
/// request body (`replay_execution`, `revoke_lease`, `reset_budget` — see
/// Copilot review on PR#100) are still protected against large uploads.
async fn enforce_body_limit(bytes: usize, mut req: Request, next: middleware::Next) -> Response {
    if let Some(content_length) = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        && content_length > bytes
    {
        let route = req
            .extensions()
            .get::<MatchedPath>()
            .map(|m| m.as_str().to_owned())
            .unwrap_or_default();
        let body = PayloadTooLargeBody {
            error: "payload_too_large",
            limit_bytes: bytes,
            route,
        };
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(body)).into_response();
    }
    req.extensions_mut().insert(BodyLimit { bytes });
    next.run(req).await
}

/// Shape for the 413 response body.
#[derive(Serialize)]
struct PayloadTooLargeBody {
    error: &'static str,
    limit_bytes: usize,
    route: String,
}

// ── Custom JSON extractor (uniform JSON error on malformed body) ──

struct AppJson<T>(T);

impl<S, T> axum::extract::FromRequest<S> for AppJson<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(
        req: axum::extract::Request,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        // Snapshot metadata for a potential 413 before `Json::from_request`
        // consumes the request. `BodyLimit` is `Copy` and `MatchedPath` is
        // cheap to clone (its payload is an `Arc<str>`), so the non-413
        // path pays at most two extension lookups plus a ref-count bump.
        let limit = req.extensions().get::<BodyLimit>().copied();
        let matched = req.extensions().get::<MatchedPath>().cloned();

        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(rejection) => {
                let status = rejection.status();
                tracing::debug!(detail = %rejection.body_text(), "JSON rejection");
                if status == StatusCode::PAYLOAD_TOO_LARGE {
                    let limit_bytes = limit.map(|l| l.bytes).unwrap_or(0);
                    let route = matched
                        .as_ref()
                        .map(|m| m.as_str().to_owned())
                        .unwrap_or_default();
                    let body = PayloadTooLargeBody {
                        error: "payload_too_large",
                        limit_bytes,
                        route,
                    };
                    return Err((status, Json(body)).into_response());
                }
                let body = ErrorBody::plain(format!(
                    "invalid JSON: {}",
                    status.canonical_reason().unwrap_or("bad request"),
                ));
                Err((status, Json(body)).into_response())
            }
        }
    }
}

// ── Error handling ──

struct ApiError(ServerError);

impl From<ServerError> for ApiError {
    fn from(e: ServerError) -> Self {
        Self(e)
    }
}

/// RFC-017 Stage C: handlers that dispatch through the backend trait
/// surface `EngineError` directly. Wrap into the existing
/// `ServerError::Engine(Box<EngineError>)` lane so the
/// `IntoResponse` mapping downstream keeps one code path for
/// `EngineError`-rooted responses.
impl From<ff_core::engine_error::EngineError> for ApiError {
    fn from(e: ff_core::engine_error::EngineError) -> Self {
        Self(ServerError::Engine(Box::new(e)))
    }
}

/// HTTP error body. `kind`/`retryable` are populated for 500s backed by
/// a backend transport fault (see `ff_core::BackendErrorKind`) so HTTP
/// clients (e.g. cairn-fabric) can make retry decisions without parsing
/// the `error` string.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
}

impl ErrorBody {
    fn plain(error: String) -> Self {
        Self { error, kind: None, retryable: None }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match &self.0 {
            ServerError::NotFound(msg) => {
                (StatusCode::NOT_FOUND, ErrorBody::plain(msg.clone()))
            }
            ServerError::InvalidInput(msg) => {
                (StatusCode::BAD_REQUEST, ErrorBody::plain(msg.clone()))
            }
            ServerError::OperationFailed(msg) => {
                (StatusCode::BAD_REQUEST, ErrorBody::plain(msg.clone()))
            }
            ServerError::ConcurrencyLimitExceeded(source, max) => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorBody {
                    error: format!(
                        "too many concurrent {source} calls (server max: {max}); retry with backoff"
                    ),
                    kind: None,
                    retryable: Some(true),
                },
            ),
            ServerError::Backend(be) => {
                let kind_str = be.kind().as_stable_str();
                tracing::error!(
                    kind = kind_str,
                    message = be.message(),
                    "backend error"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorBody {
                        error: self.0.to_string(),
                        kind: Some(kind_str.to_owned()),
                        retryable: Some(self.0.is_retryable()),
                    },
                )
            }
            ServerError::BackendContext { source, context } => {
                let kind_str = source.kind().as_stable_str();
                tracing::error!(
                    kind = kind_str,
                    message = source.message(),
                    context = %context,
                    "backend error"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorBody {
                        error: self.0.to_string(),
                        kind: Some(kind_str.to_owned()),
                        retryable: Some(self.0.is_retryable()),
                    },
                )
            }
            ServerError::LibraryLoad(load_err) => {
                let kind_str = load_err
                    .valkey_kind()
                    .map(ff_backend_valkey::classify_ferriskey_kind)
                    .map(|k| k.as_stable_str());
                tracing::error!(
                    kind = kind_str.unwrap_or(""),
                    error = %load_err,
                    "library load failure"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorBody {
                        error: format!("library load: {load_err}"),
                        kind: kind_str.map(str::to_owned),
                        retryable: Some(self.0.is_retryable()),
                    },
                )
            }
            // RFC-017 Stage B: trait-dispatched handlers surface
            // backend-pool pressure + shutdown races as typed
            // EngineError variants. Map them to their REST status so
            // the 429/503 contract preserved from the pre-Stage-B
            // `ConcurrencyLimitExceeded` arm keeps holding across
            // migrated handlers (`read_attempt_stream`,
            // `tail_attempt_stream`, …).
            ServerError::Engine(boxed) => {
                use ff_core::engine_error::EngineError as EE;
                // Peel context wrappers to find the root cause so the
                // status mapping is stable regardless of nesting.
                fn root(e: &EE) -> &EE {
                    match e {
                        EE::Contextual { source, .. } => root(source),
                        other => other,
                    }
                }
                match root(boxed) {
                    EE::ResourceExhausted { pool, max, .. } => (
                        StatusCode::TOO_MANY_REQUESTS,
                        ErrorBody {
                            error: format!(
                                "too many concurrent {pool} calls (server max: {max}); retry with backoff"
                            ),
                            kind: Some("resource_exhausted".into()),
                            retryable: Some(true),
                        },
                    ),
                    EE::Unavailable { op } => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorBody {
                            error: format!("backend op unavailable: {op}"),
                            kind: Some("unavailable".into()),
                            retryable: Some(false),
                        },
                    ),
                    EE::NotFound { entity } => (
                        StatusCode::NOT_FOUND,
                        ErrorBody::plain(format!("not found: {entity}")),
                    ),
                    // `EngineError::Validation` is caller-supplied-input
                    // rejection (invalid waitpoint HMAC, oversize payload,
                    // malformed capabilities, …). Before Stage A these
                    // surfaced as `ServerError::InvalidInput` → 400; once
                    // `deliver_signal` and friends migrated to trait
                    // dispatch the same-semantics errors arrived as
                    // `EngineError::Validation`. Preserve the 400.
                    EE::Validation { kind, detail } => {
                        use ff_core::engine_error::ValidationKind as VK;
                        let code = match kind {
                            VK::InvalidInput => "invalid_input",
                            VK::CapabilityMismatch => "capability_mismatch",
                            VK::InvalidCapabilities => "invalid_capabilities",
                            VK::InvalidPolicyJson => "invalid_policy_json",
                            VK::PayloadTooLarge => "payload_too_large",
                            VK::SignalLimitExceeded => "signal_limit_exceeded",
                            VK::InvalidWaitpointKey => "invalid_waitpoint_key",
                            VK::InvalidToken => "invalid_token",
                            VK::WaitpointNotTokenBound => "waitpoint_not_token_bound",
                            VK::RetentionLimitExceeded => "retention_limit_exceeded",
                            VK::InvalidLeaseForSuspend => "invalid_lease_for_suspend",
                            VK::InvalidDependency => "invalid_dependency",
                            VK::InvalidWaitpointForExecution => "invalid_waitpoint_for_execution",
                            VK::InvalidBlockingReason => "invalid_blocking_reason",
                            VK::InvalidOffset => "invalid_offset",
                            VK::Unauthorized => "unauthorized",
                            VK::InvalidBudgetScope => "invalid_budget_scope",
                            VK::BudgetOverrideNotAllowed => "budget_override_not_allowed",
                            VK::InvalidQuotaSpec => "invalid_quota_spec",
                            VK::InvalidKid => "invalid_kid",
                            VK::InvalidSecretHex => "invalid_secret_hex",
                            VK::InvalidGraceMs => "invalid_grace_ms",
                            VK::InvalidTagKey => "invalid_tag_key",
                            VK::InvalidFrameType => "invalid_frame_type",
                            _ => "validation_error",
                        };
                        let msg = if detail.is_empty() {
                            code.to_string()
                        } else {
                            format!("{code}: {detail}")
                        };
                        (StatusCode::BAD_REQUEST, ErrorBody::plain(msg))
                    }
                    // RFC-017 Stage C: operator-control + budget
                    // paths surface domain-level conflicts (cycle,
                    // dep-already-exists, rotation-kid-clash). 409
                    // Conflict per RFC-010 §10.7.
                    EE::Conflict(kind) => {
                        use ff_core::engine_error::ConflictKind as CK;
                        let code = match kind {
                            CK::DependencyAlreadyExists { .. } => "dependency_already_exists",
                            CK::CycleDetected => "cycle_detected",
                            CK::SelfReferencingEdge => "self_referencing_edge",
                            CK::ExecutionAlreadyInFlow => "execution_already_in_flow",
                            CK::WaitpointAlreadyExists => "waitpoint_already_exists",
                            CK::BudgetAttachConflict => "budget_attach_conflict",
                            CK::QuotaAttachConflict => "quota_attach_conflict",
                            CK::RotationConflict(_) => "rotation_conflict",
                            CK::ActiveAttemptExists => "active_attempt_exists",
                            _ => "conflict",
                        };
                        (
                            StatusCode::CONFLICT,
                            ErrorBody {
                                error: format!("{code}: {kind:?}"),
                                kind: Some(code.into()),
                                retryable: Some(false),
                            },
                        )
                    }
                    // RFC-017 Stage C: retryable contention (lease
                    // conflict, rate-limit, stale-grant). 409 preserves
                    // the pre-migration `ServerError::OperationFailed
                    // → 400` for most of these; we upgrade to 409 so
                    // clients can distinguish domain-retryable from
                    // input-validation 400s.
                    EE::Contention(ck) => {
                        use ff_core::engine_error::ContentionKind as CK;
                        let (status, code, retryable) = match ck {
                            CK::RetryExhausted => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "retry_exhausted",
                                false,
                            ),
                            CK::RateLimitExceeded => (
                                StatusCode::TOO_MANY_REQUESTS,
                                "rate_limit_exceeded",
                                true,
                            ),
                            CK::ConcurrencyLimitExceeded => (
                                StatusCode::TOO_MANY_REQUESTS,
                                "concurrency_limit_exceeded",
                                true,
                            ),
                            _ => (StatusCode::CONFLICT, "contention", true),
                        };
                        (
                            status,
                            ErrorBody {
                                error: format!("{code}: {ck:?}"),
                                kind: Some(code.into()),
                                retryable: Some(retryable),
                            },
                        )
                    }
                    // RFC-017 Stage C: legal-but-surprising state
                    // transitions surfaced by the migrated operator-
                    // control handlers. Most are benign no-ops that
                    // the client should swallow; a handful are true
                    // caller errors (ReplayNotAllowed, NotRunnable,
                    // ExecutionNotTerminal).
                    EE::State(sk) => {
                        use ff_core::engine_error::StateKind as SK;
                        let (status, code) = match sk {
                            // Replay / non-terminal gating — caller
                            // input gating error, 409.
                            SK::ExecutionNotTerminal => {
                                (StatusCode::CONFLICT, "execution_not_terminal")
                            }
                            SK::MaxReplaysExhausted => {
                                (StatusCode::CONFLICT, "max_replays_exhausted")
                            }
                            SK::ReplayNotAllowed => {
                                (StatusCode::CONFLICT, "replay_not_allowed")
                            }
                            SK::NotRunnable => (StatusCode::CONFLICT, "not_runnable"),
                            SK::Terminal => (StatusCode::CONFLICT, "terminal"),
                            SK::FlowAlreadyTerminal => {
                                (StatusCode::CONFLICT, "flow_already_terminal")
                            }
                            // Budget / admission — 409 (breach) vs 200
                            // (soft; kept client-returnable). Stage C
                            // handlers don't currently emit these as
                            // Errs (soft-breach arrives as
                            // `ReportUsageResult::SoftBreach`), so
                            // these arms are defensive.
                            SK::BudgetExceeded => (StatusCode::CONFLICT, "budget_exceeded"),
                            SK::BudgetSoftExceeded => {
                                (StatusCode::CONFLICT, "budget_soft_exceeded")
                            }
                            // Benign no-ops — caller retried something
                            // that already completed. Pre-migration
                            // this surfaced as
                            // `ServerError::OperationFailed` → 400;
                            // 409 is the correct status for "you tried
                            // to X but the system is already past X".
                            SK::AlreadySatisfied
                            | SK::DuplicateSignal
                            | SK::OkAlreadyApplied
                            | SK::AttemptAlreadyTerminal
                            | SK::StreamAlreadyClosed
                            | SK::LeaseExpired
                            | SK::LeaseRevoked => (StatusCode::CONFLICT, "already_satisfied"),
                            _ => (StatusCode::CONFLICT, "state_conflict"),
                        };
                        (
                            status,
                            ErrorBody {
                                error: format!("{code}: {sk:?}"),
                                kind: Some(code.into()),
                                retryable: Some(false),
                            },
                        )
                    }
                    _ => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ErrorBody {
                            error: self.0.to_string(),
                            kind: None,
                            retryable: Some(self.0.is_retryable()),
                        },
                    ),
                }
            }
            // Script / Config / PartitionMismatch — developer or deployment
            // errors. No Valkey ErrorKind to surface, but retryable=false is
            // informative: a client-side retry won't change the outcome.
            other => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody {
                    error: other.to_string(),
                    kind: None,
                    retryable: Some(false),
                },
            ),
        };
        (status, Json(body)).into_response()
    }
}

// ── Router ──

pub fn router(
    server: Arc<Server>,
    cors_origins: &[String],
    api_token: Option<String>,
) -> Result<Router, ConfigError> {
    router_with_metrics(server, cors_origins, api_token, None)
}

/// Router entry point that also mounts `/metrics` and the HTTP metrics
/// middleware. Used by `main.rs` when the `observability` feature is on.
///
/// When `metrics` is `Some` AND the `observability` feature is compiled
/// in, `/metrics` is mounted on an un-authenticated nested router (auth
/// middleware does not apply — Prometheus convention). When the
/// feature is off OR `metrics` is `None`, no `/metrics` route exists
/// (returns 404) and no HTTP metrics middleware is installed.
pub fn router_with_metrics(
    server: Arc<Server>,
    cors_origins: &[String],
    api_token: Option<String>,
    #[cfg_attr(not(feature = "observability"), allow(unused_variables))]
    metrics: Option<Arc<crate::Metrics>>,
) -> Result<Router, ConfigError> {
    let auth_enabled = api_token.is_some();
    let cors = build_cors_layer(cors_origins, auth_enabled)?;

    // Per-route body-size caps (#97). See module-level `BODY_LIMIT_*` consts
    // for the category rationale; each route picks the tightest cap that
    // still accommodates expected real traffic.
    let mut app = Router::new()
        // Executions
        .route(
            "/v1/executions",
            with_body_limit(
                get(list_executions).post(create_execution),
                BODY_LIMIT_LARGE_PAYLOAD,
            ),
        )
        .route("/v1/executions/{id}", get(get_execution))
        .route("/v1/executions/{id}/state", get(get_execution_state))
        .route(
            "/v1/executions/{id}/pending-waitpoints",
            get(list_pending_waitpoints),
        )
        .route("/v1/executions/{id}/result", get(get_execution_result))
        .route(
            "/v1/executions/{id}/cancel",
            with_body_limit(post(cancel_execution), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/executions/{id}/signal",
            with_body_limit(post(deliver_signal), BODY_LIMIT_MEDIUM_PAYLOAD),
        )
        .route(
            "/v1/executions/{id}/priority",
            with_body_limit(put(change_priority), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/executions/{id}/replay",
            with_body_limit(post(replay_execution), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/executions/{id}/revoke-lease",
            with_body_limit(post(revoke_lease), BODY_LIMIT_CONTROL),
        )
        // Scheduler-routed claim (Batch C item 2). Worker POSTs lane +
        // identity + capabilities; server runs budget/quota/capability
        // admission via ff-scheduler and returns a ClaimGrant on
        // success (204 No Content when no eligible execution).
        .route(
            "/v1/workers/{worker_id}/claim",
            with_body_limit(post(claim_for_worker), BODY_LIMIT_CONTROL),
        )
        // Stream read + tail (RFC-006 #2)
        .route(
            "/v1/executions/{id}/attempts/{idx}/stream",
            get(read_attempt_stream),
        )
        .route(
            "/v1/executions/{id}/attempts/{idx}/stream/tail",
            get(tail_attempt_stream),
        )
        // Flows
        .route(
            "/v1/flows",
            with_body_limit(post(create_flow), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/flows/{id}/members",
            with_body_limit(post(add_execution_to_flow), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/flows/{id}/cancel",
            with_body_limit(post(cancel_flow), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/flows/{id}/edges",
            with_body_limit(post(stage_dependency_edge), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/flows/{id}/edges/apply",
            with_body_limit(post(apply_dependency_to_child), BODY_LIMIT_CONTROL),
        )
        // Budgets
        .route(
            "/v1/budgets",
            with_body_limit(post(create_budget), BODY_LIMIT_CONTROL),
        )
        .route("/v1/budgets/{id}", get(get_budget_status))
        .route(
            "/v1/budgets/{id}/usage",
            with_body_limit(post(report_usage), BODY_LIMIT_CONTROL),
        )
        .route(
            "/v1/budgets/{id}/reset",
            with_body_limit(post(reset_budget), BODY_LIMIT_CONTROL),
        )
        // Quotas
        .route(
            "/v1/quotas",
            with_body_limit(post(create_quota_policy), BODY_LIMIT_CONTROL),
        )
        // Admin: rotate waitpoint HMAC secret (RFC-004 §Waitpoint Security)
        .route(
            "/v1/admin/rotate-waitpoint-secret",
            with_body_limit(post(rotate_waitpoint_secret), BODY_LIMIT_CONTROL),
        )
        // Health (always unauthenticated)
        .route("/healthz", get(healthz));

    if let Some(token) = api_token {
        let token = Arc::new(token);
        app = app.layer(middleware::from_fn(move |req, next| {
            let token = token.clone();
            auth_middleware(token, req, next)
        }));
    }

    // PR-94: HTTP metrics middleware. Recorded AFTER the auth layer
    // above so unauthorized 401s are still counted under their route,
    // and BEFORE trace/cors so the metric captures handler time
    // including the auth check itself. No-op when the
    // `observability` feature is off.
    #[cfg(feature = "observability")]
    if let Some(m) = metrics.as_ref() {
        let m = m.clone();
        app = app.layer(middleware::from_fn_with_state(
            m,
            crate::metrics::http_middleware,
        ));
    }

    #[cfg_attr(not(feature = "observability"), allow(unused_mut))]
    let mut app = app
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(server);

    // PR-94: `/metrics` — intentionally unauthenticated. Mounted on a
    // separate router with its own State so the main app's auth
    // middleware does not run for scrapes (Prometheus convention).
    // Network-layer auth (ingress ACL, service-mesh policy, or
    // metrics-only listen address) is the expected gate; FF does not
    // own auth for scrape endpoints.
    #[cfg(feature = "observability")]
    if let Some(m) = metrics {
        let metrics_router: Router = Router::new()
            .route("/metrics", get(crate::metrics::metrics_handler))
            .with_state(m);
        app = app.merge(metrics_router);
    }

    Ok(app)
}

async fn auth_middleware(
    token: Arc<String>,
    req: Request,
    next: middleware::Next,
) -> Response {
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }

    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let authorized = auth_header
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()));

    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody::plain(
                "missing or invalid Authorization header".to_owned(),
            )),
        )
            .into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build the CORS layer from the configured `FF_CORS_ORIGINS` list.
///
/// Fails closed (#71): if every entry fails to parse as a `HeaderValue`,
/// return `ConfigError` so startup aborts instead of silently falling back
/// to `CorsLayer::permissive()` (a typo would otherwise broaden browser
/// access to "allow any origin").
///
/// When `auth_enabled` is true (i.e. `FF_API_TOKEN` is set), `Authorization`
/// is added to `allow_headers` (#66) so browser preflights for
/// cross-origin authenticated requests succeed.
fn build_cors_layer(origins: &[String], auth_enabled: bool) -> Result<CorsLayer, ConfigError> {
    if origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::permissive());
    }

    let mut parsed = Vec::with_capacity(origins.len());
    let mut accepted = Vec::with_capacity(origins.len());
    let mut invalid = Vec::new();
    for o in origins {
        match o.parse() {
            Ok(v) => {
                parsed.push(v);
                accepted.push(o.as_str());
            }
            Err(_) => invalid.push(o.clone()),
        }
    }

    if parsed.is_empty() && !origins.is_empty() {
        return Err(ConfigError::InvalidValue {
            var: "FF_CORS_ORIGINS".to_owned(),
            message: format!(
                "all configured origins failed to parse as valid HTTP header values: {:?}; \
                 refusing to fall back to permissive CORS",
                origins
            ),
        });
    }

    if !invalid.is_empty() {
        // Collected `accepted` list during the single parse loop above to
        // avoid an O(N*M) filter-contains scan per log event.
        tracing::warn!(
            ?invalid,
            ?accepted,
            "some FF_CORS_ORIGINS entries failed to parse and were dropped"
        );
    }

    // Prefer typed `header::*` constants over stringly-typed `from_static`
    // so typos fail to compile rather than fail at runtime.
    let mut headers = vec![axum::http::header::CONTENT_TYPE];
    if auth_enabled {
        headers.push(axum::http::header::AUTHORIZATION);
    }

    Ok(CorsLayer::new()
        .allow_origin(AllowOrigin::list(parsed))
        .allow_methods([Method::GET, Method::POST, Method::PUT])
        .allow_headers(headers))
}

// ── Execution handlers ──

#[derive(Deserialize)]
struct ListExecutionsParams {
    /// Partition index (`u16`) to enumerate. Serves as the partition
    /// key for the forward-only cursor listing.
    partition: u16,
    /// Exclusive cursor: start listing strictly after this execution
    /// id. Omit for the first page.
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_limit")]
    limit: u64,
}

fn default_limit() -> u64 { 50 }

/// List executions in one partition with forward-only cursor
/// pagination.
///
/// **Breaking change (unreleased HTTP surface, not on crates.io):** as
/// of issue #182 this endpoint is a thin forwarder onto
/// [`ff_core::engine_backend::EngineBackend::list_executions`]. The
/// previous offset + lane + state filter query parameters were
/// dropped; the endpoint now returns partition-scoped execution ids
/// with an opaque `next_cursor`.
///
/// Request: `GET /v1/executions?partition=<u16>&cursor=<eid>&limit=<usize>`.
/// Response: `{ "executions": ["<eid>", ...], "next_cursor": "<eid>" | null }`.
async fn list_executions(
    State(server): State<Arc<Server>>,
    Query(params): Query<ListExecutionsParams>,
) -> Result<Json<ListExecutionsPage>, ApiError> {
    let limit = params.limit.min(1000) as usize;
    let cursor = match params.cursor {
        Some(raw) if !raw.is_empty() => Some(
            ff_core::types::ExecutionId::parse(&raw).map_err(|e| {
                ApiError::from(ServerError::InvalidInput(format!(
                    "invalid cursor: {e}"
                )))
            })?,
        ),
        _ => None,
    };
    let result = server
        .list_executions_page(params.partition, cursor, limit)
        .await?;
    Ok(Json(result))
}

async fn create_execution(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateExecutionArgs>,
) -> Result<(StatusCode, Json<CreateExecutionResult>), ApiError> {
    // RFC-017 Stage D1 (§4 row 6): dispatch through the backend trait.
    // `ValkeyBackend::create_execution` wraps the same FCALL with the
    // same 24h idempotency TTL default — zero wire impact.
    let result = server.backend().create_execution(args).await?;
    let status = match &result {
        CreateExecutionResult::Created { .. } => StatusCode::CREATED,
        CreateExecutionResult::Duplicate { .. } => StatusCode::OK,
    };
    Ok((status, Json(result)))
}

async fn get_execution(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<ExecutionInfo>, ApiError> {
    let eid = parse_execution_id(&id)?;
    Ok(Json(server.get_execution(&eid).await?))
}

async fn get_execution_state(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<PublicState>, ApiError> {
    let eid = parse_execution_id(&id)?;
    Ok(Json(server.get_execution_state(&eid).await?))
}

/// Returns the actionable (`pending`/`active`) waitpoints for an
/// execution as the sanitised `PendingWaitpointInfo` shape
/// (6 original fields + `token_kid` + `token_fingerprint` +
/// `execution_id`). The raw HMAC `waitpoint_token` is NOT returned —
/// callers correlate waitpoints via `(token_kid, token_fingerprint)`
/// and obtain the delivery credential through the worker's
/// `SuspendOutcome` at suspend time.
///
/// (RFC-017 Stage E4 / v0.8.0 §8: the legacy `waitpoint_token` wire
/// field was removed; the one-release deprecation window closed with
/// this release.)
async fn list_pending_waitpoints(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // RFC-017 Stage E4 (v0.8.0 §8): wire response serializes the
    // sanitised `PendingWaitpointInfo` directly — no raw HMAC
    // `waitpoint_token`, no `Deprecation: ff-017` header. Consumers
    // correlate via `(token_kid, token_fingerprint)`.
    let eid = parse_execution_id(&id)?;
    let args = ff_core::contracts::ListPendingWaitpointsArgs::new(eid);
    let page = server.backend().list_pending_waitpoints(args).await?;
    Ok(Json(page.entries).into_response())
}

/// Returns the raw result payload bytes written by the worker's
/// `ff_complete_execution` call. 404 when the execution has no stored
/// result (missing entirely, still in-flight, or trimmed by retention —
/// see below).
///
/// # Ordering (required)
///
/// Callers MUST poll `GET /v1/executions/{id}/state` until it returns
/// `completed` before fetching `/result`. Early polls may return 404
/// because completion writes `public_state = completed` and the result
/// `SET` in the same atomic Lua; in the normal path the window is
/// effectively zero, but network round-trip ordering between a state
/// poll and a result fetch can make the result appear briefly absent
/// during replay (`ff_replay_execution`).
///
/// # Retention / 404 after completed
///
/// `get_execution_state == completed` is authoritative for completion.
/// This endpoint additionally depends on the result bytes not having
/// been trimmed — v1 sets no retention policy, so
/// `state = completed` should always pair with a 200 here. Any
/// future retention-policy feature must call this contract out in its
/// own docs.
///
/// CONTENT-TYPE: `application/octet-stream`. The server is payload-format
/// agnostic — workers choose the encoding via the SDK's `complete(bytes)`
/// call, and callers must know the contract. The media-pipeline example
/// uses JSON by convention (`serde_json::to_vec(&Result)`); adapters can
/// pick any binary format.
///
/// SECURITY: completion payloads can contain PII (e.g. LLM summaries of
/// user audio). Treat this endpoint like any other read — gate behind
/// `FF_API_TOKEN` in any deployment reachable from untrusted networks.
/// The auth middleware only mounts when `FF_API_TOKEN` is set.
async fn get_execution_result(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // RFC-017 Stage D1 (§4 row 2): trait-dispatched execution result read.
    let eid = parse_execution_id(&id)?;
    match server.backend().get_execution_result(&eid).await? {
        Some(bytes) => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response()),
        None => Err(ApiError(ServerError::NotFound(format!(
            "execution result not found: {eid}"
        )))),
    }
}

async fn cancel_execution(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<CancelExecutionArgs>,
) -> Result<Json<CancelExecutionResult>, ApiError> {
    // RFC-017 Stage C migration (§4 row 2): dispatch through the
    // backend trait. Pre-read + FCALL + parse live inside
    // `ValkeyBackend::cancel_execution`. The inherent
    // `Server::cancel_execution` was deleted with this migration;
    // `cancel_flow_inner`'s internal dispatch keeps its own path.
    let path_eid = parse_execution_id(&id)?;
    check_id_match(&path_eid, &args.execution_id, "execution_id")?;
    args.execution_id = path_eid;
    Ok(Json(server.backend().cancel_execution(args).await?))
}

async fn deliver_signal(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<DeliverSignalArgs>,
) -> Result<Json<DeliverSignalResult>, ApiError> {
    let path_eid = parse_execution_id(&id)?;
    check_id_match(&path_eid, &args.execution_id, "execution_id")?;
    args.execution_id = path_eid;
    Ok(Json(server.deliver_signal(&args).await?))
}

// ── Admin: rotate waitpoint HMAC secret (RFC-004 §Waitpoint Security) ──

#[derive(Deserialize)]
struct RotateWaitpointSecretBody {
    new_kid: String,
    /// Hex-encoded new secret. Even-length, 0-9a-fA-F.
    new_secret_hex: String,
}

/// Hard ceiling on how long the rotate endpoint runs before the HTTP
/// handler bails. Rotation touches every execution partition (up to 256)
/// with HGET+HMGET+HDEL+HSET per partition; 6 round-trips × 30ms cross-AZ
/// × 256 partitions ≈ 46s worst-case. The internal SETNX lock is 10s TTL
/// per partition, so 120s gives ample margin for contention + slow RTTs
/// while staying below common LB idle timeouts (ALB 60s default, but
/// typically bumped to 120s+ for admin endpoints).
///
/// On timeout: returns HTTP 504 immediately. Valkey-side work may still
/// finish (the per-partition locks and HSETs are already in flight). The
/// operator observes a 504 and retries; retry is SAFE — rotation is
/// idempotent per-partition (same new_kid + same secret → no-op on
/// already-rotated partitions).
const ROTATE_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

async fn rotate_waitpoint_secret(
    State(server): State<Arc<Server>>,
    AppJson(body): AppJson<RotateWaitpointSecretBody>,
) -> Result<Response, ApiError> {
    // Cap the whole endpoint end-to-end. If this trips, the caller's
    // retry is SAFE — per-partition rotation is idempotent on the same
    // (new_kid, secret_hex) and the per-partition SETNX lock prevents
    // double-rotation under concurrent retries.
    let rotate_fut = server.rotate_waitpoint_secret(&body.new_kid, &body.new_secret_hex);
    let result = match tokio::time::timeout(ROTATE_HTTP_TIMEOUT, rotate_fut).await {
        Ok(r) => r?,
        Err(_) => {
            tracing::error!(
                target: "audit",
                new_kid = %body.new_kid,
                timeout_s = ROTATE_HTTP_TIMEOUT.as_secs(),
                "waitpoint_hmac_rotation_timeout_http_504"
            );
            let body = ErrorBody::plain(format!(
                "rotation exceeded {}s server-side timeout; retry is safe \
                 (per-partition rotation is idempotent on the same new_kid + secret_hex)",
                ROTATE_HTTP_TIMEOUT.as_secs()
            ));
            return Ok((StatusCode::GATEWAY_TIMEOUT, Json(body)).into_response());
        }
    };
    // Operator action log at audit target (per-partition detail logged inside
    // rotate_waitpoint_secret). Returns 400 only on actionable fault states.
    //
    // Two distinct rotated==0 cases:
    //   - failed.is_empty() → no partitions at all (num_flow_partitions == 0;
    //     env_u16_positive rejects this at boot so this is mostly dead code
    //     for library/Default callers).
    //   - !failed.is_empty() → every partition attempt raised a real error.
    //     Operator investigates Valkey/auth/cluster health before retrying.
    if result.rotated == 0 && result.failed.is_empty() {
        return Err(ApiError::from(ServerError::OperationFailed(
            "rotation had no partitions to operate on \
             (num_flow_partitions is 0 — server misconfigured)"
                .to_owned(),
        )));
    }
    if result.rotated == 0 && !result.failed.is_empty() {
        return Err(ApiError::from(ServerError::OperationFailed(
            "rotation failed on all partitions (check Valkey connectivity)".to_owned(),
        )));
    }
    Ok(Json(result).into_response())
}

#[derive(Deserialize)]
struct ChangePriorityBody {
    new_priority: i32,
}

async fn change_priority(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(body): AppJson<ChangePriorityBody>,
) -> Result<Json<ChangePriorityResult>, ApiError> {
    // RFC-017 Stage C migration (§4 row 17): dispatch through trait.
    let eid = parse_execution_id(&id)?;
    let args = ff_core::contracts::ChangePriorityArgs {
        execution_id: eid,
        new_priority: body.new_priority,
        // Empty lane triggers backend-internal HGET pre-read; wire
        // format carries no lane field today (the ChangePriorityBody
        // REST shape only has `new_priority`). Matches legacy
        // `Server::change_priority` behaviour.
        lane_id: LaneId::new(""),
        now: ff_core::types::TimestampMs::now(),
    };
    Ok(Json(server.backend().change_priority(args).await?))
}

async fn replay_execution(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<ReplayExecutionResult>, ApiError> {
    // RFC-017 Stage C migration (§4 row 3 Hard — variadic KEYS
    // pre-read lives inside `ValkeyBackend::replay_execution`).
    let eid = parse_execution_id(&id)?;
    let args = ff_core::contracts::ReplayExecutionArgs {
        execution_id: eid,
        now: ff_core::types::TimestampMs::now(),
    };
    Ok(Json(server.backend().replay_execution(args).await?))
}

async fn revoke_lease(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<RevokeLeaseResult>, ApiError> {
    // RFC-017 Stage C migration (§4 row 19).
    let eid = parse_execution_id(&id)?;
    let args = ff_core::contracts::RevokeLeaseArgs {
        execution_id: eid,
        expected_lease_id: None,
        // Empty WIID → backend HGET pre-read. Matches pre-migration
        // shape where `Server::revoke_lease` read `current_worker_
        // instance_id` off exec_core itself.
        worker_instance_id: WorkerInstanceId::new(""),
        reason: "operator_revoke".to_owned(),
    };
    Ok(Json(server.backend().revoke_lease(args).await?))
}

// ── Scheduler-routed claim (Batch C item 2 PR-B) ──
//
// The server exposes the scheduler's `claim_for_worker` cycle via
// HTTP so ff-sdk workers can acquire claim grants without enabling
// the `direct-valkey-claim` feature. The request body carries lane +
// identity + capabilities; the server returns a serialized
// `ClaimGrant` (or 204 No Content when no eligible execution exists).

/// Request body for `POST /v1/workers/{worker_id}/claim`.
#[derive(Deserialize)]
struct ClaimForWorkerBody {
    lane_id: String,
    worker_instance_id: String,
    /// Capability tokens this worker advertises. Sorted + validated
    /// on the scheduler side; any non-printable/CSV-breaking token
    /// surfaces as 400.
    #[serde(default)]
    capabilities: Vec<String>,
    /// Grant TTL in milliseconds. Bounded so a worker can't request a
    /// multi-hour grant and squat the execution.
    grant_ttl_ms: u64,
}

/// Wire shape for `ff_core::contracts::ClaimGrant`. Carries the
/// opaque [`ff_core::partition::PartitionKey`] on the wire; consumers
/// never see the internal `PartitionFamily` enum (issue #91).
#[derive(Serialize)]
struct ClaimGrantDto {
    execution_id: String,
    partition_key: ff_core::partition::PartitionKey,
    grant_key: String,
    expires_at_ms: u64,
}

impl From<ff_core::contracts::ClaimGrant> for ClaimGrantDto {
    fn from(g: ff_core::contracts::ClaimGrant) -> Self {
        Self {
            execution_id: g.execution_id.to_string(),
            partition_key: g.partition_key,
            grant_key: g.grant_key,
            expires_at_ms: g.expires_at_ms,
        }
    }
}

/// Maximum grant TTL accepted via HTTP. Mirrors the scheduler's
/// internal ceiling so a misconfigured worker can't squat an
/// execution on a multi-hour grant.
const CLAIM_GRANT_TTL_MS_MAX: u64 = 60_000;

/// Reject empty / whitespace / non-printable identifiers the way
/// [`LaneId::try_new`] does for lanes. WorkerId + WorkerInstanceId
/// feed into scheduler scan jitter + Valkey key construction; silent
/// acceptance of "" or "w\nork" would either mis-key or mis-hash.
fn validate_identifier(field: &str, value: &str) -> Result<(), ApiError> {
    if value.is_empty() {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "{field}: must not be empty"
        ))));
    }
    if value.len() > 256 {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "{field}: exceeds 256 bytes (got {})",
            value.len()
        ))));
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "{field}: must not contain whitespace or control characters"
        ))));
    }
    Ok(())
}

async fn claim_for_worker(
    State(server): State<Arc<Server>>,
    Path(worker_id): Path<String>,
    AppJson(body): AppJson<ClaimForWorkerBody>,
) -> Result<Response, ApiError> {
    validate_identifier("worker_id", &worker_id)?;
    validate_identifier("worker_instance_id", &body.worker_instance_id)?;
    let worker_id = WorkerId::new(worker_id);
    let worker_instance_id = WorkerInstanceId::new(body.worker_instance_id);
    let lane = LaneId::try_new(body.lane_id).map_err(|e| {
        ApiError(ServerError::InvalidInput(format!("lane_id: {e}")))
    })?;
    if body.grant_ttl_ms == 0 || body.grant_ttl_ms > CLAIM_GRANT_TTL_MS_MAX {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "grant_ttl_ms must be in 1..={CLAIM_GRANT_TTL_MS_MAX}"
        ))));
    }
    let caps: std::collections::BTreeSet<String> =
        body.capabilities.into_iter().collect();

    // RFC-017 Stage C migration (§4 row 9 / §7): dispatch through
    // the backend trait — `ValkeyBackend::claim_for_worker` forwards
    // to its wired `ff_scheduler::Scheduler` handle.
    let args = ff_core::contracts::ClaimForWorkerArgs::new(
        lane,
        worker_id,
        worker_instance_id,
        caps,
        body.grant_ttl_ms,
    );
    match server.backend().claim_for_worker(args).await? {
        ff_core::contracts::ClaimForWorkerOutcome::Granted(grant) => {
            Ok((StatusCode::OK, Json(ClaimGrantDto::from(grant))).into_response())
        }
        ff_core::contracts::ClaimForWorkerOutcome::NoWork => {
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        // `ClaimForWorkerOutcome` is `#[non_exhaustive]` for additive
        // variants (e.g. `BackPressured { retry_after_ms }`). Until
        // such a variant lands, surface any new shape as 503 with a
        // clear message pointing at the client-library version
        // mismatch.
        _ => Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorBody::plain(
                "claim_for_worker: backend returned a non-exhaustive outcome this server build does not understand".to_owned(),
            )),
        )
            .into_response()),
    }
}

// ── Stream read + tail ──

#[derive(Deserialize)]
struct ReadStreamParams {
    #[serde(default = "ff_core::contracts::StreamCursor::start")]
    from: ff_core::contracts::StreamCursor,
    #[serde(default = "ff_core::contracts::StreamCursor::end")]
    to: ff_core::contracts::StreamCursor,
    #[serde(default = "default_read_limit")]
    limit: u64,
}

fn default_read_limit() -> u64 { 100 }

#[derive(Serialize)]
struct ReadStreamResponse {
    frames: Vec<StreamFrame>,
    count: usize,
    /// When set, the producer has closed this stream — consumer should
    /// stop polling. Absent when the stream is still open (or never
    /// existed, which is indistinguishable from "still open" at this
    /// layer).
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<i64>,
    /// Reason from the closing writer: `attempt_success`, `attempt_failure`,
    /// `attempt_cancelled`, `attempt_interrupted`. Absent iff still open.
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_reason: Option<String>,
}

impl From<ff_core::contracts::StreamFrames> for ReadStreamResponse {
    fn from(sf: ff_core::contracts::StreamFrames) -> Self {
        let count = sf.frames.len();
        Self {
            frames: sf.frames,
            count,
            closed_at: sf.closed_at.map(|t| t.0),
            closed_reason: sf.closed_reason,
        }
    }
}

/// REST-layer ceiling on `limit` for stream read/tail responses. Lower
/// than the internal `STREAM_READ_HARD_CAP` (10_000) because an HTTP
/// response buffers the whole JSON body in memory in axum — a
/// `10_000 × max_payload_bytes (65_536)` body is ~640MB per call, which
/// is a DoS vector from a single client. Internal callers using FCALL or
/// the SDK directly still get the full 10_000 ceiling; REST clients must
/// paginate through `from`/`to` for larger spans.
///
/// v2 candidate: chunked-transfer / SSE when the caller wants > this bound.
const REST_STREAM_LIMIT_CEILING: u64 = 1_000;

async fn read_attempt_stream(
    State(server): State<Arc<Server>>,
    Path((id, idx)): Path<(String, u32)>,
    Query(params): Query<ReadStreamParams>,
) -> Result<Json<ReadStreamResponse>, ApiError> {
    if params.limit == 0 {
        return Err(ApiError(ServerError::InvalidInput(
            "limit must be >= 1".to_owned(),
        )));
    }
    if params.limit > REST_STREAM_LIMIT_CEILING {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "limit exceeds REST ceiling {REST_STREAM_LIMIT_CEILING}; paginate via from/to for larger spans"
        ))));
    }
    let eid = parse_execution_id(&id)?;
    let attempt_index = AttemptIndex::new(idx);
    let result = server
        .read_attempt_stream(
            &eid,
            attempt_index,
            params.from.to_wire(),
            params.to.to_wire(),
            params.limit,
        )
        .await?;
    Ok(Json(result.into()))
}

#[derive(Deserialize)]
struct TailStreamParams {
    #[serde(default = "ff_core::contracts::StreamCursor::beginning")]
    after: ff_core::contracts::StreamCursor,
    #[serde(default)]
    block_ms: u64,
    #[serde(default = "default_tail_limit")]
    limit: u64,
}

fn default_tail_limit() -> u64 { 50 }

/// Ceiling on BLOCK duration for the tail endpoint. Kept below common LB
/// idle timeouts (ALB 60s, nginx 60s, Cloudflare 100s) so the HTTP response
/// can't be cut mid-block.
///
/// Note: ferriskey's client auto-extends its `request_timeout` for XREAD
/// BLOCK to `block_ms + 500ms`, so a blocking call with the full ceiling
/// never produces a spurious transport timeout. See
/// `ff_script::stream_tail` module docs for the exact ferriskey code path.
const MAX_TAIL_BLOCK_MS: u64 = 30_000;

async fn tail_attempt_stream(
    State(server): State<Arc<Server>>,
    Path((id, idx)): Path<(String, u32)>,
    Query(params): Query<TailStreamParams>,
) -> Result<Json<ReadStreamResponse>, ApiError> {
    if params.block_ms > MAX_TAIL_BLOCK_MS {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "block_ms exceeds {MAX_TAIL_BLOCK_MS}ms ceiling"
        ))));
    }
    if params.limit == 0 {
        return Err(ApiError(ServerError::InvalidInput(
            "limit must be >= 1".to_owned(),
        )));
    }
    if params.limit > REST_STREAM_LIMIT_CEILING {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "limit exceeds REST ceiling {REST_STREAM_LIMIT_CEILING}; paginate via after for larger spans"
        ))));
    }
    // XREAD cursor must be a concrete ID — `Start`/`End` are
    // XRANGE-only. The opaque-cursor deserializer already rejects
    // the bare `-`/`+` wire tokens; this boundary also rejects the
    // structured `start`/`end` keywords because XREAD treats them
    // as invalid ids. Uses [`StreamCursor::is_concrete`] so the
    // SDK + REST guards stay in lock-step.
    if !params.after.is_concrete() {
        return Err(ApiError(ServerError::InvalidInput(
            "after: XREAD cursor must be a concrete entry id; pass '0-0' to start from the beginning"
                .to_owned(),
        )));
    }

    let eid = parse_execution_id(&id)?;
    let attempt_index = AttemptIndex::new(idx);
    let result = server
        .tail_attempt_stream(
            &eid,
            attempt_index,
            params.after.to_wire(),
            params.block_ms,
            params.limit,
        )
        .await?;
    Ok(Json(result.into()))
}

// ── Flow handlers ──

async fn create_flow(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateFlowArgs>,
) -> Result<(StatusCode, Json<CreateFlowResult>), ApiError> {
    // RFC-017 Stage D1 (§4 row 5): trait-dispatched ingress.
    let result = server.backend().create_flow(args).await?;
    let status = match &result {
        CreateFlowResult::Created { .. } => StatusCode::CREATED,
        CreateFlowResult::AlreadySatisfied { .. } => StatusCode::OK,
    };
    Ok((status, Json(result)))
}

async fn add_execution_to_flow(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<AddExecutionToFlowArgs>,
) -> Result<(StatusCode, Json<AddExecutionToFlowResult>), ApiError> {
    let path_fid = parse_flow_id(&id)?;
    check_id_match(&path_fid, &args.flow_id, "flow_id")?;
    args.flow_id = path_fid;
    // RFC-017 Stage D1 (§4 row 5): trait-dispatched ingress.
    let result = server.backend().add_execution_to_flow(args).await?;
    let status = match &result {
        AddExecutionToFlowResult::Added { .. } => StatusCode::CREATED,
        AddExecutionToFlowResult::AlreadyMember { .. } => StatusCode::OK,
    };
    Ok((status, Json(result)))
}

/// Cancel a flow.
///
/// By default the handler returns immediately with
/// [`CancelFlowResult::CancellationScheduled`] (or `Cancelled` for flows
/// with no members / non-cancel_all policies), and the individual member
/// execution cancellations run in a background task on the server.
/// Clients can track per-member progress by polling
/// `GET /v1/executions/{id}/state` for each id in `member_execution_ids`.
///
/// Pass `?wait=true` to run the dispatch loop inline; the handler will not
/// return until every member has been cancelled. Useful for tests and
/// callers that need synchronous completion.
async fn cancel_flow(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    AppJson(mut args): AppJson<CancelFlowArgs>,
) -> Result<Json<CancelFlowResult>, ApiError> {
    let path_fid = parse_flow_id(&id)?;
    check_id_match(&path_fid, &args.flow_id, "flow_id")?;
    args.flow_id = path_fid;
    let wait = params.get("wait").is_some_and(|v| v == "true" || v == "1");
    // RFC-017 Stage D1 (§4 row 5): header-only dispatch via the trait.
    // The synchronous `?wait=true` path retains the inherent
    // `Server::cancel_flow_wait` route because the trait's
    // `cancel_flow(id, policy, wait)` signature rejects `wait` variants
    // other than `NoWait` (Valkey impl returns `Unavailable` for timed
    // waits — `ff_cancel_flow` FCALL is NoWait by construction). The
    // async member-cancel dispatcher inside `Server::cancel_flow_inner`
    // continues to service `?wait=false` through the inherent path
    // until D2 relocates it into `ValkeyBackend::cancel_flow_with_args`.
    let result = if wait {
        server.cancel_flow_wait(&args).await?
    } else {
        server.cancel_flow(&args).await?
    };
    Ok(Json(result))
}

async fn stage_dependency_edge(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<StageDependencyEdgeArgs>,
) -> Result<(StatusCode, Json<StageDependencyEdgeResult>), ApiError> {
    let path_fid = parse_flow_id(&id)?;
    check_id_match(&path_fid, &args.flow_id, "flow_id")?;
    args.flow_id = path_fid;
    // RFC-017 Stage D1 (§4 row 5): trait-dispatched ingress.
    let result = server.backend().stage_dependency_edge(args).await?;
    Ok((StatusCode::CREATED, Json(result)))
}

async fn apply_dependency_to_child(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<ApplyDependencyToChildArgs>,
) -> Result<Json<ApplyDependencyToChildResult>, ApiError> {
    let path_fid = parse_flow_id(&id)?;
    check_id_match(&path_fid, &args.flow_id, "flow_id")?;
    args.flow_id = path_fid;
    // RFC-017 Stage D1 (§4 row 5): trait-dispatched ingress.
    Ok(Json(server.backend().apply_dependency_to_child(args).await?))
}

// ── Budget / Quota handlers ──

async fn create_budget(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateBudgetArgs>,
) -> Result<(StatusCode, Json<CreateBudgetResult>), ApiError> {
    // RFC-017 Stage C migration (§4 row 7).
    let result = server.backend().create_budget(args).await?;
    let status = match &result {
        CreateBudgetResult::Created { .. } => StatusCode::CREATED,
        CreateBudgetResult::AlreadySatisfied { .. } => StatusCode::OK,
    };
    Ok((status, Json(result)))
}

async fn get_budget_status(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<BudgetStatus>, ApiError> {
    // RFC-017 Stage C migration (§4 row 7 — budget read).
    let bid = parse_budget_id(&id)?;
    Ok(Json(server.backend().get_budget_status(&bid).await?))
}

#[derive(Deserialize)]
struct ReportUsageBody {
    dimensions: HashMap<String, u64>,
    now: ff_core::types::TimestampMs,
    #[serde(default)]
    dedup_key: Option<String>,
}

async fn report_usage(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(body): AppJson<ReportUsageBody>,
) -> Result<Json<ReportUsageResult>, ApiError> {
    // RFC-017 Stage C migration (§4 row 7 — admin path via
    // `report_usage_admin`; no worker handle consumed).
    let bid = parse_budget_id(&id)?;
    let dims: Vec<String> = body.dimensions.keys().cloned().collect();
    let deltas: Vec<u64> = dims.iter().map(|d| body.dimensions[d]).collect();
    let mut args = ff_core::contracts::ReportUsageAdminArgs::new(dims, deltas, body.now);
    if let Some(k) = body.dedup_key {
        args = args.with_dedup_key(k);
    }
    Ok(Json(server.backend().report_usage_admin(&bid, args).await?))
}

async fn reset_budget(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<ResetBudgetResult>, ApiError> {
    // RFC-017 Stage C migration (§4 row 7).
    let bid = parse_budget_id(&id)?;
    let args = ff_core::contracts::ResetBudgetArgs {
        budget_id: bid,
        now: ff_core::types::TimestampMs::now(),
    };
    Ok(Json(server.backend().reset_budget(args).await?))
}

async fn create_quota_policy(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateQuotaPolicyArgs>,
) -> Result<(StatusCode, Json<CreateQuotaPolicyResult>), ApiError> {
    // RFC-017 Stage C migration (§4 row 7).
    let result = server.backend().create_quota_policy(args).await?;
    let status = match &result {
        CreateQuotaPolicyResult::Created { .. } => StatusCode::CREATED,
        CreateQuotaPolicyResult::AlreadySatisfied { .. } => StatusCode::OK,
    };
    Ok((status, Json(result)))
}

// ── Health check ──

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn healthz(
    State(server): State<Arc<Server>>,
) -> Result<Json<HealthResponse>, ApiError> {
    // RFC-017 Stage D2 (§4 row 1): dispatch through the backend trait's
    // `ping` so the healthz probe remains backend-agnostic. Valkey
    // impl issues `PING`; Postgres impl (Wave 4) runs `SELECT 1`.
    server
        .backend()
        .ping()
        .await
        .map_err(|e| ApiError(ServerError::Engine(Box::new(e))))?;
    Ok(Json(HealthResponse { status: "ok" }))
}

// ── ID parsing helpers ──

/// Return 400 if the body contains an ID that differs from the path ID.
fn check_id_match<T: PartialEq + fmt::Display>(path_id: &T, body_id: &T, id_name: &str) -> Result<(), ApiError> {
    if body_id != path_id {
        return Err(ApiError(ServerError::InvalidInput(format!(
            "path {id_name} does not match body {id_name}"
        ))));
    }
    Ok(())
}

fn parse_execution_id(s: &str) -> Result<ExecutionId, ApiError> {
    ExecutionId::parse(s)
        .map_err(|e| ApiError(ServerError::InvalidInput(format!("invalid execution_id: {e}"))))
}

fn parse_flow_id(s: &str) -> Result<FlowId, ApiError> {
    FlowId::parse(s)
        .map_err(|e| ApiError(ServerError::InvalidInput(format!("invalid flow_id: {e}"))))
}

fn parse_budget_id(s: &str) -> Result<BudgetId, ApiError> {
    BudgetId::parse(s)
        .map_err(|e| ApiError(ServerError::InvalidInput(format!("invalid budget_id: {e}"))))
}

#[cfg(test)]
mod cors_tests {
    //! CORS + auth preflight tests (#66, #71).
    //!
    //! Exercised via `tower::ServiceExt::oneshot` against a minimal router
    //! that applies only the CORS layer to a noop `/healthz`-style route.
    //! This mirrors what browsers actually see (the `CorsLayer` is terminal
    //! for OPTIONS preflights) without needing a live `Server`.
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn app_with_cors(origins: &[String], auth_enabled: bool) -> Router {
        let cors = build_cors_layer(origins, auth_enabled)
            .expect("build_cors_layer succeeds for valid inputs");
        Router::new().route("/noop", get(|| async { "ok" })).layer(cors)
    }

    #[test]
    fn all_origins_invalid_returns_config_error_instead_of_permissive() {
        // #71: invalid HeaderValue (contains a control character) should
        // fail closed rather than falling back to permissive CORS.
        //
        // Note: `HeaderValue` parsing is intentionally lax — it accepts
        // almost any printable ASCII, so "not a url" parses fine. We use
        // a string with an embedded NUL byte to exercise the actual parse
        // failure path.
        let err = build_cors_layer(&["bad\0origin".to_owned()], false).unwrap_err();
        let ConfigError::InvalidValue { var, message } = err;
        assert_eq!(var, "FF_CORS_ORIGINS");
        assert!(
            message.contains("all configured origins failed to parse"),
            "message was: {message}"
        );
    }

    #[test]
    fn wildcard_still_returns_permissive() {
        // Explicit `*` is still the documented permissive path.
        let layer = build_cors_layer(&["*".to_owned()], false);
        assert!(layer.is_ok());
    }

    #[test]
    fn empty_origins_returns_empty_allowlist_ok() {
        // Empty list (no origins configured) produces an empty allowlist;
        // no browser origin matches. This is NOT the fail-open case — that
        // only happens when origins are configured but all parse-invalid.
        let layer = build_cors_layer(&[], false);
        assert!(layer.is_ok());
    }

    #[test]
    fn mixed_valid_and_invalid_keeps_valid_entries() {
        // A partial typo should not fail the whole boot — we log a warning
        // and keep the parsable entries.
        let layer = build_cors_layer(
            &["https://ok.example.com".to_owned(), "bad\0origin".to_owned()],
            false,
        );
        assert!(layer.is_ok());
    }

    #[tokio::test]
    async fn preflight_allows_authorization_when_auth_enabled() {
        // #66: with token auth on, browsers need `Authorization` in the
        // preflight's `Access-Control-Allow-Headers` response.
        let app = app_with_cors(&["https://client.example.com".to_owned()], true);

        let req = Request::builder()
            .method("OPTIONS")
            .uri("/noop")
            .header("origin", "https://client.example.com")
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization,content-type")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let allow_headers = resp
            .headers()
            .get("access-control-allow-headers")
            .expect("access-control-allow-headers present")
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            allow_headers.contains("authorization"),
            "expected `authorization` in Access-Control-Allow-Headers, got: {allow_headers}"
        );
        assert!(
            allow_headers.contains("content-type"),
            "expected `content-type` in Access-Control-Allow-Headers, got: {allow_headers}"
        );
    }

    #[tokio::test]
    async fn preflight_omits_authorization_when_auth_disabled() {
        // Negative case: without auth, Authorization is not needed and
        // should not be advertised (principle of least privilege).
        let app = app_with_cors(&["https://client.example.com".to_owned()], false);

        let req = Request::builder()
            .method("OPTIONS")
            .uri("/noop")
            .header("origin", "https://client.example.com")
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "content-type")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let allow_headers = resp
            .headers()
            .get("access-control-allow-headers")
            .expect("access-control-allow-headers present")
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            !allow_headers.contains("authorization"),
            "Authorization should not be advertised when auth is off; got: {allow_headers}"
        );
        assert!(allow_headers.contains("content-type"));
    }
}

#[cfg(test)]
mod claim_grant_dto_tests {
    //! Wire-shape tests for `ClaimGrantDto` (issue #91).
    //!
    //! Pins the exact JSON shape the server emits on the
    //! `POST /v1/workers/{id}/claim` 200 response so any drift shows
    //! up at compile-adjacent test time rather than on a live SDK.
    use super::*;
    use ff_core::contracts::ClaimGrant;
    use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
    use ff_core::types::{ExecutionId, FlowId};

    fn sample_grant(family: PartitionFamily) -> ClaimGrant {
        let config = ff_core::partition::PartitionConfig::default();
        let fid = FlowId::new();
        let eid = ExecutionId::for_flow(&fid, &config);
        let p = Partition { family, index: 7 };
        ClaimGrant::new(
            eid,
            PartitionKey::from(&p),
            "ff:exec:{fp:7}:deadbeef:claim_grant".to_owned(),
            1_700_000_000_000,
        )
    }

    #[test]
    fn claim_grant_dto_emits_opaque_partition_key() {
        let dto = ClaimGrantDto::from(sample_grant(PartitionFamily::Flow));
        let json = serde_json::to_value(&dto).unwrap();
        // `partition_key` serialises transparent: a bare string, not
        // an object, not a `partition_family`/`partition_index` pair.
        assert_eq!(json["partition_key"], serde_json::json!("{fp:7}"));
        assert!(json.get("partition_family").is_none());
        assert!(json.get("partition_index").is_none());
        assert_eq!(json["expires_at_ms"], serde_json::json!(1_700_000_000_000u64));
    }

    #[test]
    fn claim_grant_dto_collapses_execution_alias_on_wire() {
        // RFC-011 §11 alias: Execution-family grants emit the same
        // `{fp:N}` hash tag as Flow. This test pins the wire-level
        // equivalence.
        let dto_flow = ClaimGrantDto::from(sample_grant(PartitionFamily::Flow));
        let dto_exec = ClaimGrantDto::from(sample_grant(PartitionFamily::Execution));
        let jf = serde_json::to_value(&dto_flow).unwrap();
        let je = serde_json::to_value(&dto_exec).unwrap();
        assert_eq!(jf["partition_key"], je["partition_key"]);
    }
}
