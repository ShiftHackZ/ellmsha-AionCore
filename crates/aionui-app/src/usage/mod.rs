//! User-scoped usage API and trusted runtime telemetry ingestion.
use aionui_api_types::{ApiResponse, WebSocketMessage};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::{SqlitePool, SqliteUsageRepository, UsageQuery, UsageReport};
use axum::{
    Json, Router,
    extract::{Extension, Query, State},
    routing::get,
};

pub fn routes(pool: SqlitePool) -> Router {
    Router::new().route("/api/usage", get(query)).with_state(pool)
}

async fn query(
    State(pool): State<SqlitePool>,
    Extension(user): Extension<CurrentUser>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<ApiResponse<aionui_db::UsageResponse>>, ApiError> {
    q.period().map_err(|e| ApiError::BadRequest(e.into()))?;
    let data = SqliteUsageRepository::new(pool)
        .query(&user.id, &q)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(ApiResponse::ok(data)))
}

/// Keep database latency independent from the live WebSocket relay and agent execution.
pub fn start(pool: SqlitePool, mut events: tokio::sync::broadcast::Receiver<WebSocketMessage<serde_json::Value>>) {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => persist(&pool, &event).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    tracing::error!(count, "usage telemetry lagged; accounting may be incomplete")
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

async fn persist(pool: &SqlitePool, event: &WebSocketMessage<serde_json::Value>) {
    if event.name != "usage.request" {
        return;
    }
    let (Some(user), Some(conversation), Some(data)) = (
        event.data["user_id"].as_str(),
        event.data["conversation_id"].as_str(),
        event.data.get("data"),
    ) else {
        return;
    };
    let result = match serde_json::from_value::<UsageReport>(data.clone()) {
        Ok(report) => SqliteUsageRepository::new(pool.clone())
            .record(user, conversation, &report)
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(error) = result {
        tracing::error!(conversation_id=conversation, error=%error,"usage request could not be persisted");
    }
}
