//! Exercise every factory guidance slot and the concrete ACP send/resume pipeline.
//! The local ACP peer captures synthetic prompts; it is not a vendor model test.
use super::*;
use crate::agent_task::AgentInstance;
use crate::capability::tencent_memory::{register_session, unregister_session};
use crate::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use crate::manager::acp::AcpAgentManager;
use crate::registry::AgentRegistry;
use crate::session_context::{
    AcpSessionBuildContext, AionrsSessionBuildContext, AntigravitySessionBuildContext, ConversationContext,
    WorkspaceContext,
};
use crate::types::{MemoryRecallScope, SendMessageData};
use aionui_common::{AgentType, CommandSpec, ProviderWithModel};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

fn context(directory: &Path) -> AgentSessionContext {
    AgentSessionContext {
        conversation: ConversationContext {
            conversation_id: "acp-single-guidance-fixture".into(),
            user_id: "fixture-user".into(),
            agent_type: AgentType::Acp,
            source: None,
        },
        workspace: WorkspaceContext {
            path: directory.to_string_lossy().into_owned(),
            stored_path: directory.to_string_lossy().into_owned(),
            is_custom: true,
        },
        model: ProviderWithModel {
            provider_id: "fixture".into(),
            model: "fixture".into(),
            use_model: None,
        },
        skills: vec![],
        managed_skill_roots: vec![],
        runtime_env: vec![],
        team: None,
        kind: AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
            config: aionui_api_types::AcpBuildExtra {
                backend: Some("copilot".into()),
                preset_context: Some("USER_PRESET_MUST_SURVIVE".into()),
                ..Default::default()
            },
            team: None,
            belongs_to_team: false,
            session_id: None,
            session_snapshot: None,
        })),
    }
}

async fn agent(context: AgentSessionContext, directory: &Path, resume: bool) -> AgentInstance {
    let AgentSessionKind::Acp(build) = context.kind else {
        panic!("ACP fixture expected")
    };
    let preset = build.config.preset_context.as_deref().unwrap();
    assert!(preset.contains("[eLLMsha project memory startup policy]"));
    assert!(preset.ends_with("USER_PRESET_MUST_SURVIVE"));
    assert!(!preset.contains("<ellmsha_required_rules"));
    let db = aionui_db::init_database_memory().await.unwrap();
    let registry = AgentRegistry::new(Arc::new(aionui_db::SqliteAgentMetadataRepository::new(
        db.pool().clone(),
    )));
    registry.hydrate().await.unwrap();
    let metadata = registry.find_builtin_by_backend("copilot").await.unwrap();
    let params = assemble_acp_params(
        context.conversation.conversation_id,
        context.conversation.user_id,
        WorkspaceInfo {
            path: context.workspace.path,
            is_custom: true,
        },
        metadata,
        CommandSpec {
            command: "python3".into(),
            args: vec![
                directory.join("agent.py").to_string_lossy().into_owned(),
                directory.join("requests.jsonl").to_string_lossy().into_owned(),
            ],
            env: vec![],
            cwd: Some(directory.to_string_lossy().into_owned()),
        },
        build.config,
        vec![],
        None,
        directory.into(),
        false,
    )
    .await;
    let paths = Arc::new(aionui_extension::resolve_skill_paths(directory, directory));
    let (manager, _, _) = AcpAgentManager::build(
        Arc::new(params),
        crate::AcpSkillManager::new(paths),
        &registry.catalog_sender(),
    )
    .await
    .unwrap();
    if resume {
        manager.set_session_id("saved-fixture-session".into()).await;
    }
    AgentInstance::Acp(Arc::new(manager))
}

fn message() -> SendMessageData {
    SendMessageData {
        content: "SYNTHETIC_USER_TASK".into(),
        msg_id: "single-guidance-turn".into(),
        turn_id: None,
        files: vec![],
        inject_skills: vec![],
    }
}

fn requests(directory: &Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn acp_fresh_and_resumed_prompts_receive_one_current_complete_guidance_block() {
    const CHILD: &str = "ELLMSHA_ACP_SINGLE_GUIDANCE_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let server = MockServer::start().await;
        // Match the real inventory: global namespace or the bound project's V2 alias.
        let source_path = |scope: &str| {
            if scope == "global" {
                "knowledge/global/global.md"
            } else {
                "v2/projects/fixture/project.md"
            }
        };
        let records: Vec<_> = ["global", "project"]
            .into_iter()
            .map(|scope| {
                json!({
                    "id":scope,"guidance_status":"active","source_status":"active","type":"instruction",
                    "guidance_scope":scope,"source_path":source_path(scope)
                })
            })
            .collect();
        Mock::given(method("POST"))
            .and(path("/v3/atomic/search"))
            .and(body_partial_json(json!({"guidance_inventory":true})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{
                "guidance_inventory":true,"degraded":false,"returned":2,"items":records,
                "total_candidates":2,"complete":true,"next_offset":null
            }})))
            .mount(&server)
            .await;
        for scope in ["global", "project"] {
            let source = source_path(scope);
            let revision = std::sync::atomic::AtomicUsize::new(0);
            Mock::given(method("POST"))
                .and(path("/v3/scenario/read"))
                .and(body_partial_json(json!({"path":source})))
                .respond_with(move |_: &wiremock::Request| {
                    let revision = revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{
                        "path":source,"content":format!("{scope}_FULL_RULE_{revision}_BEGIN\n{}\n{scope}_FULL_RULE_{revision}_END", "complete synthetic rule body\n".repeat(100))
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
        // Only the subprocess receives this fixture gateway; parallel tests and
        // the installed application keep their own environment and profiles.
        let mut command = aionui_runtime::Builder::clean_cli(std::env::current_exe().unwrap());
        command
            .args(["--exact", "factory::tencent_memory::prompt_tests::acp_fresh_and_resumed_prompts_receive_one_current_complete_guidance_block", "--nocapture"])
            .env(CHILD, "1")
            .env("AIONUI_TDAI_MEMORY_CONFIG", config);
        let output = tokio::time::timeout(std::time::Duration::from_secs(45), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("agent.py"),
        r#"import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message: continue
    with open(sys.argv[1], 'a') as capture: capture.write(json.dumps(message) + '\n')
    method = message['method']
    if method == 'initialize':
        result = {'protocolVersion':1,'agentCapabilities':{'loadSession':True},'authMethods':[]}
    elif method == 'session/new': result = {'sessionId':'saved-fixture-session'}
    elif method == 'session/load': result = {}
    elif method == 'session/prompt':
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{
            'sessionId':'saved-fixture-session','update':{'sessionUpdate':'agent_message_chunk',
            'content':{'type':'text','text':'SYNTHETIC_RESPONSE'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    else: raise AssertionError('Unexpected fixture method: ' + method)
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
"#,
    )
    .unwrap();
    let config = std::env::var_os("AIONUI_TDAI_MEMORY_CONFIG").unwrap();
    let original = std::fs::read(&config).unwrap();
    let mut startup = context(directory.path());
    register_session(
        startup.conversation_id(),
        MemoryRecallScope {
            project_id: Some("fixture-project".into()),
            project_name: Some("Fixture".into()),
            workspace: startup.workspace.path.clone(),
            guidance_only: true,
            ..Default::default()
        },
    );
    // Construction still fails closed, even though its snapshot is no longer
    // copied into the ACP first-message prelude.
    std::fs::write(&config, b"{}").unwrap();
    assert!(
        attach_required_guidance(&mut startup)
            .await
            .unwrap_err()
            .to_string()
            .contains("memory configuration unavailable")
    );
    std::fs::write(&config, &original).unwrap();

    for (resume, current_revision) in [(false, 0), (true, 1)] {
        let mut startup = context(directory.path());
        attach_required_guidance(&mut startup).await.unwrap();
        let instance = agent(startup, directory.path(), resume).await;
        tokio::time::timeout(std::time::Duration::from_secs(10), instance.send_message(message()))
            .await
            .unwrap()
            .unwrap();
        let captured = requests(directory.path());
        let prompt = captured.iter().rev().find(|r| r["method"] == "session/prompt").unwrap();
        let text = prompt["params"]["prompt"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<String>();
        assert_eq!(text.matches("<ellmsha_required_rules ").count(), 1);
        assert_eq!(text.matches("</ellmsha_required_rules>").count(), 1);
        for scope in ["global", "project"] {
            let body = format!(
                "{scope}_FULL_RULE_{current_revision}_BEGIN\n{}\n{scope}_FULL_RULE_{current_revision}_END",
                "complete synthetic rule body\n".repeat(100)
            );
            assert_eq!(
                text.matches(&body).count(),
                1,
                "complete current rule must reach ACP exactly once"
            );
            assert!(
                current_revision == 0 || !text.contains(&format!("{scope}_FULL_RULE_{}", current_revision - 1)),
                "startup snapshot must not leak into the prompt"
            );
        }
        assert!(text.ends_with("SYNTHETIC_USER_TASK"));
        if !resume {
            assert!(text.contains("USER_PRESET_MUST_SURVIVE"));
            assert!(text.contains("[eLLMsha project memory startup policy]"));
        }
        assert!(
            captured
                .iter()
                .any(|r| r["method"] == if resume { "session/load" } else { "session/new" })
        );
        // A failed current preflight cannot reuse the successful startup check.
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
        assert_eq!(
            requests(directory.path()).len(),
            captured.len(),
            "failed preflight dispatched ACP work"
        );
        std::fs::write(&config, &original).unwrap();
        instance.kill_and_wait(None).await;
    }
    // Every factory branch validates the full current inventory without
    // freezing a duplicate in its system/preset context. Direct Claude/Codex
    // begin in the ACP factory branch; routed and team contexts use these same
    // typed slots. The shared AgentInstance barrier is tested independently for
    // every concrete production task kind in agent_task_rules_tests.rs.
    let acp = |backend: &str| {
        AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
            config: aionui_api_types::AcpBuildExtra {
                backend: Some(backend.into()),
                preset_context: Some("USER_PRESET_MUST_SURVIVE".into()),
                ..Default::default()
            },
            team: None,
            belongs_to_team: false,
            session_id: None,
            session_snapshot: None,
        }))
    };
    for kind in [
        acp("copilot"),
        acp("claude"),
        acp("codex"),
        AgentSessionKind::Aionrs(Box::new(AionrsSessionBuildContext {
            config: aionui_api_types::AionrsBuildExtra {
                system_prompt: Some("USER_PRESET_MUST_SURVIVE".into()),
                ..Default::default()
            },
            team: None,
            belongs_to_team: false,
        })),
        AgentSessionKind::Antigravity(Box::new(AntigravitySessionBuildContext {
            config: aionui_api_types::AcpBuildExtra {
                preset_context: Some("USER_PRESET_MUST_SURVIVE".into()),
                ..Default::default()
            },
            team: None,
            belongs_to_team: false,
            session_id: None,
            session_snapshot: None,
        })),
    ] {
        let mut startup = context(directory.path());
        startup.kind = kind;
        std::fs::write(&config, b"{}").unwrap();
        let error = attach_required_guidance(&mut startup).await.unwrap_err();
        assert!(error.to_string().contains("memory configuration unavailable"));
        std::fs::write(&config, &original).unwrap();
        attach_required_guidance(&mut startup).await.unwrap();
        let preset = match &startup.kind {
            AgentSessionKind::Acp(c) => c.config.preset_context.as_deref(),
            AgentSessionKind::Antigravity(c) => c.config.preset_context.as_deref(),
            AgentSessionKind::Aionrs(c) => c.config.system_prompt.as_deref(),
        }
        .unwrap();
        assert!(preset.starts_with("[eLLMsha project memory startup policy]"));
        assert!(preset.ends_with("USER_PRESET_MUST_SURVIVE"));
        assert!(!preset.contains("<ellmsha_required_rules"));
        assert!(!preset.contains("global_FULL_RULE_"));
        assert!(!preset.contains("project_FULL_RULE_"));
    }
    unregister_session("acp-single-guidance-fixture");
}
