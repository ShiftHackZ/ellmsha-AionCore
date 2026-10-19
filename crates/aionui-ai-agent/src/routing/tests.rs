use super::*;
use aionui_db::{
    CreateProviderParams, SqliteAgentMetadataRepository, SqliteClientPreferenceRepository, SqliteProviderRepository,
    init_database_memory,
};

#[tokio::test]
async fn ownership_locality_and_missing_manual_child_fail_before_inference() {
    let db = init_database_memory().await.unwrap();
    let providers = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let encrypted = aionui_common::encrypt_string("", &[7; 32]).unwrap();
    providers
        .create(CreateProviderParams {
            id: Some("owned"),
            user_id: "system_default_user",
            platform: "openai",
            name: "Fixture",
            base_url: "http://127.0.0.1:1/v1",
            api_key_encrypted: &encrypted,
            models: r#"["fixture"]"#,
            enabled: true,
            capabilities: "[]",
            context_limit: Some(8192),
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .unwrap();
    let registry = AgentRegistry::new(Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone())));
    let router = RouterService::new(
        providers,
        Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone())),
        registry,
        [7; 32],
    );
    let target = RouterTarget::Provider {
        provider_id: "owned".into(),
        model: "fixture".into(),
    };
    let mut config = RouterConfig {
        mode: RouterMode::SemiAuto,
        root_target: Some(target.clone()),
        local_only: true,
        hints: vec![RouterHint {
            target: Some(target.clone()),
            local_inference: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let scope = MemoryRecallScope {
        isolated_test: true,
        ..Default::default()
    };
    assert!(
        router
            .select(
                "other-user",
                &config,
                "review",
                &scope,
                false,
                false,
                false,
                &Default::default()
            )
            .await
            .is_err()
    );
    assert!(
        router
            .select(
                "system_default_user",
                &config,
                "review",
                &scope,
                false,
                false,
                false,
                &Default::default()
            )
            .await
            .is_ok()
    );
    config.mode = RouterMode::Manual;
    config.child_target = Some(RouterTarget::Provider {
        provider_id: "missing".into(),
        model: "fixture".into(),
    });
    assert!(
        router
            .select(
                "system_default_user",
                &config,
                "review",
                &scope,
                false,
                false,
                false,
                &Default::default()
            )
            .await
            .is_err()
    );
    config.mode = RouterMode::SemiAuto;
    config.hints[0].local_inference = false;
    assert!(
        router
            .select(
                "system_default_user",
                &config,
                "review",
                &scope,
                false,
                false,
                false,
                &Default::default()
            )
            .await
            .is_err()
    );
}

#[test]
fn advertised_cli_models_use_existing_config_options_first() {
    let options = serde_json::json!([{ "category":"model", "options":[{"value":"advertised"}] }]);
    let stale = serde_json::json!({"available_models":[{"id":"stale"}]});
    assert_eq!(advertised_models(Some(&options), Some(&stale)), vec!["advertised"]);
    assert!(!loopback_url("http://localhost.example.com"));
    assert!(loopback_url("http://127.0.0.1:11434"));
}

#[test]
fn canonical_wrapped_and_grouped_cli_model_catalog_is_authoritative() {
    let stale = serde_json::json!({"available_models":[{"id":"stale"}]});
    let wrapped = serde_json::json!({"config_options":[{
        "id":"model", "category":"model", "name":"Model", "type":"select", "current_value":"fresh",
        "options":[{"value":"fresh", "name":"Fresh"}]
    }]});
    assert_eq!(advertised_models(Some(&wrapped), Some(&stale)), vec!["fresh"]);
    let grouped = serde_json::json!({"config_options":[{
        "category":"model", "options":[{"name":"Group", "options":[{"value":"nested"}]}]
    }]});
    assert_eq!(advertised_models(Some(&grouped), Some(&stale)), vec!["nested"]);
    assert!(loopback_url("http://[::1]:11434/v1"));
    assert!(!loopback_url("http://[2001:db8::1]:11434/v1"));
}

#[tokio::test]
async fn routed_permissions_are_checked_against_selected_runtime_catalog() {
    let db = init_database_memory().await.unwrap();
    let options = serde_json::json!({"config_options":[{
        "id":"mode", "name":"Permission", "category":"mode", "type":"select", "current_value":"default",
        "options":[{"value":"default", "name":"Default"},{"value":"bypassPermissions", "name":"Full access"}]
    }]});
    sqlx::query("UPDATE agent_metadata SET config_options=? WHERE agent_id='2d23ff1c'")
        .bind(options.to_string())
        .execute(db.pool())
        .await
        .unwrap();
    let router = RouterService::new(
        Arc::new(SqliteProviderRepository::new(db.pool().clone())),
        Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone())),
        AgentRegistry::new(Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone()))),
        [7; 32],
    );
    let cli = RouterTarget::Agent {
        agent_id: "2d23ff1c".into(),
        model: "model".into(),
    };
    assert_eq!(
        router.permission("system_default_user", &cli, None).await.unwrap(),
        None
    );
    assert_eq!(
        router
            .permission("system_default_user", &cli, Some("default"))
            .await
            .unwrap(),
        Some("default".into())
    );
    assert!(
        router
            .permission("system_default_user", &cli, Some("auto_edit"))
            .await
            .is_err()
    );
    assert!(
        router
            .permission("system_default_user", &cli, Some("agent-full-access"))
            .await
            .is_err()
    );
    let native = RouterTarget::Provider {
        provider_id: "any".into(),
        model: "model".into(),
    };
    assert!(
        router
            .permission("system_default_user", &native, Some("bypassPermissions"))
            .await
            .is_err()
    );
    assert_eq!(
        router
            .permission("system_default_user", &native, Some("yolo"))
            .await
            .unwrap(),
        Some("yolo".into())
    );
}
