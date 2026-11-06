use super::*;
#[tokio::test]
async fn expired_handles_are_rejected_without_database_access() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let service = SecretHistoryService::new(Arc::new(aionui_db::SqliteSecretHistoryRepository::new(
        db.pool().clone(),
    )));
    let created = Instant::now();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    tokio::time::resume();
    service.candidates.lock().await.insert(
        "expired".into(),
        Candidate {
            owner: "owner".into(),
            caller: "caller".into(),
            scope: HistoryScope::Conversation("caller".into()),
            message: "missing".into(),
            revision: 0,
            pointer: "/content".into(),
            start: 0,
            end: 1,
            digest: [0; 32],
            created,
        },
    );
    let result = service
        .mask(
            "owner",
            "caller",
            None,
            SecretHistoryMaskRequest {
                operation_id: Uuid::now_v7().to_string(),
                candidate_ids: vec!["expired".into()],
            },
        )
        .await;
    assert!(matches!(result, Err(SecretsError::Conflict)));
    service.cursors.lock().await.insert(
        "expired".into(),
        Cursor {
            owner: "owner".into(),
            caller: "caller".into(),
            scope: HistoryScope::Conversation("caller".into()),
            request: "irrelevant".into(),
            after: String::new(),
            offset: 0,
            revision: None,
            message: None,
            created,
        },
    );
    let result = service
        .search(
            "owner",
            "caller",
            None,
            SecretHistorySearchRequest {
                scope: None,
                query: String::new(),
                include_assistant: false,
                include_tools: false,
                cursor: Some("expired".into()),
            },
        )
        .await;
    assert!(matches!(result, Err(SecretsError::Conflict)));
    assert!(service.candidates.lock().await.is_empty());
    assert!(service.cursors.lock().await.is_empty());
}

#[tokio::test]
async fn completed_mask_retries_normalize_uuid_even_after_candidate_expiry() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let owner = "system_default_user";
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,created_at,updated_at) VALUES('chat',?,'fixture','acp','{}',1,1)").bind(owner).execute(db.pool()).await.unwrap();
    sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,created_at) VALUES('m','chat','text',?,'right','finish',1)").bind(serde_json::json!({"content":"password=SYNTHETIC_HISTORY"}).to_string()).execute(db.pool()).await.unwrap();
    let service = SecretHistoryService::new(Arc::new(aionui_db::SqliteSecretHistoryRepository::new(
        db.pool().clone(),
    )));
    let found = service
        .search(
            owner,
            "chat",
            None,
            SecretHistorySearchRequest {
                scope: None,
                query: "password".into(),
                include_assistant: false,
                include_tools: false,
                cursor: None,
            },
        )
        .await
        .unwrap();
    let operation = Uuid::now_v7();
    let mut request = SecretHistoryMaskRequest {
        operation_id: operation.to_string(),
        candidate_ids: found.candidates.into_iter().map(|c| c.candidate_id).collect(),
    };
    let first = service.mask(owner, "chat", None, request.clone()).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    tokio::time::resume();
    for spelling in [
        operation.to_string().to_uppercase(),
        operation.simple().to_string(),
        operation.urn().to_string(),
    ] {
        request.operation_id = spelling;
        let replay = service.mask(owner, "chat", None, request.clone()).await.unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(&first).unwrap()
        );
    }
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_redaction_receipts")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(receipts, 1);
    request.operation_id = Uuid::now_v7().to_string();
    assert!(matches!(
        service.mask(owner, "chat", None, request).await,
        Err(SecretsError::Conflict)
    ));
}
