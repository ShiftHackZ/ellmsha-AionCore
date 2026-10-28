mod common;

use aionui_api_types::{NotionVerifiedCapabilities, NotionVerifiedIdentity};
use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use aionui_extension::{
    ExtensionRegistry, ExtensionSource, ExtensionStateStore, NotionConnectionManager, NotionConnectionProbe,
    NotionProbeFailure, NotionProbeResult, ScanPath,
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

struct IdentityProbe(AtomicUsize);
#[async_trait::async_trait]
impl NotionConnectionProbe for IdentityProbe {
    async fn verify(&self, _: &str, _: &str, auth_dir: &Path) -> Result<NotionProbeResult, NotionProbeFailure> {
        let id = self.0.fetch_add(1, Ordering::SeqCst);
        std::fs::write(auth_dir.join("oauth-cache"), "private-fixture-token").unwrap();
        Ok(NotionProbeResult {
            identity: NotionVerifiedIdentity {
                account_id: format!("person-{id}"),
                account_name: format!("Person {id}"),
                account_email: Some(format!("person-{id}@example.test")),
                workspace_id: format!("workspace-{id}"),
                workspace_name: Some(format!("Workspace {id}")),
            },
            capabilities: NotionVerifiedCapabilities {
                search: true,
                read: true,
                write: true,
            },
        })
    }
}

async fn fixture(root: &Path) -> (axum::Router, AppServices, Arc<IdentityProbe>) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(
        db,
        &AppConfig {
            data_dir: root.to_path_buf(),
            work_dir: root.to_path_buf(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let probe = Arc::new(IdentityProbe(AtomicUsize::new(0)));
    let manager = NotionConnectionManager::new(root, probe.clone()).unwrap();
    let registry = ExtensionRegistry::new_with_account_connections(
        ExtensionStateStore::new(root.join("extension-states.json")),
        services.event_bus.clone(),
        services.app_version.clone(),
        None,
        None,
        Some(manager),
    );
    // Load the real bundled plugin, not an invented manifest.
    let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../plugins");
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: plugins,
            source: ExtensionSource::Builtin,
        }])
        .await
        .unwrap();
    let (mut states, _) = build_module_states(&services).await.unwrap();
    states.extension.registry = registry;
    (create_router_with_states(&services, states), services, probe)
}

#[tokio::test]
async fn notion_routes_enforce_auth_csrf_and_local_user_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let (mut app, services, probe) = fixture(temp.path()).await;
    let base = "/api/plugins/notion/connections";
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
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        probe.0.load(Ordering::SeqCst),
        0,
        "rejected requests must not initiate OAuth"
    );

    let response = app
        .clone()
        .oneshot(json_with_token("POST", base, json!({}), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let first = body_json(response).await;
    let id = first["data"]["id"].as_str().unwrap();
    assert_eq!(first["data"]["identity"]["workspace_id"], "workspace-0");
    let public = first.to_string();
    for secret in [
        "private-fixture-token",
        "MCP_REMOTE_CONFIG_DIR",
        "plugin-auth",
        "access_token",
        "refresh_token",
    ] {
        assert!(!public.contains(secret));
    }
    let (other, other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;
    let response = app.clone().oneshot(get_with_token(base, &other)).await.unwrap();
    assert_eq!(body_json(response).await["data"]["connections"], json!([]));
    for action in ["connect", "disconnect"] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                &format!("{base}/{id}/{action}"),
                json!({"reconnect":true}),
                &other,
                &other_csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(probe.0.load(Ordering::SeqCst), 1);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/extensions/mcp-servers", &token))
        .await
        .unwrap();
    let technical = body_json(response).await;
    let server = technical["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["_extensionName"] == "notion")
        .expect("Notion Tools entry");
    assert_eq!(server["transport"]["env"]["MCP_REMOTE_CONFIG_DIR"], "<private>");
    assert!(
        !technical
            .to_string()
            .contains(&temp.path().to_string_lossy().to_string())
    );

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("{base}/{id}/connect"),
            json!({"reconnect":true}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let next = body_json(response).await;
    assert_eq!(next["data"]["id"], id);
    assert_eq!(next["data"]["identity"]["workspace_id"], "workspace-1");
    assert_ne!(
        next["data"]["namespace"], first["data"]["namespace"],
        "a changed user/workspace must not advertise the old account namespace"
    );
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("{base}/{id}/disconnect"),
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"]["status"], "disconnected");
    let response = app
        .oneshot(get_with_token("/api/extensions/mcp-servers", &token))
        .await
        .unwrap();
    assert!(
        !body_json(response).await["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|server| server["_extensionName"] == "notion")
    );
}
