use super::*;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

async fn tool(router: &Router, name: &str, token: &str, args: Value) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::post(format!("/api/memory/rules/{name}"))
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(args.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap(),
    )
}

#[tokio::test]
async fn five_rules_callbacks_bind_read_write_and_delete_to_the_correct_scope() {
    // Give this one test its own memory configuration without changing the environment of parallel tests.
    const CHILD: &str = "AIONUI_FIVE_RULES_CALLBACK_FIXTURE";
    let temp = tempfile::tempdir().unwrap();
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(
                concat!(
                    module_path!(),
                    "::five_rules_callbacks_bind_read_write_and_delete_to_the_correct_scope"
                )
                .split_once("::")
                .unwrap()
                .1,
            )
            .arg("--nocapture")
            .env(CHILD, "1")
            .env("AIONUI_TDAI_MEMORY_CONFIG", temp.path().join("memory.json"))
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}{}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(String::from_utf8_lossy(&status.stdout).contains("1 passed"));
        return;
    }
    let memory = MockServer::start().await;
    std::fs::write(std::env::var_os("AIONUI_TDAI_MEMORY_CONFIG").unwrap(),
        json!({"gatewayUrl":memory.uri(),"serviceId":"fixture","teamId":"fixture","agentId":"fixture","userId":"fixture"}).to_string()).unwrap();
    Mock::given(path("/v3/atomic/search"))
        .respond_with(|req: &wiremock::Request| {
            let args: Value = req.body_json().unwrap();
            assert_eq!(args["guidance_inventory"], true);
            assert_eq!(args["include_global"], true);
            let record = |scope: &str, path: String| {
                json!({"id":scope,"guidance_status":"active","source_status":"active",
            "type":"instruction","guidance_scope":scope,"source_path":path})
            };
            let mut items = vec![record("global", "knowledge/global/example.md".into())];
            if let Some(scope) = args["project_scopes"].get(0).and_then(Value::as_str) {
                items.push(record("project", format!("knowledge/{scope}/example.md")));
            }
            ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"guidance_inventory":true,
            "guidance_revision":"fixture","guidance_write_version":1,"degraded":false,"returned":items.len(),
            "total_candidates":items.len(),"complete":true,"next_offset":null,"items":items}}))
        })
        .mount(&memory)
        .await;
    Mock::given(path("/v3/scenario/read"))
        .respond_with(|req: &wiremock::Request| {
            let p = req.body_json::<Value>().unwrap()["path"].as_str().unwrap().to_owned();
            let body = if p.starts_with("knowledge/global/") {
                "GLOBAL_STANDING"
            } else {
                "PROJECT_STANDING"
            };
            ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"path":p,"content":body}}))
        })
        .mount(&memory)
        .await;
    Mock::given(path("/v3/scenario/rule-mutate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{}})))
        .expect(4)
        .mount(&memory)
        .await;

    let root = temp.path().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let rules_path = root.join("AGENTS.md");
    let file_rule = "# Project\n\nFILE_STANDING\n";
    std::fs::write(&rules_path, file_rule).unwrap();
    let services = crate::AppServices::from_config(
        aionui_db::init_database_memory().await.unwrap(),
        &crate::AppConfig {
            data_dir: temp.path().join("state"),
            work_dir: temp.path().join("work"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let owner = "system_default_user";
    let project = services
        .project_service
        .create_standard(owner, aionui_project::canonical::to_file_uri(&root).unwrap())
        .await
        .unwrap();
    let (workers, captured) = test_workers(services.generation_service.clone());
    let router = routes(
        services.generation_service.clone(),
        workers.clone(),
        services.conversation_repo.clone(),
        services.project_service.clone(),
        services.conversation_service.clone(),
    );
    for (id, project_id) in [("temporary", None), ("project", Some(project.project.project_id))] {
        let row = aionui_db::models::ConversationRow {
            id: id.into(),
            user_id: owner.into(),
            name: id.into(),
            r#type: "aionrs".into(),
            extra: json!({"workspace":root}).to_string(),
            model: None,
            status: Some("pending".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 0,
            updated_at: 0,
            project_id,
            folder_id: None,
            name_source: None,
        };
        services.conversation_repo.create(&row).await.unwrap();
        let token = start_worker(&services, &row, workers.as_ref(), &captured, &root).await;
        let args = json!({"conversation_id":id});
        let (status, result) = tool(&router, "read", &token, args).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        let data = &result["data"];
        assert_eq!(data["tool"], "tdai_rules_read");
        assert_eq!(data["complete"], true);
        let rules = data["rules"].as_array().unwrap();
        assert!(
            rules
                .iter()
                .any(|r| r["scope"] == "global" && r["content"] == "GLOBAL_STANDING")
        );
        if id == "temporary" {
            assert_eq!(
                rules.len(),
                1,
                "a temporary cwd in the registered Git repository must not expose project rules"
            );
        } else {
            assert!(
                rules
                    .iter()
                    .any(|r| r["scope"] == "project" && r["content"] == "PROJECT_STANDING")
            );
            assert!(rules.iter().any(|r| r["scope"] == "project file"
                && r["path"].as_str().unwrap().contains("AGENTS.md")
                && r["content"].as_str().unwrap().contains("FILE_STANDING")));
        }
        let own_scope = if id == "temporary" { "global" } else { "project" };
        let add = |scope| json!({"conversation_id":id,"mutation":{"action":"add","scope":scope,"title":"Sample","content":"Fixture"}});
        assert_eq!(
            tool(&router, &format!("{own_scope}/write"), &token, add(own_scope))
                .await
                .0,
            StatusCode::OK
        );
        if id == "temporary" {
            assert_eq!(
                tool(&router, "project/write", &token, add("project")).await.0,
                StatusCode::FORBIDDEN
            );
        }
        let current = rules.iter().find(|r| r["scope"] == own_scope).unwrap();
        let delete = |path: &Value, rev: &Value| json!({"conversation_id":id,"mutation":{"action":"delete","scope":own_scope,"path":path,"revision":rev}});
        let route = format!("{own_scope}/delete");
        assert_eq!(
            tool(&router, &route, &token, delete(&current["path"], &current["revision"]))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            tool(&router, &route, &token, delete(&current["path"], &json!("wrong")))
                .await
                .0,
            StatusCode::CONFLICT
        );
        for target in ["./AGENTS.md", "knowledge/foreign/example.md"] {
            assert_eq!(
                tool(&router, &route, &token, delete(&json!(target), &current["revision"]))
                    .await
                    .0,
                StatusCode::CONFLICT
            );
        }
        assert_eq!(std::fs::read_to_string(&rules_path).unwrap(), file_rule);
        let writes: Vec<Value> = memory
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path() == "/v3/scenario/rule-mutate")
            .map(|r| r.body_json().unwrap())
            .collect();
        let prefix = if id == "temporary" {
            "global"
        } else {
            data["project"].as_str().unwrap()
        };
        let latest = &writes[writes.len() - 2..];
        assert_eq!(latest[0]["action"], "write");
        assert_eq!(latest[0]["path"], format!("knowledge/{prefix}/sample.md"));
        assert_eq!(latest[1]["action"], "delete");
        assert_eq!(latest[1]["path"], current["path"]);
        assert_eq!(latest[1]["expected_sha256"], current["revision"]);
    }
    workers.clear().await;
    memory.verify().await;
}
