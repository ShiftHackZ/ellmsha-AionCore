use std::collections::HashMap;

use aion_config::config::McpServerConfig;
use aion_protocol::{McpApprovalIdentity, McpApprovalPersistence};
use aionui_common::mcp_approvals::{McpApprovalStore, mcp_binding};

pub(super) struct NativeMcpApprovals {
    store: McpApprovalStore,
    bindings: HashMap<String, String>,
}

impl NativeMcpApprovals {
    pub(super) fn new(
        store: McpApprovalStore,
        servers: &HashMap<String, McpServerConfig>,
        trusted: &HashMap<String, String>,
    ) -> Self {
        Self {
            store,
            bindings: servers
                .iter()
                .map(|(name, config)| {
                    (
                        name.clone(),
                        trusted
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| mcp_binding(&serde_json::json!(["native", name, config]))),
                    )
                })
                .collect(),
        }
    }
}

impl McpApprovalPersistence for NativeMcpApprovals {
    fn supports(&self, identity: &McpApprovalIdentity) -> bool {
        self.bindings.contains_key(&identity.server)
    }

    fn allows(&self, identity: &McpApprovalIdentity) -> Result<bool, String> {
        let Some(binding) = self.bindings.get(&identity.server) else {
            return Ok(false);
        };
        self.store
            .allows(&identity.server, binding, &identity.tool)
            .map_err(|_| "MCP approval store unavailable".into())
    }

    fn grant(&self, identity: &McpApprovalIdentity) -> Result<(), String> {
        let binding = self
            .bindings
            .get(&identity.server)
            .ok_or("MCP server is not managed by this application")?;
        self.store
            .grant(&identity.server, binding, &identity.tool)
            .map_err(|_| "MCP approval could not be saved".into())
    }
}

#[cfg(test)]
#[path = "mcp_approval_test.rs"]
mod tests;
