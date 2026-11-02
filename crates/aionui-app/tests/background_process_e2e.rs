mod common;
use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{body_json, build_app_with_data_dir, get_with_token, json_with_token, setup_and_login};
use serde_json::json;
use tower::ServiceExt;

#[cfg(unix)]
#[tokio::test]
async fn terminal_http_enforces_auth_csrf_owner_and_runtime_conversation_scope() {
    let directory = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(directory.path()).await;
    let (jwt, csrf) = setup_and_login(&mut app, &services, "terminal-owner", "StrongP@ss1").await;
    let owner = services
        .user_repo
        .find_by_username("terminal-owner")
        .await
        .unwrap()
        .unwrap()
        .id;
    for (id, user) in [
        ("first", owner.as_str()),
        ("second", owner.as_str()),
        ("foreign", "system_default_user"),
    ] {
        sqlx::query("INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at) VALUES (?,?,'Fixture','acp',?,'finished',1,1)")
            .bind(id).bind(user).bind(json!({"workspace":directory.path()}).to_string()).execute(services.database.pool()).await.unwrap();
    }
    let base = "/api/conversations/first/background-processes";
    assert_eq!(
        app.clone()
            .oneshot(Request::builder().uri(base).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let no_csrf = Request::builder()
        .method("POST")
        .uri(base)
        .header("authorization", format!("Bearer {jwt}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"command":"echo forbidden"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(no_csrf).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    for (route, payload, status) in [
        ("foreign", json!({"command":"echo forbidden"}), StatusCode::NOT_FOUND),
        ("first", json!({"command":"bad\0command"}), StatusCode::BAD_REQUEST),
        (
            "first",
            json!({"command":"echo forbidden", "cwd":".."}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                &format!("/api/conversations/{route}/background-processes"),
                payload,
                &jwt,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    let issued = services.runtime_token_service.issue(
        &owner,
        "first",
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );
    let runtime_request = |route: &str, conversation_header: &str| {
        Request::builder()
            .uri(route)
            .header("x-aionui-user-id", &owner)
            .header("x-aionui-conversation-id", conversation_header)
            .header("x-aionui-runtime-token", &issued.token)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(runtime_request(base, "first"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(runtime_request(
                "/api/conversations/second/background-processes",
                "first"
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(runtime_request(base, "second"))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let started = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            base,
            json!({"command":"printf fixture; sleep 30", "title":"Fixture"}),
            &jwt,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let data = body_json(started).await;
    let id = data["data"]["id"].as_str().unwrap();
    let read = app
        .clone()
        .oneshot(get_with_token(&format!("{base}/{id}"), &jwt))
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    let foreign = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/conversations/second/background-processes/{id}"),
            &jwt,
        ))
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    // New terminal operations use the same auth/CSRF/runtime-scope boundary.
    let shell = app
        .clone()
        .oneshot(json_with_token("POST", base, json!({}), &jwt, &csrf))
        .await
        .unwrap();
    assert_eq!(shell.status(), StatusCode::OK);
    let shell = body_json(shell).await;
    let sid = shell["data"]["id"].as_str().unwrap();
    let input_path = format!("{base}/{sid}/input");
    let human = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &input_path,
            json!({"data":"printf 'HUMAN_%s\\n' INPUT\r"}),
            &jwt,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(human.status(), StatusCode::OK);
    let human = body_json(human).await;
    let revision = human["data"]["input_revision"].as_u64().unwrap();
    let agent = Request::builder()
        .method("POST")
        .uri(&input_path)
        .header("content-type", "application/json")
        .header("x-aionui-user-id", &owner)
        .header("x-aionui-conversation-id", "first")
        .header("x-aionui-runtime-token", &issued.token)
        .body(Body::from(
            json!({"data":"bad","expected_revision":revision,"actor":"human"}).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(agent).await.unwrap().status(),
        StatusCode::BAD_REQUEST,
        "runtime cannot impersonate a human to bypass the typing lease"
    );
    for operation in ["input", "resize", "wait"] {
        let path = format!("/api/conversations/second/background-processes/{sid}/{operation}");
        let payload = match operation {
            "input" => json!({"data":"bad"}),
            "resize" => json!({"rows":24,"cols":80}),
            _ => json!({"text":"bad","timeout_ms":0}),
        };
        assert_eq!(
            app.clone()
                .oneshot(json_with_token("POST", &path, payload, &jwt, &csrf))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let output = app
        .clone()
        .oneshot(runtime_request(&format!("{base}/{sid}/output"), "first"))
        .await
        .unwrap();
    assert_eq!(output.status(), StatusCode::OK);
    assert!(body_json(output).await["data"]["data"].is_string());
    services
        .background_process_service
        .stop(&owner, "first", sid)
        .await
        .unwrap();
    let stopped = app
        .oneshot(json_with_token(
            "POST",
            &format!("{base}/{id}/stop"),
            json!({}),
            &jwt,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(stopped.status(), StatusCode::OK);
    assert_eq!(body_json(stopped).await["data"]["running"], false);
    services.background_process_service.shutdown().await;
    services.database.close().await;
}
