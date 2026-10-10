use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    ClockifyConnectionResponse, ClockifyConnectionStatus, ClockifyConnectionsResponse, ClockifyRealm,
    ClockifyVerifiedIdentity, ClockifyWorkspaceIdentity, SessionMcpServer, SessionMcpTransport,
};
use aionui_common::{decrypt_string, encrypt_string, now_ms};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const CLOCKIFY_PLUGIN_NAME: &str = "clockify";
pub const CLOCKIFY_API_KEY_ENV: &str = "ELLM_SHA_PRIVATE_CLOCKIFY_API_KEY";
pub const CLOCKIFY_LOCK_DIR_ENV: &str = "ELLM_SHA_PRIVATE_CLOCKIFY_LOCK_DIR";
pub const CLOCKIFY_REALM_ENV: &str = "ELLM_SHA_CLOCKIFY_REALM";
pub const CLOCKIFY_ACCOUNT_ID_ENV: &str = "ELLM_SHA_CLOCKIFY_ACCOUNT_ID";
pub const CLOCKIFY_ACCOUNT_NAME_ENV: &str = "ELLM_SHA_CLOCKIFY_ACCOUNT_NAME";
pub const CLOCKIFY_ACCOUNT_EMAIL_ENV: &str = "ELLM_SHA_CLOCKIFY_ACCOUNT_EMAIL";
pub const CLOCKIFY_CONNECTION_ID_ENV: &str = "ELLM_SHA_CLOCKIFY_CONNECTION_ID";

const STORE_VERSION: u32 = 1;
const STORE_FILE: &str = "clockify-connections.json";
const MAX_API_KEY_LEN: usize = 16_384;
const MAX_ERROR_LEN: usize = 300;
const MAX_CONNECTIONS_PER_USER: usize = 25;

#[derive(Debug, thiserror::Error)]
pub enum ClockifyConnectionError {
    #[error("Clockify connection not found")]
    NotFound,
    #[error("Clockify connection input is invalid: {0}")]
    InvalidInput(String),
    #[error("Clockify authentication failed: {0}")]
    Probe(String),
    #[error("failed to persist Clockify connection state: {0}")]
    Persistence(String),
}

#[derive(Debug, Clone)]
pub struct ClockifyProbeResult {
    pub identity: ClockifyVerifiedIdentity,
    pub workspaces: Vec<ClockifyWorkspaceIdentity>,
}

#[async_trait]
pub trait ClockifyConnectionProbe: Send + Sync {
    async fn verify(&self, realm: ClockifyRealm, api_key: &str) -> Result<ClockifyProbeResult, String>;
}

#[derive(Clone)]
pub struct ClockifyConnectionManager {
    inner: Arc<ClockifyConnectionManagerInner>,
}

struct ClockifyConnectionManagerInner {
    store_path: PathBuf,
    encryption_key: [u8; 32],
    mcp_command: String,
    state: Mutex<PersistedClockifyState>,
    operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    revisions: Mutex<HashMap<String, u64>>,
    probe: Arc<dyn ClockifyConnectionProbe>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedClockifyState {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    users: BTreeMap<String, PersistedClockifyUser>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedClockifyUser {
    #[serde(default)]
    connections: BTreeMap<String, PersistedClockifyConnection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedClockifyConnection {
    public: ClockifyConnectionResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_api_key: Option<String>,
}

const fn store_version() -> u32 {
    STORE_VERSION
}

impl ClockifyConnectionManager {
    pub fn new(
        data_dir: &Path,
        encryption_key: [u8; 32],
        mcp_command: String,
        probe: Arc<dyn ClockifyConnectionProbe>,
    ) -> Result<Self, ClockifyConnectionError> {
        let store_path = data_dir.join(STORE_FILE);
        let mut state = load_state(&store_path)?;
        let mut changed = state.version != STORE_VERSION;
        state.version = STORE_VERSION;
        for user in state.users.values_mut() {
            for connection in user.connections.values_mut() {
                if connection.public.namespace.is_empty() {
                    connection.public.namespace = stable_namespace(&connection.public);
                    changed = true;
                }
                if connection.public.status == ClockifyConnectionStatus::Connected
                    && connection
                        .encrypted_api_key
                        .as_deref()
                        .and_then(|value| decrypt_string(value, &encryption_key).ok())
                        .is_none()
                {
                    connection.public.status = ClockifyConnectionStatus::ReconnectRequired;
                    connection.public.last_error =
                        Some("Local Clockify authentication could not be decrypted. Reconnect this account.".into());
                    connection.public.updated_at = now_ms();
                    changed = true;
                }
            }
        }
        if changed {
            persist_locked(&store_path, &state)?;
        }
        Ok(Self {
            inner: Arc::new(ClockifyConnectionManagerInner {
                store_path,
                encryption_key,
                mcp_command,
                state: Mutex::new(state),
                operations: Mutex::new(HashMap::new()),
                revisions: Mutex::new(HashMap::new()),
                probe,
            }),
        })
    }

    pub async fn list(&self, user_id: &str) -> Result<ClockifyConnectionsResponse, ClockifyConnectionError> {
        let state = self.inner.state.lock().await;
        let connections = state
            .users
            .get(user_id)
            .into_iter()
            .flat_map(|user| user.connections.values())
            .map(|connection| connection.public.clone())
            .collect();
        Ok(ClockifyConnectionsResponse { connections })
    }

    pub async fn connect_new(
        &self,
        user_id: &str,
        realm: ClockifyRealm,
        api_key: String,
    ) -> Result<ClockifyConnectionResponse, ClockifyConnectionError> {
        validate_api_key(&api_key)?;
        let operation = self.connection_operation(user_id, "new").await;
        let _operation = operation.lock().await;
        let verified = self
            .inner
            .probe
            .verify(realm, &api_key)
            .await
            .map_err(|error| ClockifyConnectionError::Probe(truncate_error(&error)))?;
        validate_probe_result(&verified)?;

        let id = Uuid::now_v7().to_string();
        let now = now_ms();
        let mut public = ClockifyConnectionResponse {
            id: id.clone(),
            namespace: String::new(),
            label: identity_label(&verified.identity),
            realm,
            status: ClockifyConnectionStatus::Connected,
            identity: Some(verified.identity),
            workspaces: verified.workspaces,
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        public.namespace = stable_namespace(&public);
        let encrypted_api_key = encrypt_string(&api_key, &self.inner.encryption_key)
            .map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))?;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connections = &mut next.users.entry(user_id.to_owned()).or_default().connections;
        if connections.len() >= MAX_CONNECTIONS_PER_USER {
            return Err(ClockifyConnectionError::InvalidInput(format!(
                "at most {MAX_CONNECTIONS_PER_USER} Clockify accounts may be stored"
            )));
        }
        if connections.values().any(|connection| {
            connection.public.realm == realm
                && connection
                    .public
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.account_id == public.identity.as_ref().unwrap().account_id)
        }) {
            return Err(ClockifyConnectionError::InvalidInput(
                "this Clockify account is already connected in this region; reconnect the existing account".into(),
            ));
        }
        connections.insert(
            id,
            PersistedClockifyConnection {
                public: public.clone(),
                encrypted_api_key: Some(encrypted_api_key),
            },
        );
        persist_locked(&self.inner.store_path, &next)?;
        *state = next;
        drop(state);
        self.bump_revision(user_id).await;
        Ok(public)
    }

    pub async fn reconnect(
        &self,
        user_id: &str,
        connection_id: &str,
        realm: ClockifyRealm,
        api_key: String,
    ) -> Result<ClockifyConnectionResponse, ClockifyConnectionError> {
        validate_connection_id(connection_id)?;
        validate_api_key(&api_key)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        let verified = match self.inner.probe.verify(realm, &api_key).await {
            Ok(result) => result,
            Err(error) => {
                self.record_probe_error(user_id, connection_id, &error).await?;
                return Err(ClockifyConnectionError::Probe(truncate_error(&error)));
            }
        };
        validate_probe_result(&verified)?;
        let encrypted_api_key = encrypt_string(&api_key, &self.inner.encryption_key)
            .map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))?;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let duplicate_account = next
            .users
            .get(user_id)
            .into_iter()
            .flat_map(|user| user.connections.values())
            .any(|connection| {
                connection.public.id != connection_id
                    && connection.public.realm == realm
                    && connection
                        .public
                        .identity
                        .as_ref()
                        .is_some_and(|identity| identity.account_id == verified.identity.account_id)
            });
        if duplicate_account {
            return Err(ClockifyConnectionError::InvalidInput(
                "this Clockify account is already connected in this region; reconnect that existing account".into(),
            ));
        }
        let connection = find_connection_mut(&mut next, user_id, connection_id)?;
        let realm_changed = connection.public.realm != realm;
        connection.public.realm = realm;
        connection.public.label = identity_label(&verified.identity);
        connection.public.identity = Some(verified.identity);
        if realm_changed {
            connection.public.namespace = stable_namespace(&connection.public);
        }
        connection.public.workspaces = verified.workspaces;
        connection.public.status = ClockifyConnectionStatus::Connected;
        connection.public.last_error = None;
        connection.public.updated_at = now_ms();
        connection.encrypted_api_key = Some(encrypted_api_key);
        let response = connection.public.clone();
        persist_locked(&self.inner.store_path, &next)?;
        *state = next;
        drop(state);
        self.bump_revision(user_id).await;
        Ok(response)
    }

    pub async fn disconnect(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<ClockifyConnectionResponse, ClockifyConnectionError> {
        validate_connection_id(connection_id)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connection = find_connection_mut(&mut next, user_id, connection_id)?;
        connection.public.status = ClockifyConnectionStatus::Disconnected;
        connection.public.last_error = None;
        connection.public.updated_at = now_ms();
        connection.encrypted_api_key = None;
        let response = connection.public.clone();
        persist_locked(&self.inner.store_path, &next)?;
        *state = next;
        drop(state);
        self.bump_revision(user_id).await;
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

    fn server_for_connection(
        &self,
        user_id: &str,
        connection: &PersistedClockifyConnection,
    ) -> Option<SessionMcpServer> {
        if connection.public.status != ClockifyConnectionStatus::Connected {
            return None;
        }
        let identity = connection.public.identity.as_ref()?;
        let api_key = decrypt_string(connection.encrypted_api_key.as_deref()?, &self.inner.encryption_key).ok()?;
        let lock_dir = self
            .inner
            .store_path
            .parent()?
            .join("clockify-mutation-locks")
            .join(&hex::encode(Sha256::digest(user_id.as_bytes()))[..16])
            .join(&hex::encode(Sha256::digest(identity.account_id.as_bytes()))[..16]);
        let mut env = HashMap::from([
            (CLOCKIFY_API_KEY_ENV.into(), api_key),
            (CLOCKIFY_LOCK_DIR_ENV.into(), lock_dir.to_string_lossy().into_owned()),
            (CLOCKIFY_REALM_ENV.into(), realm_name(connection.public.realm).into()),
            (CLOCKIFY_ACCOUNT_ID_ENV.into(), identity.account_id.clone()),
            (CLOCKIFY_ACCOUNT_NAME_ENV.into(), identity.account_name.clone()),
            (CLOCKIFY_CONNECTION_ID_ENV.into(), connection.public.id.clone()),
        ]);
        if let Some(email) = &identity.account_email {
            env.insert(CLOCKIFY_ACCOUNT_EMAIL_ENV.into(), email.clone());
        }
        Some(SessionMcpServer {
            id: format!("plugin:{CLOCKIFY_PLUGIN_NAME}:{}", connection.public.id),
            name: connection.public.namespace.clone(),
            transport: SessionMcpTransport::Stdio {
                command: self.inner.mcp_command.clone(),
                args: vec!["mcp-clockify-stdio".into()],
                env,
            },
            auto_approve_in_unattended: false,
        })
    }

    async fn record_probe_error(
        &self,
        user_id: &str,
        connection_id: &str,
        error: &str,
    ) -> Result<(), ClockifyConnectionError> {
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connection = find_connection_mut(&mut next, user_id, connection_id)?;
        connection.public.status = ClockifyConnectionStatus::Error;
        connection.public.last_error = Some(truncate_error(error));
        connection.public.updated_at = now_ms();
        persist_locked(&self.inner.store_path, &next)?;
        *state = next;
        drop(state);
        self.bump_revision(user_id).await;
        Ok(())
    }

    async fn connection_operation(&self, user_id: &str, connection_id: &str) -> Arc<Mutex<()>> {
        let mut operations = self.inner.operations.lock().await;
        operations
            .entry(format!("{user_id}:{connection_id}"))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn bump_revision(&self, user_id: &str) {
        let mut revisions = self.inner.revisions.lock().await;
        let revision = revisions.entry(user_id.to_owned()).or_default();
        *revision = revision.saturating_add(1);
    }

    async fn revision(&self, user_id: &str) -> u64 {
        self.inner
            .revisions
            .lock()
            .await
            .get(user_id)
            .copied()
            .unwrap_or_default()
    }
}

pub fn clockify_api_base(realm: ClockifyRealm) -> &'static str {
    match realm {
        ClockifyRealm::Global => "https://api.clockify.me/api/v1",
        ClockifyRealm::Eu => "https://euc1.clockify.me/api/v1",
        ClockifyRealm::Us => "https://use2.clockify.me/api/v1",
        ClockifyRealm::Uk => "https://euw2.clockify.me/api/v1",
        ClockifyRealm::Au => "https://apse2.clockify.me/api/v1",
    }
}

pub fn realm_name(realm: ClockifyRealm) -> &'static str {
    match realm {
        ClockifyRealm::Global => "global",
        ClockifyRealm::Eu => "eu",
        ClockifyRealm::Us => "us",
        ClockifyRealm::Uk => "uk",
        ClockifyRealm::Au => "au",
    }
}

fn validate_api_key(api_key: &str) -> Result<(), ClockifyConnectionError> {
    if api_key.is_empty() || api_key.len() > MAX_API_KEY_LEN || api_key.trim() != api_key {
        return Err(ClockifyConnectionError::InvalidInput("API key is invalid".into()));
    }
    Ok(())
}

fn validate_probe_result(result: &ClockifyProbeResult) -> Result<(), ClockifyConnectionError> {
    if result.identity.account_id.is_empty()
        || result.identity.account_id.len() > 300
        || result.identity.account_name.is_empty()
        || result.identity.account_name.len() > 300
        || result
            .identity
            .account_email
            .as_ref()
            .is_some_and(|email| email.len() > 320)
        || result.workspaces.len() > 250
        || result.workspaces.iter().any(|workspace| {
            workspace.id.is_empty()
                || workspace.id.len() > 300
                || workspace.name.is_empty()
                || workspace.name.len() > 300
        })
    {
        return Err(ClockifyConnectionError::Probe(
            "Clockify returned invalid account metadata".into(),
        ));
    }
    Ok(())
}

fn validate_connection_id(connection_id: &str) -> Result<(), ClockifyConnectionError> {
    Uuid::parse_str(connection_id)
        .map(|_| ())
        .map_err(|_| ClockifyConnectionError::NotFound)
}

fn find_connection_mut<'a>(
    state: &'a mut PersistedClockifyState,
    user_id: &str,
    connection_id: &str,
) -> Result<&'a mut PersistedClockifyConnection, ClockifyConnectionError> {
    state
        .users
        .get_mut(user_id)
        .and_then(|user| user.connections.get_mut(connection_id))
        .ok_or(ClockifyConnectionError::NotFound)
}

fn identity_label(identity: &ClockifyVerifiedIdentity) -> String {
    match &identity.account_email {
        Some(email) => format!("{} · {email}", identity.account_name),
        None => identity.account_name.clone(),
    }
}

fn stable_namespace(connection: &ClockifyConnectionResponse) -> String {
    let identity = connection.identity.as_ref();
    let base = identity
        .and_then(|identity| identity.account_email.as_deref())
        .or_else(|| identity.map(|identity| identity.account_name.as_str()))
        .unwrap_or("account");
    let slug = base
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    let digest = hex::encode(Sha256::digest(connection.id.as_bytes()));
    format!(
        "clockify-{}-{}-{}",
        realm_name(connection.realm),
        if slug.is_empty() { "account" } else { &slug },
        &digest[..10]
    )
}

fn truncate_error(error: &str) -> String {
    let mut value = error.trim().chars().take(MAX_ERROR_LEN).collect::<String>();
    if error.trim().chars().count() > MAX_ERROR_LEN {
        value.push('…');
    }
    value
}

fn load_state(path: &Path) -> Result<PersistedClockifyState, ClockifyConnectionError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| ClockifyConnectionError::Persistence(format!("invalid state file: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PersistedClockifyState::default()),
        Err(error) => Err(ClockifyConnectionError::Persistence(error.to_string())),
    }
}

fn persist_locked(path: &Path, state: &PersistedClockifyState) -> Result<(), ClockifyConnectionError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))?;
    std::fs::write(&tmp_path, bytes).map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))?;
    set_private_file_permissions(&tmp_path)?;
    crate::state::replace_state_file(&tmp_path, path)
        .map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))
}

fn create_private_dir(path: &Path) -> Result<(), ClockifyConnectionError> {
    std::fs::create_dir_all(path).map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), ClockifyConnectionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| ClockifyConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixtureProbe;

    #[async_trait]
    impl ClockifyConnectionProbe for FixtureProbe {
        async fn verify(&self, _realm: ClockifyRealm, api_key: &str) -> Result<ClockifyProbeResult, String> {
            let suffix = api_key
                .strip_prefix("fixture-key-")
                .ok_or_else(|| "invalid fixture key".to_string())?;
            Ok(ClockifyProbeResult {
                identity: ClockifyVerifiedIdentity {
                    account_id: format!("account-{suffix}"),
                    account_name: format!("Account {suffix}"),
                    account_email: Some(format!("{suffix}@example.test")),
                },
                workspaces: vec![ClockifyWorkspaceIdentity {
                    id: format!("workspace-{suffix}"),
                    name: format!("Workspace {suffix}"),
                }],
            })
        }
    }

    fn manager(path: &Path, encryption_key: [u8; 32]) -> ClockifyConnectionManager {
        ClockifyConnectionManager::new(path, encryption_key, "/fixture/aioncore".into(), Arc::new(FixtureProbe))
            .unwrap()
    }

    fn server_env(server: &SessionMcpServer) -> &HashMap<String, String> {
        let SessionMcpTransport::Stdio { command, args, env } = &server.transport else {
            panic!("Clockify server must use stdio")
        };
        assert_eq!(command, "/fixture/aioncore");
        assert_eq!(args, &["mcp-clockify-stdio"]);
        env
    }

    #[tokio::test]
    async fn multiple_accounts_are_encrypted_isolated_and_available_without_project_binding() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path(), [7; 32]);
        let first = manager
            .connect_new("user-a", ClockifyRealm::Global, "fixture-key-one".into())
            .await
            .unwrap();
        let second = manager
            .connect_new("user-a", ClockifyRealm::Eu, "fixture-key-two".into())
            .await
            .unwrap();

        assert_ne!(first.id, second.id);
        assert_eq!(first.label, "Account one · one@example.test");
        assert_ne!(first.namespace, second.namespace);
        let stored = std::fs::read_to_string(temp.path().join(STORE_FILE)).unwrap();
        assert!(!stored.contains("fixture-key-one"));
        assert!(!stored.contains("fixture-key-two"));

        let (revision, servers) = manager.runtime_servers_snapshot("user-a").await;
        assert_eq!(revision, 2);
        assert_eq!(servers.len(), 2);
        assert!(servers.iter().all(|server| !server.auto_approve_in_unattended));
        let first_server = servers.iter().find(|server| server.id.ends_with(&first.id)).unwrap();
        let second_server = servers.iter().find(|server| server.id.ends_with(&second.id)).unwrap();
        assert_eq!(server_env(first_server)[CLOCKIFY_API_KEY_ENV], "fixture-key-one");
        assert_eq!(server_env(second_server)[CLOCKIFY_API_KEY_ENV], "fixture-key-two");
        assert_eq!(server_env(first_server)[CLOCKIFY_ACCOUNT_ID_ENV], "account-one");
        assert_eq!(server_env(second_server)[CLOCKIFY_ACCOUNT_ID_ENV], "account-two");
        assert_ne!(
            server_env(first_server)[CLOCKIFY_LOCK_DIR_ENV],
            server_env(second_server)[CLOCKIFY_LOCK_DIR_ENV]
        );
        assert!(!server_env(first_server)[CLOCKIFY_LOCK_DIR_ENV].contains("user-a"));
        assert!(!server_env(first_server)[CLOCKIFY_LOCK_DIR_ENV].contains("account-one"));
        assert!(manager.runtime_servers_snapshot("user-b").await.1.is_empty());

        let plugin_root = temp.path().join("plugins");
        let plugin_dir = plugin_root.join(CLOCKIFY_PLUGIN_NAME);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join(crate::constants::EXTENSION_MANIFEST_FILE),
            serde_json::to_vec(&serde_json::json!({
                "name": CLOCKIFY_PLUGIN_NAME,
                "version": "1.0.0",
                "apiVersion": "1.0.0",
                "contributes": {}
            }))
            .unwrap(),
        )
        .unwrap();
        let registry = crate::registry::ExtensionRegistry::new_with_plugin_connections(
            crate::state::ExtensionStateStore::new(temp.path().join("extension-states.json")),
            Arc::new(aionui_realtime::BroadcastEventBus::new(8)),
            "1.0.0".into(),
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
        let fresh = registry.runtime_snapshot("user-a", None).await;
        let resumed = registry.runtime_snapshot("user-a", Some("project-a")).await;
        let child = registry.runtime_snapshot("user-a", Some("project-a")).await;
        assert_eq!(fresh.mcp_servers.len(), 2);
        let identities = |servers: &[SessionMcpServer]| {
            servers
                .iter()
                .map(|server| (server.id.clone(), server.name.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(identities(&fresh.mcp_servers), identities(&resumed.mcp_servers));
        assert_eq!(identities(&resumed.mcp_servers), identities(&child.mcp_servers));

        manager.disconnect("user-a", &first.id).await.unwrap();
        let (_, remaining) = manager.runtime_servers_snapshot("user-a").await;
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].id.ends_with(&second.id));
    }

    #[tokio::test]
    async fn duplicate_accounts_are_rejected_and_reconnect_preserves_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path(), [9; 32]);
        let first = manager
            .connect_new("user-a", ClockifyRealm::Global, "fixture-key-one".into())
            .await
            .unwrap();

        let duplicate = manager
            .connect_new("user-a", ClockifyRealm::Global, "fixture-key-one".into())
            .await;
        assert!(matches!(duplicate, Err(ClockifyConnectionError::InvalidInput(_))));
        assert_eq!(manager.list("user-a").await.unwrap().connections.len(), 1);

        let reconnected = manager
            .reconnect("user-a", &first.id, ClockifyRealm::Global, "fixture-key-renamed".into())
            .await
            .unwrap();
        assert_eq!(reconnected.namespace, first.namespace);
        assert_eq!(reconnected.label, "Account renamed · renamed@example.test");

        let moved = manager
            .reconnect("user-a", &first.id, ClockifyRealm::Eu, "fixture-key-renamed".into())
            .await
            .unwrap();
        assert_ne!(moved.namespace, first.namespace);
        assert!(moved.namespace.starts_with("clockify-eu-"));
    }

    #[tokio::test]
    async fn same_provider_identity_in_different_realms_has_distinct_routing_names() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path(), [11; 32]);
        let global = manager
            .connect_new("user-a", ClockifyRealm::Global, "fixture-key-one".into())
            .await
            .unwrap();
        let eu = manager
            .connect_new("user-a", ClockifyRealm::Eu, "fixture-key-one".into())
            .await
            .unwrap();

        assert!(global.namespace.starts_with("clockify-global-one-example-test-"));
        assert!(eu.namespace.starts_with("clockify-eu-one-example-test-"));
        assert_ne!(global.namespace, eu.namespace);
        let (_, servers) = manager.runtime_servers_snapshot("user-a").await;
        let realms = servers
            .iter()
            .map(|server| server_env(server)[CLOCKIFY_REALM_ENV].clone())
            .collect::<Vec<_>>();
        assert!(realms.contains(&"global".into()));
        assert!(realms.contains(&"eu".into()));
    }

    #[tokio::test]
    async fn concurrent_duplicate_connects_publish_only_one_account() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path(), [10; 32]);
        let left = manager.connect_new("user-a", ClockifyRealm::Eu, "fixture-key-one".into());
        let right = manager.connect_new("user-a", ClockifyRealm::Eu, "fixture-key-one".into());
        let (left, right) = tokio::join!(left, right);
        assert_ne!(left.is_ok(), right.is_ok());
        assert_eq!(manager.list("user-a").await.unwrap().connections.len(), 1);
    }

    #[tokio::test]
    async fn encrypted_state_with_a_different_key_requires_reconnect() {
        let temp = tempfile::tempdir().unwrap();
        let connected = manager(temp.path(), [1; 32])
            .connect_new("user-a", ClockifyRealm::Us, "fixture-key-one".into())
            .await
            .unwrap();

        let reloaded = manager(temp.path(), [2; 32]);
        let listed = reloaded.list("user-a").await.unwrap();
        assert_eq!(listed.connections.len(), 1);
        assert_eq!(listed.connections[0].id, connected.id);
        assert_eq!(
            listed.connections[0].status,
            ClockifyConnectionStatus::ReconnectRequired
        );
        assert!(reloaded.runtime_servers_snapshot("user-a").await.1.is_empty());
    }

    #[tokio::test]
    async fn persistence_failure_does_not_publish_an_unstored_connection() {
        let temp = tempfile::tempdir().unwrap();
        let manager = manager(temp.path(), [3; 32]);
        std::fs::remove_file(temp.path().join(STORE_FILE)).unwrap();
        std::fs::create_dir(temp.path().join(STORE_FILE)).unwrap();

        let result = manager
            .connect_new("user-a", ClockifyRealm::Global, "fixture-key-one".into())
            .await;
        assert!(matches!(result, Err(ClockifyConnectionError::Persistence(_))));
        assert!(manager.list("user-a").await.unwrap().connections.is_empty());
        assert!(manager.runtime_servers_snapshot("user-a").await.1.is_empty());
    }

    #[test]
    fn bundled_clockify_manifest_and_skill_are_valid() {
        let plugin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../plugins/clockify");
        let loaded = crate::loader::load_single_extension(
            &plugin_dir.join(crate::constants::EXTENSION_MANIFEST_FILE),
            &plugin_dir,
            crate::types::ExtensionSource::Builtin,
        )
        .unwrap();
        assert_eq!(loaded.manifest.name, CLOCKIFY_PLUGIN_NAME);
        let skills = &loaded.manifest.contributes.unwrap().skills;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "clockify-time-tracking");
    }
}
