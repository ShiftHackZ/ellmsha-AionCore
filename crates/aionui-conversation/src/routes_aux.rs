#![allow(clippy::disallowed_types)]

use crate::state::ConversationRouterState;
use aionui_api_types::{
    ApiResponse, SetConfigOptionRequest, SetConfigOptionResponse, SideQuestionRequest, SideQuestionResponse,
    SlashCommandItem, TaskContextResponse, UpdateTaskContextRequest, WorkspaceBrowseQuery, WorkspaceEntry,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};

/// Build the conversation-ops router (no auth layer applied — the caller is
/// responsible for wrapping this with the auth middleware).
pub fn conversation_ops_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/conversations/{id}/side-question", post(side_question))
        .route("/api/conversations/{id}/slash-commands", get(get_slash_commands))
        .route("/api/conversations/{id}/usage", get(get_usage))
        .route(
            "/api/conversations/{id}/config-options/{option_id}",
            put(set_config_option),
        )
        .route("/api/conversations/{id}/workspace", get(browse_workspace))
        .route(
            "/api/conversations/{id}/task-context",
            get(get_task_context).put(update_task_context),
        )
        .with_state(state)
}

// ── Route handlers ─────────────────────────────────────────────────

/// Runtime callers may manipulate only their own brief. Ordinary authenticated
/// UI clients remain owner-scoped by the service repository lookup.
fn check_task_context_binding(headers: &HeaderMap, id: &str) -> Result<(), ApiError> {
    if headers.contains_key(aionui_auth::RUNTIME_TOKEN_HEADER)
        && headers
            .get(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            != Some(id)
    {
        return Err(ApiError::Forbidden(
            "Task context is limited to the calling conversation".into(),
        ));
    }
    Ok(())
}

async fn get_task_context(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<TaskContextResponse>>, ApiError> {
    check_task_context_binding(&headers, &id)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_task_context(&user.id, &id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn update_task_context(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Json<UpdateTaskContextRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TaskContextResponse>>, ApiError> {
    check_task_context_binding(&headers, &id)?;
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .update_task_context(&user.id, &id, request)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn set_config_option(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, option_id)): Path<(String, String)>,
    body: Result<Json<SetConfigOptionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SetConfigOptionResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .set_config_option(&user.id, &id, &option_id, req)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_usage(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Option<serde_json::Value>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_usage(&user.id, &id).await.map_err(ApiError::from)?,
    )))
}

async fn side_question(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(req): Json<SideQuestionRequest>,
) -> Result<Json<ApiResponse<SideQuestionResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .handle_side_question(&user.id, &id, req)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn get_slash_commands(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<SlashCommandItem>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_slash_commands(&user.id, &id)
            .await
            .map_err(ApiError::from)?,
    )))
}

async fn browse_workspace(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<WorkspaceBrowseQuery>,
) -> Result<Json<ApiResponse<Vec<WorkspaceEntry>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .browse_workspace(&user.id, &id, query)
            .await
            .map_err(ApiError::from)?,
    )))
}

#[cfg(test)]
mod task_context_route_tests {
    use super::*;

    #[test]
    fn task_context_runtime_binding_rejects_other_or_missing_conversation() {
        let mut headers = HeaderMap::new();
        assert!(check_task_context_binding(&headers, "fixture").is_ok());
        headers.insert(aionui_auth::RUNTIME_TOKEN_HEADER, "synthetic-token".parse().unwrap());
        assert!(matches!(
            check_task_context_binding(&headers, "fixture"),
            Err(ApiError::Forbidden(_))
        ));
        headers.insert(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "other".parse().unwrap());
        assert!(matches!(
            check_task_context_binding(&headers, "fixture"),
            Err(ApiError::Forbidden(_))
        ));
        headers.insert(aionui_auth::RUNTIME_CONVERSATION_ID_HEADER, "fixture".parse().unwrap());
        assert!(check_task_context_binding(&headers, "fixture").is_ok());
    }
}
