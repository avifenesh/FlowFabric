//! REST API layer — thin axum handlers over Server methods.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use axum::http::{HeaderName, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use ff_core::contracts::*;
use ff_core::state::PublicState;
use ff_core::types::*;

use crate::server::{Server, ServerError};

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
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(rejection) => {
                let status = rejection.status();
                tracing::debug!(detail = %rejection.body_text(), "JSON rejection");
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

/// HTTP error body. `kind`/`retryable` are populated for 500s backed by a
/// `ferriskey::Error` so HTTP clients (e.g. cairn-fabric) can make retry
/// decisions without parsing the `error` string.
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
        use ff_script::retry::kind_to_stable_str;

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
            ServerError::Valkey(e) => {
                let kind_str = kind_to_stable_str(e.kind());
                tracing::error!(
                    kind = kind_str,
                    code = e.code().unwrap_or(""),
                    detail = e.detail().unwrap_or(""),
                    "valkey error"
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
            ServerError::ValkeyContext { source, context } => {
                let kind_str = kind_to_stable_str(source.kind());
                tracing::error!(
                    kind = kind_str,
                    code = source.code().unwrap_or(""),
                    detail = source.detail().unwrap_or(""),
                    context = %context,
                    "valkey error"
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
                let kind_str = load_err.valkey_kind().map(kind_to_stable_str);
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

pub fn router(server: Arc<Server>, cors_origins: &[String], api_token: Option<String>) -> Router {
    let cors = build_cors_layer(cors_origins);

    let mut app = Router::new()
        // Executions
        .route("/v1/executions", get(list_executions).post(create_execution))
        .route("/v1/executions/{id}", get(get_execution))
        .route("/v1/executions/{id}/state", get(get_execution_state))
        .route(
            "/v1/executions/{id}/pending-waitpoints",
            get(list_pending_waitpoints),
        )
        .route("/v1/executions/{id}/result", get(get_execution_result))
        .route("/v1/executions/{id}/cancel", post(cancel_execution))
        .route("/v1/executions/{id}/signal", post(deliver_signal))
        .route("/v1/executions/{id}/priority", put(change_priority))
        .route("/v1/executions/{id}/replay", post(replay_execution))
        .route("/v1/executions/{id}/revoke-lease", post(revoke_lease))
        // Scheduler-routed claim (Batch C item 2). Worker POSTs lane +
        // identity + capabilities; server runs budget/quota/capability
        // admission via ff-scheduler and returns a ClaimGrant on
        // success (204 No Content when no eligible execution).
        .route("/v1/workers/{worker_id}/claim", post(claim_for_worker))
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
        .route("/v1/flows", post(create_flow))
        .route("/v1/flows/{id}/members", post(add_execution_to_flow))
        .route("/v1/flows/{id}/cancel", post(cancel_flow))
        .route("/v1/flows/{id}/edges", post(stage_dependency_edge))
        .route("/v1/flows/{id}/edges/apply", post(apply_dependency_to_child))
        // Budgets
        .route("/v1/budgets", post(create_budget))
        .route("/v1/budgets/{id}", get(get_budget_status))
        .route("/v1/budgets/{id}/usage", post(report_usage))
        .route("/v1/budgets/{id}/reset", post(reset_budget))
        // Quotas
        .route("/v1/quotas", post(create_quota_policy))
        // Admin: rotate waitpoint HMAC secret (RFC-004 §Waitpoint Security)
        .route("/v1/admin/rotate-waitpoint-secret", post(rotate_waitpoint_secret))
        // Health (always unauthenticated)
        .route("/healthz", get(healthz));

    if let Some(token) = api_token {
        let token = Arc::new(token);
        app = app.layer(middleware::from_fn(move |req, next| {
            let token = token.clone();
            auth_middleware(token, req, next)
        }));
    }

    app.layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(server)
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

fn build_cors_layer(origins: &[String]) -> CorsLayer {
    if origins.iter().any(|o| o == "*") {
        return CorsLayer::permissive();
    }
    let parsed: Vec<_> = origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    if parsed.is_empty() && !origins.is_empty() {
        tracing::warn!(
            configured = ?origins,
            "all configured CORS origins failed to parse, falling back to permissive"
        );
        return CorsLayer::permissive();
    }
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(parsed))
        .allow_methods([Method::GET, Method::POST, Method::PUT])
        .allow_headers([HeaderName::from_static("content-type")])
}

// ── Execution handlers ──

#[derive(Deserialize)]
struct ListExecutionsParams {
    partition: u16,
    #[serde(default = "default_lane")]
    lane: String,
    #[serde(default = "default_state_filter")]
    state: String,
    #[serde(default = "default_limit")]
    limit: u64,
    #[serde(default)]
    offset: u64,
}

fn default_lane() -> String { "default".to_owned() }
fn default_state_filter() -> String { "eligible".to_owned() }
fn default_limit() -> u64 { 50 }

async fn list_executions(
    State(server): State<Arc<Server>>,
    Query(params): Query<ListExecutionsParams>,
) -> Result<Json<ListExecutionsResult>, ApiError> {
    let lane = ff_core::types::LaneId::try_new(params.lane.clone())
        .map_err(|e| ApiError::from(ServerError::InvalidInput(format!("invalid lane: {e}"))))?;
    let limit = params.limit.min(1000);
    let result = server
        .list_executions(params.partition, &lane, &params.state, params.offset, limit)
        .await?;
    Ok(Json(result))
}

async fn create_execution(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateExecutionArgs>,
) -> Result<(StatusCode, Json<CreateExecutionResult>), ApiError> {
    let result = server.create_execution(&args).await?;
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
/// execution, including the HMAC `waitpoint_token` required to deliver
/// signals. Human reviewers use this to look up the token originally
/// returned only to the suspending worker's `SuspendOutcome`.
///
/// SECURITY: `waitpoint_token` is a bearer credential for signal
/// delivery; leaking it lets a third party forge authority to resume or
/// influence the execution. Gate the endpoint behind `FF_API_TOKEN` in
/// any deployment reachable from untrusted networks. The auth middleware
/// only mounts when `FF_API_TOKEN` is set; this endpoint is
/// unauthenticated without it, and the server logs a loud warning at
/// startup so operators notice.
async fn list_pending_waitpoints(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<PendingWaitpointInfo>>, ApiError> {
    let eid = parse_execution_id(&id)?;
    Ok(Json(server.list_pending_waitpoints(&eid).await?))
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
    let eid = parse_execution_id(&id)?;
    match server.get_execution_result(&eid).await? {
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
    let path_eid = parse_execution_id(&id)?;
    check_id_match(&path_eid, &args.execution_id, "execution_id")?;
    args.execution_id = path_eid;
    Ok(Json(server.cancel_execution(&args).await?))
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
    // rotate_waitpoint_secret). Returns 200 if at least one partition rotated
    // OR at least one partition was already in-flight under a concurrent
    // rotation. Returns 400 only on actionable fault states.
    //
    // Three distinct rotated==0 cases:
    //   - all in_progress → concurrent rotation handling it, NOT a failure.
    //     200 OK with the structured result so operators see what's happening.
    //   - failed.is_empty() && in_progress.is_empty() → no partitions at all
    //     (num_flow_partitions == 0; env_u16_positive rejects this at
    //     boot so this is mostly dead code for library/Default callers).
    //   - !failed.is_empty() → every partition attempt raised a real error.
    //     Operator investigates Valkey/auth/cluster health before retrying.
    if result.rotated == 0 && result.failed.is_empty() && result.in_progress.is_empty() {
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
    // rotated==0 but some in_progress → 200 OK; caller polls / re-runs.
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
    let eid = parse_execution_id(&id)?;
    Ok(Json(server.change_priority(&eid, body.new_priority).await?))
}

async fn replay_execution(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<ReplayExecutionResult>, ApiError> {
    let eid = parse_execution_id(&id)?;
    Ok(Json(server.replay_execution(&eid).await?))
}

async fn revoke_lease(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<RevokeLeaseResult>, ApiError> {
    let eid = parse_execution_id(&id)?;
    Ok(Json(server.revoke_lease(&eid).await?))
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

/// Wire shape for `ff_core::contracts::ClaimGrant`. Core type is not
/// serde-derived (it carries a `Partition` with a non-scalar family
/// enum); keep a DTO here so we own the HTTP shape without touching
/// the core contract.
#[derive(Serialize)]
struct ClaimGrantDto {
    execution_id: String,
    partition_family: &'static str,
    partition_index: u16,
    grant_key: String,
    expires_at_ms: u64,
}

impl From<ff_core::contracts::ClaimGrant> for ClaimGrantDto {
    fn from(g: ff_core::contracts::ClaimGrant) -> Self {
        let family = match g.partition.family {
            ff_core::partition::PartitionFamily::Flow => "flow",
            ff_core::partition::PartitionFamily::Execution => "execution",
            ff_core::partition::PartitionFamily::Budget => "budget",
            ff_core::partition::PartitionFamily::Quota => "quota",
        };
        Self {
            execution_id: g.execution_id.to_string(),
            partition_family: family,
            partition_index: g.partition.index,
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

    match server
        .claim_for_worker(
            &lane,
            &worker_id,
            &worker_instance_id,
            &caps,
            body.grant_ttl_ms,
        )
        .await?
    {
        Some(grant) => Ok((StatusCode::OK, Json(ClaimGrantDto::from(grant))).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

// ── Stream read + tail ──

#[derive(Deserialize)]
struct ReadStreamParams {
    #[serde(default = "default_from_id")]
    from: String,
    #[serde(default = "default_to_id")]
    to: String,
    #[serde(default = "default_read_limit")]
    limit: u64,
}

fn default_from_id() -> String { "-".to_owned() }
fn default_to_id() -> String { "+".to_owned() }
fn default_read_limit() -> u64 { 100 }

/// Reject malformed `from`/`to`/`after` stream IDs at the REST boundary so
/// Valkey doesn't have to. Accepts `"-"`, `"+"`, and `<ms>` or `<ms>-<seq>`
/// decimal IDs as Valkey XRANGE/XREAD does.
///
/// Without this, a request like `?from=abc` would round-trip to Valkey,
/// surface as a script error, and return HTTP 500 — which is wrong for
/// caller-input validation.
fn validate_stream_id(s: &str, field: &str, allow_open_markers: bool) -> Result<(), ApiError> {
    if allow_open_markers && (s == "-" || s == "+") {
        return Ok(());
    }
    // Allowed: `<digits>` or `<digits>-<digits>`.
    let (ms_part, seq_part) = match s.split_once('-') {
        Some((ms, seq)) => (ms, Some(seq)),
        None => (s, None),
    };
    let ms_valid = !ms_part.is_empty() && ms_part.chars().all(|c| c.is_ascii_digit());
    let seq_valid = seq_part
        .map(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(true);
    if ms_valid && seq_valid {
        Ok(())
    } else {
        Err(ApiError(ServerError::InvalidInput(format!(
            "{field}: invalid stream ID '{s}' (expected '-', '+', '<ms>', or '<ms>-<seq>')"
        ))))
    }
}

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
    validate_stream_id(&params.from, "from", true)?;
    validate_stream_id(&params.to, "to", true)?;

    let eid = parse_execution_id(&id)?;
    let attempt_index = AttemptIndex::new(idx);
    let result = server
        .read_attempt_stream(&eid, attempt_index, &params.from, &params.to, params.limit)
        .await?;
    Ok(Json(result.into()))
}

#[derive(Deserialize)]
struct TailStreamParams {
    #[serde(default = "default_tail_after")]
    after: String,
    #[serde(default)]
    block_ms: u64,
    #[serde(default = "default_tail_limit")]
    limit: u64,
}

fn default_tail_after() -> String { "0-0".to_owned() }
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
    // XREAD cursor must be a concrete ID — "-"/"+" are XRANGE-only.
    validate_stream_id(&params.after, "after", false)?;

    let eid = parse_execution_id(&id)?;
    let attempt_index = AttemptIndex::new(idx);
    let result = server
        .tail_attempt_stream(&eid, attempt_index, &params.after, params.block_ms, params.limit)
        .await?;
    Ok(Json(result.into()))
}

// ── Flow handlers ──

async fn create_flow(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateFlowArgs>,
) -> Result<(StatusCode, Json<CreateFlowResult>), ApiError> {
    let result = server.create_flow(&args).await?;
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
    let result = server.add_execution_to_flow(&args).await?;
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
    let result = server.stage_dependency_edge(&args).await?;
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
    Ok(Json(server.apply_dependency_to_child(&args).await?))
}

// ── Budget / Quota handlers ──

async fn create_budget(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateBudgetArgs>,
) -> Result<(StatusCode, Json<CreateBudgetResult>), ApiError> {
    let result = server.create_budget(&args).await?;
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
    let bid = parse_budget_id(&id)?;
    Ok(Json(server.get_budget_status(&bid).await?))
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
    let bid = parse_budget_id(&id)?;
    let dims: Vec<String> = body.dimensions.keys().cloned().collect();
    let deltas: Vec<u64> = dims.iter().map(|d| body.dimensions[d]).collect();
    let args = ReportUsageArgs {
        dimensions: dims,
        deltas,
        now: body.now,
        dedup_key: body.dedup_key,
    };
    Ok(Json(server.report_usage(&bid, &args).await?))
}

async fn reset_budget(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
) -> Result<Json<ResetBudgetResult>, ApiError> {
    let bid = parse_budget_id(&id)?;
    Ok(Json(server.reset_budget(&bid).await?))
}

async fn create_quota_policy(
    State(server): State<Arc<Server>>,
    AppJson(args): AppJson<CreateQuotaPolicyArgs>,
) -> Result<(StatusCode, Json<CreateQuotaPolicyResult>), ApiError> {
    let result = server.create_quota_policy(&args).await?;
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
    let _: String = server
        .client()
        .cmd("PING")
        .execute()
        .await
        .map_err(|e| ApiError(ServerError::ValkeyContext { source: e, context: "healthz PING".into() }))?;
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
