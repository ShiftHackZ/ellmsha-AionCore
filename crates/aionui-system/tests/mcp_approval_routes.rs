use aionui_auth::CurrentUser;
use aionui_common::mcp_approvals::McpApprovalStore;
use aionui_system::mcp_approval_routes;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn request(method: &str, path: &str, user: &str) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
    let mut principal = CurrentUser::local_default();
    principal.id = user.into();
    request.extensions_mut().insert(principal);
    request
}

#[tokio::test]
async fn list_and_revoke_are_scoped_to_authenticated_user_and_hide_bindings() {
    let temporary = tempfile::tempdir().unwrap();
    let dir = temporary.path().to_path_buf();
    let store = McpApprovalStore::new(&dir, "owner");
    store
        .grant("fixture-account", "private-transport-digest", "read")
        .unwrap();
    let id = store.list().unwrap()[0].id.clone();
    let app = mcp_approval_routes(dir.clone());
    let response = app
        .clone()
        .oneshot(request("GET", "/api/settings/mcp-approvals", "owner"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("fixture-account"));
    assert!(!text.contains("private-transport-digest"));
    let response = app
        .clone()
        .oneshot(request("DELETE", &format!("/api/settings/mcp-approvals/{id}"), "other"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        store
            .allows("fixture-account", "private-transport-digest", "read")
            .unwrap()
    );
    let response = app
        .oneshot(request("DELETE", &format!("/api/settings/mcp-approvals/{id}"), "owner"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !store
            .allows("fixture-account", "private-transport-digest", "read")
            .unwrap()
    );
}

#[tokio::test]
async fn invalid_ids_reject_and_missing_grants_are_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    let app = mcp_approval_routes(temporary.path().to_path_buf());
    let invalid = app
        .clone()
        .oneshot(request("DELETE", "/api/settings/mcp-approvals/not-an-id", "owner"))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let missing = app
        .oneshot(request(
            "DELETE",
            &format!("/api/settings/mcp-approvals/{}", "0".repeat(64)),
            "owner",
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::OK);
    let body = missing.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["data"]["revoked"],
        false
    );
}
