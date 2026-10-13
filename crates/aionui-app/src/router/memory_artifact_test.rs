use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn preview_scope_follows_owned_conversation_and_live_project_roots() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let attached = temp.path().join("attached");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&attached).unwrap();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = crate::AppConfig {
        data_dir: temp.path().join("data"),
        work_dir: temp.path().join("work"),
        ..Default::default()
    };
    let services = crate::AppServices::from_config(db, &config).await.unwrap();
    let project = services
        .project_service
        .create_standard(
            "system_default_user",
            aionui_project::canonical::to_file_uri(&workspace).unwrap(),
        )
        .await
        .unwrap();
    let row = aionui_db::models::ConversationRow {
        id: "artifact-preview-test".into(),
        user_id: "system_default_user".into(),
        name: "Fixture".into(),
        r#type: "acp".into(),
        extra: json!({"workspace":workspace}).to_string(),
        model: None,
        status: Some("pending".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 0,
        updated_at: 0,
        project_id: Some(project.project.project_id.clone()),
        folder_id: Some(project.folder.folder_id),
        name_source: None,
    };
    services.conversation_repo.create(&row).await.unwrap();
    let state = ArtifactState {
        conversations: services.conversation_repo,
        projects: services.project_service,
    };
    assert!(matches!(
        authorized_roots(&state, "another-user", &row.id).await,
        Err(ApiError::NotFound(_))
    ));
    assert!(matches!(
        authorized_roots(&state, &row.user_id, "missing").await,
        Err(ApiError::NotFound(_))
    ));
    assert_eq!(authorized_roots(&state, &row.user_id, &row.id).await.unwrap().len(), 1);
    let entry = state
        .projects
        .attach_folder(
            &row.user_id,
            aionui_project::AttachInput {
                project_id: project.project.project_id.clone(),
                uri: aionui_project::canonical::to_file_uri(&attached).unwrap(),
                display_name: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(authorized_roots(&state, &row.user_id, &row.id).await.unwrap().len(), 2);
    state
        .projects
        .remove_attached(&row.user_id, &entry.pe_id)
        .await
        .unwrap();
    assert_eq!(authorized_roots(&state, &row.user_id, &row.id).await.unwrap().len(), 1);

    let router = routes(state).layer(Extension(CurrentUser::local_default()));
    let response = router
        .oneshot(
            Request::get(format!(
                "/api/conversations/{}/memory-artifact?path=..%2Fsecret",
                row.id
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 8192).await.unwrap();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"], "Invalid memory artifact path");
}
