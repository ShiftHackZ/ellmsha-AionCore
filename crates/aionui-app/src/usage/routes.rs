#![allow(clippy::disallowed_types)] // HTTP boundary maps validation/storage errors to ApiError.

//! User-scoped usage HTTP boundary.
use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use aionui_db::{SqlitePool, SqliteUsageRepository, UsageQuery};
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
