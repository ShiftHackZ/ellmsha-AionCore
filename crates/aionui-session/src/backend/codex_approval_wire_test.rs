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
        assert_eq!(
            frame["params"]["config"]["features"],
            json!({"default_mode_request_user_input": true}),
            "Only the native human-input feature is overridden; unrelated app features stay provider-owned"
        );
    }
}

#[tokio::test]
async fn user_input_round_trip_preserves_question_ids_free_text_and_recovery() {
    use super::super::types::{PermissionDecision, QuestionAnswer};
    let params = json!({"threadId":"root","turnId":"turn","itemId":"item","isBlocking":false,"questions":[
        {"id":"db","header":"Database","question":"Which database?","isOther":false,"isSecret":false,"options":[{"label":"SQLite","description":"Local"}]},
        {"id":"tag","header":"Tag","question":"Enter a value","isOther":true,"isSecret":true,"options":null}
    ]});
    let wire = format!(
        "{}\n",
        json!({"id":"wire-id","method":"item/tool/requestUserInput","params":params})
    );
    let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_segments(vec![wire.into_bytes(), b"{}\n".to_vec()]);
    let release = fake.segment_releaser();
    let captured = fake.captured_stdin();
    let backend = CodexSessionBackend::spawn_with_wake(
        "chat".into(),
        Box::new(fake),
        CodexWakeRecipe {
            spawner: None,
            config: std::sync::RwLock::new(config(None)),
        },
        None,
    )
    .await;
    let mut events = backend.events();
    release();
    let id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            if let SessionEvent::Permission { request_id, input, .. } = event.event {
                assert_eq!(input.unwrap()["questions"], params["questions"]);
                return request_id;
            }
        }
        panic!("missing native question");
    })
    .await
    .unwrap();
    assert_eq!(
        backend.pending_permission_requests()[0].questions.as_ref(),
        Some(&params["questions"])
    );
    let answer = |answers| Command::AnswerPermission {
        request_id: id.clone(),
        decision: PermissionDecision::Approved,
        selected: None,
        answers,
    };
    assert!(backend.dispatch(answer(vec![])).await.is_err());
    let answers = vec![
        QuestionAnswer {
            question: "tag".into(),
            labels: vec!["value typed".into()],
        },
        QuestionAnswer {
            question: "db".into(),
            labels: vec!["SQLite".into()],
        },
    ];
    backend.dispatch(answer(answers.clone())).await.unwrap();
    assert!(backend.dispatch(answer(answers)).await.is_err());
    assert!(backend.pending_permission_requests().is_empty());
    let response: Value = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let frames = String::from_utf8(captured.lock().await.clone()).unwrap();
            if let Some(response) = frames
                .lines()
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .find(|v| v["id"] == "wire-id")
            {
                return response;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        response["result"],
        json!({"answers":{"db":{"answers":["SQLite"]},"tag":{"answers":["value typed"]}}})
    );
}

#[tokio::test]
async fn async_user_input_is_a_nonblocking_card_until_an_acknowledged_user_turn() {
    let item = json!({"type":"agentMessage","id":"call-fixture","text":"Which language?\n- Rust\n- Go","phase":"final_answer","delivery":"async","questions":[{"title":"Which language?","options":["Rust","Go"]}]});
    let wire = [
        json!({"method":"thread/started","params":{"thread":{"id":"root"}}}),
        json!({"method":"turn/started","params":{"threadId":"root","turn":{"id":"turn"}}}),
        json!({"method":"item/started","params":{"threadId":"root","turnId":"turn","item":item}}),
        json!({"method":"item/completed","params":{"threadId":"root","turnId":"turn","item":item}}),
    ]
    .map(|f| f.to_string() + "\n")
    .concat();
    let ack = json!({"id":1,"jsonrpc":"2.0","result":{"turnId":"turn"}}).to_string() + "\n";
    let fake = FakeAgentIo::never_exits(Vec::new()).with_gated_segments(vec![
        wire.into_bytes(),
        ack.into_bytes(),
        b"{}\n".to_vec(),
    ]);
    let release = fake.segment_releaser();
    let captured = fake.captured_stdin();
    let backend = Arc::new(
        CodexSessionBackend::spawn_with_wake(
            "chat".into(),
            Box::new(fake),
            CodexWakeRecipe {
                spawner: None,
                config: std::sync::RwLock::new(config(None)),
            },
            None,
        )
        .await,
    );
    let mut events = backend.events();
    release();
    let id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(e) = events.next().await {
            match e.event {
                SessionEvent::AdapterSpecific { tag, payload } if tag == "codex_async_question" => {
                    assert_eq!(
                        payload["input"]["questions"][0]["id"],
                        r#"["request_user_input_async","call-fixture",0]"#
                    );
                    return payload["id"].as_str().unwrap().to_owned();
                }
                SessionEvent::Permission { .. } => panic!("async question blocked the turn"),
                SessionEvent::MessageDelta { .. } => panic!("assistant fallback text duplicated the question"),
                _ => (),
            }
        }
        panic!("expected async form")
    })
    .await
    .unwrap();
    assert_eq!(backend.pending_permission_requests().len(), 1);
    let task = tokio::spawn({
        let backend = backend.clone();
        let key = id.clone();
        async move { backend.steer_async_question_reply(&key, "User answer").await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if String::from_utf8_lossy(&captured.lock().await).contains("turn/steer") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!task.is_finished(), "Writing to stdin is not provider acknowledgment");
    assert!(backend.steer_async_question_reply(&id, "duplicate").await.is_err());
    release();
    assert!(task.await.unwrap().unwrap());
    assert!(backend.pending_permission_requests().is_empty());
}

#[test]
fn malformed_or_plain_codex_messages_never_become_question_cards() {
    for item in [
        json!({"type":"agentMessage","id":"x","text":"- yes\n- no"}),
        json!({"type":"agentMessage","id":"x","delivery":"async","questions":[{"title":"  ","options":["a"]}]}),
        json!({"type":"agentMessage","id":"x","delivery":"async","questions":[{"title":"Pick","options":["a","a"]}]}),
    ] {
        assert!(async_user_questions(&item).is_none())
    }
}

#[test]
fn externally_resolved_user_input_uses_the_original_public_card_id() {
    let registry = Arc::new(std::sync::Mutex::new(HashMap::from([(
        "question:7".into(),
        PendingToolApproval {
            tool_name: "request_user_input".into(),
            input: None,
            answered: false,
        },
    )])));
    assert_eq!(remove_pending_tool_approval(&registry, "7"), "question:7");
    assert!(!has_pending_tool_approval(&registry, "7"));
}
