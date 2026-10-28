#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::path::Path as FsPath;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};

use crate::notion::NotionConnectionError;
use aionui_api_types::{
    ApiResponse, ClockifyConnectionResponse, ClockifyConnectionsResponse, ConnectClockifyConnectionRequest,
    ConnectLinearConnectionRequest, ConnectNotionConnectionRequest, DisableExtensionRequest, EnableExtensionRequest,
    ExtensionSummaryResponse, GetI18nRequest, GetPermissionsRequest, GetRiskLevelRequest, LinearConnectionResponse,
    LinearConnectionsResponse, NotionConnectionResponse, NotionConnectionsResponse, PermissionDetailResponse,
    PermissionSummaryResponse, SessionMcpTransport,
};
use aionui_auth::CurrentUser;
use aionui_common::{ApiError, now_ms};
use aionui_db::UserType;
use serde::Deserialize;
use serde_json::json;

use crate::asset_paths::normalize_relative_asset_path;
use crate::clockify::ClockifyConnectionError;
use crate::error::ExtensionError;
use crate::linear::LinearConnectionError;
use crate::permission::{build_permission_summary, calculate_risk_level};
use crate::registry::ExtensionRegistry;
use crate::{PluginInstallProvenance, PluginInstallSource, PluginInstaller};

impl From<ExtensionError> for ApiError {
    fn from(err: ExtensionError) -> Self {
        match err {
            ExtensionError::ManifestValidation(msg) => ApiError::BadRequest(msg),
            ExtensionError::ReservedNamePrefix { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::InvalidVersion { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::UndefinedEnvVariable(var) => {
                ApiError::BadRequest(format!("Undefined environment variable: {var}"))
            }
            ExtensionError::FileReferenceNotFound(path) => {
                ApiError::NotFound(format!("File reference not found: {path}"))
            }
            ExtensionError::PathTraversal(path) => ApiError::BadRequest(format!("Path traversal detected: {path}")),
            ExtensionError::EngineIncompatible { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::ApiVersionIncompatible { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::InvalidWebuiRouteNamespace { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::ReservedWebuiRoute { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::ThemeCssNotFound(path) => ApiError::NotFound(format!("Theme CSS not found: {path}")),
            ExtensionError::HookTimeout { .. } => ApiError::Internal(err.to_string()),
            ExtensionError::HookFailed { .. } => ApiError::Internal(err.to_string()),
            ExtensionError::HookNotFound(path) => ApiError::NotFound(format!("Hook script not found: {path}")),
            ExtensionError::ResolutionFailed { .. } => ApiError::Internal(err.to_string()),
            ExtensionError::NotFound(name) => ApiError::NotFound(format!("Extension not found: {name}")),
            ExtensionError::StatePersistence(msg) => ApiError::Internal(msg),
            ExtensionError::BuiltinSkillDeletion(name) => {
                ApiError::BadRequest(format!("Cannot delete built-in skill: {name}"))
            }
            ExtensionError::SkillNotFound(name) => ApiError::NotFound(format!("Skill not found: {name}")),
            ExtensionError::InvalidSkillPath(path) => ApiError::BadRequest(format!("Invalid skill path: {path}")),
            ExtensionError::SkillInvalidFrontmatter(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_INVALID_FRONTMATTER",
                "Skill frontmatter is invalid.",
                None,
            ),
            ExtensionError::SkillImportNoSkillFound(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_NO_SKILL_FOUND",
                "No valid skill was found in the selected path.",
                None,
            ),
            ExtensionError::SkillImportInvalidSource(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_INVALID_SOURCE",
                "Selected path is not a skill directory, SKILL.md, parent directory, or zip archive.",
                None,
            ),
            ExtensionError::SkillImportSymlinkEntry(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_SYMLINK_ENTRY",
                "Skill import cannot contain symlink entries.",
                None,
            ),
            ExtensionError::SkillImportFileTooLarge {
                file_path,
                file_bytes,
                limit_bytes,
            } => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_FILE_TOO_LARGE",
                "Skill import contains a file that is too large.",
                Some(json!({
                    "error_path": file_path,
                    "file_bytes": file_bytes,
                    "limit_bytes": limit_bytes,
                })),
            ),
            ExtensionError::SkillImportTotalTooLarge {
                total_bytes,
                limit_bytes,
            } => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_TOTAL_TOO_LARGE",
                "Skill import is too large.",
                Some(json!({
                    "total_bytes": total_bytes,
                    "limit_bytes": limit_bytes,
                })),
            ),
            ExtensionError::SkillImportInvalidZip(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_INVALID_ZIP",
                "Selected zip archive is not a valid skill package.",
                None,
            ),
            ExtensionError::Db(aionui_db::DbError::NotFound(msg)) => ApiError::NotFound(msg),
            ExtensionError::Db(aionui_db::DbError::Conflict(msg)) => ApiError::Conflict(msg),
            ExtensionError::Db(err) => ApiError::Internal(err.to_string()),
            ExtensionError::InvalidRequest(msg) => ApiError::BadRequest(msg),
            ExtensionError::Internal(msg) => ApiError::Internal(msg),
            ExtensionError::Io(e) => ApiError::Internal(e.to_string()),
            ExtensionError::JsonParse(e) => ApiError::BadRequest(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for extension route handlers.
#[derive(Clone)]
pub struct ExtensionRouterState {
    pub registry: ExtensionRegistry,
    pub plugin_installer: Option<PluginInstaller>,
    pub allow_machine_plugin_mutations: bool,
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the extension router with all `/api/extensions/*` routes.
///
/// Includes query routes and management routes.
/// All routes require authentication (applied by the caller).
pub fn extension_routes(state: ExtensionRouterState) -> Router {
    Router::new()
        // Query routes
        .route("/api/extensions", get(get_loaded_extensions))
        .route("/api/extensions/themes", get(get_themes))
        .route("/api/extensions/assistants", get(get_assistants))
        .route("/api/extensions/acp-adapters", get(get_acp_adapters))
        .route("/api/extensions/agents", get(get_agents))
        .route("/api/extensions/mcp-servers", get(get_mcp_servers))
        .route("/api/extensions/skills", get(get_skills))
        .route("/api/extensions/channel-plugins", get(get_channel_plugins))
        .route("/api/extensions/settings-tabs", get(get_settings_tabs))
        .route(
            "/api/extensions/{extension_name}/assets/{*asset_path}",
            get(get_extension_asset),
        )
        .route("/api/extensions/webui", get(get_webui))
        .route("/api/extensions/agent-activity", get(get_agent_activity))
        // Query routes with body. i18n also supports GET for browser callers.
        .route("/api/extensions/i18n", get(get_i18n_query).post(get_i18n))
        .route("/api/extensions/permissions", post(get_permissions))
        .route("/api/extensions/risk-level", post(get_risk_level))
        // Management routes
        .route("/api/extensions/enable", post(enable_extension))
        .route("/api/extensions/disable", post(disable_extension))
        .route("/api/plugins/install", post(install_plugin))
        .route("/api/plugins/update", post(update_plugin))
        .route("/api/plugins/uninstall", post(uninstall_plugin))
        .route("/api/plugins/enable", post(enable_plugin))
        .route("/api/plugins/disable", post(disable_plugin))
        .route(
            "/api/plugins/linear/connections",
            get(list_linear_connections).post(connect_new_linear_connection),
        )
        .route(
            "/api/plugins/linear/migrate-legacy",
            post(migrate_legacy_linear_connection),
        )
        .route(
            "/api/plugins/linear/connections/{connection_id}/connect",
            post(connect_linear_connection),
        )
        .route(
            "/api/plugins/linear/connections/{connection_id}/disconnect",
            post(disconnect_linear_connection),
        )
        .route(
            "/api/plugins/notion/connections",
            get(list_notion_connections).post(connect_new_notion_connection),
        )
        .route(
            "/api/plugins/notion/connections/{connection_id}/connect",
            post(connect_notion_connection),
        )
        .route(
            "/api/plugins/notion/connections/{connection_id}/disconnect",
            post(disconnect_notion_connection),
        )
        .route(
            "/api/plugins/clockify/connections",
            get(list_clockify_connections).post(connect_new_clockify_connection),
        )
        .route(
            "/api/plugins/clockify/connections/{connection_id}/connect",
            post(reconnect_clockify_connection),
        )
        .route(
            "/api/plugins/clockify/connections/{connection_id}/disconnect",
            post(disconnect_clockify_connection),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Query handlers
// ---------------------------------------------------------------------------

/// `GET /api/extensions` — list all loaded extensions.
async fn get_loaded_extensions(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<ExtensionSummaryResponse>>>, ApiError> {
    let summaries = state.registry.get_loaded_extensions_for_user(&user.id).await;
    let mut resp = Vec::with_capacity(summaries.len());
    for s in summaries {
        let source_str = serde_json::to_value(s.source)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "local".to_string());
        let source_provenance = if source_str == "local" {
            match &state.plugin_installer {
                Some(installer) => match installer.provenance(&s.name).await {
                    Ok(provenance) => provenance.map(provenance_response),
                    Err(error) => {
                        tracing::warn!(plugin = %s.name, error = %error, "plugin provenance could not be read");
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };
        let compatibility = s.compatibility;
        resp.push(ExtensionSummaryResponse {
            name: s.name,
            version: s.version,
            display_name: s.display_name,
            description: s.description,
            enabled: s.enabled,
            source: source_str,
            package_format: compatibility
                .as_ref()
                .and_then(|value| serde_json::to_value(value.format).ok())
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "native".into()),
            supported_components: compatibility
                .as_ref()
                .map(|value| value.supported_components.clone())
                .unwrap_or_default(),
            unsupported_components: compatibility
                .as_ref()
                .map(|value| {
                    value
                        .unsupported_components
                        .iter()
                        .map(|notice| aionui_api_types::PluginCompatibilityNoticeResponse {
                            component: notice.component.clone(),
                            message: notice.message.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            compatibility_warnings: compatibility
                .as_ref()
                .map(|value| {
                    value
                        .warnings
                        .iter()
                        .map(|notice| aionui_api_types::PluginCompatibilityNoticeResponse {
                            component: notice.component.clone(),
                            message: notice.message.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            source_provenance,
        });
    }
    Ok(Json(ApiResponse::ok(resp)))
}

fn provenance_response(provenance: PluginInstallProvenance) -> aionui_api_types::PluginSourceProvenanceResponse {
    match provenance {
        PluginInstallProvenance::LocalPath { path } => aionui_api_types::PluginSourceProvenanceResponse {
            source_type: "local_path".into(),
            path: Some(path.to_string_lossy().into_owned()),
            url: None,
            git_ref: None,
            subdirectory: None,
            resolved_revision: None,
        },
        PluginInstallProvenance::Git {
            url,
            git_ref,
            subdirectory,
            resolved_revision,
        } => aionui_api_types::PluginSourceProvenanceResponse {
            source_type: "git".into(),
            path: None,
            url: Some(url),
            git_ref,
            subdirectory: subdirectory.map(|path| path.to_string_lossy().into_owned()),
            resolved_revision: Some(resolved_revision),
        },
    }
}

/// `GET /api/extensions/themes` — get all resolved themes.
async fn get_themes(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let themes = state.registry.get_themes_for_user(&user.id).await;
    let timestamp = now_ms();
    let value = serde_json::Value::Array(
        themes
            .into_iter()
            .map(|theme| {
                serde_json::json!({
                    "id": format!("ext-{}-{}", theme.extension_name, theme.id),
                    "name": format!("{} ({})", theme.name, theme.extension_name),
                    "cover": theme.cover_image,
                    "css": theme.css_content,
                    "is_preset": true,
                    "created_at": timestamp,
                    "updated_at": timestamp,
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/assistants` — get all resolved assistants.
async fn get_assistants(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let assistants = state.registry.get_assistants_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        assistants
            .into_iter()
            .map(|assistant| {
                serde_json::json!({
                    "id": format!("ext-{}", assistant.id),
                    "name": assistant.name,
                    "description": assistant.description,
                    "avatar": assistant.icon,
                    "agentId": assistant.agent_id,
                    "context": assistant.context.unwrap_or_default(),
                    "models": assistant.models,
                    "enabledSkills": assistant.enabled_skills,
                    "prompts": assistant.prompts,
                    "isPreset": true,
                    "isBuiltin": false,
                    "enabled": true,
                    "_source": "extension",
                    "_extensionName": assistant.extension_name,
                    "_kind": "assistant",
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/acp-adapters` — get all resolved ACP adapters.
async fn get_acp_adapters(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let adapters = state.registry.get_acp_adapters_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        adapters
            .into_iter()
            .map(|adapter| {
                let cli_command = adapter.cli_command.clone();
                let default_cli_path = adapter.default_cli_path.clone().or_else(|| cli_command.clone());
                serde_json::json!({
                    "id": adapter.id,
                    "name": adapter.name,
                    "description": adapter.description,
                    "cliCommand": cli_command,
                    "defaultCliPath": default_cli_path,
                    "acpArgs": adapter.acp_args,
                    "env": adapter.env,
                    "avatar": adapter.avatar,
                    "authRequired": adapter.auth_required,
                    "supportsStreaming": adapter.supports_streaming.unwrap_or(false),
                    "connectionType": adapter.connection_type.unwrap_or_else(|| "cli".to_string()),
                    "endpoint": adapter.endpoint,
                    "models": adapter.models,
                    "yoloMode": adapter.yolo_mode,
                    "healthCheck": adapter.health_check,
                    "apiKeyFields": adapter.api_key_fields,
                    "isPreset": false,
                    "isBuiltin": false,
                    "enabled": true,
                    "_source": "extension",
                    "_extensionName": adapter.extension_name,
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/agents` — get all resolved agents.
async fn get_agents(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let agents = state.registry.get_agents_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        agents
            .into_iter()
            .map(|agent| {
                serde_json::json!({
                    "id": format!("ext-{}", agent.id),
                    "name": agent.name,
                    "description": agent.description,
                    "avatar": agent.icon,
                    "agentType": agent.agent_type,
                    "context": agent.context.unwrap_or_default(),
                    "models": agent.models,
                    "enabledSkills": agent.enabled_skills,
                    "prompts": agent.prompts,
                    "isPreset": true,
                    "isBuiltin": false,
                    "enabled": true,
                    "_source": "extension",
                    "_extensionName": agent.extension_name,
                    "_kind": "agent",
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/mcp-servers` — get all resolved MCP servers.
async fn get_mcp_servers(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let servers = state.registry.get_mcp_servers_for_user(&user.id).await;
    let timestamp = now_ms();
    let mut values = servers
        .into_iter()
        .map(|server| {
            let enabled = server
                .config
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let transport = server
                .config
                .get("transport")
                .cloned()
                .unwrap_or_else(|| imported_transport_json(&server.config));
            let original_transport = transport.clone();
            let original_json = serde_json::json!({
                "name": server.name,
                "description": server.description,
                "enabled": enabled,
                "transport": original_transport,
            });
            serde_json::json!({
                "id": format!("ext-{}-{}", server.extension_name, server.name),
                "name": server.name,
                "description": server.description,
                "enabled": enabled,
                "transport": transport,
                "created_at": timestamp,
                "updated_at": timestamp,
                "original_json": serde_json::to_string_pretty(&original_json).unwrap_or_default(),
                "_source": "extension",
                "_extensionName": server.extension_name,
            })
        })
        .collect::<Vec<_>>();
    values.extend(
        state
            .registry
            .get_linear_technical_servers_for_user(&user.id)
            .await
            .into_iter()
            .map(|server| {
                let transport = session_transport_json(server.transport);
                let original_json = serde_json::json!({
                    "name": server.name,
                    "enabled": true,
                    "transport": transport,
                });
                serde_json::json!({
                    "id": server.id,
                    "name": server.name,
                    "description": "Linear MCP connection managed in Settings → Plugins",
                    "enabled": true,
                    "transport": transport,
                    "created_at": timestamp,
                    "updated_at": timestamp,
                    "original_json": serde_json::to_string_pretty(&original_json).unwrap_or_default(),
                    "_source": "extension",
                    "_extensionName": "linear",
                })
            }),
    );
    values.extend(
        state
            .registry
            .get_notion_technical_servers_for_user(&user.id)
            .await
            .into_iter()
            .map(|server| {
                let transport = session_transport_json(server.transport);
                let original_json = serde_json::json!({
                    "name": server.name,
                    "enabled": true,
                    "transport": transport,
                });
                serde_json::json!({
                    "id": server.id,
                    "name": server.name,
                    "description": "Notion MCP connection managed in Settings → Plugins",
                    "enabled": true,
                    "transport": transport,
                    "created_at": timestamp,
                    "updated_at": timestamp,
                    "original_json": serde_json::to_string_pretty(&original_json).unwrap_or_default(),
                    "_source": "extension",
                    "_extensionName": "notion",
                })
            }),
    );
    Ok(Json(ApiResponse::ok(serde_json::Value::Array(values))))
}

fn session_transport_json(transport: SessionMcpTransport) -> serde_json::Value {
    match transport {
        SessionMcpTransport::Stdio { command, args, env } => {
            let env = env
                .into_keys()
                .map(|key| (key, "<private>".to_string()))
                .collect::<HashMap<_, _>>();
            json!({ "type": "stdio", "command": command, "args": args, "env": env })
        }
        SessionMcpTransport::Http { url, headers } => json!({ "type": "http", "url": url, "headers": headers }),
        SessionMcpTransport::Sse { url, headers } => json!({ "type": "sse", "url": url, "headers": headers }),
        SessionMcpTransport::StreamableHttp { url, headers } => {
            json!({ "type": "streamable_http", "url": url, "headers": headers })
        }
    }
}

fn imported_transport_json(config: &serde_json::Value) -> serde_json::Value {
    let kind = config.get("type").and_then(serde_json::Value::as_str);
    let private_map = |name: &str| {
        config
            .get(name)
            .and_then(serde_json::Value::as_object)
            .map(|values| {
                values
                    .keys()
                    .map(|key| (key.clone(), serde_json::Value::String("<private>".into())))
                    .collect::<serde_json::Map<_, _>>()
            })
            .unwrap_or_default()
    };
    match kind {
        Some("stdio") => json!({
            "type": "stdio",
            "command": config.get("command"),
            "args": config.get("args").cloned().unwrap_or_else(|| json!([])),
            "env": private_map("env"),
        }),
        Some("http" | "sse" | "streamable_http") => json!({
            "type": kind,
            "url": config.get("url"),
            "headers": private_map("headers"),
        }),
        _ => serde_json::Value::Null,
    }
}

/// `GET /api/extensions/skills` — get all resolved skills.
async fn get_skills(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let skills = state.registry.get_skills_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        skills
            .into_iter()
            .map(|skill| {
                serde_json::json!({
                    "name": skill.name,
                    "description": skill.description.unwrap_or_else(|| format!("Skill from extension: {}", skill.extension_name)),
                    "location": skill.path,
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/channel-plugins` — get all resolved channel plugins.
async fn get_channel_plugins(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let plugins = state.registry.get_channel_plugins_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        plugins
            .into_iter()
            .map(|plugin| {
                serde_json::json!({
                    "id": plugin.id,
                    "type": plugin.id,
                    "name": plugin.name,
                    "platform": plugin.platform,
                    "entryPoint": plugin.entry_point,
                    "enabled": true,
                    "connected": false,
                    "active_users": 0,
                    "has_token": false,
                    "is_extension": true,
                    "extension_meta": {
                        "credentialFields": plugin.credential_fields,
                        "configFields": plugin.config_fields,
                        "description": plugin.description,
                        "extensionName": plugin.extension_name,
                        "icon": plugin.icon,
                    },
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/settings-tabs` — get all resolved settings tabs.
async fn get_settings_tabs(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let tabs = state.registry.get_settings_tabs_for_user(&user.id).await;
    let value = serde_json::to_value(&tabs).unwrap_or_default();
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/{extension_name}/assets/{*asset_path}` — serve an
/// extension asset under the trusted extension root.
async fn get_extension_asset(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((extension_name, asset_path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let ext = state
        .registry
        .get_extension_by_name_for_user(&user.id, &extension_name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {extension_name}")))?;
    if !ext.state.enabled {
        return Err(ApiError::NotFound(format!("Extension not found: {extension_name}")));
    }

    let canonical_root = tokio::fs::canonicalize(&ext.directory)
        .await
        .map_err(map_asset_lookup_error)?;

    let relative_path = normalize_relative_asset_path(&asset_path).ok_or_else(|| {
        ApiError::Forbidden(format!(
            "Asset path escapes extension root: {extension_name}/{asset_path}"
        ))
    })?;

    let requested_path = canonical_root.join(&relative_path);
    let canonical_asset = tokio::fs::canonicalize(&requested_path)
        .await
        .map_err(map_asset_lookup_error)?;

    if !canonical_asset.starts_with(&canonical_root) {
        return Err(ApiError::Forbidden(format!(
            "Asset path escapes extension root: {}",
            canonical_asset.display()
        )));
    }

    let bytes = tokio::fs::read(&canonical_asset)
        .await
        .map_err(map_asset_lookup_error)?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type_for_path(&canonical_asset))
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(bytes))
        .map_err(|err| ApiError::Internal(err.to_string()))
}

/// `GET /api/extensions/webui` — get all WebUI contributions.
async fn get_webui(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let webui = state.registry.get_webui_contributions_for_user(&user.id).await;
    let value = serde_json::to_value(&webui).unwrap_or_default();
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/agent-activity` — get agent activity snapshot.
///
/// Returns an empty object as a placeholder; real implementation will
/// integrate with the agent subsystem's activity tracking.
async fn get_agent_activity(
    State(_state): State<ExtensionRouterState>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    // Agent activity snapshot is a cross-module concern;
    // return an empty object for now.
    Ok(Json(ApiResponse::ok(serde_json::json!({}))))
}

/// `POST /api/extensions/i18n` — get i18n data for a locale.
async fn get_i18n(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<GetI18nRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<HashMap<String, HashMap<String, String>>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let data = state.registry.get_i18n_for_locale_for_user(&user.id, &req.locale).await;
    Ok(Json(ApiResponse::ok(data)))
}

/// `GET /api/extensions/i18n?locale=...` — browser-safe i18n lookup.
async fn get_i18n_query(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(req): Query<GetI18nRequest>,
) -> Result<Json<ApiResponse<HashMap<String, HashMap<String, String>>>>, ApiError> {
    let data = state.registry.get_i18n_for_locale_for_user(&user.id, &req.locale).await;
    Ok(Json(ApiResponse::ok(data)))
}

/// `POST /api/extensions/permissions` — get permission summary for an extension.
async fn get_permissions(
    State(state): State<ExtensionRouterState>,
    body: Result<Json<GetPermissionsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PermissionSummaryResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    let ext = state
        .registry
        .get_extension_by_name(&req.name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {}", req.name)))?;

    let permissions = ext.manifest.permissions.clone().unwrap_or_default();
    let summary = build_permission_summary(&permissions);
    let risk_level = calculate_risk_level(&permissions);

    let details: Vec<PermissionDetailResponse> = summary
        .details
        .into_iter()
        .map(|d| PermissionDetailResponse {
            permission: d.permission,
            level: enum_to_string(&d.level),
            description: d.description,
        })
        .collect();

    let resp = PermissionSummaryResponse {
        permissions: serde_json::to_value(&permissions).unwrap_or_default(),
        risk_level: enum_to_string(&risk_level),
        details,
    };

    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/extensions/risk-level` — get risk level for an extension.
async fn get_risk_level(
    State(state): State<ExtensionRouterState>,
    body: Result<Json<GetRiskLevelRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    let ext = state
        .registry
        .get_extension_by_name(&req.name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {}", req.name)))?;

    let permissions = ext.manifest.permissions.clone().unwrap_or_default();
    let risk_level = calculate_risk_level(&permissions);

    Ok(Json(ApiResponse::ok(
        serde_json::json!({ "riskLevel": enum_to_string(&risk_level) }),
    )))
}

// ---------------------------------------------------------------------------
// Management handlers
// ---------------------------------------------------------------------------

/// `POST /api/extensions/enable` — enable an extension.
async fn enable_extension(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<EnableExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.registry.enable_extension_for_user(&user.id, &req.name).await?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/extensions/disable` — disable an extension.
async fn disable_extension(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<DisableExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .registry
        .disable_extension_for_user(&user.id, &req.name, req.reason.as_deref())
        .await?;
    Ok(Json(ApiResponse::success()))
}

#[derive(Debug, Deserialize)]
struct InstallPluginRequest {
    #[serde(flatten)]
    source: PluginInstallSource,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PluginBindingScope {
    #[default]
    Global,
    Project,
}

#[derive(Debug, Deserialize)]
struct PluginBindingRequest {
    name: String,
    #[serde(default)]
    scope: PluginBindingScope,
    #[serde(default)]
    project_id: Option<String>,
}

async fn install_plugin(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<InstallPluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    if !state.allow_machine_plugin_mutations || user.user_type != UserType::Local {
        return Err(ApiError::Forbidden(
            "plugin installation is restricted to the local desktop user".into(),
        ));
    }
    let Json(req) = body.map_err(ApiError::from)?;
    let installer = state
        .plugin_installer
        .as_ref()
        .ok_or_else(|| ApiError::Internal("plugin installer is unavailable".into()))?;
    let installed = installer.install(req.source).await?;
    Ok(Json(ApiResponse::ok(json!({
        "name": installed.name,
        "version": installed.version,
        "directory": installed.directory,
        "provenance": provenance_response(installed.provenance),
    }))))
}

#[derive(Debug, Deserialize)]
struct UninstallPluginRequest {
    name: String,
}

async fn update_plugin(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<UninstallPluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    if !state.allow_machine_plugin_mutations || user.user_type != UserType::Local {
        return Err(ApiError::Forbidden(
            "plugin updates are restricted to the local desktop user".into(),
        ));
    }
    let Json(req) = body.map_err(ApiError::from)?;
    let installer = state
        .plugin_installer
        .as_ref()
        .ok_or_else(|| ApiError::Internal("plugin installer is unavailable".into()))?;
    let installed = installer.update(&req.name).await?;
    Ok(Json(ApiResponse::ok(json!({
        "name": installed.name,
        "version": installed.version,
        "directory": installed.directory,
        "provenance": provenance_response(installed.provenance),
    }))))
}

async fn uninstall_plugin(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<UninstallPluginRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    if !state.allow_machine_plugin_mutations || user.user_type != UserType::Local {
        return Err(ApiError::Forbidden(
            "plugin removal is restricted to the local desktop user".into(),
        ));
    }
    let Json(req) = body.map_err(ApiError::from)?;
    let installer = state
        .plugin_installer
        .as_ref()
        .ok_or_else(|| ApiError::Internal("plugin installer is unavailable".into()))?;
    installer.uninstall(&req.name).await?;
    Ok(Json(ApiResponse::success()))
}

async fn enable_plugin(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<PluginBindingRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    set_plugin_binding(&state.registry, &user.id, req, true).await?;
    Ok(Json(ApiResponse::success()))
}

async fn disable_plugin(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<PluginBindingRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    set_plugin_binding(&state.registry, &user.id, req, false).await?;
    Ok(Json(ApiResponse::success()))
}

async fn set_plugin_binding(
    registry: &ExtensionRegistry,
    user_id: &str,
    request: PluginBindingRequest,
    enabled: bool,
) -> Result<(), ApiError> {
    match request.scope {
        PluginBindingScope::Global => {
            if enabled {
                registry.enable_extension_for_user(user_id, &request.name).await?;
            } else {
                registry
                    .disable_extension_for_user(user_id, &request.name, Some("global plugin binding"))
                    .await?;
            }
        }
        PluginBindingScope::Project => {
            let project_id = request
                .project_id
                .as_deref()
                .ok_or_else(|| ApiError::BadRequest("project_id is required for project scope".into()))?;
            registry
                .set_extension_enabled_for_project(user_id, project_id, &request.name, enabled)
                .await?;
        }
    }
    Ok(())
}

async fn list_linear_connections(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<LinearConnectionsResponse>>, ApiError> {
    let connections = state
        .registry
        .list_linear_connections(&user.id)
        .await
        .map_err(linear_api_error)?;
    Ok(Json(ApiResponse::ok(connections)))
}

async fn migrate_legacy_linear_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<LinearConnectionsResponse>>, ApiError> {
    let connections = state
        .registry
        .migrate_legacy_linear_connection(&user.id)
        .await
        .map_err(linear_api_error)?;
    Ok(Json(ApiResponse::ok(connections)))
}

async fn connect_new_linear_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<(StatusCode, Json<ApiResponse<LinearConnectionResponse>>), ApiError> {
    let connection = state
        .registry
        .connect_new_linear_connection(&user.id)
        .await
        .map_err(linear_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(connection))))
}

async fn connect_linear_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(connection_id): Path<String>,
    body: Result<Json<ConnectLinearConnectionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<LinearConnectionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let connection = state
        .registry
        .connect_linear_connection(&user.id, &connection_id, request.reconnect)
        .await
        .map_err(linear_api_error)?;
    Ok(Json(ApiResponse::ok(connection)))
}

async fn disconnect_linear_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(connection_id): Path<String>,
) -> Result<Json<ApiResponse<LinearConnectionResponse>>, ApiError> {
    let connection = state
        .registry
        .disconnect_linear_connection(&user.id, &connection_id)
        .await
        .map_err(linear_api_error)?;
    Ok(Json(ApiResponse::ok(connection)))
}

fn linear_api_error(error: LinearConnectionError) -> ApiError {
    match error {
        LinearConnectionError::NotFound => ApiError::NotFound(error.to_string()),
        LinearConnectionError::Probe(message) => {
            ApiError::coded(StatusCode::BAD_GATEWAY, "LINEAR_AUTH_FAILED", message, None)
        }
        LinearConnectionError::Persistence(message) => ApiError::Internal(message),
    }
}

/// GET: current authenticated user’s credential-free Notion connections.
async fn list_notion_connections(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<NotionConnectionsResponse>>, ApiError> {
    let connections = state
        .registry
        .list_notion_connections(&user.id)
        .await
        .map_err(notion_api_error)?;
    Ok(Json(ApiResponse::ok(connections)))
}

/// POST without body: authorize and verify a new independent Notion identity.
async fn connect_new_notion_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<(StatusCode, Json<ApiResponse<NotionConnectionResponse>>), ApiError> {
    let connection = state
        .registry
        .connect_new_notion_connection(&user.id)
        .await
        .map_err(notion_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(connection))))
}

/// POST {reconnect: bool}: replace this user’s connection authorization.
async fn connect_notion_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(connection_id): Path<String>,
    body: Result<Json<ConnectNotionConnectionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<NotionConnectionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let connection = state
        .registry
        .connect_notion_connection(&user.id, &connection_id, request.reconnect)
        .await
        .map_err(notion_api_error)?;
    Ok(Json(ApiResponse::ok(connection)))
}

/// POST without body: remove local credentials and invalidate active snapshots.
async fn disconnect_notion_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(connection_id): Path<String>,
) -> Result<Json<ApiResponse<NotionConnectionResponse>>, ApiError> {
    let connection = state
        .registry
        .disconnect_notion_connection(&user.id, &connection_id)
        .await
        .map_err(notion_api_error)?;
    Ok(Json(ApiResponse::ok(connection)))
}

fn notion_api_error(error: NotionConnectionError) -> ApiError {
    match error {
        NotionConnectionError::Duplicate | NotionConnectionError::LimitReached => {
            ApiError::coded(StatusCode::CONFLICT, "NOTION_ACCOUNT_CONFLICT", error.to_string(), None)
        }
        NotionConnectionError::NotFound => ApiError::NotFound(error.to_string()),
        NotionConnectionError::Probe(message) => {
            ApiError::coded(StatusCode::BAD_GATEWAY, "NOTION_AUTH_FAILED", message, None)
        }
        NotionConnectionError::Persistence(message) => ApiError::Internal(message),
    }
}

async fn list_clockify_connections(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<ClockifyConnectionsResponse>>, ApiError> {
    let connections = state
        .registry
        .list_clockify_connections(&user.id)
        .await
        .map_err(clockify_api_error)?;
    Ok(Json(ApiResponse::ok(connections)))
}

async fn connect_new_clockify_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ConnectClockifyConnectionRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ClockifyConnectionResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let connection = state
        .registry
        .connect_new_clockify_connection(&user.id, request.realm, request.api_key)
        .await
        .map_err(clockify_api_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(connection))))
}

async fn reconnect_clockify_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(connection_id): Path<String>,
    body: Result<Json<ConnectClockifyConnectionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ClockifyConnectionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let connection = state
        .registry
        .reconnect_clockify_connection(&user.id, &connection_id, request.realm, request.api_key)
        .await
        .map_err(clockify_api_error)?;
    Ok(Json(ApiResponse::ok(connection)))
}

async fn disconnect_clockify_connection(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(connection_id): Path<String>,
) -> Result<Json<ApiResponse<ClockifyConnectionResponse>>, ApiError> {
    let connection = state
        .registry
        .disconnect_clockify_connection(&user.id, &connection_id)
        .await
        .map_err(clockify_api_error)?;
    Ok(Json(ApiResponse::ok(connection)))
}

fn clockify_api_error(error: ClockifyConnectionError) -> ApiError {
    match error {
        ClockifyConnectionError::NotFound => ApiError::NotFound(error.to_string()),
        ClockifyConnectionError::InvalidInput(message) => ApiError::BadRequest(message),
        ClockifyConnectionError::Probe(message) => {
            ApiError::coded(StatusCode::BAD_GATEWAY, "CLOCKIFY_AUTH_FAILED", message, None)
        }
        ClockifyConnectionError::Persistence(message) => ApiError::Internal(message),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialize a serde enum to its JSON string representation.
fn enum_to_string<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn content_type_for_path(path: &FsPath) -> HeaderValue {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    HeaderValue::from_str(mime.as_ref()).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

fn map_asset_lookup_error(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound("Extension asset not found".into()),
        _ => ApiError::Internal(error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::state::ExtensionStateStore;
    use crate::{ExtensionSource, ScanPath};
    use aionui_realtime::BroadcastEventBus;

    fn make_state_with_machine_plugin_mutations(allow_machine_plugin_mutations: bool) -> ExtensionRouterState {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        std::mem::forget(tmp);
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        ExtensionRouterState {
            registry,
            plugin_installer: None,
            allow_machine_plugin_mutations,
        }
    }

    fn make_state() -> ExtensionRouterState {
        make_state_with_machine_plugin_mutations(true)
    }

    #[test]
    fn extension_routes_builds_router() {
        let state = make_state();
        let _router = extension_routes(state);
    }

    #[tokio::test]
    async fn extension_i18n_supports_read_only_query_requests() {
        let response = extension_routes(make_state())
            .layer(Extension(CurrentUser::local_default()))
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/i18n?locale=en-US")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert!(body["data"].is_object());
    }

    #[test]
    fn plugin_install_request_accepts_explicit_git_ref_and_subdirectory() {
        let request: InstallPluginRequest = serde_json::from_value(json!({
            "type": "git",
            "url": "https://github.com/example/plugins.git",
            "ref": "release/1.0",
            "subdirectory": "plugins/example"
        }))
        .unwrap();
        assert_eq!(
            request.source,
            PluginInstallSource::Git {
                url: "https://github.com/example/plugins.git".into(),
                git_ref: Some("release/1.0".into()),
                subdirectory: Some(PathBuf::from("plugins/example")),
            }
        );
    }

    #[test]
    fn extension_path_traversal_maps_to_bad_request() {
        let api_err = ApiError::from(ExtensionError::PathTraversal("../secret".into()));
        assert!(matches!(api_err, ApiError::BadRequest(_)));
    }

    #[test]
    fn extension_manifest_validation_maps_to_bad_request() {
        let api_err = ApiError::from(ExtensionError::ManifestValidation("test".into()));
        assert!(matches!(api_err, ApiError::BadRequest(_)));
    }

    #[test]
    fn extension_file_reference_not_found_maps_to_not_found() {
        let api_err = ApiError::from(ExtensionError::FileReferenceNotFound("missing.md".into()));
        assert!(matches!(api_err, ApiError::NotFound(_)));
    }

    #[test]
    fn extension_io_error_maps_to_internal() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let api_err = ApiError::from(ExtensionError::Io(io_err));
        assert!(matches!(api_err, ApiError::Internal(_)));
    }

    #[test]
    fn linear_probe_error_exposes_only_the_safe_actionable_message() {
        let message = "The Linear sign-in page could not be opened. Check your system browser settings and try again.";
        let error = linear_api_error(LinearConnectionError::Probe(message.to_owned()));
        assert_eq!(error.error_code(), "LINEAR_AUTH_FAILED");
        assert_eq!(error.public_message(), message);
    }

    #[test]
    fn technical_mcp_transport_redacts_private_environment_values() {
        let value = session_transport_json(SessionMcpTransport::Stdio {
            command: "npx".into(),
            args: vec!["mcp-remote".into()],
            env: HashMap::from([(
                "MCP_REMOTE_CONFIG_DIR".into(),
                "/private/app-data/plugin-auth/linear/account".into(),
            )]),
        });
        assert_eq!(value["env"]["MCP_REMOTE_CONFIG_DIR"], "<private>");
        assert!(!value.to_string().contains("/private/app-data"));
    }

    async fn make_router_with_extension() -> (Router, tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("hello");
        std::fs::create_dir_all(ext_dir.join("settings")).unwrap();
        std::fs::write(
            ext_dir.join("aion-extension.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name": "hello",
                "version": "1.0.0"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(ext_dir.join("settings").join("index.html"), "<h1>Hello</h1>").unwrap();

        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Env,
            }])
            .await
            .unwrap();

        (
            extension_routes(ExtensionRouterState {
                registry,
                plugin_installer: None,
                allow_machine_plugin_mutations: true,
            })
            .layer(Extension(CurrentUser::local_default())),
            tmp,
            ext_dir,
        )
    }

    #[tokio::test]
    async fn imported_mcp_route_reports_top_level_transport_without_private_values() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("portable");
        std::fs::create_dir_all(ext_dir.join("skills/safe")).unwrap();
        std::fs::write(
            ext_dir.join("plugin.json"),
            br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"portable","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("skills/safe/SKILL.md"),
            "---\nname: safe\ndescription: Safe fixture.\n---\n",
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("mcp.json"),
            br#"{"mcpServers":{"remote":{"type":"streamable-http","url":"https://example.invalid/mcp","headers":{"Authorization":"Bearer ${PLUGIN_TOKEN}"}}}}"#,
        )
        .unwrap();

        let registry = ExtensionRegistry::new(
            ExtensionStateStore::new(tmp.path().join("states.json")),
            Arc::new(BroadcastEventBus::new(64)),
            "1.0.0".into(),
        );
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root.clone(),
                source: ExtensionSource::Local,
            }])
            .await
            .unwrap();
        let response = extension_routes(ExtensionRouterState {
            registry,
            plugin_installer: None,
            allow_machine_plugin_mutations: true,
        })
        .layer(Extension(CurrentUser::local_default()))
        .oneshot(
            Request::builder()
                .uri("/api/extensions/mcp-servers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["data"][0]["transport"]["type"], "streamable_http");
        assert_eq!(body["data"][0]["transport"]["url"], "https://example.invalid/mcp");
        assert_eq!(body["data"][0]["transport"]["headers"]["Authorization"], "<private>");
        assert!(!body.to_string().contains("PLUGIN_TOKEN"));
    }

    #[tokio::test]
    async fn extension_catalog_serializes_compatibility_arrays_for_native_and_imported_plugins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ext_root = tmp.path().join("extensions");
        let native = ext_root.join("native");
        let imported = ext_root.join("imported");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::create_dir_all(imported.join("skills/safe")).unwrap();
        std::fs::write(
            native.join("aion-extension.json"),
            br#"{"name":"native","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            imported.join("plugin.json"),
            br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"imported","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            imported.join("skills/safe/SKILL.md"),
            "---\nname: safe\ndescription: Safe fixture.\n---\n",
        )
        .unwrap();
        std::fs::write(
            imported.join(".ellmsha-plugin-source.json"),
            br#"{"type":"git","url":"https://github.com/example/plugins.git","ref":"main","subdirectory":"plugins/imported","resolved_revision":"1111111111111111111111111111111111111111"}"#,
        )
        .unwrap();

        let registry = ExtensionRegistry::new(
            ExtensionStateStore::new(tmp.path().join("states.json")),
            Arc::new(BroadcastEventBus::new(64)),
            "1.0.0".into(),
        );
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root.clone(),
                source: ExtensionSource::Local,
            }])
            .await
            .unwrap();
        let installer = PluginInstaller::new(ext_root.clone(), registry.clone());
        let response = extension_routes(ExtensionRouterState {
            registry,
            plugin_installer: Some(installer),
            allow_machine_plugin_mutations: true,
        })
        .layer(Extension(CurrentUser::local_default()))
        .oneshot(Request::builder().uri("/api/extensions").body(Body::empty()).unwrap())
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let catalog = body["data"].as_array().unwrap();
        let native = catalog.iter().find(|item| item["name"] == "native").unwrap();
        let imported = catalog.iter().find(|item| item["name"] == "imported").unwrap();

        for item in [native, imported] {
            assert!(item["supported_components"].is_array());
            assert!(item["unsupported_components"].is_array());
            assert!(item["compatibility_warnings"].is_array());
        }
        assert_eq!(native["supported_components"], json!([]));
        assert_eq!(imported["supported_components"], json!(["skills"]));
        assert_eq!(imported["source_provenance"]["source_type"], "git");
        assert_eq!(imported["source_provenance"]["ref"], "main");
        assert_eq!(imported["source_provenance"]["subdirectory"], "plugins/imported");
        assert_eq!(
            imported["source_provenance"]["resolved_revision"],
            "1111111111111111111111111111111111111111"
        );
    }

    #[tokio::test]
    async fn get_extension_asset_serves_local_file() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/hello/assets/settings/index.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=3600");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes, "<h1>Hello</h1>");
    }

    #[tokio::test]
    async fn get_extension_asset_rejects_traversal() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/hello/assets/%2E%2E%2Fsecret.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_extension_asset_returns_not_found_for_missing_file() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/hello/assets/settings/missing.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_extension_asset_returns_not_found_for_unknown_extension() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/unknown/assets/settings/index.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn webui_identity_denies_machine_plugin_mutations_for_local_typed_admin() {
        for (path, body) in [
            (
                "/api/plugins/install",
                r#"{"type":"git","url":"https://example.invalid/plugin.git"}"#,
            ),
            ("/api/plugins/update", r#"{"name":"plugin"}"#),
            ("/api/plugins/uninstall", r#"{"name":"plugin"}"#),
        ] {
            let response = extension_routes(make_state_with_machine_plugin_mutations(false))
                .layer(Extension(CurrentUser::local_default()))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }

    #[tokio::test]
    async fn local_identity_allows_machine_plugin_mutations_to_reach_installer() {
        for (path, body) in [
            (
                "/api/plugins/install",
                r#"{"type":"git","url":"https://example.invalid/plugin.git"}"#,
            ),
            ("/api/plugins/update", r#"{"name":"plugin"}"#),
            ("/api/plugins/uninstall", r#"{"name":"plugin"}"#),
        ] {
            let response = extension_routes(make_state_with_machine_plugin_mutations(true))
                .layer(Extension(CurrentUser::local_default()))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        }
    }

    #[tokio::test]
    async fn remote_user_cannot_install_machine_wide_plugin() {
        let user = CurrentUser {
            id: "remote-user".into(),
            username: "remote-user".into(),
            user_type: UserType::Aionpro,
            status: aionui_db::UserStatus::Active,
        };
        let response = extension_routes(make_state())
            .layer(Extension(user))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/plugins/install")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"type":"local_path","path":"/tmp/plugin"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn remote_user_cannot_remove_machine_wide_plugin() {
        let user = CurrentUser {
            id: "remote-user".into(),
            username: "remote-user".into(),
            user_type: UserType::Aionpro,
            status: aionui_db::UserStatus::Active,
        };
        let response = extension_routes(make_state())
            .layer(Extension(user))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/plugins/uninstall")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"plugin"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
