#![allow(clippy::disallowed_types)] // HTTP boundary, matching shell/routes.rs.

use crate::{background_process::BackgroundProcessError, state::ShellRouterState};
use aionui_api_types::{
    ApiResponse, BackgroundProcessSnapshot, StartBackgroundProcessRequest, TerminalInputRequest, TerminalOutput,
    TerminalResizeRequest, TerminalWaitRequest,
};
use aionui_auth::{CurrentUser, RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER};
use aionui_common::ApiError;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::HeaderMap,
    routing::{get, post},
};

impl From<BackgroundProcessError> for ApiError {
    fn from(error: BackgroundProcessError) -> Self {
        match error {
            BackgroundProcessError::Invalid(message) => Self::BadRequest(message.into()),
            BackgroundProcessError::NotFound => Self::NotFound(error.to_string()),
            BackgroundProcessError::ShuttingDown => Self::BadRequest(error.to_string()),
            BackgroundProcessError::Internal => Self::Internal(error.to_string()),
        }
    }
}

fn require_runtime_scope(headers: &HeaderMap, conversation: &str) -> Result<(), ApiError> {
    if headers.contains_key(RUNTIME_TOKEN_HEADER)
        && headers
            .get(RUNTIME_CONVERSATION_ID_HEADER)
            .and_then(|header| header.to_str().ok())
            != Some(conversation)
    {
        return Err(ApiError::Forbidden(
            "Background terminal is outside this runtime conversation".into(),
        ));
    }
    Ok(())
}

pub(crate) fn routes() -> Router<ShellRouterState> {
    Router::new()
        .route(
            "/api/conversations/{conversation}/background-processes",
            get(list).post(start),
        )
        .route("/api/conversations/{conversation}/background-processes/{id}", get(read))
        .route(
            "/api/conversations/{conversation}/background-processes/{id}/stop",
            post(stop),
        )
        .route(
            "/api/conversations/{conversation}/background-processes/{id}/close",
            post(close),
        )
        .route(
            "/api/conversations/{conversation}/background-processes/{id}/input",
            post(input),
        )
        .route(
            "/api/conversations/{conversation}/background-processes/{id}/resize",
            post(resize),
        )
        .route(
            "/api/conversations/{conversation}/background-processes/{id}/output",
            get(output),
        )
        .route(
            "/api/conversations/{conversation}/background-processes/{id}/wait",
            post(wait),
        )
}

/// Start an owned command; global auth/CSRF middleware and workspace checks apply.
async fn start(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation): Path<String>,
    headers: HeaderMap,
    Json(request): Json<StartBackgroundProcessRequest>,
) -> Result<Json<ApiResponse<BackgroundProcessSnapshot>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .start(&user.id, &conversation, request)
            .await?,
    )))
}

async fn list(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(conversation): Path<String>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<Vec<BackgroundProcessSnapshot>>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state.background_process_service.list(&user.id, &conversation).await?,
    )))
}

async fn read(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<BackgroundProcessSnapshot>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .read(&user.id, &conversation, &id)
            .await?,
    )))
}

async fn stop(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<BackgroundProcessSnapshot>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .stop(&user.id, &conversation, &id)
            .await?,
    )))
}

async fn input(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<TerminalInputRequest>,
) -> Result<Json<ApiResponse<BackgroundProcessSnapshot>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    let agent = headers.contains_key(RUNTIME_TOKEN_HEADER);
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .write(&user.id, &conversation, &id, request, agent)
            .await?,
    )))
}
async fn resize(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<TerminalResizeRequest>,
) -> Result<Json<ApiResponse<BackgroundProcessSnapshot>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .resize(&user.id, &conversation, &id, request)
            .await?,
    )))
}
#[derive(serde::Deserialize)]
struct OutputQuery {
    after: Option<u64>,
    screen_revision: Option<u64>,
}
async fn output(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation, id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(query): Query<OutputQuery>,
) -> Result<Json<ApiResponse<TerminalOutput>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .output(&user.id, &conversation, &id, query.after, query.screen_revision)
            .await?,
    )))
}
async fn wait(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(request): Json<TerminalWaitRequest>,
) -> Result<Json<ApiResponse<BackgroundProcessSnapshot>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .wait(&user.id, &conversation, &id, request)
            .await?,
    )))
}

async fn close(
    State(state): State<ShellRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((conversation, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ApiResponse<BackgroundProcessSnapshot>>, ApiError> {
    require_runtime_scope(&headers, &conversation)?;
    Ok(Json(ApiResponse::ok(
        state
            .background_process_service
            .close(&user.id, &conversation, &id)
            .await?,
    )))
}
