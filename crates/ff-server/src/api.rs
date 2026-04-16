//! REST API layer — thin axum handlers over Server methods.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
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
                let body = ErrorBody {
                    error: rejection.body_text(),
                };
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

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self.0 {
            ServerError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            ServerError::InvalidInput(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            ServerError::OperationFailed(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

// ── Router ──

pub fn router(server: Arc<Server>, cors_origins: &[String]) -> Router {
    let cors = build_cors_layer(cors_origins);

    Router::new()
        // Executions
        .route("/v1/executions", get(list_executions).post(create_execution))
        .route("/v1/executions/{id}", get(get_execution))
        .route("/v1/executions/{id}/state", get(get_execution_state))
        .route("/v1/executions/{id}/cancel", post(cancel_execution))
        .route("/v1/executions/{id}/signal", post(deliver_signal))
        .route("/v1/executions/{id}/priority", put(change_priority))
        .route("/v1/executions/{id}/replay", post(replay_execution))
        .route("/v1/executions/{id}/revoke-lease", post(revoke_lease))
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
        // Health
        .route("/healthz", get(healthz))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(server)
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
    let lane = ff_core::types::LaneId::new(&params.lane);
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
) -> Result<Json<AddExecutionToFlowResult>, ApiError> {
    let path_fid = parse_flow_id(&id)?;
    check_id_match(&path_fid, &args.flow_id, "flow_id")?;
    args.flow_id = path_fid;
    Ok(Json(server.add_execution_to_flow(&args).await?))
}

async fn cancel_flow(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<CancelFlowArgs>,
) -> Result<Json<CancelFlowResult>, ApiError> {
    let path_fid = parse_flow_id(&id)?;
    check_id_match(&path_fid, &args.flow_id, "flow_id")?;
    args.flow_id = path_fid;
    Ok(Json(server.cancel_flow(&args).await?))
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
        .map_err(|e| ApiError(ServerError::Valkey(e.to_string())))?;
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
