//! Capability-authenticated callback routes used by the built-in memory MCP and
//! TencentDB jobs. Deliberately separate from browser session authentication.
#![allow(clippy::disallowed_types)] // HTTP boundary maps domain and authentication failures to ApiError.

use std::sync::Arc;

use aionui_ai_agent::generation::{GenerationContext, GenerationRequest, GenerationService};
use aionui_ai_agent::{AgentError, IWorkerTaskManager};
use axum::{
    Json, Router,
    extract::{Path, State},
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
    chat_rules: aionui_conversation::ConversationService,
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
    chat_rules: aionui_conversation::ConversationService,
) -> Router {
    Router::new()
        .route("/api/memory/generation/context", post(context))
        .route("/api/memory/project/context", post(project_context))
        .route("/api/memory/rules/{scope}/{operation}", post(mutate_rules))
        .route("/api/memory/rules/read", post(read_rules))
        .route("/api/memory/question", post(ask_user))
        .route("/api/runtime/tencent/v1/chat/completions", post(complete))
        .with_state(GenerationState {
            service,
            tasks,
            conversations,
            projects,
            chat_rules,
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesRequest {
    /// Must match the private capability; user identity comes only from that capability.
    conversation_id: String,
    /// Edits and deletions require the current Tencent source revision.
    mutation: aionui_api_types::ChatRuleMutation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRulesRequest {
    conversation_id: String,
}

fn rules_capability(
    state: &GenerationState,
    headers: &HeaderMap,
    conversation_id: &str,
) -> Result<aionui_ai_agent::generation::MemoryCapability, aionui_common::ApiError> {
    bearer(headers)
        .ok()
        .and_then(|token| state.tasks.memory_runtime(conversation_id, token))
        .map(|(capability, _)| capability)
        .ok_or_else(|| aionui_common::ApiError::Unauthorized("Invalid memory conversation capability".into()))
}

async fn read_rules(
    State(state): State<GenerationState>,
    headers: HeaderMap,
    body: Result<Json<ReadRulesRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<aionui_api_types::ApiResponse<serde_json::Value>>, aionui_common::ApiError> {
    let Json(input) = body.map_err(aionui_common::ApiError::from)?;
    let cap = rules_capability(&state, &headers, &input.conversation_id)?;
    let data = state.chat_rules.read_worker_rules(&input.conversation_id, &cap).await?;
    Ok(Json(aionui_api_types::ApiResponse::ok(data)))
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RuleOperation {
    Write,
    Delete,
}

async fn mutate_rules(
    State(state): State<GenerationState>,
    Path((scope, operation)): Path<(aionui_api_types::ChatRuleScope, RuleOperation)>,
    headers: HeaderMap,
    body: Result<Json<RulesRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<aionui_api_types::ApiResponse<()>>, aionui_common::ApiError> {
    let Json(input) = body.map_err(aionui_common::ApiError::from)?;
    let capability = rules_capability(&state, &headers, &input.conversation_id)?;
    use aionui_api_types::ChatRuleMutation::{Add, Delete, Edit};
    let allowed = match (&operation, &input.mutation) {
        (RuleOperation::Write, Add { scope: requested, .. } | Edit { scope: requested, .. })
        | (RuleOperation::Delete, Delete { scope: requested, .. }) => *requested == scope,
        _ => false,
    };
    if !allowed {
        return Err(aionui_common::ApiError::BadRequest(
            "Rule mutation does not match this tool".into(),
        ));
    }
    state
        .chat_rules
        .mutate_chat_rule(
            &capability.user,
            &input.conversation_id,
            Some(&capability),
            &input.mutation,
        )
        .await?;
    Ok(Json(aionui_api_types::ApiResponse::ok(())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserRequest {
    conversation_id: String,
    questions: Vec<aionui_common::UserQuestion>,
}

async fn ask_user(
    State(state): State<GenerationState>,
    headers: HeaderMap,
    body: Result<Json<AskUserRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<aionui_api_types::ApiResponse<serde_json::Value>>, aionui_common::ApiError> {
    let Json(input) = body.map_err(aionui_common::ApiError::from)?;
    let token =
        bearer(&headers).map_err(|_| aionui_common::ApiError::Unauthorized("Missing worker capability".into()))?;
    let response = state
        .chat_rules
        .ask_user(state.tasks.as_ref(), token, &input.conversation_id, input.questions)
        .await?;
    Ok(Json(aionui_api_types::ApiResponse::ok(response)))
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
        let (capability, _) = state
            .tasks
            .memory_runtime(&input.conversation_id, bearer(&headers)?)
            .ok_or_else(|| AgentError::unauthorized("Invalid memory conversation capability"))?;
        let owner = &capability.user;
        let row = state
            .conversations
            .get(owner, &input.conversation_id)
            .await
            .map_err(|_| AgentError::bad_gateway("Could not resolve memory project"))?
            .ok_or_else(|| AgentError::bad_request("Conversation no longer exists"))?;
        let extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|_| AgentError::bad_request("Invalid conversation workspace"))?;
        if !capability.matches_binding(
            owner,
            row.project_id.as_deref(),
            extra["workspace"].as_str().unwrap_or_default(),
        ) {
            return Err(AgentError::unauthorized("The memory runtime binding has changed"));
        }
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
                .get_project(owner, &id)
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
    mod rules_tools;

    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    // The real manager owns the capability; only the provider subprocess is replaced.
    fn test_workers(service: Arc<GenerationService>) -> (Arc<dyn IWorkerTaskManager>, Arc<std::sync::Mutex<String>>) {
        let token = Arc::new(std::sync::Mutex::new(String::new()));
        let captured = token.clone();
        let factory: aionui_ai_agent::task_manager::AgentFactory = Arc::new(move |options| {
            let result = service.mcp_env(&options).map(|env| {
                *captured.lock().unwrap() = env[1].1.clone();
                super::super::state::tests::noop_test_agent(options)
            });
            Box::pin(async move { result })
        });
        (Arc::new(aionui_ai_agent::WorkerTaskManagerImpl::new(factory)), token)
    }

    async fn start_worker(
        services: &crate::AppServices,
        row: &aionui_db::models::ConversationRow,
        workers: &dyn IWorkerTaskManager,
        captured: &std::sync::Mutex<String>,
        workspace: &std::path::Path,
    ) -> String {
        let opts = services
            .conversation_service
            .build_task_options_for_runtime(row, workspace.to_str())
            .await
            .unwrap();
        workers.get_or_build_task(&row.id, opts).await.unwrap();
        captured.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn host_user_input_is_owner_turn_and_worker_bound_and_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        let db = aionui_db::init_database_memory().await.unwrap();
        let config = crate::AppConfig {
            data_dir: temp.path().join("data"),
            work_dir: temp.path().join("work"),
            ..Default::default()
        };
        let services = crate::AppServices::from_config(db, &config).await.unwrap();
        let row = aionui_db::models::ConversationRow {
            id: "human-question-fixture".into(),
            user_id: "system_default_user".into(),
            name: "Fixture".into(),
            r#type: "aionrs".into(),
            extra: json!({"tool_scope":"session_only"}).to_string(),
            model: None,
            status: Some("pending".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
            project_id: None,
            folder_id: None,
            name_source: None,
        };
        services.conversation_repo.create(&row).await.unwrap();
        let (workers, captured) = test_workers(services.generation_service.clone());
        let token = start_worker(&services, &row, workers.as_ref(), &captured, temp.path()).await;
        let mut turn = services
            .conversation_service
            .runtime_state()
            .try_claim_turn(&row.id, "turn-one")
            .unwrap();
        let question:aionui_common::UserQuestion=serde_json::from_value(json!({
            "id":"lang","header":"Language","question":"Choose a language","options":[{"label":"Rust","description":"Compiled"}],"isOther":true,"multiSelect":false,"isSecret":true
        })).unwrap();
        let start_ask = || {
            let (service, tasks, credential, chat, form) = (
                services.conversation_service.clone(),
                workers.clone(),
                token.clone(),
                row.id.clone(),
                question.clone(),
            );
            tokio::spawn(async move { service.ask_user(tasks.as_ref(), &credential, &chat, vec![form]).await })
        };
        let current = || async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(form) = services
                        .conversation_service
                        .list_confirmations(&row.user_id, &row.id, &workers)
                        .await
                        .unwrap()
                        .into_iter()
                        .find(|c| c.call_id.starts_with("host-question:"))
                    {
                        return form;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap()
        };
        let ask = start_ask();
        let form = current().await;
        assert_eq!(
            services
                .conversation_service
                .pending_host_question_count(&row.user_id, &row.id),
            1
        );
        assert_eq!(
            services
                .conversation_service
                .pending_host_question_count("another-user", &row.id),
            0
        );
        // Ordinary authenticated operator routes can display/answer forms; cloaking
        // the same user with a helper credential (even for this chat) cannot.
        let human = aionui_conversation::conversation_routes(aionui_conversation::ConversationRouterState {
            service: services.conversation_service.clone(),
            task_manager: workers.clone(),
            active_leases: services.active_lease_registry.clone(),
        })
        .layer(axum::Extension(aionui_auth::CurrentUser::local_default()));
        for (method, uri, body) in [
            (
                "GET",
                format!("/api/conversations/{}/confirmations", row.id),
                String::new(),
            ),
            (
                "POST",
                format!("/api/conversations/{}/confirmations/{}/confirm", row.id, form.id),
                json!({"msg_id":"test","data":{"value":"reject"}}).to_string(),
            ),
        ] {
            let resp = human
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .header(aionui_auth::RUNTIME_TOKEN_HEADER, "other-chat-helper")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }

        assert_eq!(form.questions, vec![question.clone()]);
        assert!(
            services
                .conversation_service
                .ask_user(workers.as_ref(), "wrong", &row.id, vec![question.clone()])
                .await
                .is_err()
        );
        let respond = |owner: &str, data: Value| aionui_api_types::ConfirmRequest {
            msg_id: owner.into(),
            data,
            always_allow: false,
        };
        let answers = json!({"answers":[{"question":"lang","labels":["custom"]}]});
        assert!(
            services
                .conversation_service
                .confirm(
                    "another-user",
                    &row.id,
                    &form.id,
                    respond("foreign", answers.clone()),
                    &workers
                )
                .await
                .is_err()
        );
        assert!(
            services
                .conversation_service
                .confirm(
                    &row.user_id,
                    &row.id,
                    &form.id,
                    respond("invalid", json!({"answers":[]})),
                    &workers
                )
                .await
                .is_err()
        );
        services
            .conversation_service
            .confirm(
                &row.user_id,
                &row.id,
                &form.id,
                respond("ok", answers.clone()),
                &workers,
            )
            .await
            .unwrap();
        assert_eq!(ask.await.unwrap().unwrap(), answers);
        assert!(
            services
                .conversation_service
                .confirm(&row.user_id, &row.id, &form.id, respond("duplicate", answers), &workers)
                .await
                .is_err()
        );
        let revoked = start_ask();
        let withdrawn = current().await;
        turn.release();
        assert!(
            services
                .conversation_service
                .confirm(
                    &row.user_id,
                    &row.id,
                    &withdrawn.id,
                    respond("stale", json!({"answers":[{"question":"lang","labels":["Rust"]}]})),
                    &workers
                )
                .await
                .is_err(),
            "A stale form must not acknowledge an answer rejected by the origin worker"
        );

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(4), revoked)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            services
                .conversation_service
                .list_confirmations(&row.user_id, &row.id, &workers)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn chat_rules_routes_require_the_right_identity_and_preserve_isolation() {
        let temp = tempfile::tempdir().unwrap();
        let db = aionui_db::init_database_memory().await.unwrap();
        let config = crate::AppConfig {
            data_dir: temp.path().join("data"),
            work_dir: temp.path().join("work"),
            ..Default::default()
        };
        let services = crate::AppServices::from_config(db, &config).await.unwrap();
        let row = aionui_db::models::ConversationRow {
            id: "isolated-rules-test".into(),
            user_id: "system_default_user".into(),
            name: "Fixture".into(),
            r#type: "aionrs".into(),
            extra: json!({"tool_scope":"session_only"}).to_string(),
            model: None,
            status: Some("pending".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
            project_id: None,
            folder_id: None,
            name_source: None,
        };
        services.conversation_repo.create(&row).await.unwrap();
        let (workers, captured) = test_workers(services.generation_service.clone());
        let start = |row| start_worker(&services, row, workers.as_ref(), &captured, temp.path());
        let callback = routes(
            services.generation_service.clone(),
            workers.clone(),
            services.conversation_repo.clone(),
            services.project_service.clone(),
            services.conversation_service.clone(),
        );
        let editor_for = |user| {
            aionui_conversation::conversation_ops_routes(aionui_conversation::ConversationRouterState {
                service: services.conversation_service.clone(),
                task_manager: services.worker_task_manager.clone(),
                active_leases: services.active_lease_registry.clone(),
            })
            .layer(axum::Extension(user))
        };
        let editor = editor_for(aionui_auth::CurrentUser::local_default());
        let get_rules = format!("/api/conversations/{}/rules", row.id);
        let result = editor
            .clone()
            .oneshot(Request::get(&get_rules).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        let data: Value = serde_json::from_slice(&to_bytes(result.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(
            data["data"],
            json!({"enabled":false,"writable":false,"rules":[],"projectAvailable":false,"overBudget":false})
        );
        let mutation =
            json!({"action":"delete","scope":"global","path":"knowledge/global/r.md","revision":"synthetic"});
        let result = editor
            .oneshot(
                Request::post(&get_rules)
                    .header(aionui_auth::RUNTIME_TOKEN_HEADER, "fake")
                    .header("content-type", "application/json")
                    .body(Body::from(mutation.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::FORBIDDEN);
        async fn call_at(router: &Router, path: &str, token: &str, input: Value) -> (StatusCode, Value) {
            let response = router
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(input.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            (
                response.status(),
                serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap(),
            )
        }
        async fn call(router: &Router, token: &str, input: Value) -> (StatusCode, Value) {
            let path = if input.get("mutation").is_some() {
                "/api/memory/rules/global/delete"
            } else {
                "/api/memory/rules/read"
            };
            call_at(router, path, token, input).await
        }
        let args = json!({"conversation_id":row.id});
        let own = start(&row).await;
        let (status, result) = call(&callback, &own, args.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["data"]["tool"], "tdai_rules_read");
        assert_eq!(result["data"]["complete"], true);
        assert_eq!(result["data"]["enabled"], false);
        assert_eq!(result["data"]["rules"], json!([]));
        for extra in [
            json!({"scope":"project"}),
            json!({"path":"AGENTS.md"}),
            json!({"mutation":mutation}),
        ] {
            let mut spoof = args.clone();
            spoof
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(
                call_at(&callback, "/api/memory/rules/read", &own, spoof)
                    .await
                    .0
                    .is_client_error()
            );
        }
        assert_eq!(
            call(&callback, &own, json!({"conversation_id":"other-chat"})).await.0,
            StatusCode::UNAUTHORIZED
        );
        let delete = json!({"conversation_id":row.id,"mutation":mutation});
        assert_eq!(call(&callback, &own, delete.clone()).await.0, StatusCode::FORBIDDEN);
        for wrong_tool in ["/api/memory/rules/project/delete", "/api/memory/rules/global/write"] {
            assert_eq!(
                call_at(&callback, wrong_tool, &own, delete.clone()).await.0,
                StatusCode::BAD_REQUEST
            );
        }

        let other = services.user_repo.create_user("second-local", "fixture").await.unwrap();
        let mut other_chat = row.clone();
        other_chat.id = "second-owned-chat".into();
        other_chat.user_id = other.id.clone();
        services.conversation_repo.create(&other_chat).await.unwrap();
        let second_token = start_worker(&services, &other_chat, workers.as_ref(), &captured, temp.path()).await;
        let second_editor = editor_for(aionui_auth::CurrentUser {
            id: other.id.clone(),
            username: "second-local".into(),
            user_type: other.user_type,
            status: other.status,
        });
        // Check the domain denial independently of the generic HTTP 403 envelope.
        assert!(matches!(
            services.conversation_service.read_chat_rules(&other.id, &other_chat.id, None).await,
            Err(aionui_conversation::ConversationError::Forbidden { reason }) if reason.contains("memory owner")
        ));
        let public_denial = json!({"success":false,"error":"Forbidden.","code":"FORBIDDEN"});
        let second_path = format!("/api/conversations/{}/rules", other_chat.id);
        for request in [
            Request::get(&second_path).body(Body::empty()).unwrap(),
            Request::post(&second_path)
                .header("content-type", "application/json")
                .body(Body::from(mutation.to_string()))
                .unwrap(),
        ] {
            let response = second_editor.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let reason = to_bytes(response.into_body(), 8192).await.unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&reason).unwrap(), public_denial);
        }
        for request in [
            json!({"conversation_id":other_chat.id}),
            json!({"conversation_id":other_chat.id,"mutation":mutation}),
        ] {
            let (status, reason) = call(&callback, &second_token, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(reason, public_denial);
        }

        workers
            .kill_and_wait(&row.id, Some(aionui_common::AgentKillReason::IdleTimeout))
            .await;
        assert_eq!(call(&callback, &own, delete.clone()).await.0, StatusCode::UNAUTHORIZED);
        let rebuilt = start(&row).await;
        assert_ne!(rebuilt, own);
        assert_eq!(call(&callback, &own, delete.clone()).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&callback, &rebuilt, args.clone()).await.0, StatusCode::OK);

        // Legacy chats with no project ID can still select rules by workspace alias.
        sqlx::query("UPDATE conversations SET extra = ? WHERE id = ?")
            .bind(json!({"tool_scope":"session_only","workspace":temp.path()}).to_string())
            .bind(&row.id)
            .execute(services.database.pool())
            .await
            .unwrap();
        assert_eq!(
            call(&callback, &rebuilt, delete.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );

        let external = services
            .user_repo
            .ensure_external_user(
                aionui_db::models::UserType::Aionpro,
                "fixture-owner",
                Default::default(),
            )
            .await
            .unwrap();
        services
            .user_repo
            .adopt_system_default_data(&external.id)
            .await
            .unwrap();
        assert_eq!(call(&callback, &rebuilt, delete).await.0, StatusCode::FORBIDDEN);
        let adopted_chat = services
            .conversation_repo
            .get(&external.id, &row.id)
            .await
            .unwrap()
            .unwrap();
        workers.kill(&row.id, None).unwrap();
        let adopted_token = start_worker(&services, &adopted_chat, workers.as_ref(), &captured, temp.path()).await;
        assert_eq!(call(&callback, &adopted_token, args).await.0, StatusCode::OK);
        let adopted_editor = editor_for(aionui_auth::CurrentUser {
            id: external.id,
            username: "adopter".into(),
            user_type: external.user_type,
            status: external.status,
        });
        assert_eq!(
            adopted_editor
                .oneshot(Request::get(&get_rules).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        workers.clear().await;
        assert_eq!(
            call(&callback, &adopted_token, json!({"conversation_id":row.id}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert!(
            !config.work_dir.join("conversations").exists(),
            "reading rules must not provision a workspace"
        );
    }

    #[tokio::test]
    async fn repository_rules_route_reads_and_edits_only_user_bound_project_files() {
        let temp = tempfile::tempdir().unwrap();
        let product = temp.path().join("product");
        let child = product.join("repos/client");
        std::fs::create_dir_all(child.join(".git")).unwrap();
        let source = child.join("AGENTS.md");
        std::fs::write(&source, "# Conventions\n\nKeep history.\n").unwrap();
        let services = crate::AppServices::from_config(
            aionui_db::init_database_memory().await.unwrap(),
            &crate::AppConfig {
                data_dir: temp.path().join("state"),
                work_dir: temp.path().join("work"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let project = services
            .project_service
            .create_standard(
                "system_default_user",
                aionui_project::canonical::to_file_uri(&product).unwrap(),
            )
            .await
            .unwrap();
        let row = aionui_db::models::ConversationRow {
            id: "file-rules-fixture".into(),
            user_id: "system_default_user".into(),
            name: "Project".into(),
            r#type: "aionrs".into(),
            extra: json!({"workspace":product}).to_string(),
            model: None,
            status: Some("pending".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
            project_id: Some(project.project.project_id.clone()),
            folder_id: None,
            name_source: None,
        };
        services.conversation_repo.create(&row).await.unwrap();
        let router = aionui_conversation::conversation_ops_routes(aionui_conversation::ConversationRouterState {
            service: services.conversation_service.clone(),
            task_manager: services.worker_task_manager.clone(),
            active_leases: services.active_lease_registry.clone(),
        })
        .layer(axum::Extension(aionui_auth::CurrentUser::local_default()));
        let path = format!("/api/conversations/{}/project-rule-files", row.id);
        let get = router
            .clone()
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        let json: Value = serde_json::from_slice(&to_bytes(get.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(json["data"]["projectId"], project.project.project_id);
        let file = &json["data"]["files"][0];
        assert_eq!(file["path"], "./repos/client/AGENTS.md");
        let data = json!({"action":"edit","fileId":file["id"],"revision":file["revision"],"ruleId":file["rules"][0]["id"],"title":"Conventions","content":"Keep history and add tests."});
        let send = || {
            Request::post(&path)
                .header("content-type", "application/json")
                .body(Body::from(data.to_string()))
                .unwrap()
        };
        assert_eq!(router.clone().oneshot(send()).await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "# Conventions\n\nKeep history and add tests.\n"
        );
        assert_eq!(
            router.clone().oneshot(send()).await.unwrap().status(),
            StatusCode::CONFLICT
        );
        let mut helper = send();
        helper
            .headers_mut()
            .insert(aionui_auth::RUNTIME_TOKEN_HEADER, "fixture".parse().unwrap());
        assert_eq!(router.oneshot(helper).await.unwrap().status(), StatusCode::FORBIDDEN);
    }

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
        let (workers, captured) = test_workers(service.clone());
        let token = start_worker(&services, &row, workers.as_ref(), &captured, &workspace).await;
        let router = routes(
            service,
            workers.clone(),
            services.conversation_repo.clone(),
            services.project_service.clone(),
            services.conversation_service.clone(),
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
        // A live sidecar must not acquire another project's Rules when its chat is moved.
        sqlx::query("UPDATE conversations SET project_id = NULL WHERE id = ?")
            .bind(&row.id)
            .execute(services.database.pool())
            .await
            .unwrap();
        assert_eq!(read(&router, &token, &row.id).await.0, StatusCode::UNAUTHORIZED);
        let transferred = router.oneshot(Request::post("/api/memory/rules/global/delete")
            .header("authorization", format!("Bearer {token}")).header("content-type", "application/json")
            .body(Body::from(json!({"conversation_id":row.id, "mutation":{"action":"delete","scope":"global","path":"knowledge/global/r.md","revision":"synthetic"}}).to_string())).unwrap()).await.unwrap();
        assert_eq!(transferred.status(), StatusCode::UNAUTHORIZED);
    }
}
