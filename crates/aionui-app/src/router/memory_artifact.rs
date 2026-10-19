#![allow(clippy::disallowed_types)]

//! Composition boundary for authenticated, conversation-scoped memory previews.
use std::sync::Arc;

use aionui_api_types::{ApiResponse, MemoryArtifactQuery, MemoryArtifactResponse};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    routing::get,
};

#[derive(Clone)]
pub(super) struct ArtifactState {
    pub conversations: Arc<dyn aionui_db::IConversationRepository>,
    pub projects: aionui_project::ProjectService,
}

pub(super) fn routes(state: ArtifactState) -> Router {
    Router::new()
        .route("/api/conversations/{id}/memory-artifact", get(read))
        .with_state(state)
}

// Resolve on every request: detaching a root immediately removes its authority.
async fn authorized_roots(state: &ArtifactState, user: &str, id: &str) -> Result<Vec<String>, ApiError> {
    let row = state
        .conversations
        .get(user, id)
        .await
        .map_err(|_| ApiError::Internal("Could not read conversation".into()))?
        .ok_or_else(|| ApiError::NotFound("Conversation not found".into()))?;
    if let Some(project_id) = row.project_id {
        let project = state
            .projects
            .get_project(user, &project_id)
            .await
            .map_err(|_| ApiError::NotFound("Project is unavailable".into()))?;
        return project
            .explorer
            .entries
            .into_iter()
            .map(|entry| {
                aionui_project::canonical::uri_to_path(&entry.folder.resource_uri)
                    .map(|path| path.to_string_lossy().into_owned())
                    .map_err(|_| ApiError::BadRequest("Project root is not local".into()))
            })
            .collect();
    }
    let extra: serde_json::Value = serde_json::from_str(&row.extra)
        .map_err(|_| ApiError::BadRequest("Conversation workspace is invalid".into()))?;
    let workspace = extra
        .get("workspace")
        .and_then(|value| value.as_str())
        .filter(|path| std::path::Path::new(path).is_absolute())
        .ok_or_else(|| ApiError::BadRequest("Conversation has no local workspace".into()))?;
    Ok(vec![workspace.to_owned()])
}

async fn read(
    State(state): State<ArtifactState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<MemoryArtifactQuery>,
) -> Result<Json<ApiResponse<MemoryArtifactResponse>>, ApiError> {
    let roots = authorized_roots(&state, &user.id, &id).await?;
    let artifact = aionui_ai_agent::read_project_memory_artifact(&roots, &query.path)
        .await
        .map_err(|error| match error {
            aionui_ai_agent::AgentError::BadRequest(message) => ApiError::BadRequest(message),
            aionui_ai_agent::AgentError::Forbidden(message) => ApiError::Forbidden(message),
            aionui_ai_agent::AgentError::NotFound(message) => ApiError::NotFound(message),
            _ => ApiError::BadGateway("Memory artifact is unavailable".into()),
        })?;
    Ok(Json(ApiResponse::ok(artifact)))
}

#[cfg(test)]
#[path = "memory_artifact_test.rs"]
mod tests;
