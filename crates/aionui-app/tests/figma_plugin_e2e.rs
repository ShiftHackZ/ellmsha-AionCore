mod common;
use aionui_api_types::FigmaVerifiedIdentity;
use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use aionui_extension::{
    ExtensionRegistry, ExtensionSource, ExtensionStateStore, FigmaConnectionManager, FigmaConnectionProbe,
    FigmaProbeFailure, ScanPath,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{body_json, get_with_token, json_with_token, setup_and_login};
use serde_json::json;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

struct Probe(AtomicUsize);
#[async_trait::async_trait]
impl FigmaConnectionProbe for Probe {
    async fn verify(&self, token: &str) -> Result<FigmaVerifiedIdentity, FigmaProbeFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let id = token
            .strip_prefix("fixture-token-")
            .ok_or(FigmaProbeFailure::Unauthorized)?;
        Ok(FigmaVerifiedIdentity {
            account_id: id.into(),
            account_name: id.into(),
            account_email: Some(format!("{id}@example.test")),
        })
    }
}

#[tokio::test]
async fn figma_routes_verify_two_accounts_enforce_auth_csrf_and_owner_and_never_expose_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let services = AppServices::from_config(
        aionui_db::init_database_memory().await.unwrap(),
        &AppConfig {
            data_dir: temp.path().into(),
            work_dir: temp.path().into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let probe = Arc::new(Probe(AtomicUsize::new(0)));
    let manager = FigmaConnectionManager::new(temp.path(), [7; 32], "/fixture/aioncore".into(), probe.clone()).unwrap();
    let registry = ExtensionRegistry::new(
        ExtensionStateStore::new(temp.path().join("extension-states.json")),
        services.event_bus.clone(),
        services.app_version.clone(),
    )
    .with_figma_connections(Some(manager));
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../plugins"),
            source: ExtensionSource::Builtin,
        }])
        .await
        .unwrap();
    assert!(
        registry
            .get_loaded_extensions()
            .await
            .iter()
            .any(|extension| extension.name == "figma")
    );
    let (mut states, _) = build_module_states(&services).await.unwrap();
    states.extension.registry = registry.clone();
    let mut app = create_router_with_states(&services, states);
    let base = "/api/plugins/figma/connections";
    let response = app
        .clone()
        .oneshot(Request::builder().uri(base).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let (token, csrf) = setup_and_login(&mut app, &services, "owner", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(base)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"personal_access_token":"fixture-token-one"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            base,
            json!({"personal_access_token":"fixture-token-one", "expected_user_id":"foreign-user"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    let owner = services.user_repo.find_by_username("owner").await.unwrap().unwrap();
    let mut ids = Vec::new();
    for id in ["one", "two"] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                base,
                json!({"personal_access_token":format!("fixture-token-{id}")}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = body_json(response).await;
        assert!(!body.to_string().contains("fixture-token"));
        ids.push(body["data"]["id"].as_str().unwrap().to_owned());
        assert_eq!(body["data"]["identity"]["account_id"], id);
    }
    let snapshot = registry.runtime_snapshot(&owner.id, None).await;
    assert_eq!(
        snapshot
            .mcp_servers
            .iter()
            .filter(|server| server.id.starts_with("plugin:figma:"))
            .count(),
        2
    );
    let generation = snapshot.linear_revision;
    assert!(snapshot.skills.iter().any(|skill| skill.name.contains("figma")));
    assert!(
        registry
            .runtime_snapshot("foreign", None)
            .await
            .mcp_servers
            .iter()
            .all(|server| !server.id.starts_with("plugin:figma:"))
    );
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            base,
            json!({"personal_access_token":"fixture-token-one"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            base,
            json!({"personal_access_token":"fixture-token-three","unexpected":true}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let (foreign_token, foreign_csrf) = setup_and_login(&mut app, &services, "foreign", "StrongP@ss1").await;
    let response = app.clone().oneshot(get_with_token(base, &foreign_token)).await.unwrap();
    assert_eq!(body_json(response).await["data"]["connections"], json!([]));
    let calls = probe.0.load(Ordering::SeqCst);
    for operation in ["connect", "disconnect"] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                &format!("{base}/{}/{operation}", ids[0]),
                json!({"personal_access_token":"fixture-token-one"}),
                &foreign_token,
                &foreign_csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(probe.0.load(Ordering::SeqCst), calls);
    let response = app.clone().oneshot(get_with_token(base, &token)).await.unwrap();
    let body = body_json(response).await;
    assert_eq!(body["data"]["connections"].as_array().unwrap().len(), 2);
    assert!(!body.to_string().contains("encrypted"));
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("{base}/{}/disconnect", ids[0]),
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"]["status"], "disconnected");
    let snapshot = registry.runtime_snapshot(&owner.id, None).await;
    assert_ne!(snapshot.linear_revision, generation);
    assert_eq!(
        snapshot
            .mcp_servers
            .iter()
            .filter(|server| server.id.starts_with("plugin:figma:"))
            .count(),
        1
    );
    registry
        .disable_extension_for_user(&owner.id, "figma", None)
        .await
        .unwrap();
    assert!(
        registry
            .runtime_snapshot(&owner.id, None)
            .await
            .mcp_servers
            .iter()
            .all(|server| !server.id.starts_with("plugin:figma:"))
    );
}
