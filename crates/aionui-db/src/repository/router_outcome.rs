use crate::DbError;

/// Host-attributed acceptance. Target/task/scope are derived from the stored route.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RouterOutcome {
    pub user_id: String,
    pub conversation_id: String,
    pub decision_id: String,
    pub message_id: String,
    pub workspace: String,
    pub target_json: String,
    pub task: String,
    pub accepted: bool,
    pub updated_at: i64,
}
#[async_trait::async_trait]
pub trait IRouterOutcomeRepository: Send + Sync {
    async fn record(&self, outcome: &RouterOutcome) -> Result<(), DbError>;
    async fn recent(&self, user_id: &str, workspace: &str, task: &str) -> Result<Vec<RouterOutcome>, DbError>;
}
pub struct SqliteRouterOutcomeRepository {
    pool: sqlx::SqlitePool,
}
impl SqliteRouterOutcomeRepository {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }
}
#[async_trait::async_trait]
impl IRouterOutcomeRepository for SqliteRouterOutcomeRepository {
    async fn record(&self, o: &RouterOutcome) -> Result<(), DbError> {
        sqlx::query("INSERT INTO router_outcomes(user_id,conversation_id,decision_id,message_id,workspace,target_json,task,accepted,updated_at) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(user_id,conversation_id) DO UPDATE SET accepted=excluded.accepted,message_id=excluded.message_id,updated_at=excluded.updated_at")
            .bind(&o.user_id).bind(&o.conversation_id).bind(&o.decision_id).bind(&o.message_id).bind(&o.workspace).bind(&o.target_json).bind(&o.task).bind(o.accepted).bind(o.updated_at).execute(&self.pool).await?;
        Ok(())
    }
    async fn recent(&self, user_id: &str, workspace: &str, task: &str) -> Result<Vec<RouterOutcome>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM router_outcomes WHERE user_id=? AND workspace=? AND task=? ORDER BY updated_at DESC LIMIT 128").bind(user_id).bind(workspace).bind(task).fetch_all(&self.pool).await?)
    }
}
