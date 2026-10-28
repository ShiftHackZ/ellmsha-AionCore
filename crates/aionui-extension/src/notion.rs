use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    NotionConnectionResponse, NotionConnectionStatus, NotionConnectionsResponse, NotionVerifiedCapabilities,
    NotionVerifiedIdentity, SessionMcpServer, SessionMcpTransport,
};
use aionui_common::now_ms;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::ExtensionError;

pub const NOTION_PLUGIN_NAME: &str = "notion";
pub const NOTION_MCP_URL: &str = "https://mcp.notion.com/mcp";
use crate::linear::MCP_REMOTE_VERSION;
const MAX_CONNECTIONS: usize = 25;

const STORE_VERSION: u32 = 1;
const STORE_FILE: &str = "notion-connections.json";
const AUTH_ROOT: &str = "plugin-auth";

#[derive(Debug, thiserror::Error)]
pub enum NotionConnectionError {
    #[error("Notion connection not found")]
    NotFound,
    #[error("This Notion user and workspace are already connected")]
    Duplicate,
    #[error("Disconnect an unused Notion account before adding another (maximum 25)")]
    LimitReached,
    #[error("Notion authentication failed: {0}")]
    Probe(String),
    #[error("failed to persist Notion connection state: {0}")]
    Persistence(String),
}

/// Public, credential-free failure stages for an interactive Notion probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NotionProbeFailure {
    #[error("The Notion sign-in page could not be opened. Check your system browser settings and try again.")]
    BrowserLaunch,
    #[error("This Notion user and workspace are already connected. Disconnect the duplicate and try again.")]
    DuplicateIdentity,
    #[error("Disconnect an unused Notion account before reconnecting (maximum 25 connected accounts).")]
    LimitReached,
    #[error("Notion sign-in timed out before authorization completed. Try connecting again.")]
    AuthorizationTimeout,
    #[error(
        "Notion authorized the connection, but the current account identity could not be verified. Try reconnecting."
    )]
    IdentityUnavailable,
    #[error("Notion authorization could not be completed. Try connecting again.")]
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct NotionProbeResult {
    pub identity: NotionVerifiedIdentity,
    pub capabilities: NotionVerifiedCapabilities,
}

#[async_trait]
pub trait NotionConnectionProbe: Send + Sync {
    async fn verify(
        &self,
        user_id: &str,
        connection_id: &str,
        auth_dir: &Path,
    ) -> Result<NotionProbeResult, NotionProbeFailure>;
}

#[derive(Clone)]
pub struct NotionConnectionManager {
    inner: Arc<NotionConnectionManagerInner>,
}

struct NotionConnectionManagerInner {
    store_path: PathBuf,
    auth_root: PathBuf,
    state: Mutex<PersistedNotionState>,
    operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    revisions: Mutex<HashMap<String, u64>>,
    probe: Arc<dyn NotionConnectionProbe>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedNotionState {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    users: BTreeMap<String, PersistedNotionUser>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedNotionUser {
    #[serde(default)]
    connections: BTreeMap<String, NotionConnectionResponse>,
}

const fn store_version() -> u32 {
    STORE_VERSION
}

impl NotionConnectionManager {
    pub fn new(data_dir: &Path, probe: Arc<dyn NotionConnectionProbe>) -> Result<Self, NotionConnectionError> {
        let store_path = data_dir.join(STORE_FILE);
        let auth_root = data_dir.join(AUTH_ROOT).join(NOTION_PLUGIN_NAME);
        let mut state = load_state(&store_path)?;
        if state.version > STORE_VERSION {
            return Err(NotionConnectionError::Persistence(
                "unsupported Notion state version".into(),
            ));
        }
        for user in state.users.values() {
            for (id, connection) in &user.connections {
                if id != &connection.id || Uuid::parse_str(id).is_err() {
                    return Err(NotionConnectionError::Persistence(
                        "invalid Notion connection identity".into(),
                    ));
                }
            }
        }
        let mut normalized = state.version != STORE_VERSION;
        state.version = STORE_VERSION;
        for (user_id, user) in &mut state.users {
            for connection in user.connections.values_mut() {
                if connection.namespace.is_empty() {
                    connection.namespace = stable_namespace(connection);
                    normalized = true;
                }
                let auth_dir = auth_dir_for(&auth_root, user_id, &connection.id);
                if connection.status == NotionConnectionStatus::Connecting
                    || (connection.status == NotionConnectionStatus::Connected && !is_private_auth_dir(&auth_dir))
                {
                    connection.status = NotionConnectionStatus::ReconnectRequired;
                    connection.identity = None;
                    connection.capabilities = NotionVerifiedCapabilities::default();
                    connection.last_error =
                        Some("Local Notion authentication is missing. Reconnect and try again.".into());
                    connection.updated_at = now_ms();
                    normalized = true;
                }
            }
        }
        if normalized {
            persist_locked(&store_path, &state)?;
        }
        Ok(Self {
            inner: Arc::new(NotionConnectionManagerInner {
                store_path,
                auth_root,
                state: Mutex::new(state),
                operations: Mutex::new(HashMap::new()),
                revisions: Mutex::new(HashMap::new()),
                probe,
            }),
        })
    }

    pub async fn list(&self, user_id: &str) -> Result<NotionConnectionsResponse, NotionConnectionError> {
        let state = self.inner.state.lock().await;
        let user = state.users.get(user_id).cloned().unwrap_or_default();
        Ok(NotionConnectionsResponse {
            connections: user.connections.into_values().collect(),
        })
    }

    /// Start OAuth for a new identity and publish the connection only after the
    /// remote user has been verified.
    pub async fn connect_new(&self, user_id: &str) -> Result<NotionConnectionResponse, NotionConnectionError> {
        let manager = self.clone();
        let user_id = user_id.to_owned();
        tokio::spawn(async move { manager.connect_new_owned(&user_id).await })
            .await
            .map_err(|_| {
                NotionConnectionError::Probe(
                    "Notion authorization could not be completed. Try connecting again.".into(),
                )
            })?
    }

    /// Runs independently from the request task so an HTTP disconnect cannot
    /// skip credential cleanup while an OAuth callback is still active.
    async fn connect_new_owned(&self, user_id: &str) -> Result<NotionConnectionResponse, NotionConnectionError> {
        if self
            .list(user_id)
            .await?
            .connections
            .iter()
            .filter(|connection| connection.status != NotionConnectionStatus::Disconnected)
            .count()
            >= MAX_CONNECTIONS
        {
            return Err(NotionConnectionError::LimitReached);
        }
        let connection_id = Uuid::now_v7().to_string();
        let operation = self.connection_operation(user_id, &connection_id).await;
        let _operation = operation.lock().await;
        let auth_dir = self.auth_dir(user_id, &connection_id);
        create_private_dir(&auth_dir)?;

        let verified = match self.inner.probe.verify(user_id, &connection_id, &auth_dir).await {
            Ok(verified) => verified,
            Err(error) => {
                let _ = remove_auth_dir(&auth_dir);
                return Err(NotionConnectionError::Probe(error.to_string()));
            }
        };
        let now = now_ms();
        let namespace = verified_namespace(&verified.identity, &connection_id);
        let connection = NotionConnectionResponse {
            id: connection_id,
            namespace,
            label: identity_label(&verified.identity),
            status: NotionConnectionStatus::Connected,
            identity: Some(verified.identity),
            capabilities: verified.capabilities,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        if let Err(error) = validate_new_identity(&next, user_id, None, connection.identity.as_ref().unwrap()) {
            let _ = remove_auth_dir(&auth_dir);
            return Err(error);
        }
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
        tracing::info!(
            connection_id = connection.id,
            stage = "connected",
            "Notion account connected"
        );
        Ok(connection)
    }

    pub async fn connect(
        &self,
        user_id: &str,
        connection_id: &str,
        reconnect: bool,
    ) -> Result<NotionConnectionResponse, NotionConnectionError> {
        let manager = self.clone();
        let user_id = user_id.to_owned();
        let connection_id = connection_id.to_owned();
        tokio::spawn(async move { manager.connect_owned(&user_id, &connection_id, reconnect).await })
            .await
            .map_err(|_| {
                NotionConnectionError::Probe("Notion authorization could not be completed. Try reconnecting.".into())
            })?
    }

    /// Runs independently from the request task so cancellation still reaches
    /// the reconnect-required state and removes partial credentials.
    async fn connect_owned(
        &self,
        user_id: &str,
        connection_id: &str,
        _reconnect: bool,
    ) -> Result<NotionConnectionResponse, NotionConnectionError> {
        validate_connection_id(connection_id)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        let auth_dir = self.auth_dir(user_id, connection_id);

        let previous_identity = {
            let mut state = self.inner.state.lock().await;
            let mut next = state.clone();
            connection_mut(&mut next, user_id, connection_id)?;
            if next.users[user_id]
                .connections
                .values()
                .filter(|connection| {
                    connection.id != connection_id && connection.status != NotionConnectionStatus::Disconnected
                })
                .count()
                >= MAX_CONNECTIONS
            {
                return Err(NotionConnectionError::LimitReached);
            }
            let connection = connection_mut(&mut next, user_id, connection_id)?;
            let previous_identity = connection.identity.clone();
            connection.status = NotionConnectionStatus::Connecting;
            connection.identity = None;
            connection.capabilities = NotionVerifiedCapabilities::default();
            connection.last_error = None;
            connection.updated_at = now_ms();
            persist_and_replace(&self.inner.store_path, &mut state, next)?;
            self.bump_revision(user_id).await;
            previous_identity
        };

        let auth_setup = (|| {
            // Always start from an empty per-connection directory so an old
            // token cannot satisfy a new authorization attempt.
            remove_auth_dir(&auth_dir)?;
            create_private_dir(&auth_dir)
        })();
        if let Err(error) = auth_setup {
            let mut state = self.inner.state.lock().await;
            let connection = connection_mut(&mut state, user_id, connection_id)?;
            connection.status = NotionConnectionStatus::ReconnectRequired;
            connection.identity = None;
            connection.capabilities = NotionVerifiedCapabilities::default();
            connection.last_error =
                Some("Local Notion authentication could not be prepared. Reconnect and try again.".into());
            connection.updated_at = now_ms();
            // Recovery must remain visible in the live process even when the
            // same storage fault also prevents this best-effort write.
            let _ = persist_locked(&self.inner.store_path, &state);
            self.bump_revision(user_id).await;
            return Err(error);
        }

        let mut probe_result = self.inner.probe.verify(user_id, connection_id, &auth_dir).await;
        let mut state = self.inner.state.lock().await;
        if let Ok(verified) = &probe_result
            && let Err(error) = validate_new_identity(&state, user_id, Some(connection_id), &verified.identity)
        {
            probe_result = Err(match error {
                NotionConnectionError::Duplicate => NotionProbeFailure::DuplicateIdentity,
                NotionConnectionError::LimitReached => NotionProbeFailure::LimitReached,
                _ => NotionProbeFailure::Unavailable,
            });
        }
        let cleanup_failed = probe_result.is_err() && remove_auth_dir(&auth_dir).is_err();
        let mut next = state.clone();
        let connection = connection_mut(&mut next, user_id, connection_id)?;
        match probe_result {
            Ok(verified) => {
                // Reauthorizing as a different user/workspace must not retain a
                // namespace that advertises the previous account to agents.
                if previous_identity.as_ref().is_none_or(|previous| {
                    previous.account_id != verified.identity.account_id
                        || previous.workspace_id != verified.identity.workspace_id
                }) || connection.namespace.is_empty()
                    || connection.namespace == fallback_namespace(connection_id)
                {
                    connection.namespace = verified_namespace(&verified.identity, connection_id);
                }
                connection.label = identity_label(&verified.identity);
                connection.status = NotionConnectionStatus::Connected;
                connection.identity = Some(verified.identity);
                connection.capabilities = verified.capabilities;
                connection.last_error = None;
            }
            Err(error) => {
                connection.status = NotionConnectionStatus::ReconnectRequired;
                connection.identity = None;
                connection.capabilities = NotionVerifiedCapabilities::default();
                connection.last_error = Some(if cleanup_failed {
                    "Notion verification failed and local authentication cleanup did not complete. Disconnect or reconnect and try again."
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
                connection.status = NotionConnectionStatus::ReconnectRequired;
                connection.identity = None;
                connection.capabilities = NotionVerifiedCapabilities::default();
                connection.last_error = Some(if cleanup_failed {
                    "Notion connection state could not be saved and local authentication cleanup did not complete. Disconnect or reconnect and try again."
                        .into()
                } else {
                    "Notion connection state could not be saved. Reconnect and try again.".into()
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
        if response.status == NotionConnectionStatus::Connected {
            Ok(response)
        } else {
            Err(NotionConnectionError::Probe(
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
    ) -> Result<NotionConnectionResponse, NotionConnectionError> {
        validate_connection_id(connection_id)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        // Remove credentials before publishing a disconnected state. A failed
        // cleanup leaves the prior state visible so the user can retry it.
        remove_auth_dir(&self.auth_dir(user_id, connection_id))?;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connection = connection_mut(&mut next, user_id, connection_id)?;
        connection.status = NotionConnectionStatus::Disconnected;
        connection.identity = None;
        connection.capabilities = NotionVerifiedCapabilities::default();
        connection.last_error = None;
        connection.updated_at = now_ms();
        let response = connection.clone();
        let persisted = persist_and_replace(&self.inner.store_path, &mut state, next);
        // The auth directory is already gone, so invalidate active runtimes
        // even when the state file write fails.
        self.bump_revision(user_id).await;
        if let Err(error) = persisted {
            if let Ok(connection) = connection_mut(&mut state, user_id, connection_id) {
                connection.status = NotionConnectionStatus::ReconnectRequired;
                connection.identity = None;
                connection.capabilities = NotionVerifiedCapabilities::default();
                connection.last_error =
                    Some("Local Notion credentials were removed, but connection state could not be saved.".into());
                connection.updated_at = now_ms();
            }
            let _ = persist_locked(&self.inner.store_path, &state);
            return Err(error);
        }
        tracing::info!(
            connection_id,
            stage = "disconnected",
            "Notion local authorization removed"
        );
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

    fn server_for_connection(&self, user_id: &str, connection: &NotionConnectionResponse) -> Option<SessionMcpServer> {
        if connection.status != NotionConnectionStatus::Connected || connection.identity.is_none() {
            return None;
        }
        let auth_dir = self.auth_dir(user_id, &connection.id);
        if !is_private_auth_dir(&auth_dir) {
            return None;
        }
        let runtime_id = format!("plugin:{NOTION_PLUGIN_NAME}:{}", connection.id);
        Some(SessionMcpServer {
            id: runtime_id,
            name: connection.namespace.clone(),
            transport: SessionMcpTransport::Stdio {
                command: "npx".into(),
                args: vec![
                    "-y".into(),
                    format!("mcp-remote@{MCP_REMOTE_VERSION}"),
                    NOTION_MCP_URL.into(),
                    "--transport".into(),
                    "http-only".into(),
                ],
                env: HashMap::from([("MCP_REMOTE_CONFIG_DIR".into(), auth_dir.to_string_lossy().into_owned())]),
            },
            approval_identity: connection.identity.as_ref().map(|identity| {
                format!(
                    "notion:{}:{}:{}",
                    connection.id, identity.account_id, identity.workspace_id
                )
            }),
            auto_approve_in_unattended: false,
        })
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
    current: &mut PersistedNotionState,
    next: PersistedNotionState,
) -> Result<(), NotionConnectionError> {
    persist_locked(path, &next)?;
    *current = next;
    Ok(())
}

fn connection_mut<'a>(
    state: &'a mut PersistedNotionState,
    user_id: &str,
    connection_id: &str,
) -> Result<&'a mut NotionConnectionResponse, NotionConnectionError> {
    state
        .users
        .get_mut(user_id)
        .and_then(|user| user.connections.get_mut(connection_id))
        .ok_or(NotionConnectionError::NotFound)
}

fn identity_label(identity: &NotionVerifiedIdentity) -> String {
    let name = identity.account_name.trim();
    if name.is_empty() {
        identity
            .account_email
            .clone()
            .unwrap_or_else(|| identity.account_id.clone())
    } else {
        name.to_owned()
    }
}

fn verified_namespace(identity: &NotionVerifiedIdentity, connection_id: &str) -> String {
    let workspace = identity.workspace_name.as_deref().unwrap_or(&identity.account_id);
    format!(
        "notion-{}-{}-{}",
        namespace_segment(workspace),
        namespace_segment(&identity.account_id),
        connection_id
    )
}

fn stable_namespace(connection: &NotionConnectionResponse) -> String {
    connection
        .identity
        .as_ref()
        .map(|identity| verified_namespace(identity, &connection.id))
        .unwrap_or_else(|| fallback_namespace(&connection.id))
}

fn fallback_namespace(connection_id: &str) -> String {
    format!("notion-account-{connection_id}")
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

fn validate_connection_id(connection_id: &str) -> Result<(), NotionConnectionError> {
    Uuid::parse_str(connection_id)
        .map(|_| ())
        .map_err(|_| NotionConnectionError::NotFound)
}

fn load_state(path: &Path) -> Result<PersistedNotionState, NotionConnectionError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| NotionConnectionError::Persistence(format!("invalid state file: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PersistedNotionState {
            version: STORE_VERSION,
            users: BTreeMap::new(),
        }),
        Err(error) => Err(NotionConnectionError::Persistence(error.to_string())),
    }
}

fn persist_locked(path: &Path, state: &PersistedNotionState) -> Result<(), NotionConnectionError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|error| NotionConnectionError::Persistence(error.to_string()))?;
    std::fs::write(&tmp_path, bytes).map_err(|error| NotionConnectionError::Persistence(error.to_string()))?;
    set_private_file_permissions(&tmp_path)?;
    crate::state::replace_state_file(&tmp_path, path)
        .map_err(|error| NotionConnectionError::Persistence(error.to_string()))
}

fn create_private_dir(path: &Path) -> Result<(), NotionConnectionError> {
    std::fs::create_dir_all(path).map_err(|error| NotionConnectionError::Persistence(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| NotionConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), NotionConnectionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| NotionConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

fn remove_auth_dir(path: &Path) -> Result<(), NotionConnectionError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(path).map_err(|error| NotionConnectionError::Persistence(error.to_string()))
        }
        Ok(_) => std::fs::remove_dir_all(path).map_err(|error| NotionConnectionError::Persistence(error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(NotionConnectionError::Persistence(error.to_string())),
    }
}

impl From<NotionConnectionError> for ExtensionError {
    fn from(error: NotionConnectionError) -> Self {
        ExtensionError::StatePersistence(error.to_string())
    }
}

fn validate_new_identity(
    state: &PersistedNotionState,
    user_id: &str,
    exclude: Option<&str>,
    identity: &NotionVerifiedIdentity,
) -> Result<(), NotionConnectionError> {
    let Some(user) = state.users.get(user_id) else {
        return Ok(());
    };
    if user
        .connections
        .values()
        .filter(|connection| {
            Some(connection.id.as_str()) != exclude && connection.status != NotionConnectionStatus::Disconnected
        })
        .count()
        >= MAX_CONNECTIONS
    {
        return Err(NotionConnectionError::LimitReached);
    }
    if user.connections.values().any(|connection| {
        Some(connection.id.as_str()) != exclude
            && connection.identity.as_ref().is_some_and(|other| {
                other.account_id == identity.account_id && other.workspace_id == identity.workspace_id
            })
    }) {
        return Err(NotionConnectionError::Duplicate);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct VerifiedProbe;

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

    fn verified_result(connection_id: &str) -> NotionProbeResult {
        NotionProbeResult {
            identity: NotionVerifiedIdentity {
                account_id: format!("account-{connection_id}"),
                account_name: format!("Account {connection_id}"),
                account_email: Some(format!("{connection_id}@example.test")),
                workspace_id: format!("workspace-{connection_id}"),
                workspace_name: Some(format!("Workspace {connection_id}")),
            },
            capabilities: NotionVerifiedCapabilities {
                search: true,
                read: true,
                write: true,
            },
        }
    }

    #[async_trait]
    impl NotionConnectionProbe for VerifiedProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<NotionProbeResult, NotionProbeFailure> {
            std::fs::write(auth_dir.join("probe-marker"), connection_id)
                .map_err(|_| NotionProbeFailure::Unavailable)?;
            Ok(verified_result(connection_id))
        }
    }

    #[async_trait]
    impl NotionConnectionProbe for SecretErrorProbe {
        async fn verify(
            &self,
            _user_id: &str,
            _connection_id: &str,
            _auth_dir: &Path,
        ) -> Result<NotionProbeResult, NotionProbeFailure> {
            Err(NotionProbeFailure::Unavailable)
        }
    }

    #[async_trait]
    impl NotionConnectionProbe for FailAfterFirstProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<NotionProbeResult, NotionProbeFailure> {
            std::fs::write(auth_dir.join("oauth-token"), "secret material")
                .map_err(|_| NotionProbeFailure::Unavailable)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(verified_result(connection_id))
            } else {
                Err(NotionProbeFailure::Unavailable)
            }
        }
    }

    #[async_trait]
    impl NotionConnectionProbe for FailFinalPersistenceProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<NotionProbeResult, NotionProbeFailure> {
            std::fs::write(auth_dir.join("oauth-token"), "secret material")
                .map_err(|_| NotionProbeFailure::Unavailable)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::remove_file(&self.store_path).map_err(|_| NotionProbeFailure::Unavailable)?;
                std::fs::create_dir(&self.store_path).map_err(|_| NotionProbeFailure::Unavailable)?;
            }
            Ok(verified_result(connection_id))
        }
    }

    #[async_trait]
    impl NotionConnectionProbe for BlockingFailureProbe {
        async fn verify(
            &self,
            _user_id: &str,
            connection_id: &str,
            auth_dir: &Path,
        ) -> Result<NotionProbeResult, NotionProbeFailure> {
            std::fs::write(auth_dir.join("oauth-token"), "partial secret material")
                .map_err(|_| NotionProbeFailure::Unavailable)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 && self.succeed_first {
                return Ok(verified_result(connection_id));
            }
            self.started.notify_one();
            self.release.notified().await;
            Err(NotionProbeFailure::Unavailable)
        }
    }

    fn user_auth_root(manager: &NotionConnectionManager, user_id: &str) -> PathBuf {
        manager
            .inner
            .auth_root
            .join(&hex::encode(Sha256::digest(user_id.as_bytes()))[..16])
    }

    fn auth_dir(server: &SessionMcpServer) -> PathBuf {
        let SessionMcpTransport::Stdio { env, .. } = &server.transport else {
            panic!("Notion server must use stdio")
        };
        PathBuf::from(env.get("MCP_REMOTE_CONFIG_DIR").expect("isolated auth directory"))
    }

    #[tokio::test]
    async fn verified_connections_are_all_available_with_stable_isolated_namespaces() {
        let temp = tempfile::tempdir().unwrap();
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
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
        let notion_plugin = plugin_root.join(NOTION_PLUGIN_NAME);
        std::fs::create_dir_all(&notion_plugin).unwrap();
        std::fs::write(
            notion_plugin.join(crate::constants::EXTENSION_MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "name": NOTION_PLUGIN_NAME,
                "version": "1.0.0",
                "apiVersion": "1.0.0",
                "contributes": {}
            }))
            .unwrap(),
        )
        .unwrap();
        let registry = crate::registry::ExtensionRegistry::new_with_account_connections(
            crate::state::ExtensionStateStore::new(temp.path().join("extension-states.json")),
            Arc::new(aionui_realtime::BroadcastEventBus::new(8)),
            "1.0.0".into(),
            None,
            None,
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
        assert_ne!(disconnected.linear_revision, parent.linear_revision);
        assert_eq!(disconnected.mcp_servers.len(), 1);
        assert!(disconnected.mcp_servers[0].id.ends_with(&second.id));
        registry
            .disable_extension_for_user("user-a", NOTION_PLUGIN_NAME, None)
            .await
            .unwrap();
        assert!(registry.runtime_snapshot("user-a", None).await.mcp_servers.is_empty());
        assert!(
            registry
                .get_notion_technical_servers_for_user("user-a")
                .await
                .is_empty()
        );
        registry
            .enable_extension_for_user("user-a", NOTION_PLUGIN_NAME)
            .await
            .unwrap();
        assert_eq!(registry.runtime_snapshot("user-a", None).await.mcp_servers.len(), 1);
    }

    #[tokio::test]
    async fn probe_errors_do_not_expose_upstream_auth_material() {
        let temp = tempfile::tempdir().unwrap();
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(SecretErrorProbe)).unwrap();
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
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        remove_auth_dir(&manager.auth_dir("user-a", &connection.id)).unwrap();
        drop(manager);

        let reloaded = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
        let listed = reloaded.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, NotionConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
    }

    #[tokio::test]
    async fn failed_reconnect_removes_partial_auth_state() {
        let temp = tempfile::tempdir().unwrap();
        let manager = NotionConnectionManager::new(
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
        assert_eq!(listed.connections[0].status, NotionConnectionStatus::ReconnectRequired);
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
        let manager = NotionConnectionManager::new(temp.path(), probe.clone()).unwrap();
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
        let manager = NotionConnectionManager::new(temp.path(), probe.clone()).unwrap();
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
                    current.status == NotionConnectionStatus::ReconnectRequired
                        && current.identity.is_none()
                        && current.last_error.as_deref()
                            == Some("Notion authorization could not be completed. Try connecting again.")
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
        let manager = NotionConnectionManager::new(
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
        assert_eq!(listed.connections[0].status, NotionConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
        assert_eq!(listed.connections[0].namespace, connection.namespace);
    }

    #[tokio::test]
    async fn revisions_are_isolated_by_user() {
        let temp = tempfile::tempdir().unwrap();
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
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
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);
        let auth_parent = auth_dir.parent().unwrap();
        std::fs::set_permissions(auth_parent, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = manager.disconnect("user-a", &connection.id).await;
        std::fs::set_permissions(auth_parent, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err());
        assert!(auth_dir.exists());
        let listed = manager.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, NotionConnectionStatus::Connected);
        assert!(listed.connections[0].identity.is_some());
        manager.disconnect("user-a", &connection.id).await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_persistence_failure_reports_removed_credentials_in_live_state() {
        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join(STORE_FILE);
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
        let connection = manager.connect_new("user-a").await.unwrap();
        let auth_dir = manager.auth_dir("user-a", &connection.id);
        std::fs::remove_file(&store_path).unwrap();
        std::fs::create_dir(&store_path).unwrap();

        assert!(manager.disconnect("user-a", &connection.id).await.is_err());
        assert!(!auth_dir.exists());
        let listed = manager.list("user-a").await.unwrap();
        assert_eq!(listed.connections[0].status, NotionConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auth_setup_and_recovery_persistence_failure_does_not_stay_connecting() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join(STORE_FILE);
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
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
        assert_eq!(listed.connections[0].status, NotionConnectionStatus::ReconnectRequired);
        assert!(listed.connections[0].identity.is_none());
    }

    #[test]
    fn failed_persistence_does_not_commit_the_staged_state() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("not-a-directory");
        std::fs::write(&blocker, "file").unwrap();
        let mut current = PersistedNotionState::default();
        let mut next = current.clone();
        next.users.insert("user-a".into(), PersistedNotionUser::default());

        assert!(persist_and_replace(&blocker.join("state.json"), &mut current, next).is_err());
        assert!(current.users.is_empty());
    }

    #[tokio::test]
    async fn new_connection_persistence_failure_removes_verified_auth_state() {
        let temp = tempfile::tempdir().unwrap();
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
        std::fs::create_dir(temp.path().join(STORE_FILE)).unwrap();

        assert!(manager.connect_new("user-a").await.is_err());
        assert!(manager.list("user-a").await.unwrap().connections.is_empty());
        let user_auth_root = manager
            .inner
            .auth_root
            .join(&hex::encode(Sha256::digest(b"user-a"))[..16]);
        assert!(!user_auth_root.exists() || std::fs::read_dir(user_auth_root).unwrap().next().is_none());
    }

    struct FixedIdentityProbe;
    #[async_trait]
    impl NotionConnectionProbe for FixedIdentityProbe {
        async fn verify(&self, _: &str, _: &str, _: &Path) -> Result<NotionProbeResult, NotionProbeFailure> {
            Ok(verified_result("same-remote-identity"))
        }
    }
    #[tokio::test]
    async fn duplicate_identity_and_cross_user_mutations_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(FixedIdentityProbe)).unwrap();
        let first = manager.connect_new("owner").await.unwrap();
        assert!(matches!(
            manager.connect_new("owner").await,
            Err(NotionConnectionError::Duplicate)
        ));
        assert_eq!(std::fs::read_dir(user_auth_root(&manager, "owner")).unwrap().count(), 1);
        assert!(manager.list("other").await.unwrap().connections.is_empty());
        assert!(matches!(
            manager.connect("other", &first.id, true).await,
            Err(NotionConnectionError::NotFound)
        ));
        assert!(matches!(
            manager.disconnect("other", &first.id).await,
            Err(NotionConnectionError::NotFound)
        ));
        assert_eq!(manager.technical_servers("owner").await.len(), 1);
        manager.connect_new("other").await.unwrap();
        let restored = NotionConnectionManager::new(temp.path(), Arc::new(FixedIdentityProbe)).unwrap();
        assert_eq!(restored.list("owner").await.unwrap().connections, vec![first]);
        let stored = std::fs::read_to_string(temp.path().join(STORE_FILE)).unwrap();
        for private in ["access_token", "refresh_token", "MCP_REMOTE_CONFIG_DIR", "auth_dir"] {
            assert!(!stored.contains(private));
        }
    }
    #[tokio::test]
    async fn account_limit_is_enforced_before_oauth() {
        let temp = tempfile::tempdir().unwrap();
        let manager = NotionConnectionManager::new(temp.path(), Arc::new(VerifiedProbe)).unwrap();
        for _ in 0..MAX_CONNECTIONS {
            manager.connect_new("owner").await.unwrap();
        }
        assert!(matches!(
            manager.connect_new("owner").await,
            Err(NotionConnectionError::LimitReached)
        ));
        assert_eq!(manager.list("owner").await.unwrap().connections.len(), MAX_CONNECTIONS);
        let first = manager.list("owner").await.unwrap().connections[0].id.clone();
        manager.disconnect("owner", &first).await.unwrap();
        manager.connect_new("owner").await.unwrap();
        assert!(matches!(
            manager.connect("owner", &first, true).await,
            Err(NotionConnectionError::LimitReached)
        ));
        assert_eq!(
            manager
                .list("owner")
                .await
                .unwrap()
                .connections
                .iter()
                .find(|connection| connection.id == first)
                .unwrap()
                .status,
            NotionConnectionStatus::Disconnected
        );
    }
}
