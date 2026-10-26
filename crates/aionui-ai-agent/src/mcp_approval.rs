//! Stable approval identities are supplied by trusted account registrations,
//! otherwise bind the original transport before runtime discovery adds ephemeral credentials.
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};
use aionui_common::mcp_approvals::mcp_binding;
use std::collections::HashMap;

const BROWSER_IDENTITY: &str = "builtin-browser-v1";

/// Bind the stored registration before launchers resolve bundled runtimes.
pub(crate) fn registration_identity(id: &str, name: &str, kind: &str, config: &serde_json::Value) -> String {
    if kind == "stdio" {
        let mut tagged = config.clone();
        let Some(object) = tagged.as_object_mut() else {
            return format!(
                "registration:{}",
                mcp_binding(&serde_json::json!([id, name, kind, config]))
            );
        };
        object.insert("type".into(), serde_json::json!("stdio"));
        if let Ok(transport) = serde_json::from_value::<SessionMcpTransport>(tagged) {
            let server = SessionMcpServer {
                id: id.into(),
                name: name.into(),
                transport,
                approval_identity: None,
                auto_approve_in_unattended: false,
            };
            if aionui_session::is_owned_browser_launcher(&crate::session_agent::session_server_to_spec(&server)) {
                return BROWSER_IDENTITY.into();
            }
        }
    }
    format!(
        "registration:{}",
        mcp_binding(&serde_json::json!([id, name, kind, config]))
    )
}

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
        } else {
            let spec = crate::session_agent::session_server_to_spec(server);
            let binding = if aionui_session::is_owned_browser_launcher(&spec) {
                // The exact catalog launcher is verified by the shared browser
                // boundary. Per-chat scope tokens and bridge ports are not an account.
                mcp_binding(&serde_json::json!(["managed-account", BROWSER_IDENTITY, server.name]))
            } else {
                mcp_binding(&serde_json::json!([
                    "configured-server",
                    server.id,
                    server.name,
                    server.transport
                ]))
            };
            bindings.insert(server.name.clone(), binding);
        }
    }
    bindings
}

#[cfg(test)]
#[path = "mcp_approval_test.rs"]
mod tests;
