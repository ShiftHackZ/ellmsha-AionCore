use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    LinearConnectionResponse, LinearConnectionStatus, LinearConnectionsResponse, LinearVerifiedCapabilities,
    LinearVerifiedIdentity, SessionMcpServer, SessionMcpTransport,
};
use aionui_common::now_ms;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::ExtensionError;

pub const LINEAR_PLUGIN_NAME: &str = "linear";
pub const LINEAR_MCP_URL: &str = "https://mcp.linear.app/mcp";
pub const MCP_REMOTE_VERSION: &str = "0.14.3";

const STORE_VERSION: u32 = 2;
const STORE_FILE: &str = "linear-connections.json";
const AUTH_ROOT: &str = "plugin-auth";
const MAX_ERROR_LEN: usize = 300;

/// Use the installed plugin revision's input adapter without changing account
/// identity, OAuth storage, or the upstream transport arguments. Older plugin
/// revisions remain usable through their original transport.
pub(crate) fn with_image_normalization(mut server: SessionMcpServer, plugin_dir: &Path) -> SessionMcpServer {
    let adapter = plugin_dir.join("skills/linear-project-work/scripts/linear-mcp-adapter.mjs");
    // Windows' npx.cmd requires shell resolution. Keep its existing launch
    // path until the adapter is verified there; the skill fallback still works.
    if cfg!(windows) || !adapter.is_file() {
        return server;
    }
    if let SessionMcpTransport::Stdio { command, args, .. } = &mut server.transport {
        let mut wrapped_args = vec![adapter.to_string_lossy().into_owned(), command.clone()];
        wrapped_args.append(args);
        *command = "node".into();
        *args = wrapped_args;
    }
    server
}

#[derive(Debug, thiserror::Error)]
pub enum LinearConnectionError {
    #[error("Linear connection not found")]
    NotFound,
    #[error("Linear authentication failed: {0}")]
    Probe(String),
    #[error("failed to persist Linear connection state: {0}")]
    Persistence(String),
}

/// Public, credential-free failure stages for an interactive Linear probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LinearProbeFailure {
    #[error("The Linear sign-in page could not be opened. Check your system browser settings and try again.")]
    BrowserLaunch,
    #[error("Linear sign-in timed out before authorization completed. Try connecting again.")]
    AuthorizationTimeout,
    #[error(
        "Linear authorized the connection, but the current account identity could not be verified. Try reconnecting."
    )]
    IdentityUnavailable,
    #[error("Linear authorization could not be completed. Try connecting again.")]
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct LinearProbeResult {
    pub identity: LinearVerifiedIdentity,
    pub capabilities: LinearVerifiedCapabilities,
}

#[async_trait]
pub trait LinearConnectionProbe: Send + Sync {
    async fn verify(
        &self,
        user_id: &str,
        connection_id: &str,
        auth_dir: &Path,
    ) -> Result<LinearProbeResult, LinearProbeFailure>;

    async fn has_legacy_oauth(&self, user_id: &str) -> Result<bool, String>;
}

#[derive(Clone)]
pub struct LinearConnectionManager {
    inner: Arc<LinearConnectionManagerInner>,
}

struct LinearConnectionManagerInner {
    store_path: PathBuf,
    auth_root: PathBuf,
    state: Mutex<PersistedLinearState>,
    operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    revisions: Mutex<HashMap<String, u64>>,
    probe: Arc<dyn LinearConnectionProbe>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedLinearState {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    users: BTreeMap<String, PersistedLinearUser>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedLinearUser {
    #[serde(default)]
    connections: BTreeMap<String, LinearConnectionResponse>,
}

const fn store_version() -> u32 {
    STORE_VERSION
}

impl LinearConnectionManager {
    pub fn new(data_dir: &Path, probe: Arc<dyn LinearConnectionProbe>) -> Result<Self, LinearConnectionError> {
        let store_path = data_dir.join(STORE_FILE);
        let auth_root = data_dir.join(AUTH_ROOT).join(LINEAR_PLUGIN_NAME);
        let mut state = load_state(&store_path)?;
        let mut normalized = state.version != STORE_VERSION;
        state.version = STORE_VERSION;
        for (user_id, user) in &mut state.users {
            for connection in user.connections.values_mut() {
                if connection.namespace.is_empty() {
                    connection.namespace = stable_namespace(connection);
                    normalized = true;
                }
                let auth_dir = auth_dir_for(&auth_root, user_id, &connection.id);
                if connection.status == LinearConnectionStatus::Connecting
                    || (connection.status == LinearConnectionStatus::Connected && !is_private_auth_dir(&auth_dir))
                {
                    connection.status = LinearConnectionStatus::ReconnectRequired;
                    connection.identity = None;
                    connection.capabilities = LinearVerifiedCapabilities::default();
                    connection.last_error =
                        Some("Local Linear authentication is missing. Reconnect and try again.".into());
                    connection.updated_at = now_ms();
                    normalized = true;
                }
            }
        }
        if normalized {
            persist_locked(&store_path, &state)?;
        }
        Ok(Self {
            inner: Arc::new(LinearConnectionManagerInner {
                store_path,
                auth_root,
                state: Mutex::new(state),
                operations: Mutex::new(HashMap::new()),
                revisions: Mutex::new(HashMap::new()),
                probe,
            }),
        })
    }

    pub async fn list(&self, user_id: &str) -> Result<LinearConnectionsResponse, LinearConnectionError> {
        let state = self.inner.state.lock().await;
        let user = state.users.get(user_id).cloned().unwrap_or_default();
        Ok(LinearConnectionsResponse {
            connections: user.connections.into_values().collect(),
        })
    }

    pub async fn migrate_legacy(&self, user_id: &str) -> Result<LinearConnectionsResponse, LinearConnectionError> {
        self.ensure_legacy_connection(user_id).await?;
        self.list(user_id).await
    }

    /// Start OAuth for a new identity and publish the connection only after the
    /// remote user has been verified.
    pub async fn connect_new(&self, user_id: &str) -> Result<LinearConnectionResponse, LinearConnectionError> {
        let manager = self.clone();
        let user_id = user_id.to_owned();
        tokio::spawn(async move { manager.connect_new_owned(&user_id).await })
            .await
            .map_err(|_| {
                LinearConnectionError::Probe(
                    "Linear authorization could not be completed. Try connecting again.".into(),
                )
            })?
    }

    /// Runs independently from the request task so an HTTP disconnect cannot
    /// skip credential cleanup while an OAuth callback is still active.
    async fn connect_new_owned(&self, user_id: &str) -> Result<LinearConnectionResponse, LinearConnectionError> {
        let connection_id = Uuid::now_v7().to_string();
        let operation = self.connection_operation(user_id, &connection_id).await;
        let _operation = operation.lock().await;
        let auth_dir = self.auth_dir(user_id, &connection_id);
        create_private_dir(&auth_dir)?;

        let verified = match self.inner.probe.verify(user_id, &connection_id, &auth_dir).await {
            Ok(verified) => verified,
            Err(error) => {
                let _ = remove_auth_dir(&auth_dir);
                return Err(LinearConnectionError::Probe(error.to_string()));
            }
        };
        let now = now_ms();
        let namespace = verified_namespace(&verified.identity, &connection_id);
        let connection = LinearConnectionResponse {
            id: connection_id,
            namespace,
            label: identity_label(&verified.identity),
            status: LinearConnectionStatus::Connected,
            identity: Some(verified.identity),
            capabilities: verified.capabilities,
            last_error: None,
            migrated: false,
            created_at: now,
            updated_at: now,
        };
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        next.users
            .entry(user_id.to_owned())
            .or_default()
            .connections
            .insert(connection.id.clone(), connection.clone());
        if let Err(error) = persist_and_replace(&self.inner.store_path, &mut state, next) {
            let _ = remove_auth_dir(&auth_dir);
            return Err(error);
        }
        self.bump_revision(user_id).await;
        Ok(connection)
    }

    pub async fn connect(
        &self,
        user_id: &str,
        connection_id: &str,
        reconnect: bool,
    ) -> Result<LinearConnectionResponse, LinearConnectionError> {
        let manager = self.clone();
        let user_id = user_id.to_owned();
        let connection_id = connection_id.to_owned();
        tokio::spawn(async move { manager.connect_owned(&user_id, &connection_id, reconnect).await })
            .await
            .map_err(|_| {
                LinearConnectionError::Probe("Linear authorization could not be completed. Try reconnecting.".into())
            })?
    }

    /// Runs independently from the request task so cancellation still reaches
    /// the reconnect-required state and removes partial credentials.
    async fn connect_owned(
        &self,
        user_id: &str,
        connection_id: &str,
        _reconnect: bool,
    ) -> Result<LinearConnectionResponse, LinearConnectionError> {
        validate_connection_id(connection_id)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        let auth_dir = self.auth_dir(user_id, connection_id);

        {
            let mut state = self.inner.state.lock().await;
            let mut next = state.clone();
            let connection = connection_mut(&mut next, user_id, connection_id)?;
            connection.status = LinearConnectionStatus::Connecting;
            connection.identity = None;
            connection.capabilities = LinearVerifiedCapabilities::default();
            connection.last_error = None;
            connection.updated_at = now_ms();
            persist_and_replace(&self.inner.store_path, &mut state, next)?;
            self.bump_revision(user_id).await;
        }

        let auth_setup = (|| {
            // Always start from an empty per-connection directory so an old
            // token cannot satisfy a new authorization attempt.
            remove_auth_dir(&auth_dir)?;
            create_private_dir(&auth_dir)
        })();
        if let Err(error) = auth_setup {
            let mut state = self.inner.state.lock().await;
            let connection = connection_mut(&mut state, user_id, connection_id)?;
            connection.status = LinearConnectionStatus::ReconnectRequired;
            connection.identity = None;
            connection.capabilities = LinearVerifiedCapabilities::default();
            connection.last_error =
                Some("Local Linear authentication could not be prepared. Reconnect and try again.".into());
            connection.updated_at = now_ms();
            // Recovery must remain visible in the live process even when the
            // same storage fault also prevents this best-effort write.
            let _ = persist_locked(&self.inner.store_path, &state);
            self.bump_revision(user_id).await;
            return Err(error);
        }

        let probe_result = self.inner.probe.verify(user_id, connection_id, &auth_dir).await;
        let cleanup_failed = probe_result.is_err() && remove_auth_dir(&auth_dir).is_err();

        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connection = connection_mut(&mut next, user_id, connection_id)?;
        match probe_result {
            Ok(verified) => {
                if connection.namespace.is_empty() || connection.namespace == fallback_namespace(connection_id) {
                    connection.namespace = verified_namespace(&verified.identity, connection_id);
                }
                connection.label = identity_label(&verified.identity);
                connection.status = LinearConnectionStatus::Connected;
                connection.identity = Some(verified.identity);
                connection.capabilities = verified.capabilities;
                connection.last_error = None;
            }
            Err(error) => {
                connection.status = LinearConnectionStatus::ReconnectRequired;
                connection.identity = None;
                connection.capabilities = LinearVerifiedCapabilities::default();
                connection.last_error = Some(if cleanup_failed {
                    "Linear verification failed and local authentication cleanup did not complete. Disconnect or reconnect and try again."
                        .into()
                } else {
                    error.to_string()
                });
            }
        }
        connection.updated_at = now_ms();
        let response = connection.clone();
        if let Err(error) = persist_and_replace(&self.inner.store_path, &mut state, next) {
            let cleanup_failed = remove_auth_dir(&auth_dir).is_err();
            if let Ok(connection) = connection_mut(&mut state, user_id, connection_id) {
                connection.status = LinearConnectionStatus::ReconnectRequired;
                connection.identity = None;
                connection.capabilities = LinearVerifiedCapabilities::default();
                connection.last_error = Some(if cleanup_failed {
                    "Linear connection state could not be saved and local authentication cleanup did not complete. Disconnect or reconnect and try again."
                        .into()
                } else {
                    "Linear connection state could not be saved. Reconnect and try again.".into()
                });
                connection.updated_at = now_ms();
            }
            // Keep the live process recoverable even when storage is still
            // unavailable. A restart also normalizes persisted `connecting`.
            let _ = persist_locked(&self.inner.store_path, &state);
            self.bump_revision(user_id).await;
            return Err(error);
        }
        self.bump_revision(user_id).await;
        if response.status == LinearConnectionStatus::Connected {
            Ok(response)
        } else {
            Err(LinearConnectionError::Probe(
                response
                    .last_error
                    .unwrap_or_else(|| "identity verification failed".into()),
            ))
        }
    }

    pub async fn disconnect(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<LinearConnectionResponse, LinearConnectionError> {
        validate_connection_id(connection_id)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        // Remove credentials before publishing a disconnected state. A failed
        // cleanup leaves the prior state visible so the user can retry it.
        remove_auth_dir(&self.auth_dir(user_id, connection_id))?;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connection = connection_mut(&mut next, user_id, connection_id)?;
        connection.status = LinearConnectionStatus::Disconnected;
        connection.identity = None;
        connection.capabilities = LinearVerifiedCapabilities::default();
        connection.last_error = None;
        connection.updated_at = now_ms();
        let response = connection.clone();
        let persisted = persist_and_replace(&self.inner.store_path, &mut state, next);
        // The auth directory is already gone, so invalidate active runtimes
        // even when the state file write fails.
        self.bump_revision(user_id).await;
        if let Err(error) = persisted {
            if let Ok(connection) = connection_mut(&mut state, user_id, connection_id) {
                connection.status = LinearConnectionStatus::ReconnectRequired;
                connection.identity = None;
                connection.capabilities = LinearVerifiedCapabilities::default();
                connection.last_error =
                    Some("Local Linear credentials were removed, but connection state could not be saved.".into());
                connection.updated_at = now_ms();
            }
            let _ = persist_locked(&self.inner.store_path, &state);
            return Err(error);
        }
        Ok(response)
    }

    pub async fn runtime_servers_snapshot(&self, user_id: &str) -> (u64, Vec<SessionMcpServer>) {
        let state = self.inner.state.lock().await;
        let revision = self.revision(user_id).await;
        let servers = state
            .users
            .get(user_id)
            .into_iter()
            .flat_map(|user| user.connections.values())
            .filter_map(|connection| self.server_for_connection(user_id, connection))
            .collect();
        (revision, servers)
    }

    pub async fn technical_servers(&self, user_id: &str) -> Vec<SessionMcpServer> {
        let state = self.inner.state.lock().await;
        state
            .users
            .get(user_id)
            .into_iter()
            .flat_map(|user| user.connections.values())
            .filter_map(|connection| self.server_for_connection(user_id, connection))
            .collect()
    }

    fn server_for_connection(&self, user_id: &str, connection: &LinearConnectionResponse) -> Option<SessionMcpServer> {
        if connection.status != LinearConnectionStatus::Connected || connection.identity.is_none() {
            return None;
        }
        let auth_dir = self.auth_dir(user_id, &connection.id);
        if !is_private_auth_dir(&auth_dir) {
            return None;
        }
        let runtime_id = format!("plugin:{LINEAR_PLUGIN_NAME}:{}", connection.id);
        Some(SessionMcpServer {
            id: runtime_id,
            name: connection.namespace.clone(),
            transport: SessionMcpTransport::Stdio {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    format!("mcp-remote@{MCP_REMOTE_VERSION}"),
                    LINEAR_MCP_URL.into(),
                    "--transport".into(),
                    "http-only".into(),
                ],
                env: HashMap::from([("MCP_REMOTE_CONFIG_DIR".into(), auth_dir.to_string_lossy().into_owned())]),
            },
            approval_identity: connection.identity.as_ref().map(|identity| {
                format!(
                    "linear:{}:{}:{}",
                    connection.id,
                    identity.account_id,
                    identity.workspace_id.as_deref().unwrap_or("")
                )
            }),
            auto_approve_in_unattended: false,
        })
    }

    async fn ensure_legacy_connection(&self, user_id: &str) -> Result<(), LinearConnectionError> {
        {
            let state = self.inner.state.lock().await;
            if state
                .users
                .get(user_id)
                .is_some_and(|user| !user.connections.is_empty())
            {
                return Ok(());
            }
        }
        if !self
            .inner
            .probe
            .has_legacy_oauth(user_id)
            .await
            .map_err(|error| LinearConnectionError::Persistence(truncate_error(&error)))?
        {
            return Ok(());
        }

        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let user = next.users.entry(user_id.to_owned()).or_default();
        if !user.connections.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        let connection_id = Uuid::now_v7().to_string();
        let connection = LinearConnectionResponse {
            namespace: fallback_namespace(&connection_id),
            id: connection_id,
            label: "Linear account".into(),
            status: LinearConnectionStatus::ReconnectRequired,
            identity: None,
            capabilities: LinearVerifiedCapabilities::default(),
            last_error: None,
            migrated: true,
            created_at: now,
            updated_at: now,
        };
        user.connections.insert(connection.id.clone(), connection);
        persist_and_replace(&self.inner.store_path, &mut state, next)?;
        self.bump_revision(user_id).await;
        Ok(())
    }

    fn auth_dir(&self, user_id: &str, connection_id: &str) -> PathBuf {
        auth_dir_for(&self.inner.auth_root, user_id, connection_id)
    }

    async fn connection_operation(&self, user_id: &str, connection_id: &str) -> Arc<Mutex<()>> {
        let key = format!("{user_id}\0{connection_id}");
        let mut operations = self.inner.operations.lock().await;
        operations
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn revision(&self, user_id: &str) -> u64 {
        self.inner.revisions.lock().await.get(user_id).copied().unwrap_or(0)
    }

    async fn bump_revision(&self, user_id: &str) {
        let mut revisions = self.inner.revisions.lock().await;
        let revision = revisions.entry(user_id.to_owned()).or_default();
        *revision = revision.saturating_add(1);
    }
}

fn auth_dir_for(auth_root: &Path, user_id: &str, connection_id: &str) -> PathBuf {
    let user_hash = hex::encode(Sha256::digest(user_id.as_bytes()));
    auth_root.join(&user_hash[..16]).join(connection_id)
}

fn is_private_auth_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
}

fn persist_and_replace(
    path: &Path,
    current: &mut PersistedLinearState,
    next: PersistedLinearState,
) -> Result<(), LinearConnectionError> {
    persist_locked(path, &next)?;
    *current = next;
    Ok(())
}

fn connection_mut<'a>(
    state: &'a mut PersistedLinearState,
    user_id: &str,
    connection_id: &str,
) -> Result<&'a mut LinearConnectionResponse, LinearConnectionError> {
    state
        .users
        .get_mut(user_id)
        .and_then(|user| user.connections.get_mut(connection_id))
        .ok_or(LinearConnectionError::NotFound)
}

fn identity_label(identity: &LinearVerifiedIdentity) -> String {
    let name = identity.account_name.trim();
    if name.is_empty() {
        identity.account_email.clone()
    } else {
        name.to_owned()
    }
}

fn verified_namespace(identity: &LinearVerifiedIdentity, connection_id: &str) -> String {
    let workspace = identity.workspace_name.as_deref().unwrap_or(&identity.account_id);
    format!(
        "linear-{}-{}-{}",
        namespace_segment(workspace),
        namespace_segment(&identity.account_email),
        connection_id
    )
}

fn stable_namespace(connection: &LinearConnectionResponse) -> String {
    connection
        .identity
        .as_ref()
        .map(|identity| verified_namespace(identity, &connection.id))
        .unwrap_or_else(|| fallback_namespace(&connection.id))
}

fn fallback_namespace(connection_id: &str) -> String {
    format!("linear-account-{connection_id}")
}

fn namespace_segment(value: &str) -> String {
    let mut segment = String::new();
    let mut needs_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if needs_separator && !segment.is_empty() && segment.len() < 32 {
                segment.push('-');
            }
            if segment.len() < 32 {
                segment.push(character.to_ascii_lowercase());
            }
            needs_separator = false;
        } else {
            needs_separator = true;
        }
    }
    if segment.is_empty() { "account".into() } else { segment }
}

fn validate_connection_id(connection_id: &str) -> Result<(), LinearConnectionError> {
    Uuid::parse_str(connection_id)
        .map(|_| ())
        .map_err(|_| LinearConnectionError::NotFound)
}

fn truncate_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_LEN).collect()
}

fn load_state(path: &Path) -> Result<PersistedLinearState, LinearConnectionError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| LinearConnectionError::Persistence(format!("invalid state file: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PersistedLinearState {
            version: STORE_VERSION,
            users: BTreeMap::new(),
        }),
        Err(error) => Err(LinearConnectionError::Persistence(error.to_string())),
    }
}

fn persist_locked(path: &Path, state: &PersistedLinearState) -> Result<(), LinearConnectionError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|error| LinearConnectionError::Persistence(error.to_string()))?;
    std::fs::write(&tmp_path, bytes).map_err(|error| LinearConnectionError::Persistence(error.to_string()))?;
    set_private_file_permissions(&tmp_path)?;
    crate::state::replace_state_file(&tmp_path, path)
        .map_err(|error| LinearConnectionError::Persistence(error.to_string()))
}

fn create_private_dir(path: &Path) -> Result<(), LinearConnectionError> {
    std::fs::create_dir_all(path).map_err(|error| LinearConnectionError::Persistence(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| LinearConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), LinearConnectionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| LinearConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

fn remove_auth_dir(path: &Path) -> Result<(), LinearConnectionError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(path).map_err(|error| LinearConnectionError::Persistence(error.to_string()))
        }
        Ok(_) => std::fs::remove_dir_all(path).map_err(|error| LinearConnectionError::Persistence(error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LinearConnectionError::Persistence(error.to_string())),
    }
}

impl From<LinearConnectionError> for ExtensionError {
    fn from(error: LinearConnectionError) -> Self {
        ExtensionError::StatePersistence(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct VerifiedProbe {
        legacy: bool,
    }

    struct SecretErrorProbe;

    struct FailAfterFirstProbe {
        calls: AtomicUsize,
    }

    struct FailFinalPersistenceProbe {
        calls: AtomicUsize,
        store_path: PathBuf,
    }

    struct BlockingFailureProbe {
        calls: AtomicUsize,
        succeed_first: bool,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    fn verified_result(connection_id: &str) -> LinearProbeResult {
        LinearProbeResult {
            identity: LinearVerifiedIdentity {
                account_id: format!("account-{connection_id}"),
                account_name: format!("Account {connection_id}"),
                account_email: format!("{connection_id}@example.test"),
                workspace_id: Some(format!("workspace-{connection_id}")),
                workspace_name: Some(format!("Workspace {connection_id}")),
                workspace_url: Some("https://linear.app/example".into()),
            },
            capabilities: LinearVerifiedCapabilities {
                issues_read: true,
                issues_write: true,
                comments_read: true,
                comments_write: true,
                attachment_metadata: true,
                attachment_images: true,
            },
        }
    }

    #[async_trait]
    impl LinearConnectionProbe for VerifiedProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<LinearProbeResult, LinearProbeFailure> {
            std::fs::write(auth_dir.join("probe-marker"), connection_id)
                .map_err(|_| LinearProbeFailure::Unavailable)?;
            Ok(verified_result(connection_id))
        }

        async fn has_legacy_oauth(&self, _user_id: &str) -> Result<bool, String> {
            Ok(self.legacy)
        }
    }

    #[async_trait]
    impl LinearConnectionProbe for SecretErrorProbe {
        async fn verify(
            &self,
            _user_id: &str,
            _connection_id: &str,
            _auth_dir: &Path,
        ) -> Result<LinearProbeResult, LinearProbeFailure> {
            Err(LinearProbeFailure::Unavailable)
        }

        async fn has_legacy_oauth(&self, _user_id: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[async_trait]
    impl LinearConnectionProbe for FailAfterFirstProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<LinearProbeResult, LinearProbeFailure> {
            std::fs::write(auth_dir.join("oauth-token"), "secret material")
                .map_err(|_| LinearProbeFailure::Unavailable)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(verified_result(connection_id))
            } else {
                Err(LinearProbeFailure::Unavailable)
            }
        }

        async fn has_legacy_oauth(&self, _user_id: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[async_trait]
    impl LinearConnectionProbe for FailFinalPersistenceProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<LinearProbeResult, LinearProbeFailure> {
            std::fs::write(auth_dir.join("oauth-token"), "secret material")
                .map_err(|_| LinearProbeFailure::Unavailable)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::remove_file(&self.store_path).map_err(|_| LinearProbeFailure::Unavailable)?;
                std::fs::create_dir(&self.store_path).map_err(|_| LinearProbeFailure::Unavailable)?;
            }
            Ok(verified_result(connection_id))
        }

        async fn has_legacy_oauth(&self, _user_id: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[async_trait]
    impl LinearConnectionProbe for BlockingFailureProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<LinearProbeResult, LinearProbeFailure> {
            std::fs::write(auth_dir.join("oauth-token"), "partial secret material")
                .map_err(|_| LinearProbeFailure::Unavailable)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 && self.succeed_first {
                return Ok(verified_result(connection_id));
            }
            self.started.notify_one();
            self.release.notified().await;
            Err(LinearProbeFailure::Unavailable)
        }

        async fn has_legacy_oauth(&self, _user_id: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    fn user_auth_root(manager: &LinearConnectionManager, user_id: &str) -> PathBuf {
        manager
            .inner
            .auth_root
            .join(&hex::encode(Sha256::digest(user_id.as_bytes()))[..16])
    }

    fn auth_dir(server: &SessionMcpServer) -> PathBuf {
        let SessionMcpTransport::Stdio { env, .. } = &server.transport else {
            panic!("Linear server must use stdio")
        };
        PathBuf::from(env.get("MCP_REMOTE_CONFIG_DIR").expect("isolated auth directory"))
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn image_adapter_preserves_account_binding_and_legacy_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        manager.connect_new("fixture-owner").await.unwrap();
        let (_, servers) = manager.runtime_servers_snapshot("fixture-owner").await;
        let server = servers.into_iter().next().unwrap();
        let baseline = serde_json::to_value(&server).unwrap();
        assert_eq!(
            serde_json::to_value(with_image_normalization(server.clone(), temp.path())).unwrap(),
            baseline
        );

        let adapter = temp
            .path()
            .join("skills/linear-project-work/scripts/linear-mcp-adapter.mjs");
        std::fs::create_dir_all(adapter.parent().unwrap()).unwrap();
        std::fs::write(&adapter, "// isolated transport fixture").unwrap();
        let wrapped = with_image_normalization(server.clone(), temp.path());
        assert_eq!(wrapped.id, server.id);
        assert_eq!(wrapped.name, server.name);
        assert_eq!(auth_dir(&wrapped), auth_dir(&server));
        let SessionMcpTransport::Stdio {
            command: before_command,
            args: before_args,
            env: before_env,
            ..
        } = server.transport
        else {
            panic!("expected stdio")
        };
        let SessionMcpTransport::Stdio { command, args, env, .. } = wrapped.transport else {
            panic!("expected stdio")
        };
        assert_eq!(command, "node");
        assert_eq!(args[0], adapter.to_string_lossy());
        assert_eq!(args[1], before_command);
        assert_eq!(&args[2..], before_args.as_slice());
        assert_eq!(env, before_env);
    }

    #[tokio::test]
    async fn verified_connections_are_all_available_with_stable_isolated_namespaces() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        let first = manager.connect_new("user-a").await.unwrap();
        let second = manager.connect_new("user-a").await.unwrap();

        assert_ne!(first.id, second.id);
        assert_eq!(
            first.label,
            first.identity.as_ref().unwrap().account_name,
            "the account label must come from verified OAuth identity"
        );
        let (initial_revision, servers) = manager.runtime_servers_snapshot("user-a").await;
        assert_eq!(servers.len(), 2);
        let first_server = servers.iter().find(|server| server.id.ends_with(&first.id)).unwrap();
        let second_server = servers.iter().find(|server| server.id.ends_with(&second.id)).unwrap();
        let first_auth = auth_dir(first_server);
        let second_auth = auth_dir(second_server);
        assert_ne!(first_server.id, second_server.id);
        assert_ne!(first_server.name, second_server.name);
        assert_eq!(first_server.name, first.namespace);
        assert_eq!(second_server.name, second.namespace);
        assert_ne!(first_auth, second_auth);
        assert!(!first_server.auto_approve_in_unattended);
        assert!(!second_server.auto_approve_in_unattended);
        assert!(first_server.name.ends_with(&first.id));
        assert!(second_server.name.ends_with(&second.id));
        assert!(
            first_server
                .name
                .chars()
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
        );
        assert!(first_auth.ends_with(&first.id));
        assert!(second_auth.ends_with(&second.id));

        let plugin_root = temp.path().join("plugins");
        let linear_plugin = plugin_root.join(LINEAR_PLUGIN_NAME);
        std::fs::create_dir_all(&linear_plugin).unwrap();
        std::fs::write(
            linear_plugin.join(crate::constants::EXTENSION_MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "name": LINEAR_PLUGIN_NAME,
                "version": "1.0.0",
                "apiVersion": "1.0.0",
                "contributes": {}
            }))
            .unwrap(),
        )
        .unwrap();
        let registry = crate::registry::ExtensionRegistry::new_with_linear_connections(
            crate::state::ExtensionStateStore::new(temp.path().join("extension-states.json")),
            Arc::new(aionui_realtime::BroadcastEventBus::new(8)),
            "1.0.0".into(),
            Some(manager.clone()),
        );
        registry
            .initialize_with_scan_paths(vec![crate::loader::ScanPath {
                path: plugin_root,
                source: crate::types::ExtensionSource::Local,
            }])
            .await
            .unwrap();

        // Parent, child, and unscoped sessions receive the same independently
        // namespaced identities without project configuration.
        let parent = registry.runtime_snapshot("user-a", Some("project-sequel")).await;
        let child = registry.runtime_snapshot("user-a", Some("project-sequel")).await;
        let unscoped = registry.runtime_snapshot("user-a", None).await;
        assert_eq!(parent.mcp_servers.len(), 2);
        assert_eq!(child.mcp_servers.len(), 2);
        assert_eq!(unscoped.mcp_servers.len(), 2);
        assert_eq!(
            parent.mcp_servers.iter().map(|server| &server.id).collect::<Vec<_>>(),
            child.mcp_servers.iter().map(|server| &server.id).collect::<Vec<_>>()
        );

        std::fs::write(first_auth.join("old-auth"), "first").unwrap();
        std::fs::write(second_auth.join("keep-auth"), "second").unwrap();
        let reconnected = manager.connect("user-a", &first.id, true).await.unwrap();
        assert_eq!(reconnected.namespace, first.namespace);
        assert!(!first_auth.join("old-auth").exists());
        assert!(second_auth.join("keep-auth").exists());
        assert!(manager.revision("user-a").await > initial_revision);

        manager.disconnect("user-a", &first.id).await.unwrap();
        assert!(!first_auth.exists());
        assert!(second_auth.exists());
        let disconnected = registry.runtime_snapshot("user-a", Some("project-sequel")).await;
        assert!(disconnected.linear_revision > parent.linear_revision);
        assert_eq!(disconnected.mcp_servers.len(), 1);
        assert!(disconnected.mcp_servers[0].id.ends_with(&second.id));
    }

    #[tokio::test]
    async fn state_persists_without_auth_material_and_legacy_requires_reconnect() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: true })).unwrap();
        let migrated = manager.migrate_legacy("user-a").await.unwrap();
        assert_eq!(migrated.connections.len(), 1);
        assert!(migrated.connections[0].migrated);
        assert_eq!(
            migrated.connections[0].status,
            LinearConnectionStatus::ReconnectRequired
        );
        assert!(migrated.connections[0].identity.is_none());

        let raw = std::fs::read_to_string(temp.path().join(STORE_FILE)).unwrap();
        assert!(!raw.contains("access_token"));
        assert!(!raw.contains("refresh_token"));
        assert!(!raw.contains("MCP_REMOTE_CONFIG_DIR"));

        let reloaded = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        assert_eq!(reloaded.list("user-a").await.unwrap(), migrated);
        assert_eq!(reloaded.migrate_legacy("user-a").await.unwrap().connections.len(), 1);
    }

    #[test]
    fn version_two_migration_removes_obsolete_project_bindings() {
        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join(STORE_FILE);
        std::fs::write(
            &store_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "users": {
                    "user-a": {
                        "connections": {},
                        "project_bindings": {"project-a": "connection-a"}
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();

        let raw = std::fs::read_to_string(store_path).unwrap();
        assert!(raw.contains("\"version\": 2"));
        assert!(!raw.contains("project_bindings"));
    }

    #[tokio::test]
    async fn probe_errors_do_not_expose_upstream_auth_material() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(SecretErrorProbe)).unwrap();
        let error = manager.connect_new("user-a").await.unwrap_err();
        assert!(!error.to_string().contains("must-not-leak"));

        let listed = manager.list("user-a").await.unwrap();
        assert!(listed.connections.is_empty());
        let stored = std::fs::read_to_string(temp.path().join(STORE_FILE)).unwrap_or_default();
        assert!(!stored.contains("must-not-leak"));
        let user_auth_root = manager
            .inner
            .auth_root
            .join(&hex::encode(Sha256::digest(b"user-a"))[..16]);
        assert!(!user_auth_root.exists() || std::fs::read_dir(user_auth_root).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn reload_requires_reconnect_when_the_private_auth_directory_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        remove_auth_dir(&manager.auth_dir("user-a", &connection.id)).unwrap();
        drop(manager);

        let reloaded = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        let listed = reloaded.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, LinearConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
    }

    #[tokio::test]
    async fn failed_reconnect_removes_partial_auth_state() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(
            temp.path(),
            Arc::new(FailAfterFirstProbe {
                calls: AtomicUsize::new(0),
            }),
        )
        .unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);
        assert!(auth_dir.join("oauth-token").exists());

        let error = manager.connect("user-a", &connection.id, true).await.unwrap_err();
        assert!(!error.to_string().contains("must-not-leak"));
        assert!(!auth_dir.exists());
        let listed = manager.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, LinearConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
        assert_eq!(listed.connections[0].namespace, connection.namespace);
    }

    #[tokio::test]
    async fn cancelled_new_connection_finishes_partial_auth_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let probe = Arc::new(BlockingFailureProbe {
            calls: AtomicUsize::new(0),
            succeed_first: false,
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = LinearConnectionManager::new(temp.path(), probe.clone()).unwrap();
        let request_manager = manager.clone();
        let request = tokio::spawn(async move { request_manager.connect_new("user-a").await });

        probe.started.notified().await;
        request.abort();
        let _ = request.await;
        probe.release.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let auth_root = user_auth_root(&manager, "user-a");
                let auth_is_empty = !auth_root.exists()
                    || std::fs::read_dir(&auth_root).is_ok_and(|mut entries| entries.next().is_none());
                if auth_is_empty && manager.list("user-a").await.unwrap().connections.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned connection operation should clean partial auth after request cancellation");
    }

    #[tokio::test]
    async fn cancelled_reconnect_finishes_cleanup_and_state_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let probe = Arc::new(BlockingFailureProbe {
            calls: AtomicUsize::new(0),
            succeed_first: true,
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = LinearConnectionManager::new(temp.path(), probe.clone()).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);
        let request_manager = manager.clone();
        let connection_id = connection.id.clone();
        let request = tokio::spawn(async move { request_manager.connect("user-a", &connection_id, true).await });

        probe.started.notified().await;
        request.abort();
        let _ = request.await;
        probe.release.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let listed = manager.list("user-a").await.unwrap();
                let recovered = listed.connections.first().is_some_and(|current| {
                    current.status == LinearConnectionStatus::ReconnectRequired
                        && current.identity.is_none()
                        && current.last_error.as_deref()
                            == Some("Linear authorization could not be completed. Try connecting again.")
                });
                if recovered && !auth_dir.exists() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned reconnect operation should recover state after request cancellation");
    }

    #[tokio::test]
    async fn reconnect_final_persistence_failure_recovers_without_staying_connecting() {
        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join(STORE_FILE);
        let manager = LinearConnectionManager::new(
            temp.path(),
            Arc::new(FailFinalPersistenceProbe {
                calls: AtomicUsize::new(0),
                store_path,
            }),
        )
        .unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);

        assert!(manager.connect("user-a", &connection.id, true).await.is_err());
        assert!(!auth_dir.exists());
        let listed = manager.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, LinearConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
        assert_eq!(listed.connections[0].namespace, connection.namespace);
    }

    #[tokio::test]
    async fn revisions_are_isolated_by_user() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        manager.connect_new("user-a").await.unwrap();
        let user_a_revision = manager.revision("user-a").await;
        assert!(user_a_revision > 0);
        assert_eq!(manager.revision("user-b").await, 0);

        manager.connect_new("user-b").await.unwrap();
        assert_eq!(manager.revision("user-a").await, user_a_revision);
        assert!(manager.revision("user-b").await > 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_disconnect_cleanup_does_not_publish_disconnected_state() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);
        let auth_parent = auth_dir.parent().unwrap();
        std::fs::set_permissions(auth_parent, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = manager.disconnect("user-a", &connection.id).await;
        std::fs::set_permissions(auth_parent, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert!(auth_dir.exists());
        let listed = manager.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, LinearConnectionStatus::Connected);
        assert!(listed.connections[0].identity.is_some());
        manager.disconnect("user-a", &connection.id).await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_persistence_failure_reports_removed_credentials_in_live_state() {
        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join(STORE_FILE);
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);
        std::fs::remove_file(&store_path).unwrap();
        std::fs::create_dir(&store_path).unwrap();

        assert!(manager.disconnect("user-a", &connection.id).await.is_err());
        assert!(!auth_dir.exists());
        let listed = manager.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, LinearConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auth_setup_and_recovery_persistence_failure_does_not_stay_connecting() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join(STORE_FILE);
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);
        let auth_parent = auth_dir.parent().unwrap().to_owned();
        std::fs::set_permissions(&auth_parent, std::fs::Permissions::from_mode(0o500)).unwrap();

        // Hold the per-user revision update after `connecting` is persisted so
        // the test can make both auth preparation and recovery persistence fail.
        let revision_guard = manager.inner.revisions.lock().await;
        let reconnect_manager = manager.clone();
        let connection_id = connection.id.clone();
        let reconnect = tokio::spawn(async move { reconnect_manager.connect("user-a", &connection_id, true).await });
        loop {
            if std::fs::read_to_string(&store_path).is_ok_and(|stored| stored.contains("\"status\": \"connecting\"")) {
                break;
            }
            tokio::task::yield_now().await;
        }
        std::fs::remove_file(&store_path).unwrap();
        std::fs::create_dir(&store_path).unwrap();
        drop(revision_guard);

        let result = reconnect.await.unwrap();
        std::fs::set_permissions(&auth_parent, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert!(auth_dir.exists());
        let listed = manager.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, LinearConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
    }

    #[test]
    fn failed_persistence_does_not_commit_the_staged_state() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("not-a-directory");
        std::fs::write(&blocker, "file").unwrap();
        let mut current = PersistedLinearState::default();
        let mut next = current.clone();
        next.users.insert("user-a".into(), PersistedLinearUser::default());

        assert!(persist_and_replace(&blocker.join("state.json"), &mut current, next).is_err());
        assert!(current.users.is_empty());
    }

    #[tokio::test]
    async fn new_connection_persistence_failure_removes_verified_auth_state() {
        let temp = tempfile::tempdir().unwrap();
        let manager = LinearConnectionManager::new(temp.path(), Arc::new(VerifiedProbe { legacy: false })).unwrap();
        std::fs::create_dir(temp.path().join(STORE_FILE)).unwrap();

        assert!(manager.connect_new("user-a").await.is_err());
        assert!(manager.list("user-a").await.unwrap().connections.is_empty());
        let user_auth_root = manager
            .inner
            .auth_root
            .join(&hex::encode(Sha256::digest(b"user-a"))[..16]);
        assert!(!user_auth_root.exists() || std::fs::read_dir(user_auth_root).unwrap().next().is_none());
    }
}
