//! Ownership filtering happens in SQL before history content is returned.
use crate::DbError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryScope {
    Conversation(String),
    Project(String),
    Temporary,
}

#[derive(Clone, sqlx::FromRow)]
pub struct SecretHistoryRow {
    pub id: String,
    pub conversation_id: String,
    pub content: String,
    pub revision: i64,
    pub byte_size: i64,
    pub position: Option<String>,
    pub status: Option<String>,
    pub r#type: String,
}

#[derive(Clone)]
pub struct HistoryEdit {
    pub message_id: String,
    pub revision: i64,
    pub content: String,
}

/// Caller scope read on the acquired writer transaction, never a second pool checkout.
pub struct HistoryCommitContext {
    pub project_id: Option<String>,
    pub project_kind: Option<String>,
}
async fn commit_context(
    connection: &mut sqlx::SqliteConnection,
    owner: &str,
    caller: &str,
) -> Result<HistoryCommitContext, DbError> {
    let row = sqlx::query("SELECT c.project_id,p.kind FROM conversations c LEFT JOIN projects p ON p.project_id=c.project_id AND p.user_id=c.user_id WHERE c.id=? AND c.user_id=?")
        .bind(caller).bind(owner).fetch_optional(connection).await?
        .ok_or_else(|| DbError::Conflict("CONTEXT_REQUIRED".into()))?;
    Ok(HistoryCommitContext {
        project_id: row.try_get("project_id")?,
        project_kind: row.try_get("kind")?,
    })
}
/// Revalidates non-database runtime authority without reentering the main pool.
#[async_trait]
pub trait HistoryCommitGuard: Send + Sync {
    async fn authorize(&self, context: &HistoryCommitContext) -> Result<(), DbError>;
}
pub struct UncheckedHistoryCommit;
#[async_trait]
impl HistoryCommitGuard for UncheckedHistoryCommit {
    async fn authorize(&self, _: &HistoryCommitContext) -> Result<(), DbError> {
        Ok(())
    }
}

#[async_trait]
pub trait ISecretHistoryRepository: Send + Sync {
    async fn page(
        &self,
        owner: &str,
        scope: &HistoryScope,
        after: &str,
        assistant: bool,
        tools: bool,
    ) -> Result<Vec<SecretHistoryRow>, DbError>;
    async fn selected(
        &self,
        owner: &str,
        scope: &HistoryScope,
        ids: &[String],
    ) -> Result<Vec<SecretHistoryRow>, DbError>;
    async fn receipt(
        &self,
        owner: &str,
        caller: &str,
        operation: &str,
        fingerprint: &str,
    ) -> Result<Option<String>, DbError>;
    #[allow(clippy::too_many_arguments)] // Explicit transaction authorization/CAS inputs remain independently typed.
    async fn mask(
        &self,
        owner: &str,
        caller: &str,
        scope: &HistoryScope,
        operation: &str,
        fingerprint: &str,
        edits: &[HistoryEdit],
        receipt: &str,
        guard: &dyn HistoryCommitGuard,
    ) -> Result<String, DbError>;
}

pub struct SqliteSecretHistoryRepository {
    pool: SqlitePool,
}
impl SqliteSecretHistoryRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn predicate(scope: &HistoryScope) -> (&'static str, &str) {
    match scope {
        HistoryScope::Conversation(id) => ("c.id = ?", id),
        HistoryScope::Project(id) => (
            "c.project_id = ? AND EXISTS(SELECT 1 FROM projects p WHERE p.project_id=c.project_id AND p.user_id=c.user_id AND p.kind='standard')",
            id,
        ),
        HistoryScope::Temporary => (
            "? = '' AND (c.project_id IS NULL OR EXISTS(SELECT 1 FROM projects p WHERE p.project_id=c.project_id AND p.user_id=c.user_id AND p.kind='temp'))",
            "",
        ),
    }
}

#[async_trait]
impl ISecretHistoryRepository for SqliteSecretHistoryRepository {
    async fn receipt(
        &self,
        owner: &str,
        caller: &str,
        operation: &str,
        fingerprint: &str,
    ) -> Result<Option<String>, DbError> {
        let row=sqlx::query("SELECT fingerprint,receipt FROM message_redaction_receipts WHERE owner_user_id=? AND conversation_id=? AND operation_id=?")
            .bind(owner).bind(caller).bind(operation).fetch_optional(&self.pool).await?;
        row.map(|row| {
            if row.get::<String, _>("fingerprint") == fingerprint {
                Ok(row.get("receipt"))
            } else {
                Err(DbError::Conflict("operation id".into()))
            }
        })
        .transpose()
    }
    async fn selected(
        &self,
        owner: &str,
        scope: &HistoryScope,
        ids: &[String],
    ) -> Result<Vec<SecretHistoryRow>, DbError> {
        if ids.len() > 50 {
            return Err(DbError::Conflict("too many targets".into()));
        }
        let (clause, binding) = predicate(scope);
        let mut rows = vec![];
        let sql = format!(
            "SELECT m.id,m.conversation_id,m.content,length(CAST(m.content AS BLOB)) AS byte_size,m.revision,m.position,m.status,m.type FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE c.user_id=? AND ({clause}) AND m.id=?"
        );
        for id in ids {
            rows.push(
                sqlx::query_as(&sql)
                    .bind(owner)
                    .bind(binding)
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?
                    .ok_or_else(|| DbError::Conflict("history target unavailable".into()))?,
            );
        }
        Ok(rows)
    }
    async fn page(
        &self,
        owner: &str,
        scope: &HistoryScope,
        after: &str,
        assistant: bool,
        tools: bool,
    ) -> Result<Vec<SecretHistoryRow>, DbError> {
        let (clause, binding) = predicate(scope);
        // Fetch only bounded metadata first. No unbounded message content enters process memory.
        let sql = format!(
            "SELECT m.id,m.conversation_id,'' AS content,length(CAST(m.content AS BLOB)) AS byte_size,m.revision,m.position,m.status,m.type FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE c.user_id=? AND ({clause}) AND m.id>? AND ((m.type='text' AND (m.position='right' OR ?)) OR (? AND m.type IN ('tool_call','acp_tool_call'))) ORDER BY m.id LIMIT 201"
        );
        let mut rows: Vec<SecretHistoryRow> = sqlx::query_as(&sql)
            .bind(owner)
            .bind(binding)
            .bind(after)
            .bind(assistant)
            .bind(tools)
            .fetch_all(&self.pool)
            .await?;
        let mut bytes = 0usize;
        for row in &mut rows {
            if row.byte_size > 1024 * 1024 {
                continue;
            }
            if bytes + row.byte_size as usize > 1024 * 1024 {
                break;
            }
            let sql = format!(
                "SELECT m.content FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE c.user_id=? AND ({clause}) AND m.id=? AND m.revision=? AND length(CAST(m.content AS BLOB))=?"
            );
            row.content = sqlx::query_scalar(&sql)
                .bind(owner)
                .bind(binding)
                .bind(&row.id)
                .bind(row.revision)
                .bind(row.byte_size)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| DbError::Conflict("history changed".into()))?;
            bytes += row.content.len();
        }
        Ok(rows)
    }
    #[allow(clippy::too_many_arguments)] // Explicit transaction authorization/CAS inputs remain independently typed.
    async fn mask(
        &self,
        owner: &str,
        caller: &str,
        scope: &HistoryScope,
        operation: &str,
        fingerprint: &str,
        edits: &[HistoryEdit],
        receipt: &str,
        guard: &dyn HistoryCommitGuard,
    ) -> Result<String, DbError> {
        let mut connection = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let outcome:Result<String,DbError>=async{
            guard.authorize(&commit_context(&mut connection, owner, caller).await?).await?;
            if let Some(row)=sqlx::query("SELECT fingerprint,receipt FROM message_redaction_receipts WHERE owner_user_id=? AND conversation_id=? AND operation_id=?")
                .bind(owner).bind(caller).bind(operation).fetch_optional(&mut *connection).await? {
                if row.get::<String,_>("fingerprint")!=fingerprint{return Err(DbError::Conflict("operation id".into()));}
                return Ok(row.get("receipt"));
            }
            let (clause,binding)=predicate(scope);
            for edit in edits{
                let sql=format!("UPDATE messages SET content=?,revision=revision+1,redacted=1 WHERE id=? AND revision=? AND status IS NOT 'work' AND status IS NOT 'pending' AND EXISTS(SELECT 1 FROM conversations c WHERE c.id=messages.conversation_id AND c.user_id=? AND ({clause}))");
                if sqlx::query(&sql).bind(&edit.content).bind(&edit.message_id).bind(edit.revision).bind(owner).bind(binding).execute(&mut *connection).await?.rows_affected()!=1{
                    return Err(DbError::Conflict("history changed or target unavailable".into()));
                }
            }
            sqlx::query("INSERT INTO message_redaction_receipts(owner_user_id,conversation_id,operation_id,fingerprint,receipt) VALUES(?,?,?,?,?)")
                .bind(owner).bind(caller).bind(operation).bind(fingerprint).bind(receipt).execute(&mut *connection).await?;
            Ok(receipt.into())
        }.await;
        match outcome {
            Ok(receipt) => {
                guard
                    .authorize(&commit_context(&mut connection, owner, caller).await?)
                    .await?;
                connection.commit().await?;
                Ok(receipt)
            }
            Err(error) => {
                connection.rollback().await?;
                Err(error)
            }
        }
    }
}
