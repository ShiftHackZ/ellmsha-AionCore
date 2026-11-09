//! Disposable owner/scope fixtures; no provider, model or production profile access.
use aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION;
use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
const OWNER: &str = "system_default_user";
const HUMAN: &str = "synthetic-human-secrets-capability-00000000000000000000000000000000";
async fn response(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let result = app.clone().oneshot(request).await.unwrap();
    let status = result.status();
    assert_eq!(result.headers().get("cache-control").unwrap(), "no-store");
    let bytes = result.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
fn runtime(tool: &str, token: &str, chat: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/runtime/secrets/tools/{tool}"))
        .header("content-type", "application/json")
        .header("x-aionui-user-id", OWNER)
        .header("x-aionui-conversation-id", chat)
        .header("x-aionui-runtime-token", token)
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn human(path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/api/secrets/{path}"))
        .header("content-type", "application/json")
        .header("x-ellmsha-secrets-human", HUMAN)
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn record() -> Value {
    json!({"operation_id":uuid::Uuid::now_v7().to_string(),"record":{"title":"Synthetic QA","kind":"login","fields":{"password":{"kind":"password","value":"SYNTHETIC_RAW_SECRET"}},"bindings":[]}})
}
#[tokio::test]
async fn actual_routes_enforce_current_owner_project_vault_and_human_capability() {
    let dir = tempfile::tempdir().unwrap();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        local: true,
        data_dir: dir.path().into(),
        work_dir: dir.path().into(),
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let (states, _) = build_module_states(&services).await.unwrap();
    let registry = states.extension.registry.clone();
    let plugins = dir.path().join("plugins/secrets/skills/secrets");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(plugins.join("SKILL.md"),"---\nname: automation-secrets\ndescription: Synthetic fixture\n---\n# Synthetic Secrets fixture\nUse only disposable test credentials.").unwrap();
    std::fs::write(dir.path().join("plugins/secrets/aion-extension.json"),json!({"name":"secrets","version":"1.0.0","displayName":"Synthetic Secrets","description":"Local test fixture","permissions":{"storage":true,"shell":false,"filesystem":"extension-only","active_user":true},"contributes":{"skills":[{"name":"automation-secrets","description":"Synthetic fixture","path":"skills/secrets/SKILL.md"}]},"author":"Test","license":"MIT","apiVersion":"1.0.0"}).to_string()).unwrap();
    registry
        .initialize_with_scan_paths(vec![aionui_extension::ScanPath {
            path: dir.path().join("plugins"),
            source: aionui_extension::types::ExtensionSource::Builtin,
        }])
        .await
        .unwrap();
    registry.enable_extension_for_user(OWNER, "secrets").await.unwrap();
    for (id, kind) in [("p", "standard"), ("q", "standard"), ("temporary", "temp")] {
        sqlx::query("INSERT INTO projects(project_id,user_id,name,kind,created_at,updated_at) VALUES(?,?,?, ?,1,1)")
            .bind(id)
            .bind(OWNER)
            .bind(id)
            .bind(kind)
            .execute(services.database.pool())
            .await
            .unwrap();
    }
    for (id, project) in [("unbound", None), ("project", Some("p")), ("temp", Some("temporary"))] {
        sqlx::query("INSERT INTO conversations(id,user_id,name,type,project_id,extra,created_at,updated_at) VALUES(?,?,?,'acp',?,'{}',1,1)").bind(id).bind(OWNER).bind(id).bind(project).execute(services.database.pool()).await.unwrap();
    }
    let vault = services
        .secrets
        .initialize(OWNER, zeroize::Zeroizing::new("synthetic vault passphrase".into()))
        .await
        .unwrap();
    let token = |chat: &str, project: Option<&str>| {
        services
            .runtime_token_service
            .issue_secrets(
                OWNER,
                chat,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
                json!({"project":project,"vault":vault.vault_id,"incarnation":vault.incarnation}).to_string(),
            )
            .token
    };
    let unbound = token("unbound", None);
    let project = token("project", Some("p"));
    let temp = token("temp", Some("temporary"));
    // Encoded desktop-only operations cannot bypass the decoded operation guard in WebUI.
    let web = aionui_secrets::routes::domain_routes(states.secrets.clone())
        .layer(axum::Extension(aionui_auth::CurrentUser::local_default()))
        .layer(axum::Extension(aionui_secrets::state::HumanSecretsAccess));
    assert_eq!(
        response(&web, human("vault/%64esktop-export", json!({}))).await.0,
        StatusCode::FORBIDDEN
    );
    aionui_app::initialize_human_capability(HUMAN.into());
    let secrets_state = states.secrets.clone();
    let app = create_router_with_states(&services, states);
    for (chat, token) in [("unbound", &unbound), ("temp", &temp)] {
        for tool in [
            "ellmsha_project_secrets_list",
            "ellmsha_project_secret_read",
            "ellmsha_project_secret_write",
            "ellmsha_project_secret_delete",
        ] {
            assert_eq!(
                response(&app, runtime(tool, token, chat, json!({}))).await.0,
                StatusCode::FORBIDDEN
            );
        }
    }
    let (status, created) = response(
        &app,
        runtime("ellmsha_global_secret_write", &unbound, "unbound", record()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    for (chat, token) in [("unbound", &unbound), ("temp", &temp), ("project", &project)] {
        let (status, read) = response(
            &app,
            runtime("ellmsha_global_secret_read", token, chat, json!({"id":id})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(read["fields"]["password"]["value"], "SYNTHETIC_RAW_SECRET");
    }
    let (status, created) = response(
        &app,
        runtime("ellmsha_project_secret_write", &project, "project", record()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        response(
            &app,
            runtime(
                "ellmsha_global_secret_read",
                &project,
                "project",
                json!({"id":created["id"]})
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        response(
            &app,
            runtime(
                "ellmsha_project_secrets_list",
                &project,
                "project",
                json!({"scope":{"project_id":"q"}})
            )
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut forged = runtime("ellmsha_global_secrets_list", &unbound, "unbound", json!({}));
    forged
        .headers_mut()
        .insert("x-aionui-user-id", "foreign".parse().unwrap());
    assert_eq!(response(&app, forged).await.0, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE conversations SET project_id='q' WHERE id='project'")
        .execute(services.database.pool())
        .await
        .unwrap();
    assert_eq!(
        response(
            &app,
            runtime("ellmsha_project_secrets_list", &project, "project", json!({}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut omitted = human("records/read", json!({"scope":{"project_id":null},"id":id}));
    omitted.headers_mut().remove("x-ellmsha-secrets-human");
    assert_eq!(response(&app, omitted).await.0, StatusCode::UNAUTHORIZED);
    let mut helper = human("vault/desktop-export", json!({}));
    helper
        .headers_mut()
        .insert("x-aionui-runtime-token", unbound.parse().unwrap());
    assert_eq!(response(&app, helper).await.0, StatusCode::UNAUTHORIZED);

    // Every scoped operation uses the actual runtime route, not a mocked service dispatch.
    let fresh_project = token("project", Some("q"));
    for (prefix, chat, token) in [("global", "temp", &temp), ("project", "project", &fresh_project)] {
        let (_, created) = response(
            &app,
            runtime(&format!("ellmsha_{prefix}_secret_write"), token, chat, record()),
        )
        .await;
        let (id, revision) = (created["id"].clone(), created["revision"].clone());
        assert_eq!(
            response(
                &app,
                runtime(&format!("ellmsha_{prefix}_secrets_list"), token, chat, json!({}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            response(
                &app,
                runtime(&format!("ellmsha_{prefix}_secret_read"), token, chat, json!({"id":id}))
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            response(
                &app,
                runtime(
                    &format!("ellmsha_{prefix}_secret_delete"),
                    token,
                    chat,
                    json!({"id":id,"expected_revision":revision,"expected_incarnation":vault.incarnation,"operation_id":uuid::Uuid::now_v7().to_string()})
                )
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (_, visible_groups) = response(&app, human("records/groups", json!({}))).await;
    let visible = visible_groups["data"].as_array().unwrap();
    assert_eq!(visible.len(), 2);
    assert!(visible.iter().any(|g| g["scope"]["project_id"].is_null()));
    assert!(visible.iter().any(|g| g["scope"]["project_id"] == "q"));
    let (_, retained) = response(
        &app,
        runtime("ellmsha_project_secret_write", &fresh_project, "project", record()),
    )
    .await;
    sqlx::query("UPDATE conversations SET project_id=NULL WHERE project_id='q'")
        .execute(services.database.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM projects WHERE project_id='q'")
        .execute(services.database.pool())
        .await
        .unwrap();
    let (_, retained_groups) = response(&app, human("records/groups", json!({}))).await;
    assert!(
        retained_groups["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["scope"]["project_id"] != "q" && g["retained"] == false)
    );
    assert_eq!(
        response(
            &app,
            human("records/read", json!({"scope":{"project_id":"q"},"id":retained["id"]}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        response(
            &app,
            runtime(
                "ellmsha_project_secret_read",
                &fresh_project,
                "project",
                json!({"id":retained["id"]})
            )
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let retained_delete = json!({"scope":{"project_id":"q"},"id":retained["id"],"expected_revision":retained["revision"],"expected_incarnation":vault.incarnation,"operation_id":uuid::Uuid::now_v7().to_string()});
    let first = response(&app, human("records/delete", retained_delete.clone())).await;
    assert_eq!(first.0, StatusCode::OK);
    // Simulate response loss after deleting the last retained-project record.
    assert_eq!(
        response(&app, human("records/delete", retained_delete.clone())).await,
        first
    );
    let mut alternate = retained_delete.clone();
    alternate["operation_id"] = json!(retained_delete["operation_id"].as_str().unwrap().to_uppercase());
    assert_eq!(response(&app, human("records/delete", alternate)).await, first);
    for field in ["id", "expected_incarnation"] {
        let mut changed = retained_delete.clone();
        changed[field] = json!(uuid::Uuid::now_v7().to_string());
        assert_eq!(
            response(&app, human("records/delete", changed)).await.0,
            StatusCode::CONFLICT
        );
    }
    let mut changed_revision = retained_delete.clone();
    changed_revision["expected_revision"] = json!(999);
    assert_eq!(
        response(&app, human("records/delete", changed_revision)).await.0,
        StatusCode::CONFLICT
    );
    let mut new_operation = retained_delete.clone();
    new_operation["operation_id"] = json!(uuid::Uuid::now_v7().to_string());
    assert_eq!(
        response(&app, human("records/delete", new_operation)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        response(
            &app,
            human("records/read", json!({"scope":{"project_id":"q"},"id":retained["id"]}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let mut chat_retry = retained_delete.clone();
    chat_retry["conversation_id"] = json!("unbound");
    assert_eq!(
        response(&app, human("records/delete", chat_retry)).await.0,
        StatusCode::FORBIDDEN
    );
    services.secrets.lock(OWNER).await;
    assert_eq!(
        response(&app, human("records/delete", retained_delete.clone())).await.0,
        StatusCode::FORBIDDEN
    );
    services
        .secrets
        .unlock(OWNER, zeroize::Zeroizing::new("synthetic vault passphrase".into()))
        .await
        .unwrap();
    let mut foreign_user = aionui_auth::CurrentUser::local_default();
    foreign_user.id = "foreign-owner".into();
    registry
        .enable_extension_for_user("foreign-owner", "secrets")
        .await
        .unwrap();
    services
        .secrets
        .initialize(
            "foreign-owner",
            zeroize::Zeroizing::new("synthetic foreign passphrase".into()),
        )
        .await
        .unwrap();
    let foreign_app = aionui_secrets::routes::domain_routes(secrets_state)
        .layer(axum::Extension(foreign_user))
        .layer(axum::Extension(aionui_secrets::state::HumanSecretsAccess));
    assert_eq!(
        response(&foreign_app, human("records/delete", retained_delete.clone()))
            .await
            .0,
        StatusCode::CONFLICT
    );

    sqlx::query(
        "INSERT INTO projects(project_id,user_id,name,kind,created_at,updated_at) VALUES('q',?,'q','standard',1,1)",
    )
    .bind(OWNER)
    .execute(services.database.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE conversations SET project_id='q' WHERE id='project'")
        .execute(services.database.pool())
        .await
        .unwrap();
    // Neutral MFA questions work with no memory runtime or provider worker, including reconnect retrieval.
    let _turn = services
        .conversation_runtime_state
        .try_claim_turn("unbound", "synthetic-turn")
        .unwrap();
    // MCP cancellation is bound to this exact pending question request.
    // The later independent question remains answerable.
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut request = runtime(
        "ellmsha_ask_user",
        &unbound,
        "unbound",
        json!({"questions":[{"id":"mfa","header":"MFA","question":"Synthetic cancel","options":[],"multiSelect":false,"isOther":true}]}),
    );
    request
        .headers_mut()
        .insert("x-aionui-secrets-request-id", request_id.parse().unwrap());
    let waiting = app.clone();
    let pending = tokio::spawn(async move { response(&waiting, request).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while services
            .conversation_service
            .pending_host_question_count(OWNER, "unbound")
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut cancel = runtime("ellmsha_ask_user", &unbound, "unbound", json!({}));
    *cancel.uri_mut() = format!("/api/runtime/secrets/questions/{request_id}/cancel")
        .parse()
        .unwrap();
    assert_eq!(response(&app, cancel).await.0, StatusCode::OK);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(4), pending)
            .await
            .unwrap()
            .unwrap()
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        services
            .conversation_service
            .pending_host_question_count(OWNER, "unbound"),
        0
    );
    for decline in [false, true] {
        let request = runtime(
            "ellmsha_ask_user",
            &unbound,
            "unbound",
            json!({"questions":[{"id":"mfa","header":"MFA","question":"Complete synthetic authentication","options":[],"multiSelect":false,"isOther":true,"isSecret":true}]}),
        );
        let waiting = app.clone();
        let call = tokio::spawn(async move { response(&waiting, request).await });
        let forms = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let forms = services
                    .conversation_service
                    .list_confirmations(OWNER, "unbound", &services.worker_task_manager)
                    .await
                    .unwrap();
                if !forms.is_empty() {
                    break forms;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(forms[0].questions[0].is_secret);
        let data = if decline {
            json!({"value":"reject"})
        } else {
            json!({"answers":[{"question":"mfa","labels":["SYNTHETIC_MFA_ANSWER"]}]})
        };
        services
            .conversation_service
            .confirm(
                OWNER,
                "unbound",
                &forms[0].call_id,
                aionui_api_types::ConfirmRequest {
                    msg_id: forms[0].id.clone(),
                    data,
                    always_allow: false,
                },
                &services.worker_task_manager,
            )
            .await
            .unwrap();
        let (status, answer) = call.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        if decline {
            assert_eq!(answer["declined"], true)
        } else {
            assert_eq!(answer["answers"][0]["labels"][0], "SYNTHETIC_MFA_ANSWER")
        }
        assert_eq!(
            services
                .conversation_service
                .pending_host_question_count(OWNER, "unbound"),
            0
        );
    }
    let persisted: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id='unbound'")
        .fetch_all(services.database.pool())
        .await
        .unwrap();
    assert!(!persisted.is_empty());
    assert!(!persisted.join("").contains("SYNTHETIC_MFA_ANSWER"));
    services.secrets.lock(OWNER).await;
    for tool in ["ellmsha_secret_history_search", "ellmsha_secret_history_mask"] {
        assert_eq!(
            response(&app, runtime(tool, &unbound, "unbound", json!({}))).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let overlong = human("records/write", json!({"oversized":"x".repeat(129*1024)}));
    assert_eq!(response(&app, overlong).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    let helper = services.runtime_token_service.issue(
        OWNER,
        "unbound",
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [aionui_ai_agent::RuntimeTokenScope::ConversationHelper],
    );
    registry
        .disable_extension_for_user(OWNER, "secrets", None)
        .await
        .unwrap();
    assert!(!services.secrets.status(OWNER).await.unwrap().unlocked);
    assert_eq!(
        response(
            &app,
            runtime("ellmsha_global_secrets_list", &unbound, "unbound", json!({}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(response(&app, human("vault/lock", json!({}))).await.0, StatusCode::OK);
    registry.enable_extension_for_user(OWNER, "secrets").await.unwrap();
    services
        .secrets
        .unlock(OWNER, zeroize::Zeroizing::new("synthetic vault passphrase".into()))
        .await
        .unwrap();
    assert_eq!(
        response(
            &app,
            runtime("ellmsha_global_secrets_list", &unbound, "unbound", json!({}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    services
        .runtime_token_service
        .validate(
            Some(&helper.token),
            OWNER,
            "unbound",
            aionui_ai_agent::RuntimeTokenScope::ConversationHelper,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .unwrap();
    let renewed = token("unbound", None);
    assert_ne!(renewed, unbound);
    assert_eq!(
        response(
            &app,
            runtime("ellmsha_global_secrets_list", &renewed, "unbound", json!({}))
        )
        .await
        .0,
        StatusCode::OK
    );
    registry
        .disable_extension_for_user(OWNER, "secrets", None)
        .await
        .unwrap();
    // Project enablement remains usable even with the global plugin disabled.
    registry
        .set_extension_enabled_for_project(OWNER, "q", "secrets", true)
        .await
        .unwrap();
    assert_eq!(
        response(
            &app,
            human("vault/unlock", json!({"passphrase":"synthetic vault passphrase"}))
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, groups) = response(&app, human("records/groups", json!({"conversation_id":"project"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        groups["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|group| group["scope"]["project_id"] == "q")
    );
    assert_eq!(
        response(&app, human("records/list", json!({"scope":{"project_id":"q"}})))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        response(&app, human("records/list", json!({"scope":{"project_id":null}})))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        response(
            &app,
            runtime("ellmsha_project_secrets_list", &fresh_project, "project", json!({}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let fresh_project = token("project", Some("q"));
    assert_eq!(
        response(
            &app,
            runtime("ellmsha_project_secrets_list", &fresh_project, "project", json!({}))
        )
        .await
        .0,
        StatusCode::OK
    );
    // The chat's effective project enablement also authorizes its global group.
    let (status, _) = response(
        &app,
        human(
            "records/list",
            json!({"conversation_id":"project","expected_project_id":"q","scope":{"project_id":null}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, stale) = response(
        &app,
        human(
            "records/list",
            json!({"conversation_id":"project","expected_project_id":"p","scope":{"project_id":null}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(stale["code"], "CONTEXT_REQUIRED");
    let (status, disabled) = response(
        &app,
        human(
            "records/list",
            json!({"conversation_id":"unbound","expected_project_id":null,"scope":{"project_id":null}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(disabled["code"], "PLUGIN_DISABLED");
    // A restored revision may be reused, but its authenticated incarnation is not.
    // A newly issued host still cannot execute a pre-restore approved write/delete.
    let (_, aba_record) = response(
        &app,
        runtime("ellmsha_project_secret_write", &fresh_project, "project", record()),
    )
    .await;
    let backup = services.secrets.backup(OWNER).await.unwrap();
    let original_incarnation = services.secrets.status(OWNER).await.unwrap().incarnation.unwrap();
    let patch = |revision: u64, title: &str, incarnation: &str| json!({"id":aba_record["id"],"operation_id":uuid::Uuid::now_v7().to_string(),"expected_revision":revision,"expected_incarnation":incarnation,"patch":{"title":title}});
    assert_eq!(
        response(
            &app,
            runtime(
                "ellmsha_project_secret_write",
                &fresh_project,
                "project",
                patch(1, "r2-B", &original_incarnation)
            )
        )
        .await
        .0,
        StatusCode::OK
    );
    let stale_write = patch(2, "stale-approved-write", &original_incarnation);
    let stale_delete = json!({"id":aba_record["id"],"operation_id":uuid::Uuid::now_v7().to_string(),"expected_revision":2,"expected_incarnation":original_incarnation});
    services
        .secrets
        .restore(
            OWNER,
            backup,
            zeroize::Zeroizing::new("synthetic vault passphrase".into()),
        )
        .await
        .unwrap();
    services
        .secrets
        .unlock(OWNER, zeroize::Zeroizing::new("synthetic vault passphrase".into()))
        .await
        .unwrap();
    let restored = services.secrets.status(OWNER).await.unwrap();
    assert_ne!(restored.incarnation.as_ref().unwrap(), &original_incarnation);
    assert_eq!(
        response(
            &app,
            runtime("ellmsha_project_secrets_list", &fresh_project, "project", json!({}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let restored_token = services
        .runtime_token_service
        .issue_secrets(
            OWNER,
            "project",
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            json!({"project":"q","vault":restored.vault_id,"incarnation":restored.incarnation}).to_string(),
        )
        .token;
    assert_eq!(
        response(
            &app,
            runtime(
                "ellmsha_project_secret_write",
                &restored_token,
                "project",
                patch(1, "r2-C", restored.incarnation.as_ref().unwrap())
            )
        )
        .await
        .0,
        StatusCode::OK
    );
    for (tool, body) in [
        ("ellmsha_project_secret_write", stale_write),
        ("ellmsha_project_secret_delete", stale_delete),
    ] {
        let (status, error) = response(&app, runtime(tool, &restored_token, "project", body)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["code"], "REVISION_CONFLICT");
    }
    let (_, current) = response(
        &app,
        runtime(
            "ellmsha_project_secret_read",
            &restored_token,
            "project",
            json!({"id":aba_record["id"]}),
        ),
    )
    .await;
    assert_eq!(current["title"], "r2-C");
    registry
        .set_extension_enabled_for_project(OWNER, "q", "secrets", false)
        .await
        .unwrap();
    assert!(!services.secrets.status(OWNER).await.unwrap().unlocked);
    let denied = response(&app, human("records/delete", retained_delete)).await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    assert_eq!(denied.1["code"], "PLUGIN_DISABLED");
}
