//! Stable approval identities are supplied by trusted account registrations,
//! never inferred from MCP descriptions or deserialized conversation snapshots.
use aionui_api_types::SessionMcpServer;
use aionui_common::mcp_approvals::mcp_binding;
use std::collections::HashMap;

pub(crate) fn trusted_bindings(servers: &[SessionMcpServer]) -> HashMap<String, String> {
    let mut bindings = HashMap::new();
    for server in servers {
        // Last snapshot wins in both factories; an untrusted replacement removes
        // earlier provenance rather than inheriting its account authorization.
        bindings.remove(&server.name);
        if let Some(identity) = &server.approval_identity {
            bindings.insert(
                server.name.clone(),
                mcp_binding(&serde_json::json!(["managed-account", identity, server.name])),
            );
        }
    }
    bindings
}

#[cfg(test)]
#[path = "mcp_approval_test.rs"]
mod tests;
