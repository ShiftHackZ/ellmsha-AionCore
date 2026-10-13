use serde::{Deserialize, Serialize};

/// A saved exact-tool permission belonging to the authenticated app user.
#[derive(Debug, Serialize, Deserialize)]
pub struct McpApprovalResponse {
    /// Opaque grant identifier used for revocation. It is not a credential.
    pub id: String,
    /// Host-selected MCP server/account name, not a provider display alias.
    pub server: String,
    /// Original MCP tool name; no wildcard or category grants are supported.
    pub tool: String,
}

/// Result of revoking one exact MCP grant for the authenticated user.
#[derive(Debug, Serialize, Deserialize)]
pub struct RevokeMcpApprovalResponse {
    /// True if the grant existed and was removed; false when already absent.
    pub revoked: bool,
}
