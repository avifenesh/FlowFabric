//! REST API layer — thin axum handlers over Server methods.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
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
            ServerError::Script(msg) if msg.contains("not found") => {
                (StatusCode::NOT_FOUND, msg.clone())
            }
            ServerError::Script(msg) if msg.contains("invalid") => {
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            ServerError::Script(msg) if msg.contains("failed:") => {
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

// ── Router ──

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        // Executions
        .route("/v1/executions", post(create_execution))
        .route("/v1/executions/{id}", get(get_execution))
        .route("/v1/executions/{id}/state", get(get_execution_state))
        .route("/v1/executions/{id}/cancel", post(cancel_execution))
        .route("/v1/executions/{id}/signal", post(deliver_signal))
        .route("/v1/executions/{id}/priority", put(change_priority))
        .route("/v1/executions/{id}/replay", post(replay_execution))
        // Flows
        .route("/v1/flows", post(create_flow))
        .route("/v1/flows/{id}/members", post(add_execution_to_flow))
        .route("/v1/flows/{id}/cancel", post(cancel_flow))
        // Budgets
        .route("/v1/budgets", post(create_budget))
        .route("/v1/budgets/{id}", get(get_budget_status))
        // Quotas
        .route("/v1/quotas", post(create_quota_policy))
        // Health
        .route("/healthz", get(healthz))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(server)
}

// ── Execution handlers ──

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
    args.execution_id = parse_execution_id(&id)?;
    Ok(Json(server.cancel_execution(&args).await?))
}

async fn deliver_signal(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<DeliverSignalArgs>,
) -> Result<Json<DeliverSignalResult>, ApiError> {
    args.execution_id = parse_execution_id(&id)?;
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
    args.flow_id = parse_flow_id(&id)?;
    Ok(Json(server.add_execution_to_flow(&args).await?))
}

async fn cancel_flow(
    State(server): State<Arc<Server>>,
    Path(id): Path<String>,
    AppJson(mut args): AppJson<CancelFlowArgs>,
) -> Result<Json<CancelFlowResult>, ApiError> {
    args.flow_id = parse_flow_id(&id)?;
    Ok(Json(server.cancel_flow(&args).await?))
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

fn parse_execution_id(s: &str) -> Result<ExecutionId, ApiError> {
    ExecutionId::parse(s)
        .map_err(|e| ApiError(ServerError::Script(format!("invalid execution_id: {e}"))))
}

fn parse_flow_id(s: &str) -> Result<FlowId, ApiError> {
    FlowId::parse(s)
        .map_err(|e| ApiError(ServerError::Script(format!("invalid flow_id: {e}"))))
}

fn parse_budget_id(s: &str) -> Result<BudgetId, ApiError> {
    BudgetId::parse(s)
        .map_err(|e| ApiError(ServerError::Script(format!("invalid budget_id: {e}"))))
}
