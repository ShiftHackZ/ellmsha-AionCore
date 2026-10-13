//! Exact-tool permissions, shared by native and direct CLI sessions.
#![allow(clippy::disallowed_types)] // HTTP route boundary, like `routes.rs`.
use std::path::PathBuf;

use aionui_api_types::{ApiResponse, McpApprovalResponse, RevokeMcpApprovalResponse};
use aionui_auth::CurrentUser;
use aionui_common::{ApiError, mcp_approvals::McpApprovalStore};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::{delete, get},
};

/// Mount beneath the application's authenticated and CSRF-protected router.
/// GET returns only this user's grants; DELETE revokes one opaque ID. Neither
/// endpoint can create grants or expose connection configuration/credentials.
pub fn mcp_approval_routes(data_dir: PathBuf) -> Router {
    Router::new()
        .route("/api/settings/mcp-approvals", get(list))
        .route("/api/settings/mcp-approvals/{id}", delete(revoke))
        .with_state(data_dir)
}

async fn list(
    State(data_dir): State<PathBuf>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<McpApprovalResponse>>>, ApiError> {
    let grants = McpApprovalStore::new(&data_dir, &user.id)
        .list()
        .map_err(|_| ApiError::Internal("MCP permissions could not be read".into()))?;
    Ok(Json(ApiResponse::ok(
        grants
            .into_iter()
            .map(|grant| McpApprovalResponse {
                id: grant.id,
                server: grant.server,
                tool: grant.tool,
            })
            .collect(),
    )))
}

async fn revoke(
    State(data_dir): State<PathBuf>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<RevokeMcpApprovalResponse>>, ApiError> {
    if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest("Invalid MCP permission ID".into()));
    }
    let revoked = McpApprovalStore::new(&data_dir, &user.id)
        .revoke(&id)
        .map_err(|_| ApiError::Internal("MCP permission could not be revoked".into()))?;
    Ok(Json(ApiResponse::ok(RevokeMcpApprovalResponse { revoked })))
}
