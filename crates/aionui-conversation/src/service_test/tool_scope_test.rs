use super::*;
use aionui_api_types::ConversationToolScope;
use aionui_extension::{ExtensionRegistry, ExtensionSource, ExtensionStateStore, ScanPath};

fn fixture_agent(conversation_id: &str, workspace: &Path) -> AgentInstance {
    let mut agent = MockAgent::new(conversation_id);
    agent.workspace_override = Some(workspace.to_string_lossy().into_owned());
    AgentInstance::Mock(Arc::new(agent))
}

fn isolated_request(workspace: &Path) -> CreateConversationRequest {
    serde_json::from_value(json!({
        "type":"aionrs", "tool_scope":"session_only",
        "model":{"provider_id":"synthetic", "model":"fake-local-model"},
        "extra":{
            "workspace":workspace, "selected_mcp_server_ids":[],
            "selected_session_mcp_servers":[{"id":"qa-ledger", "name":"qa-ledger", "transport":{"type":"http", "url":"http://127.0.0.1:9/mcp"}}]
        }
    })).unwrap()
}

#[tokio::test]
async fn session_only_scope_preserves_selected_mcp_and_excludes_ambient_on_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let plugin = tmp.path().join("plugins/account-fixture");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(plugin.join("aion-extension.json"), json!({
        "name":"account-fixture", "version":"1.0.0", "apiVersion":"1.0.0",
        "contributes":{"mcpServers":[{"id":"account-server", "name":"account-server", "transport":{"type":"http", "url":"http://127.0.0.1:9/mcp"}}]}
    }).to_string()).unwrap();
    let (svc, broadcaster, _repo, _) = make_service_with_resolver(Arc::new(FixedSkillResolver {
        names: vec!["ambient-skill".into()],
    }));
    let registry = ExtensionRegistry::new(
        ExtensionStateStore::new(tmp.path().join("states.json")),
        broadcaster,
        "1.0.0".into(),
    );
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: tmp.path().join("plugins"),
            source: ExtensionSource::Local,
        }])
        .await
        .unwrap();
    assert_eq!(
        registry.runtime_snapshot("user_1", None).await.mcp_servers.len(),
        1,
        "fixture plugin must really be enabled"
    );
    svc.with_plugin_registry(registry);

    let isolated = svc.create("user_1", isolated_request(tmp.path())).await.unwrap();
    assert_eq!(isolated.extra["tool_scope"], "session_only");
    assert_eq!(isolated.extra["skills"], json!([]));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        fixture_agent(&isolated.id, tmp.path()),
        fixture_agent(&isolated.id, tmp.path()),
    ]));
    let tasks: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.warmup("user_1", &isolated.id, &tasks).await.unwrap();
    svc.warmup("user_1", &isolated.id, &tasks).await.unwrap();
    for options in task_mgr.captured_options() {
        assert!(options.context.is_session_only());
        assert!(options.memory_recall.isolated_test);
        assert!(options.context.skills.is_empty());
        assert_eq!(options.runtime_capabilities.plugin_generation, None);
        assert!(
            !options
                .context
                .runtime_env
                .iter()
                .any(|(key, _)| key == AIONUI_RUNTIME_TOKEN_ENV)
        );
        let AgentSessionKind::Aionrs(context) = options.context.kind else {
            panic!("native context expected")
        };
        assert_eq!(context.config.mcp_server_ids, Some(vec![]));
        assert_eq!(context.config.session_mcp_servers.len(), 1);
        assert_eq!(context.config.session_mcp_servers[0].id, "qa-ledger");
    }

    let mut ordinary = isolated_request(tmp.path());
    ordinary.tool_scope = ConversationToolScope::Default;
    let ordinary = svc.create("user_1", ordinary).await.unwrap();
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![fixture_agent(
        &ordinary.id,
        tmp.path(),
    )]));
    let tasks: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.warmup("user_1", &ordinary.id, &tasks).await.unwrap();
    let options = task_mgr.captured_options().pop().unwrap();
    assert!(!options.context.is_session_only());
    assert!(!options.memory_recall.isolated_test);
    assert!(options.context.skills.contains(&"ambient-skill".into()));
    let AgentSessionKind::Aionrs(context) = options.context.kind else {
        panic!("native context expected")
    };
    assert_eq!(
        context.config.session_mcp_servers.len(),
        2,
        "ordinary plugin attachment must be preserved"
    );
}

#[tokio::test]
async fn session_only_scope_rejects_unsupported_agents_and_immutable_patch() {
    let tmp = tempfile::tempdir().unwrap();
    let (svc, _, _, tasks) = make_service();
    let mut acp = isolated_request(tmp.path());
    acp.r#type = Some(AgentType::Acp);
    assert!(
        svc.create("user_1", acp)
            .await
            .unwrap_err()
            .to_string()
            .contains("requires a native aionrs")
    );
    let mut invalid = isolated_request(tmp.path());
    invalid.extra = json!(false);
    assert!(
        svc.create("user_1", invalid)
            .await
            .unwrap_err()
            .to_string()
            .contains("object extra payload")
    );
    let mut stored = isolated_request(tmp.path());
    stored.extra["selected_mcp_server_ids"] = json!(["account-server"]);
    assert!(
        svc.create("user_1", stored)
            .await
            .unwrap_err()
            .to_string()
            .contains("not stored MCP IDs")
    );
    let isolated = svc.create("user_1", isolated_request(tmp.path())).await.unwrap();
    let patch = serde_json::from_value(json!({"extra":{"tool_scope":"default"}})).unwrap();
    assert!(
        svc.update("user_1", &isolated.id, patch, &tasks)
            .await
            .unwrap_err()
            .to_string()
            .contains("immutable post-creation")
    );
    assert!(matches!(
        svc.get("another-user", &isolated.id).await,
        Err(ConversationError::NotFound { .. })
    ));
}

#[tokio::test]
async fn session_only_scope_is_inherited_by_persisted_child() {
    let tmp = tempfile::tempdir().unwrap();
    let (svc, broadcaster, repo, _) = make_service();
    let parent = svc.create("user_1", isolated_request(tmp.path())).await.unwrap();
    let mut store = crate::stream_persistence::SubagentTranscriptStore::new(
        "user_1".into(),
        parent.id.clone(),
        repo.clone(),
        broadcaster,
    );
    store
        .apply(&aionui_ai_agent::protocol::events::SubagentEventData {
            r#ref: "owned-child".into(),
            label: Some("Synthetic child".into()),
            parent_ref: None,
            status: Some("working".into()),
            event: None,
        })
        .await;
    let child_id = format!("subagent:{}:owned-child", parent.id);
    let child = repo.get("user_1", &child_id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&child.extra).unwrap();
    assert_eq!(extra["tool_scope"], "session_only");
    assert_eq!(extra["session_mcp_servers"], parent.extra["session_mcp_servers"]);
    assert_eq!(extra["skills"], json!([]));
    let options = svc.build_task_options(&child).await.unwrap();
    assert!(options.context.is_session_only());
    assert!(options.memory_recall.isolated_test);
}

#[tokio::test]
async fn session_only_scope_cannot_fall_back_to_ambient_on_malformed_runtime_config() {
    let tmp = tempfile::tempdir().unwrap();
    let (svc, _, repo, tasks) = make_service();
    let isolated = svc.create("user_1", isolated_request(tmp.path())).await.unwrap();
    let patch = serde_json::from_value(json!({"extra":{"max_turns":"invalid"}})).unwrap();
    svc.update("user_1", &isolated.id, patch, &tasks).await.unwrap();
    let row = repo.get("user_1", &isolated.id).await.unwrap().unwrap();
    let error = svc.build_task_options(&row).await.unwrap_err();
    assert!(error.to_string().contains("Invalid session_only runtime configuration"));
}

#[tokio::test]
async fn session_only_project_chat_keeps_required_guidance_but_not_optional_recall() {
    let tmp = tempfile::tempdir().unwrap();
    let work_root = tempfile::tempdir().unwrap();
    let (svc, _broadcaster, repo, _) = make_service_with_workspace_root(work_root.path().to_path_buf());
    svc.with_project_service(make_injected_project_service(work_root.path()).await);
    let chat = svc.create("user_1", isolated_request(tmp.path())).await.unwrap();
    let row = repo.get("user_1", &chat.id).await.unwrap().unwrap();
    assert!(row.project_id.is_some());
    let options = svc.build_task_options(&row).await.unwrap();
    assert!(options.context.is_session_only());
    assert!(options.memory_recall.project_id.is_some());
    assert!(options.memory_recall.guidance_only);
    assert!(
        !options.memory_recall.isolated_test,
        "a registered project must not bypass required rules"
    );
}

#[tokio::test]
async fn unresolved_registered_project_cannot_fall_back_to_workspace_rules() {
    let (svc, _broadcaster, repo, _) = make_service();
    let chat = svc.create("user_1", make_create_req()).await.unwrap();
    let mut row = repo.get("user_1", &chat.id).await.unwrap().unwrap();
    row.project_id = Some("registered-project-cannot-be-resolved".into());
    let error = svc.build_task_options(&row).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("project service is not ready; task not started")
    );
    let error = svc.build_task_options_for_runtime(&row, None).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("project service is not ready; task not started")
    );
}
