//! App-owned, exact-tool MCP grants. The file is re-read for every decision so
//! revocation affects existing sessions as well as newly constructed managers.
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

static STORE_LOCK: Mutex<()> = Mutex::new(());
const MAX_STORE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpToolGrant {
    pub id: String,
    pub server: String,
    pub tool: String,
    /// Digest of the host-selected server/account transport, never credentials.
    pub binding: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpApprovalStore {
    path: PathBuf,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGrants {
    grants: BTreeMap<String, McpToolGrant>,
}

/// Hash a canonical JSON value. Only the digest is persisted, not its inputs.
pub fn mcp_binding(value: &serde_json::Value) -> String {
    let mut canonical = value.clone();
    canonical.sort_all_objects();
    format!("{:x}", Sha256::digest(canonical.to_string().as_bytes()))
}

impl McpApprovalStore {
    pub fn new(data_dir: &Path, user_id: &str) -> Self {
        // Hash the complete ID, rather than normalizing two users onto one path.
        let user = mcp_binding(&serde_json::json!(user_id));
        Self {
            path: data_dir.join("mcp-approvals").join(format!("{user}.json")),
        }
    }

    pub fn list(&self) -> io::Result<Vec<McpToolGrant>> {
        let _lock = STORE_LOCK
            .lock()
            .map_err(|_| io::Error::other("MCP grant lock unavailable"))?;
        Ok(self.read()?.grants.into_values().collect())
    }

    pub fn allows(&self, server: &str, binding: &str, tool: &str) -> io::Result<bool> {
        Ok(self
            .list()?
            .iter()
            .any(|grant| grant.server == server && grant.binding == binding && grant.tool == tool))
    }

    pub fn grant(&self, server: &str, binding: &str, tool: &str) -> io::Result<()> {
        if server.is_empty() || tool.is_empty() || binding.is_empty() {
            return Err(io::Error::other("MCP grant identity is incomplete"));
        }
        let _lock = STORE_LOCK
            .lock()
            .map_err(|_| io::Error::other("MCP grant lock unavailable"))?;
        let mut state = self.read()?;
        let id = mcp_binding(&serde_json::json!([server, binding, tool]));
        if state.grants.contains_key(&id) {
            return Ok(());
        }
        state.grants.insert(
            id.clone(),
            McpToolGrant {
                id,
                server: server.into(),
                binding: binding.into(),
                tool: tool.into(),
            },
        );
        self.write(&state)
    }

    pub fn revoke(&self, id: &str) -> io::Result<bool> {
        let _lock = STORE_LOCK
            .lock()
            .map_err(|_| io::Error::other("MCP grant lock unavailable"))?;
        let mut state = self.read()?;
        let removed = state.grants.remove(id).is_some();
        if removed {
            self.write(&state)?;
        }
        Ok(removed)
    }

    fn read(&self) -> io::Result<StoredGrants> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(StoredGrants::default()),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.len() > MAX_STORE_BYTES {
            return Err(io::Error::other("MCP grant store is not a bounded regular file"));
        }
        let state: StoredGrants = serde_json::from_slice(&fs::read(&self.path)?)
            .map_err(|_| io::Error::other("MCP grant store is invalid"))?;
        if state.grants.iter().any(|(id, grant)| {
            id != &grant.id
                || *id != mcp_binding(&serde_json::json!([grant.server, grant.binding, grant.tool]))
                || grant.server.is_empty()
                || grant.binding.is_empty()
                || grant.tool.is_empty()
        }) {
            return Err(io::Error::other("MCP grant store identity is invalid"));
        }
        Ok(state)
    }

    fn write(&self, state: &StoredGrants) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("MCP grant path invalid"))?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let data = serde_json::to_vec(state).map_err(io::Error::other)?;
        if data.len() as u64 > MAX_STORE_BYTES {
            return Err(io::Error::other("MCP grant store capacity exceeded"));
        }
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut file, &data)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "mcp_approvals_test.rs"]
mod tests;
