//! Thin Console-attested ingress for workflow-native approvals.
use crate::{ApiError, AppState};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use centaur_workflows::approvals::{Identity, Request};
use serde_json::Value;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/tool-approvals/context", post(context))
        .route("/api/tool-approvals/request", post(request))
        .route("/api/tool-approvals/{id}/read", post(read))
        .route("/api/tool-approvals/{id}/cancel", post(cancel))
}
async fn context(
    State(state): State<AppState>,
    Json(identity): Json<Identity>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.workflows()?.approval_context(identity).await?))
}
async fn request(
    State(state): State<AppState>,
    Json(request): Json<Request>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.workflows()?.request_approval(request).await?))
}
async fn read(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(identity): Json<Identity>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(state.workflows()?.read_approval(id, identity).await?))
}
async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(identity): Json<Identity>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state.workflows()?.cancel_approval(id, identity).await?,
    ))
}
