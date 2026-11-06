#![allow(clippy::disallowed_types)]
use crate::{
    service::SecretsError,
    state::{DesktopSecretsAccess, HumanSecretsAccess, SecretsRouterState},
};
use aionui_api_types::secrets::*;
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::{
    Extension, Json, Router,
    extract::Request,
    extract::{Path, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use zeroize::Zeroizing;

pub fn domain_routes(state: SecretsRouterState) -> Router {
    Router::new()
        .route("/api/secrets/status", get(status))
        .route("/api/secrets/vault/{operation}", post(vault))
        .route("/api/secrets/records/{operation}", post(records))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(middleware::from_fn(body_bound))
        .layer(middleware::from_fn(no_store))
        .with_state(state)
}
async fn body_bound(request: Request, next: Next) -> Response {
    let limit = if request.uri().path() == "/api/secrets/vault/restore" {
        64 * 1024 * 1024
    } else {
        128 * 1024
    };
    let (parts, body) = request.into_parts();
    match axum::body::to_bytes(body, limit).await {
        Ok(bytes) => {
            next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                .await
        }
        Err(_) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    }
}
pub async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}
pub fn error(error: SecretsError) -> ApiError {
    let status = match error {
        SecretsError::Locked
        | SecretsError::Forbidden
        | SecretsError::ContextRequired
        | SecretsError::ProjectScopeRequired
        | SecretsError::PluginDisabled => StatusCode::FORBIDDEN,
        SecretsError::NotFound | SecretsError::NotInitialized => StatusCode::NOT_FOUND,
        SecretsError::Conflict | SecretsError::MessageBusy => StatusCode::CONFLICT,
        SecretsError::InvalidInput | SecretsError::Crypto => StatusCode::BAD_REQUEST,
        SecretsError::Storage => StatusCode::SERVICE_UNAVAILABLE,
    };
    let code = match error {
        SecretsError::ContextRequired => "CONTEXT_REQUIRED",
        SecretsError::ProjectScopeRequired => "PROJECT_SCOPE_REQUIRED",
        SecretsError::PluginDisabled => "PLUGIN_DISABLED",
        SecretsError::MessageBusy => "MESSAGE_BUSY",
        SecretsError::Locked => "VAULT_LOCKED",
        SecretsError::NotInitialized => "VAULT_NOT_INITIALIZED",
        SecretsError::InvalidInput => "INVALID_INPUT",
        SecretsError::Crypto => "AUTHENTICATION_FAILED",
        SecretsError::Conflict => "REVISION_CONFLICT",
        SecretsError::NotFound => "NOT_FOUND",
        SecretsError::Forbidden => "FORBIDDEN",
        SecretsError::Storage => "STORAGE_UNAVAILABLE",
    };
    ApiError::coded(status, code, code, None)
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| error(SecretsError::InvalidInput))
}
fn encode(value: impl serde::Serialize) -> Result<Response, ApiError> {
    Ok(Json(json!({"success":true,"data":value})).into_response())
}
async fn status(
    State(state): State<SecretsRouterState>,
    Extension(user): Extension<CurrentUser>,
    _human: Extension<HumanSecretsAccess>,
) -> Result<Response, ApiError> {
    encode(state.service.status(&user.id).await.map_err(error)?)
}
async fn vault(
    State(state): State<SecretsRouterState>,
    Extension(user): Extension<CurrentUser>,
    _human: Extension<HumanSecretsAccess>,
    desktop: Option<Extension<DesktopSecretsAccess>>,
    Path(operation): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    if operation.starts_with("desktop-") && desktop.is_none() {
        return Err(error(SecretsError::Forbidden));
    }
    match operation.as_str() {
        "initialize" => {
            let r: VaultPassphraseRequest = parse(body)?;
            encode(
                state
                    .service
                    .initialize(&user.id, Zeroizing::new(r.passphrase))
                    .await
                    .map_err(error)?,
            )
        }
        "unlock" => {
            let r: VaultPassphraseRequest = parse(body)?;
            encode(
                state
                    .service
                    .unlock(&user.id, Zeroizing::new(r.passphrase))
                    .await
                    .map_err(error)?,
            )
        }
        "desktop-export" => encode(state.service.desktop_key(&user.id).await.map_err(error)?),
        "desktop-unlock" => encode(
            state
                .service
                .unlock_desktop(&user.id, parse(body)?)
                .await
                .map_err(error)?,
        ),
        "lock" => {
            state.service.lock(&user.id).await;
            encode(json!({}))
        }
        "change-passphrase" => {
            let r: VaultChangePassphraseRequest = parse(body)?;
            state
                .service
                .change_passphrase(
                    &user.id,
                    Zeroizing::new(r.current_passphrase),
                    Zeroizing::new(r.new_passphrase),
                )
                .await
                .map_err(error)?;
            encode(json!({}))
        }
        "backup" => encode(state.service.backup(&user.id).await.map_err(error)?),
        "restore" => {
            let r: VaultRestoreRequest = parse(body)?;
            state
                .service
                .restore(&user.id, r.backup, Zeroizing::new(r.passphrase))
                .await
                .map_err(error)?;
            encode(json!({}))
        }
        _ => Err(error(SecretsError::NotFound)),
    }
}
async fn records(
    State(state): State<SecretsRouterState>,
    Extension(user): Extension<CurrentUser>,
    _human: Extension<HumanSecretsAccess>,
    Path(operation): Path<String>,
    Json(mut body): Json<Value>,
) -> Result<Response, ApiError> {
    if operation == "groups" {
        let request: SecretGroupsRequest = parse(body)?;
        return encode(
            state
                .access
                .groups(&user.id, request.conversation_id.as_deref())
                .await
                .map_err(error)?,
        );
    }
    let object = body.as_object_mut().ok_or_else(|| error(SecretsError::InvalidInput))?;
    let context: aionui_api_types::secrets::SecretHumanContext = parse(json!({
        "conversation_id":object.remove("conversation_id"),
        "expected_project_id":object.remove("expected_project_id")
    }))?;
    let scope: SecretScope = parse(
        body.get("scope")
            .cloned()
            .ok_or_else(|| error(SecretsError::InvalidInput))?,
    )?;
    let authorize = || async {
        if let Some(chat) = context.conversation_id.as_deref() {
            state
                .access
                .authorize_chat(&user.id, chat, context.expected_project_id.as_deref(), &scope)
                .await
        } else {
            state.access.authorize(&user.id, &scope).await
        }
    };
    let settings_delete = operation == "delete" && context.conversation_id.is_none() && scope.project_id.is_some();
    // Settings deletion authorizes under the writer lock, so a concurrent final
    // deletion can still recover its receipt after removing scope evidence.
    if !settings_delete {
        authorize().await.map_err(error)?;
    }
    let response = match operation.as_str() {
        "list" => encode(state.service.list(&user.id, parse(body)?).await.map_err(error)?),
        "read" => encode(state.service.read(&user.id, parse(body)?).await.map_err(error)?),
        "write" => encode(
            state
                .service
                .write_checked(&user.id, parse(body)?, authorize)
                .await
                .map_err(error)?,
        ),
        "delete" => {
            let request: SecretDeleteRequest = parse(body)?;
            let result = state.service.delete_checked(&user.id, request.clone(), authorize).await;
            match result {
                Err(SecretsError::Forbidden) if settings_delete => encode(
                    state
                        .service
                        .recover_settings_delete(&user.id, request, || state.access.authorize_binding(&user.id, None))
                        .await
                        .map_err(error)?
                        .ok_or_else(|| error(SecretsError::Forbidden))?,
                ),
                result => encode(result.map_err(error)?),
            }
        }
        _ => Err(error(SecretsError::NotFound)),
    }?;
    // Mutations authorize immediately before commit. Deleting the last retained
    // record intentionally removes its settings group; do not turn that success into 403.
    if matches!(operation.as_str(), "list" | "read") {
        authorize().await.map_err(error)?;
    }
    Ok(response)
}
