use aionui_api_types::secrets::*;
use aionui_db::{ISecretHistoryRepository, SqliteSecretHistoryRepository, init_database_memory};
use aionui_secrets::history::SecretHistoryService;
use std::sync::Arc;
use uuid::Uuid;
const OWNER: &str = "system_default_user";
async fn setup() -> (
    aionui_db::Database,
    Arc<SqliteSecretHistoryRepository>,
    SecretHistoryService,
) {
    let db = init_database_memory().await.unwrap();
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,created_at,updated_at) VALUES('chat',?,'fixture','acp','{}',1,1)").bind(OWNER).execute(db.pool()).await.unwrap();
    let repo = Arc::new(SqliteSecretHistoryRepository::new(db.pool().clone()));
    let service = SecretHistoryService::new(repo.clone());
    (db, repo, service)
}
async fn message(db: &aionui_db::Database, id: &str, text: &str) {
    sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,created_at) VALUES(?,'chat','text',?,'right','finish',1)").bind(id).bind(serde_json::json!({"content":text,"id":"password=STRUCTURAL_NEVER_EDIT"}).to_string()).execute(db.pool()).await.unwrap();
}
fn search(cursor: Option<String>) -> SecretHistorySearchRequest {
    SecretHistorySearchRequest {
        scope: None,
        query: "password".into(),
        include_assistant: false,
        include_tools: false,
        cursor,
    }
}
#[tokio::test]
async fn pagination_keeps_every_span_in_one_row_and_skips_oversized_without_stalling() {
    let (db, _, service) = setup().await;
    message(&db, "a", &"x".repeat(1024 * 1024 + 1)).await;
    let text = (0..45).map(|i| format!("password=SYNTHETIC_{i}\n")).collect::<String>();
    message(&db, "b", &text).await;
    let first = service.search(OWNER, "chat", None, search(None)).await.unwrap();
    assert_eq!(first.skipped_oversized_messages, 1);
    assert_eq!(first.candidates.len(), 20);
    let second = service.search(OWNER, "chat", None, search(first.cursor)).await.unwrap();
    assert_eq!(second.candidates.len(), 20);
    let third = service
        .search(OWNER, "chat", None, search(second.cursor))
        .await
        .unwrap();
    assert_eq!(third.candidates.len(), 5);
    assert!(third.complete);
    let values = first
        .candidates
        .into_iter()
        .chain(second.candidates)
        .chain(third.candidates)
        .map(|c| c.value)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(values.len(), 45);
    assert!(values.contains("SYNTHETIC_44"));
}
#[tokio::test]
async fn atomic_mask_cas_retry_and_late_database_merge_cannot_restore_secret() {
    let (db, _, service) = setup().await;
    message(&db, "a", "password=SYNTHETIC_A").await;
    message(&db, "b", "password=SYNTHETIC_B").await;
    let found = service.search(OWNER, "chat", None, search(None)).await.unwrap();
    let request = SecretHistoryMaskRequest {
        operation_id: Uuid::now_v7().to_string(),
        candidate_ids: found.candidates.iter().map(|c| c.candidate_id.clone()).collect(),
    };
    sqlx::query("UPDATE messages SET status='work' WHERE id='b'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        service.mask(OWNER, "chat", None, request.clone()).await,
        Err(aionui_secrets::service::SecretsError::MessageBusy)
    ));
    let before: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='a'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(before.contains("SYNTHETIC_A"));
    sqlx::query("UPDATE messages SET status='finish' WHERE id='b'")
        .execute(db.pool())
        .await
        .unwrap();
    let result = service.mask(OWNER, "chat", None, request.clone()).await.unwrap();
    assert_eq!(result.selected_occurrences, 2);
    assert_eq!(
        service.mask(OWNER, "chat", None, request).await.unwrap().revisions,
        result.revisions
    );
    let after: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='a'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!after.contains("SYNTHETIC_A"));
    assert!(after.contains("STRUCTURAL_NEVER_EDIT"));
    use aionui_db::IConversationRepository;
    let repo = aionui_db::SqliteConversationRepository::new(db.pool().clone());
    repo.update_message(
        OWNER,
        "chat",
        "a",
        &aionui_db::MessageRowUpdate {
            content: Some(before),
            status: Some(Some("finish".into())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let final_content: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='a'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(after, final_content);
    // Canonical export read and future fork both use committed rows, including fences.
    let exported = repo
        .list_messages_page(
            OWNER,
            "chat",
            &aionui_db::MessagePageParams {
                limit: 100,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    assert!(
        !exported
            .iter()
            .map(|row| row.content.as_str())
            .collect::<String>()
            .contains("SYNTHETIC_A")
    );
    sqlx::query("INSERT INTO conversations(id,user_id,name,type,extra,created_at,updated_at) VALUES('fork',?,'fixture','acp','{}',1,1)").bind(OWNER).execute(db.pool()).await.unwrap();
    repo.copy_messages_up_to(OWNER, "chat", "fork", (1, "b")).await.unwrap();
    let copied = repo
        .list_messages_page(
            OWNER,
            "fork",
            &aionui_db::MessagePageParams {
                limit: 100,
                direction: aionui_db::MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(copied.len(), 2);
    assert!(copied.iter().all(|row| row.redacted && row.revision > 0));
    assert!(
        !copied
            .iter()
            .map(|row| row.content.as_str())
            .collect::<String>()
            .contains("SYNTHETIC_")
    );
}
#[tokio::test]
async fn deleted_target_fails_closed() {
    let (db, repo, service) = setup().await;
    message(&db, "a", "password=SYNTHETIC_A").await;
    let found = service.search(OWNER, "chat", None, search(None)).await.unwrap();
    sqlx::query("DELETE FROM messages WHERE id='a'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        service
            .mask(
                OWNER,
                "chat",
                None,
                SecretHistoryMaskRequest {
                    operation_id: Uuid::now_v7().to_string(),
                    candidate_ids: found.candidates.into_iter().map(|c| c.candidate_id).collect()
                }
            )
            .await,
        Err(aionui_secrets::service::SecretsError::Conflict)
    ));
    assert!(matches!(
        repo.selected(
            "someone-else",
            &aionui_db::HistoryScope::Conversation("chat".into()),
            &["a".into()]
        )
        .await,
        Err(aionui_db::DbError::Conflict(_))
    ));
}

#[tokio::test]
// The current conversation model archives legacy runtimes, including `gemini`;
// see aionui-conversation/session_context.rs::legacy_agent_types_are_archived_before_runtime_context.
async fn owned_standard_temp_unbound_and_archived_history_remain_separate() {
    let (db, repo, service) = setup().await;
    sqlx::query(
        "INSERT INTO users(id,password_hash,created_at,updated_at) VALUES('foreign','synthetic-not-a-real-hash',1,1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    for (id, owner, kind) in [
        ("project", OWNER, "standard"),
        ("temp", OWNER, "temp"),
        ("foreign-project", "foreign", "standard"),
    ] {
        sqlx::query("INSERT INTO projects(project_id,user_id,name,kind,created_at,updated_at) VALUES(?,?,?, ?,1,1)")
            .bind(id)
            .bind(owner)
            .bind(id)
            .bind(kind)
            .execute(db.pool())
            .await
            .unwrap();
    }
    for (id, owner, project, kind) in [
        ("project-chat", OWNER, Some("project"), "acp"),
        ("archived", OWNER, Some("project"), "gemini"),
        ("temp-chat", OWNER, Some("temp"), "acp"),
        ("foreign-chat", "foreign", Some("foreign-project"), "acp"),
    ] {
        sqlx::query("INSERT INTO conversations(id,user_id,name,type,project_id,extra,created_at,updated_at) VALUES(?,?,?,?,?,'{}',1,1)").bind(id).bind(owner).bind(id).bind(kind).bind(project).execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,created_at) VALUES(?,?,'text',?,'right','finish',1)").bind(format!("message-{id}")).bind(id).bind(serde_json::json!({"content":format!("password=SYNTHETIC_{id}")}).to_string()).execute(db.pool()).await.unwrap();
    }
    message(&db, "unbound", "password=SYNTHETIC_UNBOUND").await;
    let mut project_query = search(None);
    project_query.scope = Some(SecretHistoryScope::CurrentProject);
    let project = service
        .search(OWNER, "project-chat", Some("project"), project_query.clone())
        .await
        .unwrap();
    let chats = project
        .candidates
        .into_iter()
        .map(|row| row.conversation_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        chats,
        std::collections::BTreeSet::from(["archived".into(), "project-chat".into()])
    );
    assert!(matches!(
        service.search(OWNER, "chat", None, project_query).await,
        Err(aionui_secrets::service::SecretsError::ProjectScopeRequired)
    ));
    let mut temp_query = search(None);
    temp_query.scope = Some(SecretHistoryScope::TemporaryHistory);
    let temporary = service.search(OWNER, "chat", None, temp_query).await.unwrap();
    let chats = temporary
        .candidates
        .into_iter()
        .map(|row| row.conversation_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        chats,
        std::collections::BTreeSet::from(["chat".into(), "temp-chat".into()])
    );
    assert!(matches!(
        repo.selected(
            "foreign",
            &aionui_db::HistoryScope::Conversation("project-chat".into()),
            &["message-project-chat".into()]
        )
        .await,
        Err(aionui_db::DbError::Conflict(_))
    ));
    assert!(
        repo.page(
            OWNER,
            &aionui_db::HistoryScope::Project("foreign-project".into()),
            "",
            false,
            false
        )
        .await
        .unwrap()
        .is_empty()
    );
}

#[tokio::test]
async fn cursor_and_candidate_are_bound_to_owner_caller_and_query() {
    let (db, _, service) = setup().await;
    message(
        &db,
        "a",
        &(0..25).map(|i| format!("password=SYNTHETIC_{i}\n")).collect::<String>(),
    )
    .await;
    let found = service.search(OWNER, "chat", None, search(None)).await.unwrap();
    let cursor = found.cursor.unwrap();
    assert!(matches!(
        service
            .search("foreign", "chat", None, search(Some(cursor.clone())))
            .await,
        Err(aionui_secrets::service::SecretsError::Conflict)
    ));
    assert!(matches!(
        service
            .search(OWNER, "other-chat", None, search(Some(cursor.clone())))
            .await,
        Err(aionui_secrets::service::SecretsError::Conflict)
    ));
    let mut different = search(Some(cursor));
    different.query = "token".into();
    assert!(matches!(
        service.search(OWNER, "chat", None, different).await,
        Err(aionui_secrets::service::SecretsError::Conflict)
    ));
    let request = SecretHistoryMaskRequest {
        operation_id: Uuid::now_v7().to_string(),
        candidate_ids: vec![found.candidates[0].candidate_id.clone()],
    };
    assert!(matches!(
        service.mask(OWNER, "other-chat", None, request.clone()).await,
        Err(aionui_secrets::service::SecretsError::Conflict)
    ));
    assert!(matches!(
        service.mask("foreign", "chat", None, request).await,
        Err(aionui_secrets::service::SecretsError::Conflict)
    ));
}

#[tokio::test]
async fn partial_cursor_rejects_removed_message_even_when_next_revision_matches() {
    let (db, _, service) = setup().await;
    message(
        &db,
        "a",
        &(0..25).map(|i| format!("password=SYNTHETIC_{i}\n")).collect::<String>(),
    )
    .await;
    message(
        &db,
        "b",
        &(0..25).map(|i| format!("password=SECOND_{i}\n")).collect::<String>(),
    )
    .await;
    let first = service.search(OWNER, "chat", None, search(None)).await.unwrap();
    assert_eq!(first.candidates.len(), 20);
    sqlx::query("DELETE FROM messages WHERE id='a'")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        service.search(OWNER, "chat", None, search(first.cursor)).await,
        Err(aionui_secrets::service::SecretsError::Conflict)
    ));
}

#[tokio::test]
async fn structured_credential_field_is_detected_and_masked_without_label_in_value() {
    let (db, _, service) = setup().await;
    sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,created_at) VALUES('structured','chat','tool_call',?,'left','finish',1)")
        .bind(serde_json::json!({"args":{"credentials":{"password":"SYNTHETIC_STRUCTURED"},"id":"STRUCTURAL"}}).to_string()).execute(db.pool()).await.unwrap();
    let mut query = search(None);
    query.include_tools = true;
    query.include_assistant = true;
    let found = service.search(OWNER, "chat", None, query).await.unwrap();
    assert_eq!(found.candidates.len(), 1);
    assert_eq!(found.candidates[0].value, "SYNTHETIC_STRUCTURED");
    service
        .mask(
            OWNER,
            "chat",
            None,
            SecretHistoryMaskRequest {
                operation_id: Uuid::now_v7().to_string(),
                candidate_ids: vec![found.candidates[0].candidate_id.clone()],
            },
        )
        .await
        .unwrap();
    let content: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='structured'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!content.contains("SYNTHETIC_STRUCTURED"));
    assert!(content.contains("STRUCTURAL"));
}

#[tokio::test]
async fn writer_wait_revalidates_authority_and_rolls_back_without_pool_reentry() {
    use aionui_db::{DbError, HistoryCommitContext, HistoryCommitGuard, HistoryEdit, HistoryScope};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Guard(Arc<AtomicBool>);
    #[async_trait::async_trait]
    impl HistoryCommitGuard for Guard {
        async fn authorize(&self, _: &HistoryCommitContext) -> Result<(), DbError> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DbError::Conflict("PLUGIN_DISABLED".into()))
            }
        }
    }
    let (db, repo, _) = setup().await;
    message(&db, "queued", "password=SYNTHETIC_QUEUED").await;
    let valid = Arc::new(AtomicBool::new(true));
    let writer = db.pool().begin_with("BEGIN IMMEDIATE").await.unwrap();
    let permit = valid.clone();
    let masking = tokio::spawn(async move {
        repo.mask(
            OWNER,
            "chat",
            &HistoryScope::Conversation("chat".into()),
            "queued-operation",
            "fingerprint",
            &[HistoryEdit {
                message_id: "queued".into(),
                revision: 0,
                content: "masked".into(),
            }],
            "receipt",
            &Guard(permit),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!masking.is_finished());
    valid.store(false, Ordering::SeqCst);
    writer.commit().await.unwrap();
    assert!(
        matches!(tokio::time::timeout(std::time::Duration::from_secs(2),masking).await.unwrap().unwrap(),Err(DbError::Conflict(code)) if code=="PLUGIN_DISABLED")
    );
    let content: String = sqlx::query_scalar("SELECT content FROM messages WHERE id='queued'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(content.contains("SYNTHETIC_QUEUED"));
}

#[tokio::test]
async fn canonical_acp_payloads_and_legacy_keys_search_and_mask_only_payloads() {
    let (db, _, service) = setup().await;
    for (id, canonical) in [("canonical", true), ("legacy", false)] {
        let mut content = serde_json::json!({"session_id":"s","update":{"tool_call_id":"structural-password=KEEP","sessionUpdate":"tool_call","rawInput":{"password":"SYNTHETIC_INPUT"},"rawOutput":{"token":"SYNTHETIC_OUTPUT"}}});
        if canonical {
            aionui_common::normalize_keys_to_snake_case(&mut content);
        }
        sqlx::query("INSERT INTO messages(id,conversation_id,type,content,position,status,created_at) VALUES(?,'chat','acp_tool_call',?,'left','finish',1)").bind(id).bind(content.to_string()).execute(db.pool()).await.unwrap();
    }
    let mut request = search(None);
    request.query.clear();
    request.include_tools = true;
    let found = service.search(OWNER, "chat", None, request).await.unwrap();
    assert_eq!(found.candidates.len(), 4);
    assert!(
        found
            .candidates
            .iter()
            .any(|c| c.pointer == "/update/raw_input/password")
    );
    assert!(
        found
            .candidates
            .iter()
            .any(|c| c.pointer == "/update/rawInput/password")
    );
    service
        .mask(
            OWNER,
            "chat",
            None,
            SecretHistoryMaskRequest {
                operation_id: Uuid::now_v7().to_string(),
                candidate_ids: found.candidates.into_iter().map(|c| c.candidate_id).collect(),
            },
        )
        .await
        .unwrap();
    let rows: Vec<String> = sqlx::query_scalar("SELECT content FROM messages")
        .fetch_all(db.pool())
        .await
        .unwrap();
    for row in rows {
        assert!(!row.contains("SYNTHETIC_"));
        assert!(row.contains("structural-password=KEEP"));
        assert!(row.contains("<masked>"));
    }
}

#[tokio::test]
async fn masked_context_is_bounded_unicode_and_hides_neighboring_candidates() {
    let (db, _, service) = setup().await;
    let text = format!(
        "{}
Portal 東京 password=FIRST_SECRET
Tenant 🔑 token=NEIGHBOR_SECRET
Second password=SECOND_SECRET
{}",
        "界".repeat(120),
        "🙂".repeat(120)
    );
    message(&db, "context", &text).await;
    let found = service.search(OWNER, "chat", None, search(None)).await.unwrap();
    assert_eq!(found.candidates.len(), 2);
    for c in &found.candidates {
        assert!(!c.masked_context.contains("_SECRET"));
        assert!(c.masked_context.contains("<masked>"));
        assert!(c.masked_context.chars().count() < 200);
    }
    assert!(found.candidates[0].masked_context.contains("Portal 東京 password="));
    assert_ne!(found.candidates[0].masked_context, found.candidates[1].masked_context);
    message(
        &db,
        "literal",
        "前🙂 repeatedXYZ 中 repeatedXYZ 後🙂 token=OTHER_SECRET",
    )
    .await;
    let mut request = search(None);
    request.query = "repeatedXYZ".into();
    let found = service.search(OWNER, "chat", None, request).await.unwrap();
    assert_eq!(found.candidates.len(), 2);
    for c in found.candidates {
        assert_eq!(c.value, "repeatedXYZ");
        assert!(!c.masked_context.contains("repeatedXYZ"));
        assert!(!c.masked_context.contains("OTHER_SECRET"));
        assert!(c.masked_context.contains("前🙂"));
    }
}
