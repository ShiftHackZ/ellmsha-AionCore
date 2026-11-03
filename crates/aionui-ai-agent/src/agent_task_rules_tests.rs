//! Concrete production routes share the mandatory guidance barrier. These
//! fixtures prove host preparation/blocking, not a vendor model's obedience.
use super::*;
use crate::capability::tencent_memory::{NativeChildRules, register_session, unregister_session};
use crate::types::MemoryRecallScope;
use aion_agent::spawner::ChildPromptPreflight;
use aionui_session::{BackendError, Capabilities, Command, CommandReceipt, SessionBackend, SessionEnvelope};
use futures_util::{StreamExt, stream::BoxStream};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

struct NoDispatchBackend;

#[async_trait::async_trait]
impl SessionBackend for NoDispatchBackend {
    async fn dispatch(&self, _: Command) -> Result<CommandReceipt, BackendError> {
        panic!("mandatory preflight must not dispatch a backend command");
    }
    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        futures_util::stream::pending().boxed()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
}

#[cfg(unix)]
async fn acp_instance(directory: &std::path::Path) -> AgentInstance {
    use crate::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
    use crate::registry::AgentRegistry;
    let script = directory.join("agent.py");
    std::fs::write(
        &script,
        r#"import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message: continue
    assert message['method'] == 'initialize', 'preflight dispatched an ACP task'
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':{
        'protocolVersion':1,'agentCapabilities':{},'authMethods':[]}}), flush=True)
"#,
    )
    .unwrap();
    let db = aionui_db::init_database_memory().await.unwrap();
    let registry = AgentRegistry::new(Arc::new(aionui_db::SqliteAgentMetadataRepository::new(
        db.pool().clone(),
    )));
    registry.hydrate().await.unwrap();
    let metadata = registry.find_builtin_by_backend("copilot").await.unwrap();
    let params = assemble_acp_params(
        "rules-acp-fixture".into(),
        "fixture-user".into(),
        WorkspaceInfo {
            path: directory.to_string_lossy().into_owned(),
            is_custom: true,
        },
        metadata,
        aionui_common::CommandSpec {
            command: "python3".into(),
            args: vec![script.to_string_lossy().into_owned()],
            env: vec![],
            cwd: None,
        },
        aionui_api_types::AcpBuildExtra {
            backend: Some("copilot".into()),
            ..Default::default()
        },
        vec![],
        None,
        directory.into(),
        false,
    )
    .await;
    let paths = Arc::new(aionui_extension::resolve_skill_paths(directory, directory));
    let (manager, _, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        AcpAgentManager::build(
            Arc::new(params),
            crate::AcpSkillManager::new(paths),
            &registry.catalog_sender(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    AgentInstance::Acp(Arc::new(manager))
}

fn message() -> SendMessageData {
    SendMessageData {
        content: "SYNTHETIC_USER_TASK".into(),
        msg_id: "rules-fixture".into(),
        turn_id: None,
        files: vec![],
        inject_skills: vec![],
    }
}

/// The subprocess owns its configuration environment; no parallel test can
/// accidentally read this synthetic gateway or affect the production profile.
#[tokio::test]
async fn all_production_routes_reload_complete_rules_and_block_without_host_binding() {
    const CHILD: &str = "ELLMSHA_RULES_DISPATCH_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let server = MockServer::start().await;
        let records: Vec<_> = ["global", "project"]
            .into_iter()
            .map(|scope| {
                json!({
                    "id":scope,"guidance_status":"active","source_status":"active","type":"instruction",
                    "guidance_scope":scope,"source_path":format!("knowledge/fixture/{scope}.md")
                })
            })
            .collect();
        Mock::given(method("POST"))
            .and(path("/v3/atomic/search"))
            .and(body_partial_json(
                json!({"guidance_inventory":true,"include_global":true}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{
                "guidance_inventory":true,"degraded":false,"returned":2,"items":records,
                "total_candidates":2,"complete":true,"next_offset":null
            }})))
            .mount(&server)
            .await;
        for scope in ["global", "project"] {
            let source = format!("knowledge/fixture/{scope}.md");
            let revisions = std::sync::atomic::AtomicUsize::new(0);
            Mock::given(method("POST"))
                .and(path("/v3/scenario/read"))
                .and(body_partial_json(json!({"path":source})))
                .respond_with(move |_: &wiremock::Request| {
                    let revision = revisions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{
                        "path":source,"content":format!("{scope}_FULL_RULE_{revision}_BEGIN\n{}\n{scope}_FULL_RULE_{revision}_END", "complete scoped guidance\n".repeat(100))
                    }}))
                })
                .mount(&server)
                .await;
        }
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("memory.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "gatewayUrl":server.uri(),"serviceId":"fixture","teamId":"fixture",
                "agentId":"fixture","userId":"fixture"
            }))
            .unwrap(),
        )
        .unwrap();
        let output = tokio::task::spawn_blocking(move || std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agent_task::rules_dispatch_tests::all_production_routes_reload_complete_rules_and_block_without_host_binding", "--nocapture"])
            .env(CHILD, "1").env("AIONUI_TDAI_MEMORY_CONFIG", config).output().unwrap()).await.unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let provider = MockServer::start().await;
    // Exercise the real native manager/provider serializer against loopback.
    // The fixture returns text only and cannot request tools or reach an account.
    let response = [
        ("message_start", json!({"type":"message_start","message":{"id":"fixture","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}})),
        ("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
        ("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"fixture complete"}})),
        ("content_block_stop", json!({"type":"content_block_stop","index":0})),
        ("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}})),
        ("message_stop", json!({"type":"message_stop"})),
    ].into_iter().map(|(event, data)| format!("event: {event}\ndata: {data}\n\n")).collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(response, "text/event-stream"))
        .expect(2)
        .mount(&provider)
        .await;
    let mut native_config = super::aionrs_config_option_tests::make_test_config();
    native_config.tool_scope = aionui_api_types::ConversationToolScope::SessionOnly;
    // Even a regression must never send the fixture credential to a real API.
    native_config.base_url = Some(provider.uri());
    native_config.session_directory = directory.path().join("native-sessions");
    let native_session_directory = native_config.session_directory.clone();
    let native = AionrsAgentManager::new(
        "rules-native-fixture".into(),
        directory.path().to_string_lossy().into_owned(),
        native_config,
        None,
    )
    .await
    .unwrap();
    let instances = vec![
        AgentInstance::Aionrs(Arc::new(native)),
        AgentInstance::Session(crate::session_agent::SessionAgentTask::new(
            AgentType::Acp,
            "rules-session-fixture".into(),
            "fixture-user".into(),
            directory.path().to_string_lossy().into_owned(),
            Arc::new(NoDispatchBackend),
            None,
        )),
        #[cfg(unix)]
        acp_instance(directory.path()).await,
    ];
    for instance in instances {
        register_session(
            instance.conversation_id(),
            MemoryRecallScope {
                project_id: Some("fixture-project".into()),
                project_name: Some("Fixture".into()),
                workspace: instance.workspace().into(),
                guidance_only: true,
                ..Default::default()
            },
        );
        let mut previous = None;
        for turn in 0..2 {
            let prepared = instance.prepare_message(message()).await.unwrap();
            let is_native = matches!(instance, AgentInstance::Aionrs(_));
            let text = if is_native {
                assert_eq!(prepared.data.content, "SYNTHETIC_USER_TASK");
                format!(
                    "{}\n\n{}",
                    prepared.native_guidance.as_deref().unwrap(),
                    prepared.data.content
                )
            } else {
                assert!(prepared.native_guidance.is_none());
                prepared.data.content.clone()
            };
            assert!(text.starts_with("<ellmsha_required_rules"));
            assert_eq!(text.matches("<ellmsha_required_rules ").count(), 1);
            assert_eq!(text.matches("</ellmsha_required_rules>").count(), 1);
            for scope in ["global", "project"] {
                assert_eq!(text.matches(&format!("{scope}_FULL_RULE_")).count(), 2);
            }
            assert_eq!(text.matches("complete scoped guidance\n").count(), 200);
            let global = text.find("global_FULL_RULE_").unwrap();
            let project = text.find("project_FULL_RULE_").unwrap();
            assert!(global < project && project < text.find("SYNTHETIC_USER_TASK").unwrap());
            assert_eq!(text.matches("SYNTHETIC_USER_TASK").count(), 1);
            assert_ne!(previous.as_ref(), Some(&text), "rules must reload on the next turn");
            if is_native {
                let current_rules = prepared.native_guidance.clone().unwrap();
                prepared.send().await.unwrap();
                let requests = provider.received_requests().await.unwrap();
                assert_eq!(requests.len(), turn + 1);
                let request: serde_json::Value = serde_json::from_slice(&requests[turn].body).unwrap();
                let request_text = serde_json::to_string(&request).unwrap();
                assert_eq!(request_text.matches("<ellmsha_required_rules ").count(), 1);
                for scope in ["global", "project"] {
                    assert_eq!(request_text.matches(&format!("{scope}_FULL_RULE_")).count(), 2);
                }
                let blocks = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|message| message["content"].as_array().unwrap())
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>();
                assert!(
                    blocks.contains(&current_rules.as_str()),
                    "complete current rules must reach the provider unchanged"
                );
                assert!(!request["system"].to_string().contains("FULL_RULE_"));
                let saved = aion_agent::session::SessionManager::new(native_session_directory.clone(), 10)
                    .load(instance.conversation_id())
                    .unwrap();
                let saved_text = serde_json::to_string(&saved.messages).unwrap();
                assert!(
                    !saved_text.contains("FULL_RULE_"),
                    "mandatory snapshots must not accumulate in saved history"
                );
                assert_eq!(saved_text.matches("SYNTHETIC_USER_TASK").count(), turn + 1);
            }
            previous = Some(text);
        }
        let child = NativeChildRules {
            conversation_id: instance.conversation_id().into(),
            workspace: instance.workspace().into(),
        };
        assert!(child.instructions().await.unwrap().contains("project_FULL_RULE_"));
        // A bound chat must also fail closed when its knowledge configuration
        // becomes unreadable. This file belongs only to the fixture subprocess.
        let config = std::env::var_os("AIONUI_TDAI_MEMORY_CONFIG").unwrap();
        let original = std::fs::read(&config).unwrap();
        std::fs::write(&config, b"{}").unwrap();
        let error = instance.send_message(message()).await.unwrap_err();
        assert!(
            error
                .stream_error()
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("memory configuration unavailable")
        );
        assert!(
            child
                .instructions()
                .await
                .unwrap_err()
                .contains("memory configuration unavailable")
        );
        std::fs::write(config, original).unwrap();
        unregister_session(instance.conversation_id());
        let error = instance.send_message(message()).await.unwrap_err();
        assert!(
            error
                .stream_error()
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("host project binding unavailable")
        );
        assert!(
            child
                .instructions()
                .await
                .unwrap_err()
                .contains("host project binding unavailable")
        );
        // Only synthetic fixture sessions are stopped; no user process is used.
        instance.kill_and_wait(None).await;
    }
}
