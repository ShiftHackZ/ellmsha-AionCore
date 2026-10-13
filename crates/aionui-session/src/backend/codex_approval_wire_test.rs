use super::*;
use crate::testing::FakeAgentIo;
use aionui_common::mcp_approvals::McpApprovalStore;
use futures_util::StreamExt;

fn config(store: Option<McpApprovalStore>) -> SessionConfig {
    SessionConfig {
        mcp_approval_store: store,
        init: super::super::SessionInit {
            mcp_servers: vec![super::super::McpServerSpec {
                name: "fixture".into(),
                transport: super::super::McpTransport::Http {
                    url: "http://127.0.0.1:1234".into(),
                    headers: vec![],
                },
                auto_approve_in_unattended: false,
            }],
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn actual_mcp_approval_wire_saves_host_grant_and_fresh_reader_answers_exact_call() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    let wire = concat!(
        "{\"method\":\"thread/started\",\"params\":{\"thread\":{\"id\":\"root\"}}}\n",
        "{\"method\":\"item/started\",\"params\":{\"threadId\":\"root\",\"turnId\":\"turn\",\"item\":{\"type\":\"mcpToolCall\",\"id\":\"call\",\"server\":\"fixture\",\"tool\":\"probe\",\"arguments\":{}}}}\n",
        "{\"id\":7,\"method\":\"mcpServer/elicitation/request\",\"params\":{\"threadId\":\"root\",\"turnId\":\"turn\",\"serverName\":\"fixture\",\"mode\":\"form\",\"_meta\":{\"codex_approval_kind\":\"mcp_tool_call\",\"tool_params\":{}},\"requestedSchema\":{\"type\":\"object\",\"properties\":{}}}}\n"
    );
    for fresh in [false, true] {
        let fake =
            FakeAgentIo::never_exits(Vec::new()).with_gated_segments(vec![wire.as_bytes().to_vec(), b"{}\n".to_vec()]);
        let release = fake.segment_releaser();
        let captured = fake.captured_stdin();
        let backend = CodexSessionBackend::spawn_with_wake(
            "fixture-chat".into(),
            Box::new(fake),
            CodexWakeRecipe {
                spawner: None,
                config: std::sync::RwLock::new(config(Some(store.clone()))),
            },
            None,
        )
        .await;
        let mut events = backend.events();
        release();
        if !fresh {
            let request_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(event) = events.next().await {
                    if let SessionEvent::Permission { request_id, input, .. } = event.event {
                        assert_eq!(input.unwrap()["_meta"]["ellmsha_exact_tool"]["tool"], "probe");
                        return request_id;
                    }
                }
                panic!("permission not emitted")
            })
            .await
            .unwrap();
            backend.prepare_permission(&request_id, Some("ellmsha_always")).unwrap();
            backend
                .dispatch(Command::AnswerPermission {
                    request_id,
                    decision: super::super::types::PermissionDecision::AllowAlways,
                    selected: Some("ellmsha_always".into()),
                    answers: vec![],
                })
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let bytes = captured.lock().await.clone();
                let text = String::from_utf8(bytes).unwrap();
                if text.contains("\"action\":\"accept\"") {
                    assert!(
                        !text.contains("persist"),
                        "host scope must not masquerade as provider persistence"
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(backend.pending_permission_requests().is_empty());
    }
    assert_eq!(store.list().unwrap().len(), 1);
}

#[tokio::test]
async fn linear_app_catalog_overrides_fresh_and_resume_without_disabling_other_integrations() {
    for resume in [false, true] {
        let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_segments(vec![b"{\"id\":2,\"result\":{\"data\":[{\"id\":\"actual-linear\",\"name\":\"Linear\"},{\"id\":\"other-app\",\"name\":\"Calendar\"}],\"nextCursor\":null}}\n".to_vec(), b"{}\n".to_vec()]);
        let release = fake.segment_releaser();
        let captured = fake.captured_stdin();
        let cfg = SessionConfig {
            prefer_managed_linear: true,
            ..Default::default()
        };
        let backend = Arc::new(
            CodexSessionBackend::spawn_with_wake(
                "linear-fixture".into(),
                Box::new(fake),
                CodexWakeRecipe {
                    spawner: None,
                    config: std::sync::RwLock::new(cfg),
                },
                None,
            )
            .await,
        );
        let runner = backend.clone();
        let task = tokio::spawn(async move {
            runner
                .run_handshake(if resume {
                    HandshakeMode::Resume("owned-thread")
                } else {
                    HandshakeMode::Fresh
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if String::from_utf8(captured.lock().await.clone())
                    .unwrap()
                    .contains("app/list")
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        release();
        task.await.unwrap().unwrap();
        let frames: Vec<Value> = String::from_utf8(captured.lock().await.clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let method = if resume { "thread/resume" } else { "thread/start" };
        let frame = frames.iter().find(|frame| frame["method"] == method).unwrap();
        assert_eq!(
            frame["params"]["config"]["apps"],
            json!({"actual-linear":{"enabled":false}})
        );
        assert!(frame["params"]["config"]["features"].is_null());
    }
}
