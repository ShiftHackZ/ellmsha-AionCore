use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A. Extension query responses
// ---------------------------------------------------------------------------

/// Summary of a loaded extension returned by `GET /api/extensions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtensionSummaryResponse {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub source: String,
    pub package_format: String,
    #[serde(default)]
    pub supported_components: Vec<String>,
    #[serde(default)]
    pub unsupported_components: Vec<PluginCompatibilityNoticeResponse>,
    #[serde(default)]
    pub compatibility_warnings: Vec<PluginCompatibilityNoticeResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_provenance: Option<PluginSourceProvenanceResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCompatibilityNoticeResponse {
    pub component: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSourceProvenanceResponse {
    pub source_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdirectory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_revision: Option<String>,
}

/// Response for `GET /api/hub/extensions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubExtensionListResponse {
    pub extensions: Vec<HubExtensionListItem>,
}

/// Single item in the Hub extension list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubExtensionListItem {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub bundled: bool,
    pub status: String,
}

// ---------------------------------------------------------------------------
// B. Permission responses
// ---------------------------------------------------------------------------

/// Response for `POST /api/extensions/permissions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionSummaryResponse {
    pub permissions: serde_json::Value,
    pub risk_level: String,
    pub details: Vec<PermissionDetailResponse>,
}

/// A single permission entry in the summary response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionDetailResponse {
    pub permission: String,
    pub level: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// C. Extension management requests
// ---------------------------------------------------------------------------

/// Request body for `POST /api/hub/install` and `POST /api/hub/update`.
#[derive(Debug, Clone, Deserialize)]
pub struct InstallExtensionRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/enable`.
#[derive(Debug, Clone, Deserialize)]
pub struct EnableExtensionRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/disable`.
#[derive(Debug, Clone, Deserialize)]
pub struct DisableExtensionRequest {
    pub name: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// User-visible state of one independently authenticated Linear connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinearConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    ReconnectRequired,
    Error,
}

/// Identity returned by Linear after a successful MCP probe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinearVerifiedIdentity {
    pub account_id: String,
    pub account_name: String,
    pub account_email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_url: Option<String>,
}

/// Capabilities observed from the authenticated Linear MCP tool catalog.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinearVerifiedCapabilities {
    pub issues_read: bool,
    pub issues_write: bool,
    pub comments_read: bool,
    pub comments_write: bool,
    pub attachment_metadata: bool,
    pub attachment_images: bool,
}

/// One Linear connection. OAuth tokens and auth-directory paths are never serialized here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinearConnectionResponse {
    pub id: String,
    /// Stable, protocol-safe MCP server name assigned after identity verification.
    #[serde(default)]
    pub namespace: String,
    pub label: String,
    pub status: LinearConnectionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<LinearVerifiedIdentity>,
    #[serde(default)]
    pub capabilities: LinearVerifiedCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub migrated: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Linear connection state for the current user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinearConnectionsResponse {
    pub connections: Vec<LinearConnectionResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectLinearConnectionRequest {
    #[serde(default)]
    pub reconnect: bool,
}

/// User-visible state of one independently authorized Notion connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotionConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    ReconnectRequired,
    Error,
}

/// Identity returned by Notion after a successful MCP probe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotionVerifiedIdentity {
    /// Verified Notion user or bot ID; independent of its display name.
    pub account_id: String,
    /// Verified display name, falling back to the remote user ID when unnamed.
    pub account_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional email returned by Notion; never an authentication credential.
    pub account_email: Option<String>,
    /// Verified Notion workspace ID for this authorization.
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional workspace display name.
    pub workspace_name: Option<String>,
}

/// Capabilities observed from the authenticated Notion MCP tool catalog.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotionVerifiedCapabilities {
    /// The authenticated tool catalog exposes Notion search.
    pub search: bool,
    /// The authenticated tool catalog exposes page fetch.
    pub read: bool,
    /// The catalog exposes page creation or update; user permissions still apply.
    pub write: bool,
}

/// One Notion connection. OAuth tokens and auth-directory paths are never serialized here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotionConnectionResponse {
    /// Local opaque connection UUID, scoped to the authenticated eLLMsha user.
    pub id: String,
    /// Stable MCP namespace for the verified identity; rotates if authorization changes user/workspace.
    #[serde(default)]
    pub namespace: String,
    /// Display label derived from the verified remote identity.
    pub label: String,
    /// Current connection and authorization state.
    pub status: NotionConnectionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Verified identity; absent when disconnected or verification is incomplete.
    pub identity: Option<NotionVerifiedIdentity>,
    #[serde(default)]
    /// Capabilities observed during the most recent successful connection.
    pub capabilities: NotionVerifiedCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Credential-free recoverable authorization failure, if any.
    pub last_error: Option<String>,
    /// Connection creation time in Unix milliseconds.
    pub created_at: i64,
    /// Last connection state change in Unix milliseconds.
    pub updated_at: i64,
}

/// Notion connection state for the current user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotionConnectionsResponse {
    /// Connections owned by the current authenticated eLLMsha user.
    pub connections: Vec<NotionConnectionResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectNotionConnectionRequest {
    #[serde(default)]
    /// Starts fresh authorization for this connection; retained for parity with other plugin account APIs.
    pub reconnect: bool,
}

/// Clockify data region selected for an API-key connection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClockifyRealm {
    Global,
    Eu,
    Us,
    Uk,
    Au,
}

/// User-visible state of one independently authenticated Clockify connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClockifyConnectionStatus {
    Connected,
    Disconnected,
    ReconnectRequired,
    Error,
}

/// Identity returned by Clockify after an API key is verified.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClockifyVerifiedIdentity {
    pub account_id: String,
    pub account_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
}

/// One bounded workspace visible to the authenticated Clockify identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClockifyWorkspaceIdentity {
    pub id: String,
    pub name: String,
}

/// One Clockify connection. The encrypted API key is deliberately absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClockifyConnectionResponse {
    pub id: String,
    pub namespace: String,
    pub label: String,
    pub realm: ClockifyRealm,
    pub status: ClockifyConnectionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ClockifyVerifiedIdentity>,
    #[serde(default)]
    pub workspaces: Vec<ClockifyWorkspaceIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClockifyConnectionsResponse {
    pub connections: Vec<ClockifyConnectionResponse>,
}

/// API keys are entered by the user and verified before a connection is saved.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectClockifyConnectionRequest {
    pub api_key: String,
    pub realm: ClockifyRealm,
}

/// Request body for `POST /api/extensions/permissions`.
#[derive(Debug, Clone, Deserialize)]
pub struct GetPermissionsRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/risk-level`.
#[derive(Debug, Clone, Deserialize)]
pub struct GetRiskLevelRequest {
    pub name: String,
}

/// Request body for `POST /api/extensions/i18n`.
#[derive(Debug, Clone, Deserialize)]
pub struct GetI18nRequest {
    pub locale: String,
}

/// Response for Hub install/update/uninstall operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubOperationResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
}

/// Hub update info returned by `POST /api/hub/check-updates`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HubUpdateInfo {
    pub name: String,
    pub current_version: String,
    pub latest_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extension_summary_response_serde() {
        let resp = ExtensionSummaryResponse {
            name: "my-ext".into(),
            version: "1.0.0".into(),
            display_name: Some("My Extension".into()),
            description: None,
            enabled: true,
            source: "env".into(),
            package_format: "native".into(),
            supported_components: vec![],
            unsupported_components: vec![],
            compatibility_warnings: vec![],
            source_provenance: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "my-ext");
        assert_eq!(json["display_name"], "My Extension");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["supported_components"], json!([]));
        assert_eq!(json["unsupported_components"], json!([]));
        assert_eq!(json["compatibility_warnings"], json!([]));
        assert!(json.get("description").is_none());
    }

    #[test]
    fn test_hub_extension_list_item_serde() {
        let item = HubExtensionListItem {
            name: "cool-ext".into(),
            version: "2.0.0".into(),
            display_name: None,
            description: Some("Cool extension".into()),
            author: Some("Author".into()),
            icon: None,
            tags: vec!["tools".into()],
            bundled: false,
            status: "installed".into(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["status"], "installed");
        assert_eq!(json["tags"], json!(["tools"]));
        assert!(json.get("display_name").is_none());
    }

    #[test]
    fn test_install_extension_request_deserialize() {
        let raw = json!({"name": "test-ext"});
        let req: InstallExtensionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "test-ext");
    }

    #[test]
    fn test_enable_extension_request_deserialize() {
        let raw = json!({"name": "my-ext"});
        let req: EnableExtensionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "my-ext");
    }

    #[test]
    fn test_disable_extension_request_with_reason() {
        let raw = json!({"name": "bad-ext", "reason": "Security concern"});
        let req: DisableExtensionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "bad-ext");
        assert_eq!(req.reason.as_deref(), Some("Security concern"));
    }

    #[test]
    fn test_disable_extension_request_without_reason() {
        let raw = json!({"name": "ext"});
        let req: DisableExtensionRequest = serde_json::from_value(raw).unwrap();
        assert!(req.reason.is_none());
    }

    #[test]
    fn test_get_permissions_request() {
        let raw = json!({"name": "my-ext"});
        let req: GetPermissionsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "my-ext");
    }

    #[test]
    fn test_get_i18n_request() {
        let raw = json!({"locale": "zh-CN"});
        let req: GetI18nRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.locale, "zh-CN");
    }

    #[test]
    fn test_permission_summary_response_serde() {
        let resp = PermissionSummaryResponse {
            permissions: json!({"storage": true, "events": true}),
            risk_level: "moderate".into(),
            details: vec![PermissionDetailResponse {
                permission: "network".into(),
                level: "limited".into(),
                description: "Access to api.example.com only".into(),
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["permissions"]["storage"], true);
        assert_eq!(json["risk_level"], "moderate");
        assert_eq!(json["details"][0]["permission"], "network");
        assert_eq!(json["details"][0]["level"], "limited");
    }

    #[test]
    fn test_hub_operation_response_success() {
        let resp = HubOperationResponse {
            success: true,
            msg: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);
        assert!(json.get("msg").is_none());
    }

    #[test]
    fn test_hub_operation_response_failure() {
        let resp = HubOperationResponse {
            success: false,
            msg: Some("Extension not found".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["msg"], "Extension not found");
    }

    #[test]
    fn test_hub_update_info_serde() {
        let info = HubUpdateInfo {
            name: "ext".into(),
            current_version: "1.0.0".into(),
            latest_version: "2.0.0".into(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["current_version"], "1.0.0");
        assert_eq!(json["latest_version"], "2.0.0");
    }
}
