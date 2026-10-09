//! Capability-authenticated callback routes used by the built-in memory MCP and
//! TencentDB jobs. Deliberately separate from browser session authentication.
use std::sync::Arc;

use aionui_ai_agent::generation::{GenerationContext, GenerationRequest, GenerationService};
use aionui_ai_agent::{AgentError, IWorkerTaskManager};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct GenerationState {
    service: Arc<GenerationService>,
    tasks: Arc<dyn IWorkerTaskManager>,
    conversations: Arc<dyn aionui_db::IConversationRepository>,
    projects: aionui_project::ProjectService,
}

/// Context request bound to a private, conversation-specific MCP capability.
#[derive(Deserialize)]
struct ContextRequest {
    /// Application conversation whose currently selected model is captured.
    conversation_id: String,
}

/// Successful context response for the private memory MCP integration.
#[derive(Serialize)]
struct ContextResponse {
    /// True when a frozen primary-agent route was issued.
    success: bool,
    /// Job configuration to forward privately to TencentDB.
    data: ContextData,
}
#[derive(Serialize)]
struct ContextData {
    /// Immutable model selection, callback URL, and expiring capability.
    llm_context: GenerationContext,
}
/// OpenAI-compatible error envelope; contains no provider credentials.
#[derive(Serialize)]
struct CallbackError {
    /// Actionable failure details, with no alternate-model fallback.
    error: CallbackErrorDetails,
}
#[derive(Serialize)]
struct CallbackErrorDetails {
    /// Sanitized reason the callback was rejected or failed.
    message: String,
    /// Stable error category for callback clients.
    r#type: &'static str,
}

pub(super) fn routes(
    service: Arc<GenerationService>,
    tasks: Arc<dyn IWorkerTaskManager>,
    conversations: Arc<dyn aionui_db::IConversationRepository>,
    projects: aionui_project::ProjectService,
) -> Router {
    Router::new()
        .route("/api/memory/generation/context", post(context))
        .route("/api/memory/project/context", post(project_context))
        .route("/api/runtime/tencent/v1/chat/completions", post(complete))
        .with_state(GenerationState {
            service,
            tasks,
            conversations,
            projects,
        })
}

fn bearer(headers: &HeaderMap) -> Result<&str, AgentError> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AgentError::unauthorized("Memory generation requires a bearer capability"))
}
async fn context(
    State(state): State<GenerationState>,
    headers: HeaderMap,
    Json(input): Json<ContextRequest>,
) -> Response {
    let result = async {
        let context = state
            .service
            .issue_context(bearer(&headers)?, &input.conversation_id, state.tasks.as_ref())
            .await?;
        Ok::<_, AgentError>(Json(ContextResponse {
            success: true,
            data: ContextData { llm_context: context },
        }))
    }
    .await;
    match result {
        Ok(value) => value.into_response(),
        Err(error) => failure(error),
    }
}
async fn complete(
    State(state): State<GenerationState>,
    headers: HeaderMap,
    Json(input): Json<GenerationRequest>,
) -> Response {
    let result = async { state.service.complete(bearer(&headers)?, input).await }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(error),
    }
}
fn failure(error: AgentError) -> Response {
    let status = match error {
        AgentError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        AgentError::BadRequest(_) => StatusCode::BAD_REQUEST,
        AgentError::Conflict(_) => StatusCode::CONFLICT,
        AgentError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        AgentError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(CallbackError {
            error: CallbackErrorDetails {
                message: error.to_string(),
                r#type: "memory_generation_error",
            },
        }),
    )
        .into_response()
}

/// Read-only capability endpoint. Resolve roots afresh so attach/detach takes effect
/// without restarting the MCP process. No model call or generation job is needed.
async fn project_context(
    State(state): State<GenerationState>,
    headers: HeaderMap,
    Json(input): Json<aionui_api_types::MemoryProjectRequest>,
) -> Response {
    let result = async {
        state
            .service
            .validate_context_token(bearer(&headers)?, &input.conversation_id)?;
        let owner = state
            .conversations
            .owner_user_id(&input.conversation_id)
            .await
            .map_err(|_| AgentError::bad_gateway("Could not resolve memory project"))?
            .ok_or_else(|| AgentError::bad_request("Conversation no longer exists"))?;
        let row = state
            .conversations
            .get(&owner, &input.conversation_id)
            .await
            .map_err(|_| AgentError::bad_gateway("Could not resolve memory project"))?
            .ok_or_else(|| AgentError::bad_request("Conversation no longer exists"))?;
        let extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|_| AgentError::bad_request("Invalid conversation workspace"))?;
        let workspace = extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AgentError::bad_request("Conversation has no workspace"))?
            .to_owned();
        let mut output = aionui_api_types::MemoryProjectContext {
            project_id: row.project_id.clone(),
            name: row.name,
            roots: vec![workspace.clone()],
            workspace,
        };
        if let Some(id) = row.project_id {
            let detail = state
                .projects
                .get_project(&owner, &id)
                .await
                .map_err(|_| AgentError::bad_request("Project is unavailable"))?;
            output.name = detail.name;
            output.roots.clear();
            for entry in detail.explorer.entries {
                let path = aionui_project::canonical::uri_to_path(&entry.folder.resource_uri)
                    .map_err(|_| AgentError::bad_request("Project root is not a local directory"))?
                    .to_string_lossy()
                    .into_owned();
                if entry.role == "workspace" {
                    output.workspace = path.clone();
                }
                output.roots.push(path);
            }
        }
        Ok::<_, AgentError>(Json(aionui_api_types::ApiResponse::ok(output)))
    }
    .await;
    match result {
        Ok(value) => value.into_response(),
        Err(error) => failure(error),
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    #[tokio::test]
    async fn project_context_requires_chat_capability_and_reads_live_attached_roots() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("product");
        let attached = temp.path().join("mobile");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&attached).unwrap();
        let db = aionui_db::init_database_memory().await.unwrap();
        let config = crate::AppConfig {
            data_dir: temp.path().join("data"),
            work_dir: temp.path().join("work"),
            ..Default::default()
        };
        let services = crate::AppServices::from_config(db, &config).await.unwrap();
        let project = services
            .project_service
            .create_standard(
                "system_default_user",
                aionui_project::canonical::to_file_uri(&workspace).unwrap(),
            )
            .await
            .unwrap();
        let row = aionui_db::models::ConversationRow {
            id: "chat-project-test".into(),
            user_id: "system_default_user".into(),
            name: "Project".into(),
            r#type: "acp".into(),
            extra: json!({"workspace": workspace}).to_string(),
            model: None,
            status: Some("pending".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
            project_id: Some(project.project.project_id.clone()),
            folder_id: Some(project.folder.folder_id),
            name_source: None,
        };
        services.conversation_repo.create(&row).await.unwrap();
        let service = services.generation_service.clone();
        let env = service.mcp_env(&row.id);
        let token = env
            .iter()
            .find(|(key, _)| key == "AIONUI_GENERATION_TOKEN")
            .unwrap()
            .1
            .clone();
        let router = routes(
            service,
            services.worker_task_manager.clone(),
            services.conversation_repo.clone(),
            services.project_service.clone(),
        );
        async fn read(router: &Router, token: &str, chat: &str) -> (StatusCode, Value) {
            let request = Request::post("/api/memory/project/context")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(json!({"conversation_id": chat}).to_string()))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            let status = response.status();
            (
                status,
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap(),
            )
        }
        assert_eq!(read(&router, "invalid", &row.id).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(read(&router, &token, "another-chat").await.0, StatusCode::UNAUTHORIZED);
        let (status, first) = read(&router, &token, &row.id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["data"]["roots"].as_array().unwrap().len(), 1);
        let entry = services
            .project_service
            .attach_folder(
                "system_default_user",
                aionui_project::AttachInput {
                    project_id: project.project.project_id.clone(),
                    uri: aionui_project::canonical::to_file_uri(&attached).unwrap(),
                    display_name: None,
                },
            )
            .await
            .unwrap();
        let (_, second) = read(&router, &token, &row.id).await;
        assert_eq!(second["data"]["roots"].as_array().unwrap().len(), 2);
        assert_eq!(second["data"]["project_id"], project.project.project_id);
        services
            .project_service
            .remove_attached("system_default_user", &entry.pe_id)
            .await
            .unwrap();
        assert_eq!(
            read(&router, &token, &row.id).await.1["data"]["roots"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
