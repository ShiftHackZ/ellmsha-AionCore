//! Catalog/assistant projection for the desktop's provider-backed native agent.
mod common;
use axum::http::StatusCode;
use common::{body_json, build_app_with_data_dir, get_with_token, json_with_token, setup_and_login};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn copilot_is_a_separate_agent_and_generated_chat_choice() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "TestPass123!").await;
    let response = app
        .clone()
        .oneshot(get_with_token("/api/agents/management", &token))
        .await
        .unwrap();
    let body = body_json(response).await;
    let row = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "copilot-ide")
        .unwrap();
    assert_eq!(row["name"], "Copilot IDE");
    assert_eq!(row["enabled"], false);
    assert_eq!(row["icon"], "/api/assets/logos/tools/copilot.svg");
    let asset = app
        .clone()
        .oneshot(get_with_token("/api/assets/logos/tools/copilot.svg", &token))
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PATCH",
            "/api/agents/copilot-ide/enabled",
            json!({"enabled": true}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/assistants", &token))
        .await
        .unwrap();
    let body = body_json(response).await;
    let row = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "bare:copilot-ide")
        .unwrap();
    assert_eq!(row["agent_id"], "copilot-ide");
    assert_eq!(row["name"], "Copilot IDE");
    assert_eq!(row["agent"]["acp_backend"], "aionrs");
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({"extra": {}, "name": "Copilot isolation", "assistant": {"id": "bare:copilot-ide"},
            "model": {"provider_id": "other-provider", "model": "test-model"}}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = body_json(response).await;
    assert!(error.to_string().contains("Copilot IDE requires"), "{error}");
    let native = aionui_db::resolve_agent_binding(services.database.pool(), "aionrs")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(native.agent_id, "copilot-ide");
}
