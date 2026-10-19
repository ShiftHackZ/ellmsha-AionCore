//! Host delegation through the real HTTP gateway, SQLite and turn orchestrator; no model/CLI calls.
mod common;
use aionui_ai_agent::protocol::events::{FinishEventData, TextEventData};
use aionui_ai_agent::types::{AIONUI_RUNTIME_TOKEN_ENV, BuildTaskOptions, SendMessageData};
use aionui_ai_agent::{AgentInstance, AgentStreamEvent, IAgentTask, IMockAgent, WorkerTaskManagerImpl};
use aionui_api_types::{RouterChildCall, RouterChildResponse, RouterChildStatus};
use aionui_app::{AppConfig, AppServices, create_router};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tower::ServiceExt;

struct FixtureAgent {
    id: String,
    workspace: String,
    events: broadcast::Sender<AgentStreamEvent>,
}
#[async_trait::async_trait]
impl IAgentTask for FixtureAgent {
    fn agent_type(&self) -> aionui_common::AgentType {
        aionui_common::AgentType::Aionrs
    }
    fn conversation_id(&self) -> &str {
        &self.id
    }
    fn workspace(&self) -> &str {
        &self.workspace
    }
    fn status(&self) -> Option<aionui_common::ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> i64 {
        aionui_common::now_ms()
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.events.subscribe()
    }
    async fn send_message(&self, data: SendMessageData) -> Result<(), aionui_ai_agent::AgentSendError> {
        if !data.content.contains("ROUTER_TEST_HOLD") {
            let _ = self.events.send(AgentStreamEvent::Text(TextEventData {
                content: "child final".into(),
            }));
            let _ = self.events.send(AgentStreamEvent::Finish(FinishEventData::default()));
        }
        Ok(())
    }
    async fn cancel(&self) -> Result<(), aionui_ai_agent::AgentError> {
        let _ = self.events.send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }
    fn kill(&self, _: Option<aionui_common::AgentKillReason>) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl IMockAgent for FixtureAgent {
    async fn mode(&self) -> Result<aionui_api_types::AgentModeResponse, aionui_ai_agent::AgentError> {
        Ok(aionui_api_types::AgentModeResponse {
            mode: "default".into(),
            initialized: true,
        })
    }
}
fn runtime_request(parent: &str, token: &str, call: RouterChildCall) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/runtime/router-tools/call")
        .header("content-type", "application/json")
        .header("x-aionui-user-id", "system_default_user")
        .header("x-aionui-conversation-id", parent)
        .header("x-aionui-runtime-token", token)
        .body(Body::from(serde_json::to_vec(&call).unwrap()))
        .unwrap()
}
async fn call(app: &axum::Router, parent: &str, token: &str, request: RouterChildCall) -> RouterChildResponse {
    let response = app
        .clone()
        .oneshot(runtime_request(parent, token, request))
        .await
        .unwrap();
    let status = response.status();
    let body = common::body_json(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}
#[tokio::test]
async fn router_child_real_gateway_preserves_policy_permissions_and_cancellation() {
    let tmp = tempfile::tempdir().unwrap();
    let observed = Arc::new(Mutex::new(Vec::<BuildTaskOptions>::new()));
    let factory: aionui_ai_agent::task_manager::AgentFactory = Arc::new({
        let observed = observed.clone();
        move |opts| {
            let observed = observed.clone();
            Box::pin(async move {
                observed.lock().unwrap().push(opts.clone());
                Ok(AgentInstance::Mock(Arc::new(FixtureAgent {
                    id: opts.conversation_id().into(),
                    workspace: opts.context.workspace.path.clone(),
                    events: broadcast::channel(32).0,
                })))
            })
        }
    });
    let config = AppConfig {
        local: true,
        data_dir: tmp.path().join("data"),
        work_dir: tmp.path().into(),
        ..Default::default()
    };
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &config).await.unwrap();
    let manager = Arc::new(
        WorkerTaskManagerImpl::new(factory).with_runtime_token_service(services.runtime_token_service.clone()),
    );
    let services = services.with_worker_task_manager(manager);
    let app = create_router(&services).await.unwrap();
    let response = app
        .clone()
        .oneshot(Request::builder().uri("/api/assistants").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for id in ["root-local", "child-local"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"id":id,"platform":"ollama","name":id,
                "base_url":"http://127.0.0.1:9","api_key":"fixture","models":["qwen3.5:35b"]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "{}",
            common::body_json(response).await
        );
    }
    let target = |id| json!({"kind":"provider","provider_id":id,"model":"qwen3.5:35b"});
    let root = services.conversation_service.create("system_default_user", serde_json::from_value(json!({
        "assistant":{"id":"bare:auto-router"},"extra":{"workspace":tmp.path()},
        "routing":{"initial_prompt":"root task","config":{"mode":"manual","local_only":true,
            "root_target":target("root-local"),"child_target":target("child-local"),
            "hints":[{"target":target("root-local"),"local_inference":true},{"target":target("child-local"),"local_inference":true}]}}
    })).unwrap()).await.unwrap();
    assert_eq!(
        root.extra["routing"]["config"]["schema_version"], 2,
        "new public roots migrate legacy requests"
    );
    services
        .conversation_service
        .ensure_runtime("system_default_user", &root.id, &services.worker_task_manager)
        .await
        .unwrap();
    let mut root_turn = services
        .conversation_runtime_state
        .try_claim_turn(&root.id, "root-turn")
        .unwrap();
    let token = observed.lock().unwrap()[0]
        .context
        .runtime_env
        .iter()
        .find(|(k, _)| k == AIONUI_RUNTIME_TOKEN_ENV)
        .unwrap()
        .1
        .clone();
    let spawn = || RouterChildCall::Spawn {
        task: "finish".into(),
        name: Some("worker".into()),
    };
    let denied = app
        .clone()
        .oneshot(runtime_request(&root.id, "invalid", spawn()))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let task = call(&app, &root.id, &token, spawn()).await;
    let result = call(
        &app,
        &root.id,
        &token,
        RouterChildCall::Wait {
            task_id: task.task_id,
            timeout_ms: 5000,
        },
    )
    .await;
    assert_eq!(result.status, RouterChildStatus::Completed, "{result:?}");
    assert_eq!(result.result.as_deref(), Some("child final"));
    let child_id = result.conversation_id.unwrap();
    let child = services
        .conversation_repo
        .get("system_default_user", &child_id)
        .await
        .unwrap()
        .unwrap();
    let extra: serde_json::Value = serde_json::from_str(&child.extra).unwrap();
    assert_eq!(extra["routing"]["decision"]["target"], target("child-local"));
    assert_eq!(extra["router_child"]["parent_conversation_id"], root.id);
    assert_eq!(extra["router_child"]["parent_turn_id"], "root-turn");
    assert_eq!(extra["workspace"], root.extra["workspace"]);
    assert!(extra.get("team_id").is_none());
    let builds = observed.lock().unwrap().clone();
    let child_build = builds.iter().find(|b| b.conversation_id() == child_id).unwrap();
    assert_eq!(child_build.context.model.provider_id, "child-local");
    let aionui_ai_agent::AgentSessionKind::Aionrs(context) = &child_build.context.kind else {
        panic!("native fixture")
    };
    assert_eq!(context.config.session_mode.as_deref(), Some("default"));
    assert!(
        context
            .config
            .preset_rules
            .as_ref()
            .unwrap()
            .contains("Auto Router delegation")
    );
    assert!(
        context
            .config
            .session_mcp_servers
            .iter()
            .any(|s| s.name == "ellmsha-router" && !s.auto_approve_in_unattended)
    );

    let task = call(
        &app,
        &root.id,
        &token,
        RouterChildCall::Spawn {
            task: "ROUTER_TEST_HOLD".into(),
            name: None,
        },
    )
    .await;
    let running = call(
        &app,
        &root.id,
        &token,
        RouterChildCall::Wait {
            task_id: task.task_id.clone(),
            timeout_ms: 100,
        },
    )
    .await;
    assert_eq!(running.status, RouterChildStatus::Running, "{running:?}");
    let held_id = running.conversation_id.unwrap();
    let held_turn = services
        .conversation_runtime_state
        .active_turn_id_for(&held_id)
        .unwrap();
    services
        .conversation_service
        .cancel(
            "system_default_user",
            &held_id,
            &held_turn,
            &services.worker_task_manager,
        )
        .await
        .unwrap();
    let cancelled = call(
        &app,
        &root.id,
        &token,
        RouterChildCall::Wait {
            task_id: task.task_id,
            timeout_ms: 5000,
        },
    )
    .await;
    assert_eq!(
        cancelled.status,
        RouterChildStatus::Interrupted,
        "direct child cancel must never become Completed"
    );
    root_turn.release();
    services
        .conversation_service
        .delete("system_default_user", &root.id)
        .await
        .unwrap();
    assert!(
        services
            .conversation_repo
            .get("system_default_user", &child_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        services
            .conversation_repo
            .get("system_default_user", &held_id)
            .await
            .unwrap()
            .is_none()
    );
    services.worker_task_manager.clear().await;
}
