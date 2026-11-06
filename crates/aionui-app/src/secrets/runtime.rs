#![allow(clippy::disallowed_types)]
use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{WebSocketMessage, secrets::*};
use aionui_common::ApiError;
use aionui_db::{IConversationRepository, IProjectStore};
use aionui_realtime::WebSocketManager;
use aionui_secrets::{routes::error, service::SecretsError, state::SecretsRouterState};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::Arc;

type QuestionCancellations =
    Arc<std::sync::Mutex<std::collections::HashMap<(String, String, String), std::time::Instant>>>;

#[derive(Clone)]
pub(crate) struct SecretsRuntimeState {
    pub question_cancellations: QuestionCancellations,
    pub secrets: SecretsRouterState,
    pub tokens: Arc<RuntimeTokenService>,
    pub conversations: Arc<dyn IConversationRepository>,
    pub projects: Arc<dyn IProjectStore>,
    pub events: Arc<WebSocketManager>,
    pub conversation_service: aionui_conversation::ConversationService,
}

pub(crate) fn runtime_routes(state: SecretsRuntimeState) -> Router {
    Router::new()
        .route("/api/runtime/secrets/tools/{tool}", post(call))
        .route("/api/runtime/secrets/questions/{id}/cancel", post(cancel_question))
        .layer(axum::extract::DefaultBodyLimit::max(128 * 1024))
        .layer(axum::middleware::from_fn(aionui_secrets::routes::no_store))
        .with_state(state)
}
impl SecretsRuntimeState {
    async fn binding(&self, headers: &HeaderMap) -> Result<(String, String, Option<String>), SecretsError> {
        let get = |key| {
            headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
                .ok_or(SecretsError::ContextRequired)
        };
        let owner = get("x-aionui-user-id")?;
        let chat = get("x-aionui-conversation-id")?;
        let claims = self
            .tokens
            .validate(
                Some(get("x-aionui-runtime-token")?),
                owner,
                chat,
                RuntimeTokenScope::SecretsCall,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            )
            .map_err(|_| SecretsError::ContextRequired)?;
        let row = self
            .conversations
            .get(owner, chat)
            .await
            .map_err(|_| SecretsError::Storage)?
            .ok_or(SecretsError::ContextRequired)?;
        let vault_status = self.secrets.service.status(owner).await?;
        let vault = vault_status.vault_id.ok_or(SecretsError::NotInitialized)?;
        let binding = serde_json::to_string(
            &json!({"project":row.project_id,"vault":vault,"incarnation":vault_status.incarnation}),
        )
        .map_err(|_| SecretsError::ContextRequired)?;
        if claims.secrets_binding.as_deref() != Some(binding.as_str()) {
            return Err(SecretsError::ContextRequired);
        }
        let project = if let Some(id) = row.project_id {
            let project = self
                .projects
                .get_project(owner, &id)
                .await
                .map_err(|_| SecretsError::Storage)?
                .ok_or(SecretsError::ContextRequired)?;
            (project.kind == "standard").then_some(id)
        } else {
            None
        };
        self.secrets.access.authorize_binding(owner, project.as_deref()).await?;
        Ok((owner.into(), chat.into(), project))
    }
}
struct RuntimeHistoryGuard<'a> {
    state: &'a SecretsRuntimeState,
    headers: &'a HeaderMap,
    owner: &'a str,
    chat: &'a str,
    project: Option<&'a str>,
}
#[async_trait::async_trait]
impl aionui_db::HistoryCommitGuard for RuntimeHistoryGuard<'_> {
    async fn authorize(&self, context: &aionui_db::HistoryCommitContext) -> Result<(), aionui_db::DbError> {
        let denied = || aionui_db::DbError::Conflict("CONTEXT_REQUIRED".into());
        let claims = self
            .state
            .tokens
            .validate(
                self.headers.get("x-aionui-runtime-token").and_then(|v| v.to_str().ok()),
                self.owner,
                self.chat,
                RuntimeTokenScope::SecretsCall,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            )
            .map_err(|_| denied())?;
        let raw_project = &context.project_id;
        let kind = &context.project_kind;
        if raw_project.is_some() && kind.is_none() {
            return Err(denied());
        }
        let project = raw_project.as_deref().filter(|_| kind.as_deref() == Some("standard"));
        if project != self.project {
            return Err(denied());
        }
        let status = self
            .state
            .secrets
            .service
            .status(self.owner)
            .await
            .map_err(|e| aionui_db::DbError::Conflict(e.to_string()))?;
        if !status.unlocked {
            return Err(aionui_db::DbError::Conflict("VAULT_LOCKED".into()));
        }
        let binding =
            json!({"project":raw_project,"vault":status.vault_id,"incarnation":status.incarnation}).to_string();
        if claims.secrets_binding.as_deref() != Some(&binding) {
            return Err(denied());
        }
        self.state
            .secrets
            .access
            .authorize_binding(self.owner, project)
            .await
            .map_err(|e| aionui_db::DbError::Conflict(e.to_string()))
    }
}

async fn cancel_question(
    State(state): State<SecretsRuntimeState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let (owner, chat, _) = state.binding(&headers).await.map_err(error)?;
    uuid::Uuid::parse_str(&id).map_err(|_| error(SecretsError::InvalidInput))?;
    let mut cancellations = state.question_cancellations.lock().unwrap_or_else(|e| e.into_inner());
    cancellations.retain(|_, at| at.elapsed().as_secs() < 620);
    if cancellations.len() >= 1024 {
        return Err(error(SecretsError::InvalidInput));
    }
    cancellations.insert((owner, chat, id), std::time::Instant::now());
    Ok(Json(json!({"cancelled":true})).into_response())
}

fn parse<T: serde::de::DeserializeOwned>(body: Value) -> Result<T, ApiError> {
    serde_json::from_value(body).map_err(|_| error(SecretsError::InvalidInput))
}

async fn call(
    State(state): State<SecretsRuntimeState>,
    headers: HeaderMap,
    Path(tool): Path<String>,
    Json(mut body): Json<Value>,
) -> Result<Response, ApiError> {
    let (owner, chat, project) = state.binding(&headers).await.map_err(error)?;
    let scoped = match tool.as_str() {
        "ellmsha_global_secrets_list"
        | "ellmsha_global_secret_read"
        | "ellmsha_global_secret_write"
        | "ellmsha_global_secret_delete" => Some(SecretScope { project_id: None }),
        "ellmsha_project_secrets_list"
        | "ellmsha_project_secret_read"
        | "ellmsha_project_secret_write"
        | "ellmsha_project_secret_delete" => Some(SecretScope {
            project_id: Some(
                project
                    .clone()
                    .ok_or_else(|| error(SecretsError::ProjectScopeRequired))?,
            ),
        }),
        "ellmsha_secret_history_search" | "ellmsha_secret_history_mask" | "ellmsha_ask_user" => None,
        _ => return Err(error(SecretsError::NotFound)),
    };
    if let Some(scope) = scoped {
        let object = body.as_object_mut().ok_or_else(|| error(SecretsError::InvalidInput))?;
        if object.contains_key("scope") || object.contains_key("project_id") || object.contains_key("owner") {
            return Err(error(SecretsError::InvalidInput));
        }
        object.insert("scope".into(), json!(scope));
    }
    if !state.secrets.service.status(&owner).await.map_err(error)?.unlocked {
        return Err(error(SecretsError::Locked));
    }
    let valid = || async {
        let current = state.binding(&headers).await?;
        if current != (owner.clone(), chat.clone(), project.clone()) {
            return Err(SecretsError::ContextRequired);
        }
        if !state.secrets.service.status(&owner).await?.unlocked {
            return Err(SecretsError::Locked);
        }
        Ok(())
    };
    let result = match tool.as_str() {
        "ellmsha_ask_user" => {
            let request_id = headers
                .get("x-aionui-secrets-request-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            if let Some(id) = &request_id {
                uuid::Uuid::parse_str(id).map_err(|_| error(SecretsError::InvalidInput))?;
            }
            let mut raw_questions = body
                .get("questions")
                .cloned()
                .ok_or_else(|| error(SecretsError::InvalidInput))?;
            if let Some(questions) = raw_questions.as_array_mut() {
                for question in questions {
                    if let Some(fields) = question.as_object_mut() {
                        fields.entry("isSecret").or_insert(Value::Bool(false));
                    }
                }
            }
            let questions: Vec<aionui_common::UserQuestion> = parse(raw_questions)?;
            let initial = (owner.clone(), chat.clone(), project.clone());
            state
                .conversation_service
                .ask_user_authorized(&owner, &chat, questions, || async {
                    if let Some(id) = &request_id {
                        let cancelled = state
                            .question_cancellations
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .contains_key(&(owner.clone(), chat.clone(), id.clone()));
                        if cancelled {
                            return Err(aionui_conversation::ConversationError::Forbidden {
                                reason: "Question cancelled".into(),
                            });
                        }
                    }
                    let next = state.binding(&headers).await.map_err(|_| {
                        aionui_conversation::ConversationError::Forbidden {
                            reason: "Secrets capability changed".into(),
                        }
                    })?;
                    if next != initial
                        || !state
                            .secrets
                            .service
                            .status(&owner)
                            .await
                            .map_err(|_| aionui_conversation::ConversationError::Forbidden {
                                reason: "Secrets vault unavailable".into(),
                            })?
                            .unlocked
                    {
                        return Err(aionui_conversation::ConversationError::Forbidden {
                            reason: "Secrets binding changed".into(),
                        });
                    }
                    Ok(())
                })
                .await
                .map_err(|_| error(SecretsError::Forbidden))?
        }
        "ellmsha_global_secrets_list" | "ellmsha_project_secrets_list" => {
            json!(state.secrets.service.list(&owner, parse(body)?).await.map_err(error)?)
        }
        "ellmsha_global_secret_read" | "ellmsha_project_secret_read" => {
            json!(state.secrets.service.read(&owner, parse(body)?).await.map_err(error)?)
        }
        "ellmsha_global_secret_write" | "ellmsha_project_secret_write" => json!(
            state
                .secrets
                .service
                .write_checked(&owner, parse(body)?, valid)
                .await
                .map_err(error)?
        ),
        "ellmsha_global_secret_delete" | "ellmsha_project_secret_delete" => json!(
            state
                .secrets
                .service
                .delete_checked(&owner, parse(body)?, valid)
                .await
                .map_err(error)?
        ),
        "ellmsha_secret_history_search" => json!(
            state
                .secrets
                .history
                .search(&owner, &chat, project.as_deref(), parse(body)?)
                .await
                .map_err(error)?
        ),
        "ellmsha_secret_history_mask" => {
            let receipt = state
                .secrets
                .history
                .mask_checked(
                    &owner,
                    &chat,
                    project.as_deref(),
                    parse(body)?,
                    &RuntimeHistoryGuard {
                        state: &state,
                        headers: &headers,
                        owner: &owner,
                        chat: &chat,
                        project: project.as_deref(),
                    },
                )
                .await
                .map_err(error)?;
            state.events.broadcast_to_user(
                &owner,
                WebSocketMessage::new(
                    "message.historyChanged",
                    json!({"user_id":owner,"conversation_ids":receipt.conversation_ids,"revisions":receipt.revisions}),
                ),
            );
            json!(receipt)
        }
        _ => return Err(error(SecretsError::NotFound)),
    };
    if state.binding(&headers).await.map_err(error)? != (owner.clone(), chat, project) {
        return Err(error(SecretsError::Forbidden));
    }
    if !state.secrets.service.status(&owner).await.map_err(error)?.unlocked {
        return Err(error(SecretsError::Locked));
    }
    let mut response = Json(result).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    Ok(response)
}
