use super::*;
use aionui_db::{
    CreateProviderParams, Database, SqliteAgentMetadataRepository, SqliteClientPreferenceRepository,
    SqliteProviderRepository, init_database_memory,
};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const USER: &str = "system_default_user";
fn target(id: &str, model: &str) -> RouterTarget {
    RouterTarget::Provider {
        provider_id: id.into(),
        model: model.into(),
    }
}
async fn fixture() -> (Database, RouterService) {
    let db = init_database_memory().await.unwrap();
    let router = RouterService::new(
        Arc::new(SqliteProviderRepository::new(db.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone())),
        AgentRegistry::new(Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()))),
        [7; 32],
    );
    (db, router)
}
async fn provider(db: &Database, id: &str, base_url: &str, enabled: bool, health: Option<&str>) {
    let encrypted = aionui_common::encrypt_string("fixture-private-key", &[7; 32]).unwrap();
    SqliteProviderRepository::new(db.pool().clone())
        .create(CreateProviderParams {
            id: Some(id),
            user_id: USER,
            platform: "openai",
            name: "Fixture account",
            base_url,
            api_key_encrypted: &encrypted,
            models: r#"["default","classifier","qa"]"#,
            enabled,
            capabilities: "[]",
            context_limit: Some(8192),
            model_protocols: None,
            model_enabled: None,
            model_health: health,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .unwrap();
}
fn policy() -> RouterConfig {
    RouterConfig {
        schema_version: 2,
        mode: RouterMode::Manual,
        root_target: Some(target("owned", "default")),
        child_target: Some(target("owned", "default")),
        classifier_target: Some(target("owned", "classifier")),
        allowed_targets: vec![target("owned", "default")],
        classes: vec![RouterClass {
            id: "qa".into(),
            name: "QA".into(),
            description: "Test the implementation".into(),
            target: target("worker", "qa"),
        }],
        ..Default::default()
    }
}
async fn select(
    router: &RouterService,
    config: &RouterConfig,
    child: bool,
) -> Result<(RouterDecision, String), AgentError> {
    router
        .select(
            USER,
            config,
            "Test the implementation",
            &MemoryRecallScope {
                isolated_test: true,
                ..Default::default()
            },
            child,
            false,
            true,
            &Default::default(),
        )
        .await
}
async fn classifier_response(server: &MockServer, content: &str) {
    let data = json!({"id":"fixture", "choices":[{"index":0,"delta":{"content":content},"finish_reason":null}]});
    let done = json!({"id":"fixture", "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":12,"total_tokens":112}});
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("data: {data}\n\ndata: {done}\n\ndata: [DONE]\n\n")),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn graph_catalog_retains_unhealthy_and_disabled_owned_models_without_credentials() {
    let (db, router) = fixture().await;
    provider(
        &db,
        "owned",
        "http://127.0.0.1:1/v1",
        true,
        Some(r#"{"qa":{"status":"unhealthy","last_check":1}}"#),
    )
    .await;
    provider(&db, "disabled", "http://127.0.0.1:1/v1", false, None).await;
    let config = RouterConfig {
        schema_version: 2,
        ..Default::default()
    };
    let catalog = router.catalog(USER, &config).await.unwrap();
    let qa = catalog.iter().find(|e| e.target == target("owned", "qa")).unwrap();
    assert_eq!(qa.classifier.reason.as_deref(), Some("model_unhealthy"));
    assert!(!qa.root.supported);
    assert!(
        catalog
            .iter()
            .any(|e| e.target == target("owned", "classifier") && e.classifier.supported)
    );
    assert!(
        catalog
            .iter()
            .any(|e| e.target == target("disabled", "qa") && e.root.reason.as_deref() == Some("account_disabled"))
    );
    let serialized = serde_json::to_string(&catalog).unwrap();
    for secret in ["fixture-private-key", "api_key", "base_url", "spawn_env"] {
        assert!(!serialized.contains(secret));
    }
    // Legacy duplicate hints use the first entry, matching the actual selector.
    let duplicates = RouterConfig {
        schema_version: 2,
        hints: vec![
            RouterHint {
                target: Some(target("owned", "default")),
                supports_tools: Some(false),
                ..Default::default()
            },
            RouterHint {
                target: Some(target("owned", "default")),
                local_inference: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let catalog = router.catalog(USER, &duplicates).await.unwrap();
    let hinted = catalog
        .iter()
        .find(|entry| entry.target == target("owned", "default"))
        .unwrap();
    assert!(!hinted.local);
    assert_eq!(hinted.root.reason.as_deref(), Some("tool_calling_disabled"));
    assert!(hinted.classifier.supported);
    let other = router.catalog("other-user", &config).await.unwrap();
    assert!(!other.iter().any(|e| matches!(e.target, RouterTarget::Provider { .. })));
    let saved = RouterConfig {
        schema_version: 2,
        root_target: Some(target("disabled", "qa")),
        mode: RouterMode::SemiAuto,
        ..Default::default()
    };
    router.validate_configuration(USER, &saved).await.unwrap();
    let mut legacy = saved;
    legacy.schema_version = 1;
    assert!(
        matches!(router.validate_configuration(USER,&legacy).await,Err(AgentError::BadRequest(message)) if message.contains("unavailable target"))
    );
}

#[tokio::test]
async fn graph_saved_missing_selections_survive_but_new_foreign_references_are_rejected() {
    let (db, router) = fixture().await;
    provider(&db, "owned", "http://127.0.0.1:1/v1", true, None).await;
    let config = RouterConfig {
        schema_version: 2,
        mode: RouterMode::SemiAuto,
        root_target: Some(target("owned", "default")),
        ..Default::default()
    };
    let settings = RouterSettings::migrate(config.clone());
    router
        .preferences
        .upsert_batch(USER, &[(ROUTER_CONFIG_KEY, &serde_json::to_string(&settings).unwrap())])
        .await
        .unwrap();
    sqlx::query("UPDATE providers SET models='[]' WHERE id='owned'")
        .execute(db.pool())
        .await
        .unwrap();
    router.validate_configuration(USER, &config).await.unwrap();
    let catalog = router.catalog(USER, &config).await.unwrap();
    assert!(
        catalog
            .iter()
            .any(|e| e.target == target("owned", "default") && e.root.reason.as_deref() == Some("target_missing"))
    );
    let mut injected = config.clone();
    injected.root_target = Some(target("foreign", "model"));
    assert!(
        matches!(router.validate_configuration(USER,&injected).await,Err(AgentError::BadRequest(message)) if message.contains("unknown or unowned"))
    );
    let mut rule = config;
    rule.classifier_target = rule.root_target.clone();
    rule.classes = vec![RouterClass {
        id: "qa".into(),
        name: "QA".into(),
        description: "Test".into(),
        target: target("foreign", "model"),
    }];
    assert!(
        matches!(router.validate_configuration(USER,&rule).await,Err(AgentError::BadRequest(message)) if message.contains("unknown or unowned"))
    );
}

#[tokio::test]
async fn graph_root_and_empty_classes_bypass_classifier_and_child_match_overrides_default() {
    let server = MockServer::start().await;
    classifier_response(&server, r#"{"class_id":"qa"}"#).await;
    let (db, router) = fixture().await;
    provider(&db, "owned", &format!("{}/v1", server.uri()), true, None).await;
    provider(&db, "worker", "http://127.0.0.1:1/v1", true, None).await;
    let config = policy();
    let root = select(&router, &config, false).await.unwrap().0;
    assert_eq!(root.target, target("owned", "default"));
    assert!(root.reasons.contains(&"root_policy_custom_classes_not_applied".into()));
    let mut empty = config.clone();
    empty.classes.clear();
    empty.classifier_target = Some(target("missing", "model"));
    let default = select(&router, &empty, true).await.unwrap().0;
    assert_eq!(default.target, target("owned", "default"));
    assert!(server.received_requests().await.unwrap().is_empty());
    let child = select(&router, &config, true).await.unwrap().0;
    assert_eq!(child.target, target("worker", "qa"));
    assert!(child.reasons.contains(&"custom_class:qa".into()));
    let calls = server.received_requests().await.unwrap();
    assert_eq!(calls.len(), 1);
    let body: Value = serde_json::from_slice(&calls[0].body).unwrap();
    assert_eq!(body["model"], "classifier");
    assert!(
        body.get("tools")
            .is_none_or(|t| t.as_array().is_some_and(Vec::is_empty))
    );
    let content = body["messages"].to_string();
    assert!(!content.contains("provider_id"));
    assert!(!content.contains("fixture-private-key"));
}

#[tokio::test]
async fn graph_no_match_and_invalid_classification_use_explicit_default() {
    for (content, reason) in [
        (r#"{"class_id":null}"#, "classification_no_match_default"),
        (r#"{"class_id":"unknown"}"#, "classification_failed_default"),
        ("not-json", "classification_failed_default"),
    ] {
        let server = MockServer::start().await;
        classifier_response(&server, content).await;
        let (db, router) = fixture().await;
        provider(&db, "owned", &format!("{}/v1", server.uri()), true, None).await;
        provider(&db, "worker", "http://127.0.0.1:1/v1", true, None).await;
        let decision = select(&router, &policy(), true).await.unwrap().0;
        assert_eq!(decision.target, target("owned", "default"));
        assert!(decision.reasons.contains(&reason.into()), "{:?}", decision.reasons);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn graph_effective_custom_route_does_not_require_unused_default() {
    for (content, matched) in [(r#"{"class_id":"qa"}"#, true), (r#"{"class_id":null}"#, false)] {
        let server = MockServer::start().await;
        classifier_response(&server, content).await;
        let (db, router) = fixture().await;
        provider(&db, "owned", &format!("{}/v1", server.uri()), true, None).await;
        provider(&db, "worker", "http://127.0.0.1:1/v1", true, None).await;
        provider(&db, "disabled", "http://127.0.0.1:1/v1", false, None).await;
        let mut config = policy();
        config.child_target = Some(target("disabled", "default"));
        let root = select(&router, &config, false).await.unwrap().0;
        assert_eq!(root.target, target("owned", "default"));
        assert!(server.received_requests().await.unwrap().is_empty());
        let child = select(&router, &config, true).await;
        if matched {
            let child = child.unwrap().0;
            assert_eq!(child.target, target("worker", "qa"));
            assert!(child.reasons.contains(&"custom_class:qa".into()));
        } else {
            assert!(matches!(child, Err(AgentError::BadRequest(message))
                if message == "Fixed router target is missing, disabled or incompatible with this task"));
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn graph_matched_unavailable_rule_fails_and_local_only_conflict_blocks_inference() {
    let server = MockServer::start().await;
    classifier_response(&server, r#"{"class_id":"qa"}"#).await;
    let (db, router) = fixture().await;
    provider(&db, "owned", &format!("{}/v1", server.uri()), true, None).await;
    provider(&db, "worker", "http://127.0.0.1:1/v1", false, None).await;
    assert!(
        matches!(select(&router,&policy(),true).await,Err(AgentError::BadRequest(message)) if message.contains("Matched custom class target"))
    );
    let mut config = policy();
    config.local_only = true;
    config.hints = vec![
        RouterHint {
            target: config.root_target.clone(),
            local_inference: true,
            ..Default::default()
        },
        RouterHint {
            target: config.classifier_target.clone(),
            local_inference: true,
            ..Default::default()
        },
    ];
    assert!(
        matches!(select(&router,&config,false).await,Err(AgentError::BadRequest(message)) if message.contains("Local-only routing conflicts"))
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "local conflict must not run inference"
    );
}

#[tokio::test]
async fn graph_settings_migration_does_not_rewrite_frozen_legacy_policy() {
    let (_db, router) = fixture().await;
    let legacy = RouterConfig {
        profile: RouterProfile::Speed,
        ..Default::default()
    };
    router
        .preferences
        .upsert_batch(USER, &[(ROUTER_CONFIG_KEY, &serde_json::to_string(&legacy).unwrap())])
        .await
        .unwrap();
    let migrated = router.settings(USER).await.unwrap();
    assert_eq!(migrated.schema_version, 2);
    assert_eq!(migrated.profile, RouterProfile::Speed);
    assert_eq!(legacy.schema_version, 1);
    let frozen: RouterConfig = serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
    assert_eq!(frozen, legacy);
}

#[tokio::test]
async fn generic_acp_root_and_child_routes_use_protocol_transport_not_vendor_whitelist() {
    let (db, router) = fixture().await;
    sqlx::query("INSERT INTO agent_metadata(id,agent_id,user_id,name,backend,agent_type,agent_source,command,enabled,last_check_status,available_models,agent_capabilities,created_at,updated_at) VALUES('router-acp-row','router-acp',?,'Generic ACP','test-acp','acp','custom','/bin/sh',1,'success',?,?,0,0)")
        .bind(USER).bind(r#"{"available_models":[{"id":"worker"}]}"#)
        .bind(r#"{"mcpCapabilities":{"http":false,"sse":false}}"#)
        .execute(db.pool()).await.unwrap();
    let catalog = router.catalog(USER, &RouterConfig::default()).await.unwrap();
    let entry = catalog
        .iter()
        .find(|entry| {
            entry.target
                == RouterTarget::Agent {
                    agent_id: "router-acp".into(),
                    model: "worker".into(),
                }
        })
        .expect("generic ACP model in catalog");
    assert!(entry.root.supported, "{:?}", entry.root.reason);
    assert!(entry.child.supported, "{:?}", entry.child.reason);
    sqlx::query("UPDATE agent_metadata SET agent_capabilities=? WHERE agent_id='router-acp'")
        .bind(r#"{"mcpCapabilities":{"stdio":false}}"#)
        .execute(db.pool())
        .await
        .unwrap();
    let catalog = router.catalog(USER, &RouterConfig::default()).await.unwrap();
    let entry = catalog
        .iter()
        .find(|entry| {
            entry.target
                == RouterTarget::Agent {
                    agent_id: "router-acp".into(),
                    model: "worker".into(),
                }
        })
        .unwrap();
    assert_eq!(entry.root.reason.as_deref(), Some("router_mcp_transport_unsupported"));
}

#[tokio::test]
async fn malformed_classifier_answer_still_records_its_actual_spend() {
    #[derive(Default)]
    struct Bus(std::sync::Mutex<Vec<aionui_api_types::WebSocketMessage<Value>>>);
    impl aionui_realtime::EventBroadcaster for Bus {
        fn broadcast(&self, message: aionui_api_types::WebSocketMessage<Value>) {
            self.0.lock().unwrap().push(message);
        }
    }
    let (db, router) = fixture().await;
    let bus = Arc::new(Bus::default());
    let router = router.with_usage_broadcaster(bus.clone());
    let server = MockServer::start().await;
    provider(&db, "owned", &format!("{}/v1", server.uri()), true, None).await;
    classifier_response(&server, "malformed").await;
    let scope = MemoryRecallScope {
        conversation_id: Some("parent".into()),
        isolated_test: true,
        ..Default::default()
    };
    assert!(
        router
            .classify_custom(USER, &policy(), &target("owned", "classifier"), "task", "", &scope)
            .await
            .is_err()
    );
    let events = bus.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "usage.request");
    assert_eq!(events[0].data["conversation_id"], "parent");
    assert_eq!(events[0].data["data"]["input_tokens"], 100);
    assert_eq!(events[0].data["data"]["context_reported"], false);
}
