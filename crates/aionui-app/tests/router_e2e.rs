//! Real fresh migrations and HTTP projection, with an isolated data directory.
mod common;
use axum::http::StatusCode;
use common::{body_json, build_app_with_data_dir, get_with_token, setup_and_login};
use tower::ServiceExt;

#[tokio::test]
async fn fresh_router_migration_projects_generated_assistant_without_hijacking_native() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(tmp.path()).await;
    let (token, _) = setup_and_login(&mut app, &services, "admin", "TestPass123!").await;
    let explicit = aionui_db::resolve_agent_binding(services.database.pool(), "auto-router")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(explicit.runtime_backend, "auto-router");
    let native = aionui_db::resolve_agent_binding(services.database.pool(), "aionrs")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(native.runtime_backend, "aionrs");
    assert_ne!(native.agent_id, "auto-router");
    let response = app
        .clone()
        .oneshot(get_with_token("/api/agents/management", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let router = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "auto-router")
        .unwrap();
    assert_eq!(router["installed"], true);
    assert_eq!(router["enabled"], true);
    let response = app
        .clone()
        .oneshot(get_with_token("/api/assistants", &token))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let assistants = body["data"].as_array().unwrap();
    let router = assistants.iter().find(|r| r["id"] == "bare:auto-router").unwrap();
    assert_eq!(router["agent_id"], "auto-router");
    assert_eq!(router["agent"]["acp_backend"], "auto-router");
    let native_id = format!("bare:{}", native.agent_id);
    let native = assistants.iter().find(|r| r["id"] == native_id).unwrap();
    assert_ne!(native["agent"]["acp_backend"], "auto-router");
}

#[tokio::test]
async fn projectless_router_freezes_one_real_workspace_and_preserves_custom_validation() {
    use common::{delete_with_token, json_with_token};
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "TestPass123!").await;
    assert_eq!(
        app.clone()
            .oneshot(get_with_token("/api/assistants", &token))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/providers",
            json!({"id":"router-local","platform":"ollama","name":"Local fixture",
                "base_url":"http://127.0.0.1:9","api_key":"local-fixture","models":["qwen3.5:35b"]}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED, "{}", body_json(response).await);
    let target = json!({"kind":"provider","provider_id":"router-local","model":"qwen3.5:35b"});
    let config = json!({"mode":"manual","local_only":true,"root_target":target,"child_target":target,
        "hints":[{"target":target,"local_inference":true}]});
    let custom = tmp.path().join("custom-project");
    std::fs::create_dir(&custom).unwrap();
    for supplied in [None, Some(String::new()), Some(custom.to_string_lossy().into_owned())] {
        let mut extra = json!({});
        if let Some(workspace) = &supplied {
            extra["workspace"] = json!(workspace);
        }
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/conversations",
                json!({"assistant":{"id":"bare:auto-router"},"extra":extra,
                    "routing":{"initial_prompt":"Return a short greeting","config":config}}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        let status = response.status();
        let body = body_json(response).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let conversation = &body["data"];
        let id = conversation["id"].as_str().unwrap();
        let workspace = std::path::PathBuf::from(conversation["extra"]["workspace"].as_str().unwrap());
        assert!(workspace.is_absolute() && workspace.is_dir());
        assert_eq!(
            conversation["extra"]["routing"]["decision"]["scope_workspace"],
            json!(workspace)
        );
        let auto = supplied.as_deref().is_none_or(str::is_empty);
        if auto {
            assert_eq!(
                workspace.file_name().unwrap(),
                format!("auto-router-temp-{id}").as_str()
            );
            assert_eq!(std::fs::read_dir(workspace.parent().unwrap()).unwrap().count(), 1);
        } else {
            assert_eq!(
                std::fs::canonicalize(&workspace).unwrap(),
                std::fs::canonicalize(&custom).unwrap()
            );
        }
        assert_eq!(
            app.clone()
                .oneshot(delete_with_token(&format!("/api/conversations/{id}"), &token, &csrf))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(workspace.exists(), !auto);
    }
    let file = tmp.path().join("not-a-directory");
    std::fs::write(&file, "fixture").unwrap();
    for invalid in [tmp.path().join("missing"), file] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/conversations",
                json!({"assistant":{"id":"bare:auto-router"},"extra":{"workspace":invalid},
                    "routing":{"initial_prompt":"Return a greeting","config":config}}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{}",
            body_json(response).await
        );
    }
}

#[tokio::test]
async fn custom_router_team_assistant_uses_task_and_ordinary_member_stays_unrouted() {
    use common::json_with_token;
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "TestPass123!").await;
    let target = json!({"kind":"provider","provider_id":"router-local","model":"qwen3.5:35b"});
    let config = json!({"mode":"manual","local_only":true,"root_target":target,"child_target":target,
        "hints":[{"target":target,"local_inference":true}]});
    for (method, uri, payload) in [
        (
            "POST",
            "/api/providers",
            json!({"id":"router-local","platform":"ollama","name":"Local fixture",
            "base_url":"http://127.0.0.1:9","api_key":"local-fixture","models":["qwen3.5:35b"]}),
        ),
        (
            "POST",
            "/api/assistants",
            json!({"id":"custom-router","name":"My router","agent_id":"auto-router"}),
        ),
        ("PUT", "/api/settings/client", json!({"router.config":config})),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_token(method, uri, payload, &token, &csrf))
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", body_json(response).await);
    }
    let ordinary_request = serde_json::from_value(json!({
        "type":"aionrs","extra":{"workspace":tmp.path()},
        "model":{"provider_id":"router-local","model":"qwen3.5:35b"}
    }))
    .unwrap();
    let ordinary = services
        .conversation_service
        .create_team_routed("system_default_user", ordinary_request, None, None)
        .await
        .unwrap();
    assert!(ordinary.extra.get("routing").is_none());
    let custom_request = || {
        serde_json::from_value(json!({
            "assistant":{"id":"custom-router"},"extra":{"workspace":tmp.path()}
        }))
        .unwrap()
    };
    for parent in [None, Some(ordinary.id.as_str())] {
        let routed = services
            .conversation_service
            .create_team_routed(
                "system_default_user",
                custom_request(),
                parent,
                Some("Review a small fixture"),
            )
            .await
            .unwrap();
        assert_eq!(routed.extra["routing"]["decision"]["target"], target);
        assert_eq!(
            routed.extra["routing"]["decision"]["scope_workspace"],
            routed.extra["workspace"]
        );
    }
    assert!(
        services
            .conversation_service
            .create_team_routed("system_default_user", custom_request(), None, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn routed_team_child_keeps_source_assistant_preferences_and_ordinary_reuse() {
    use common::json_with_token;
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "TestPass123!").await;
    let claude = aionui_db::resolve_agent_binding(services.database.pool(), "claude")
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE agent_metadata SET available_models = ? WHERE id = ?")
        .bind(json!({"available_models":[{"id":"claude-fixture"},{"id":"claude-next"}]}).to_string())
        .bind(&claude.agent_id)
        .execute(services.database.pool())
        .await
        .unwrap();
    let target = json!({"kind":"provider","provider_id":"router-local","model":"qwen3.5:35b"});
    let config = json!({"mode":"manual","local_only":true,"root_target":target,"child_target":target,
        "hints":[{"target":target,"local_inference":true}]});
    for (method, uri, payload) in [
        (
            "POST",
            "/api/providers",
            json!({"id":"router-local","platform":"ollama","name":"Local fixture",
            "base_url":"http://127.0.0.1:9","api_key":"local-fixture","models":["qwen3.5:35b"]}),
        ),
        (
            "POST",
            "/api/assistants",
            json!({"id":"source-assistant","name":"Source assistant","agent_id":claude.agent_id}),
        ),
        (
            "POST",
            "/api/assistants",
            json!({"id":"routing-lead","name":"Routing lead","agent_id":"auto-router"}),
        ),
        ("PUT", "/api/settings/client", json!({"router.config":config})),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_token(method, uri, payload, &token, &csrf))
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", body_json(response).await);
    }
    let request = |overrides: serde_json::Value| {
        serde_json::from_value(json!({
            "assistant":{"id":"source-assistant","conversation_overrides":overrides},
            "extra":{"workspace":tmp.path()}
        }))
        .unwrap()
    };
    let ordinary = services
        .conversation_service
        .create(
            "system_default_user",
            request(json!({
                "model":"claude-fixture","permission":"default","thought_level":"low"
            })),
        )
        .await
        .unwrap();
    assert!(ordinary.extra.get("routing").is_none());
    let preference_query = "SELECT p.last_model_id, p.last_permission_value, p.last_thought_level_value, p.last_skill_ids, p.last_disabled_builtin_skill_ids, p.last_mcp_ids, p.updated_at FROM assistant_preferences p JOIN assistant_definitions d ON d.id = p.assistant_definition_id WHERE p.user_id = 'system_default_user' AND d.assistant_id = 'source-assistant'";
    type Preference = (
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        i64,
    );
    let before: Preference = sqlx::query_as(preference_query)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(before.0.as_deref(), Some("claude-fixture"));
    let lead_request =
        serde_json::from_value(json!({"assistant":{"id":"routing-lead"},"extra":{"workspace":tmp.path()}})).unwrap();
    let lead = services
        .conversation_service
        .create_team_routed(
            "system_default_user",
            lead_request,
            None,
            Some("Coordinate fixture review"),
        )
        .await
        .unwrap();
    let child = services
        .conversation_service
        .create_team_routed(
            "system_default_user",
            request(json!({})),
            Some(&lead.id),
            Some("Review fixture independently"),
        )
        .await
        .unwrap();
    assert_eq!(child.extra["routing"]["decision"]["target"], target);
    assert_eq!(child.model.as_ref().unwrap().model, "qwen3.5:35b");
    let after: Preference = sqlx::query_as(preference_query)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "routed execution must not write source assistant preferences"
    );
    let reused = services
        .conversation_service
        .create("system_default_user", request(json!({})))
        .await
        .unwrap();
    assert!(reused.extra.get("routing").is_none());
    assert_eq!(reused.extra["current_model_id"], "claude-fixture");
    services
        .conversation_service
        .create("system_default_user", request(json!({"model":"claude-next"})))
        .await
        .unwrap();
    let changed: Preference = sqlx::query_as(preference_query)
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(
        changed.0.as_deref(),
        Some("claude-next"),
        "ordinary creation still remembers model choices"
    );
}

#[tokio::test]
async fn router_settings_validate_owned_available_references_before_any_write() {
    use common::json_with_token;
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "TestPass123!").await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "other", "OtherPass123!").await;
    for (id, auth, anti_csrf, enabled) in [
        ("owned", &token, &csrf, true),
        ("disabled", &token, &csrf, false),
        ("foreign", &other_token, &other_csrf, true),
    ] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/providers",
                json!({
                    "id":id,"platform":"ollama","name":id,"base_url":"http://127.0.0.1:9","api_key":"fixture",
                    "models":["fixture-model"],"enabled":enabled
                }),
                auth,
                anti_csrf,
            ))
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", body_json(response).await);
    }
    let owned = json!({"kind":"provider","provider_id":"owned","model":"fixture-model"});
    let foreign = json!({"kind":"provider","provider_id":"foreign","model":"fixture-model"});
    let valid = json!({"mode":"manual","root_target":owned,"child_target":owned});
    let response = app
        .clone()
        .oneshot(json_with_token(
            "PUT",
            "/api/settings/client",
            json!({"router.config":valid,"theme":"dark"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{}", body_json(response).await);
    let mut invalid = Vec::new();
    for field in ["root_target", "child_target", "classifier_target"] {
        let mut config = valid.clone();
        config[field] = foreign.clone();
        invalid.push(config);
    }
    for (field, value) in [
        ("allowed_targets", json!([foreign])),
        ("hints", json!([{"target":foreign}])),
        (
            "root_target",
            json!({"kind":"provider","provider_id":"disabled","model":"fixture-model"}),
        ),
        (
            "root_target",
            json!({"kind":"agent","agent_id":"missing-agent","model":"fixture-model"}),
        ),
    ] {
        let mut config = valid.clone();
        config[field] = value;
        invalid.push(config);
    }
    for config in invalid {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "PUT",
                "/api/settings/client",
                json!({"router.config":config,"theme":"must-not-write"}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{}",
            body_json(response).await
        );
        let saved = body_json(
            app.clone()
                .oneshot(get_with_token("/api/settings/client", &token))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(saved["data"]["router.config"], valid);
        assert_eq!(saved["data"]["theme"], "dark");
    }
}

#[tokio::test]
async fn outcome_requires_owned_completed_response_user_csrf_and_idle_runtime() {
    use common::json_with_token;
    use serde_json::json;
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_data_dir(tmp.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "TestPass123!").await;
    let (other, other_csrf) = setup_and_login(&mut app, &services, "other", "OtherPass123!").await;
    let snapshot = aionui_api_types::RouterSnapshot {
        config: Default::default(),
        decision: aionui_api_types::RouterDecision {
            id: "decision".into(),
            target: aionui_api_types::RouterTarget::Provider {
                provider_id: "owned".into(),
                model: "model".into(),
            },
            requirements: Default::default(),
            reasons: vec![],
            evidence_count: 0,
            elapsed_ms: 0,
            managed_children: true,
            scope_workspace: "frozen-scope".into(),
        },
    };
    let extra = json!({"routing":snapshot,"workspace":"moved-after-selection"});
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,status,created_at,updated_at) VALUES('review','system_default_user','Review','aionrs',?,'finished',1,1)")
        .bind(extra.to_string()).execute(services.database.pool()).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,created_at) VALUES('answer','review','text','{}','left','finish',1)")
        .execute(services.database.pool()).await.unwrap();
    let uri = "/api/conversations/review/routing/outcome";
    let review = json!({"accepted":true,"message_id":"answer"});
    assert_eq!(
        app.clone()
            .oneshot(json_with_token("POST", uri, review.clone(), &other, &other_csrf))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let mut unauth = json_with_token("POST", uri, review.clone(), &token, &csrf);
    unauth.headers_mut().remove("authorization");
    assert_eq!(
        app.clone().oneshot(unauth).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let mut no_csrf = json_with_token("POST", uri, review.clone(), &token, &csrf);
    no_csrf.headers_mut().remove("x-csrf-token");
    assert_eq!(
        app.clone().oneshot(no_csrf).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut helper = json_with_token("POST", uri, review.clone(), &token, &csrf);
    helper
        .headers_mut()
        .insert("x-aionui-runtime-token", "helper".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(helper).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let claim = services
        .conversation_runtime_state
        .try_claim_turn("review", "new-turn")
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(json_with_token("POST", uri, review.clone(), &token, &csrf))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    drop(claim);
    assert_eq!(
        app.clone()
            .oneshot(json_with_token(
                "POST",
                uri,
                json!({"accepted":true,"message_id":"missing"}),
                &token,
                &csrf
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    for column in ["status='error'", "position='right'", "hidden=1"] {
        sqlx::query("UPDATE messages SET status='finish',position='left',hidden=0 WHERE id='answer'")
            .execute(services.database.pool())
            .await
            .unwrap();
        sqlx::query(&format!("UPDATE messages SET {column} WHERE id='answer'"))
            .execute(services.database.pool())
            .await
            .unwrap();
        assert_eq!(
            app.clone()
                .oneshot(json_with_token("POST", uri, review.clone(), &token, &csrf))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    sqlx::query("UPDATE messages SET status='finish',position='left',hidden=0 WHERE id='answer'")
        .execute(services.database.pool())
        .await
        .unwrap();
    let mut transcript = extra.clone();
    transcript["subagent"] = json!({"parent":"other"});
    sqlx::query("UPDATE conversations SET extra=? WHERE id='review'")
        .bind(transcript.to_string())
        .execute(services.database.pool())
        .await
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(json_with_token("POST", uri, review.clone(), &token, &csrf))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE conversations SET extra=? WHERE id='review'")
        .bind(extra.to_string())
        .execute(services.database.pool())
        .await
        .unwrap();
    for accepted in [true, false] {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                uri,
                json!({"accepted":accepted,"message_id":"answer"}),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["data"]["source"], "explicit_user_review");
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM router_outcomes")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    let scope: String = sqlx::query_scalar("SELECT workspace FROM router_outcomes")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(scope, "frozen-scope");
}

#[tokio::test]
async fn embedded_desktop_review_uses_real_local_identity_without_fabricated_cookies() {
    use aionui_app::{AppConfig, AppServices, create_router};
    use axum::{body::Body, http::Request};
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let database = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        local: true,
        data_dir: dir.path().into(),
        work_dir: dir.path().into(),
        ..Default::default()
    };
    let services = AppServices::from_config(database, &config).await.unwrap();
    let app = create_router(&services).await.unwrap();
    let status = app
        .clone()
        .oneshot(Request::builder().uri("/api/auth/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert!(!status.headers().contains_key("set-cookie"));
    let extra = json!({"routing":{
        "config":{},"decision":{
            "id":"local-decision","target":{"kind":"provider","provider_id":"local","model":"fixture"},
            "requirements":{},"reasons":[],"evidence_count":0,"elapsed_ms":0,"managed_children":true,"scope_workspace":"local-scope"
        }
    }});
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,status,created_at,updated_at) VALUES('local-review','system_default_user','Local review','aionrs',?,'finished',1,1)")
        .bind(extra.to_string()).execute(services.database.pool()).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,created_at) VALUES('answer','local-review','text','{\"content\":\"Reviewed answer\"}','left','finish',1)")
        .execute(services.database.pool()).await.unwrap();
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/api/conversations/local-review/routing/outcome")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"accepted":true,"message_id":"answer"}"#))
            .unwrap()
    };
    let response = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"]["decision_id"], "local-decision");
    for header in ["x-aionui-runtime-token", "x-aionui-user-id", "x-aionui-conversation-id"] {
        let mut helper = request();
        helper.headers_mut().insert(header, "helper".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(helper).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}
