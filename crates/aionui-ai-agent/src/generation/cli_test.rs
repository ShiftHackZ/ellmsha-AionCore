use super::*;

fn request() -> GenerationRequest {
    serde_json::from_value(json!({"model":"selected-exact","messages":[{"role":"user","content":"memory"}],"tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}}]})).unwrap()
}
#[test]
fn generation_cli_isolation_drops_parent_tools_history_skills_and_runtime_tokens() {
    let source = SessionConfig {
        init: SessionInit {
            skills: vec!["old-memory".into()],
            preset_context: Some("Old system".into()),
            session_snapshot: Some(json!({"secret":"old chat"})),
            resume: true,
            ..Default::default()
        },
        extra_args: vec!["--continue".into()],
        spawn_env: vec![
            EnvVar {
                name: "AIONUI_GENERATION_TOKEN".into(),
                value: "private".into(),
            },
            EnvVar {
                name: "ANTHROPIC_API_KEY".into(),
                value: "test-provider-key".into(),
            },
        ],
        ..Default::default()
    };
    let config = isolated_config(&source, Path::new("/tmp/empty-generation"), &request());
    assert!(config.init.mcp_servers.is_empty());
    assert!(config.init.skills.is_empty());
    assert!(config.init.session_snapshot.is_none());
    assert!(!config.init.resume);
    assert!(config.extra_args.is_empty());
    assert_eq!(config.model.as_deref(), Some("selected-exact"));
    assert_eq!(config.sandbox_mode.as_deref(), Some("read-only"));
    assert_eq!(config.approval_policy.as_deref(), Some("never"));
    assert!(config.spawn_env.iter().all(|v| !v.name.starts_with("AIONUI_")));
    assert_eq!(config.spawn_env.len(), 1);
    assert!(config.generation_only);
}
#[test]
fn generation_cli_projects_external_tool_calls_and_rejects_unknown_functions() {
    let mut result = Completion {
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        content: r#"{"content":"","tool_calls":[{"name":"read_file","arguments":"{\"path\":\"scene.md\"}"}]}"#.into(),
        tools: vec![],
        input_tokens: 0,
        output_tokens: 0,
        truncated: false,
    };
    project_tool_response(&mut result, &request()).unwrap();
    assert_eq!(result.tools[0].function.name, "read_file");
    result.content = r#"{"content":"","tool_calls":[{"name":"Bash","arguments":"{}"}]}"#.into();
    assert!(project_tool_response(&mut result, &request()).is_err());
}

#[test]
fn classifier_cli_preserves_selected_auth_source_but_drops_execution_context() {
    let mut source = SessionConfig {
        cli_program: Some("/configured/claude".into()),
        spawn_env: vec![
            EnvVar {
                name: "ANTHROPIC_BASE_URL".into(),
                value: "https://configured.example".into(),
            },
            EnvVar {
                name: "ANTHROPIC_AUTH_TOKEN".into(),
                value: "private-fixture".into(),
            },
            EnvVar {
                name: "AIONUI_RUNTIME_TOKEN".into(),
                value: "never-inherit".into(),
            },
        ],
        ..Default::default()
    };
    source.init.resume = true;
    source.extra_args = vec!["--dangerously-skip-permissions".into()];
    let request:GenerationRequest=serde_json::from_value(json!({"model":"configured-model","messages":[{"role":"user","content":"Classify"}],"tool_choice":"none","max_tokens":128,"response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object","required":["class_id"],"properties":{"class_id":{"type":["string","null"]}},"additionalProperties":false}}}})).unwrap();
    let isolated = isolated_config(&source, Path::new("/tmp/classifier-fixture"), &request);
    assert_eq!(isolated.cli_program, source.cli_program);
    assert_eq!(isolated.model.as_deref(), Some("configured-model"));
    assert_eq!(isolated.spawn_env.len(), 2);
    assert!(
        isolated
            .spawn_env
            .iter()
            .all(|entry| !entry.name.starts_with("AIONUI_"))
    );
    assert!(isolated.init.mcp_servers.is_empty());
    assert!(isolated.init.skills.is_empty());
    assert!(!isolated.init.resume);
    assert!(isolated.extra_args.is_empty());
    assert!(isolated.generation_only);
    assert_eq!(isolated.approval_policy.as_deref(), Some("never"));
    assert_eq!(
        isolated.generation_response_schema.unwrap()["required"],
        json!(["class_id"])
    );
}

/// Opt-in acceptance probe: uses the installed authenticated Codex CLI and
/// does real text generation in an isolated ephemeral session.
#[tokio::test]
#[ignore = "requires explicitly configured authenticated CLI and real inference"]
async fn generation_live_codex_json_response() {
    let cli = std::env::var_os("ELLM_SHA_LIVE_CODEX_CLI").expect("ELLM_SHA_LIVE_CODEX_CLI");
    let model = std::env::var("ELLM_SHA_LIVE_CODEX_MODEL").expect("ELLM_SHA_LIVE_CODEX_MODEL");
    let registry = tempfile::tempdir().unwrap();
    let spawner = Arc::new(aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(registry.path())),
        uuid::Uuid::now_v7(),
        aionui_process::local_machine_id(registry.path()),
    ));
    let source = SessionConfig {
        cli_program: Some(cli.into()),
        ..Default::default()
    };
    let request=serde_json::from_value(json!({"model":model,"messages":[{"role":"user","content":"Return exactly a JSON object with key ok and boolean value true. No tools."}],"response_format":{"type":"json_object"},"max_tokens":8192})).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        complete("codex", source.clone(), spawner.clone(), &request),
    )
    .await
    .expect("generation timed out")
    .expect("generation failed");
    result.validate(&request).unwrap();
    let output: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(output["ok"], true);
    let tool_request:GenerationRequest=serde_json::from_value(json!({"model":request.model,"messages":[{"role":"user","content":"Call the provided read_file function with path scene.md. Return the external function call, do not execute any built-in tool."}],"tools":[{"type":"function","function":{"name":"read_file","description":"Read a TencentDB memory scene","parameters":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}}}],"tool_choice":"required","max_tokens":8192})).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        complete("codex", source, spawner, &tool_request),
    )
    .await
    .expect("function generation timed out")
    .expect("function generation failed");
    result.validate(&tool_request).unwrap();
    assert_eq!(result.tools.len(), 1);
    assert_eq!(result.tools[0].function.name, "read_file");
    assert_eq!(
        serde_json::from_str::<Value>(&result.tools[0].function.arguments).unwrap()["path"],
        "scene.md"
    );
}
