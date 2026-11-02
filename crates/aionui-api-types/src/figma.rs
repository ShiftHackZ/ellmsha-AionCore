//! Account-management contract for the bundled Figma REST plugin.
use serde::{Deserialize, Serialize};

/// Last locally verified state; provider access is checked again by every tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FigmaConnectionStatus {
    Connected,
    Disconnected,
    ReconnectRequired,
    Error,
}

/// Identity verified by Figma's authenticated GET /v1/me endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FigmaVerifiedIdentity {
    /// Stable provider user ID, never inferred from email or display name.
    pub account_id: String,
    /// Provider handle, with the stable ID as fallback if absent.
    pub account_name: String,
    /// Email when returned by /v1/me; not a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
}

/// Public account metadata. Neither plaintext nor encrypted tokens are exposed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FigmaConnectionResponse {
    /// Opaque local connection UUID, scoped to the authenticated eLLMsha user.
    pub id: String,
    /// Stable, account-specific MCP server namespace.
    pub namespace: String,
    /// Human-readable verified account label.
    pub label: String,
    /// Last local verification state, not a promise of current provider access.
    pub status: FigmaConnectionStatus,
    /// Verified provider identity, retained after disconnect for account selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<FigmaVerifiedIdentity>,
    /// Credential-free actionable failure, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Creation time, Unix milliseconds.
    pub created_at: i64,
    /// Last local state update, Unix milliseconds.
    pub updated_at: i64,
}

/// All connections owned by the current authenticated eLLMsha user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FigmaConnectionsResponse {
    /// Credential-free account list; empty when none are configured.
    pub connections: Vec<FigmaConnectionResponse>,
}

/// Connect/reconnect input, deliberately neither Debug nor Serialize.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectFigmaConnectionRequest {
    /// User-supplied PAT, bounded and verified before encrypted persistence.
    pub personal_access_token: String,
    /// Browser import guard: when present, must match the authenticated eLLMsha user.
    #[serde(default)]
    pub expected_user_id: Option<String>,
}
