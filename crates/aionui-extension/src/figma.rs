use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{
    FigmaConnectionResponse, FigmaConnectionStatus, FigmaConnectionsResponse, FigmaVerifiedIdentity, SessionMcpServer,
    SessionMcpTransport,
};
use aionui_common::{decrypt_string, encrypt_string, now_ms};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const FIGMA_PLUGIN_NAME: &str = "figma";
pub const FIGMA_TOKEN_ENV: &str = "ELLM_SHA_PRIVATE_FIGMA_TOKEN";
pub const FIGMA_ACCOUNT_ID_ENV: &str = "ELLM_SHA_FIGMA_ACCOUNT_ID";
pub const FIGMA_ACCOUNT_NAME_ENV: &str = "ELLM_SHA_FIGMA_ACCOUNT_NAME";
pub const FIGMA_ACCOUNT_EMAIL_ENV: &str = "ELLM_SHA_FIGMA_ACCOUNT_EMAIL";
pub const FIGMA_CONNECTION_ID_ENV: &str = "ELLM_SHA_FIGMA_CONNECTION_ID";

const STORE_VERSION: u32 = 1;
const STORE_FILE: &str = "figma-connections.json";
const MAX_TOKEN_LEN: usize = 16_384;
const MAX_CONNECTIONS_PER_USER: usize = 25;

#[derive(Debug, thiserror::Error)]
pub enum FigmaConnectionError {
    #[error("Figma connection not found")]
    NotFound,
    #[error("Figma connection input is invalid: {0}")]
    InvalidInput(String),
    #[error("Figma authentication failed: {0}")]
    Probe(String),
    #[error("failed to persist Figma connection state: {0}")]
    Persistence(String),
}

/// Only safe, typed provider failures may cross the account-management boundary.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum FigmaProbeFailure {
    #[error("Figma rejected the token or its current_user:read scope. Create a valid token and reconnect.")]
    Unauthorized,
    #[error("Figma account verification is temporarily unavailable. Try again later.")]
    Unavailable,
    #[error("Figma returned an invalid account identity. The connection was not saved.")]
    InvalidIdentity,
}
#[async_trait]
pub trait FigmaConnectionProbe: Send + Sync {
    async fn verify(&self, token: &str) -> Result<FigmaVerifiedIdentity, FigmaProbeFailure>;
}

#[derive(Clone)]
pub struct FigmaConnectionManager {
    inner: Arc<FigmaConnectionManagerInner>,
}

struct FigmaConnectionManagerInner {
    store_path: PathBuf,
    encryption_key: [u8; 32],
    mcp_command: String,
    state: Mutex<PersistedFigmaState>,
    operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    revisions: Mutex<HashMap<String, u64>>,
    probe: Arc<dyn FigmaConnectionProbe>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedFigmaState {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    users: BTreeMap<String, PersistedFigmaUser>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedFigmaUser {
    #[serde(default)]
    connections: BTreeMap<String, PersistedFigmaConnection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedFigmaConnection {
    public: FigmaConnectionResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_token: Option<String>,
}

const fn store_version() -> u32 {
    STORE_VERSION
}

impl FigmaConnectionManager {
    pub fn new(
        data_dir: &Path,
        encryption_key: [u8; 32],
        mcp_command: String,
        probe: Arc<dyn FigmaConnectionProbe>,
    ) -> Result<Self, FigmaConnectionError> {
        let store_path = data_dir.join(STORE_FILE);
        let mut state = load_state(&store_path)?;
        if state.version > STORE_VERSION {
            return Err(FigmaConnectionError::Persistence("unsupported state version".into()));
        }
        for user in state.users.values() {
            for (id, connection) in &user.connections {
                if id != &connection.public.id || Uuid::parse_str(id).is_err() {
                    return Err(FigmaConnectionError::Persistence("invalid connection identity".into()));
                }
            }
        }
        let mut changed = state.version != STORE_VERSION;
        state.version = STORE_VERSION;
        for user in state.users.values_mut() {
            for connection in user.connections.values_mut() {
                if connection.public.namespace.is_empty() {
                    connection.public.namespace = stable_namespace(&connection.public);
                    changed = true;
                }
                if connection.public.status == FigmaConnectionStatus::Connected
                    && (connection.public.identity.is_none()
                        || connection
                            .encrypted_token
                            .as_deref()
                            .and_then(|value| decrypt_string(value, &encryption_key).ok())
                            .is_none())
                {
                    connection.public.status = FigmaConnectionStatus::ReconnectRequired;
                    connection.public.last_error =
                        Some("Local Figma authentication could not be decrypted. Reconnect this account.".into());
                    connection.public.updated_at = now_ms();
                    changed = true;
                }
            }
        }
        if changed {
            persist_locked(&store_path, &state)?;
        }
        Ok(Self {
            inner: Arc::new(FigmaConnectionManagerInner {
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

    pub async fn list(&self, user_id: &str) -> Result<FigmaConnectionsResponse, FigmaConnectionError> {
        let state = self.inner.state.lock().await;
        let connections = state
            .users
            .get(user_id)
            .into_iter()
            .flat_map(|user| user.connections.values())
            .map(|connection| connection.public.clone())
            .collect();
        Ok(FigmaConnectionsResponse { connections })
    }

    pub async fn connect_new(
        &self,
        user_id: &str,
        token: String,
    ) -> Result<FigmaConnectionResponse, FigmaConnectionError> {
        validate_token(&token)?;
        let operation = self.connection_operation(user_id, "new").await;
        let _operation = operation.lock().await;
        let verified = self
            .inner
            .probe
            .verify(&token)
            .await
            .map_err(|error| FigmaConnectionError::Probe(error.to_string()))?;
        validate_identity(&verified)?;

        let id = Uuid::now_v7().to_string();
        let now = now_ms();
        let mut public = FigmaConnectionResponse {
            id: id.clone(),
            namespace: String::new(),
            label: identity_label(&verified),
            status: FigmaConnectionStatus::Connected,
            identity: Some(verified),
            last_error: None,
            created_at: now,
            updated_at: now,
        };
        public.namespace = stable_namespace(&public);
        let encrypted_token = encrypt_string(&token, &self.inner.encryption_key)
            .map_err(|error| FigmaConnectionError::Persistence(error.to_string()))?;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connections = &mut next.users.entry(user_id.to_owned()).or_default().connections;
        if connections.len() >= MAX_CONNECTIONS_PER_USER {
            return Err(FigmaConnectionError::InvalidInput(format!(
                "at most {MAX_CONNECTIONS_PER_USER} Figma accounts may be stored"
            )));
        }
        if connections.values().any(|connection| {
            connection.public.identity.as_ref().is_some_and(|identity| {
                identity.account_id == public.identity.as_ref().expect("verified identity").account_id
            })
        }) {
            return Err(FigmaConnectionError::InvalidInput(
                "this Figma account is already connected; reconnect the existing account".into(),
            ));
        }
        connections.insert(
            id,
            PersistedFigmaConnection {
                public: public.clone(),
                encrypted_token: Some(encrypted_token),
            },
        );
        persist_locked(&self.inner.store_path, &next)?;
        *state = next;
        self.bump_revision(user_id).await;
        drop(state);
        tracing::info!(
            connection_id = public.id,
            stage = "connected",
            "Figma account connected"
        );
        Ok(public)
    }

    pub async fn reconnect(
        &self,
        user_id: &str,
        connection_id: &str,
        token: String,
    ) -> Result<FigmaConnectionResponse, FigmaConnectionError> {
        validate_connection_id(connection_id)?;
        validate_token(&token)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        {
            let mut state = self.inner.state.lock().await;
            find_connection_mut(&mut state, user_id, connection_id)?;
        }
        let verified = match self.inner.probe.verify(&token).await {
            Ok(result) => result,
            Err(error) => {
                self.record_probe_error(user_id, connection_id, &error.to_string())
                    .await?;
                return Err(FigmaConnectionError::Probe(error.to_string()));
            }
        };
        validate_identity(&verified)?;
        let encrypted_token = encrypt_string(&token, &self.inner.encryption_key)
            .map_err(|error| FigmaConnectionError::Persistence(error.to_string()))?;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let duplicate_account = next
            .users
            .get(user_id)
            .into_iter()
            .flat_map(|user| user.connections.values())
            .any(|connection| {
                connection.public.id != connection_id
                    && connection
                        .public
                        .identity
                        .as_ref()
                        .is_some_and(|identity| identity.account_id == verified.account_id)
            });
        if duplicate_account {
            return Err(FigmaConnectionError::InvalidInput(
                "this Figma account is already connected; reconnect that existing account".into(),
            ));
        }
        let connection = find_connection_mut(&mut next, user_id, connection_id)?;
        if connection
            .public
            .identity
            .as_ref()
            .is_some_and(|identity| identity.account_id != verified.account_id)
        {
            return Err(FigmaConnectionError::InvalidInput(
                "Reconnect requires the same Figma account. Add a separate account instead.".into(),
            ));
        }
        connection.public.label = identity_label(&verified);
        connection.public.identity = Some(verified);
        connection.public.status = FigmaConnectionStatus::Connected;
        connection.public.last_error = None;
        connection.public.updated_at = now_ms();
        connection.encrypted_token = Some(encrypted_token);
        let response = connection.public.clone();
        persist_locked(&self.inner.store_path, &next)?;
        *state = next;
        self.bump_revision(user_id).await;
        drop(state);
        tracing::info!(connection_id, stage = "account_updated", "Figma account state updated");
        Ok(response)
    }

    pub async fn disconnect(
        &self,
        user_id: &str,
        connection_id: &str,
    ) -> Result<FigmaConnectionResponse, FigmaConnectionError> {
        validate_connection_id(connection_id)?;
        let operation = self.connection_operation(user_id, connection_id).await;
        let _operation = operation.lock().await;
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connection = find_connection_mut(&mut next, user_id, connection_id)?;
        connection.public.status = FigmaConnectionStatus::Disconnected;
        connection.public.last_error = None;
        connection.public.updated_at = now_ms();
        connection.encrypted_token = None;
        let response = connection.public.clone();
        let persisted = persist_locked(&self.inner.store_path, &next);
        if persisted.is_err() {
            let connection = find_connection_mut(&mut next, user_id, connection_id)?;
            connection.public.status = FigmaConnectionStatus::ReconnectRequired;
            connection.public.last_error = Some("Figma credentials are disabled for this process, but the change could not be saved. Repair local storage before restarting.".into());
        }
        *state = next;
        self.bump_revision(user_id).await;
        drop(state);
        persisted?;
        tracing::info!(connection_id, stage = "account_updated", "Figma account state updated");
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

    fn server_for_connection(&self, _user_id: &str, connection: &PersistedFigmaConnection) -> Option<SessionMcpServer> {
        if connection.public.status != FigmaConnectionStatus::Connected {
            return None;
        }
        let identity = connection.public.identity.as_ref()?;
        let token = decrypt_string(connection.encrypted_token.as_deref()?, &self.inner.encryption_key).ok()?;
        let mut env = HashMap::from([
            (FIGMA_TOKEN_ENV.into(), token),
            (FIGMA_ACCOUNT_ID_ENV.into(), identity.account_id.clone()),
            (FIGMA_ACCOUNT_NAME_ENV.into(), identity.account_name.clone()),
            (FIGMA_CONNECTION_ID_ENV.into(), connection.public.id.clone()),
        ]);
        if let Some(email) = &identity.account_email {
            env.insert(FIGMA_ACCOUNT_EMAIL_ENV.into(), email.clone());
        }
        Some(SessionMcpServer {
            id: format!("plugin:{FIGMA_PLUGIN_NAME}:{}", connection.public.id),
            name: connection.public.namespace.clone(),
            transport: SessionMcpTransport::Stdio {
                command: self.inner.mcp_command.clone(),
                args: vec!["mcp-figma-stdio".into()],
                env,
            },
            approval_identity: Some(format!("figma:{}:{}", connection.public.id, identity.account_id)),
            auto_approve_in_unattended: false,
        })
    }

    async fn record_probe_error(
        &self,
        user_id: &str,
        connection_id: &str,
        error: &str,
    ) -> Result<(), FigmaConnectionError> {
        let mut state = self.inner.state.lock().await;
        let mut next = state.clone();
        let connection = find_connection_mut(&mut next, user_id, connection_id)?;
        connection.public.status = FigmaConnectionStatus::ReconnectRequired;
        connection.encrypted_token = None;
        connection.public.last_error = Some(error.to_owned());
        connection.public.updated_at = now_ms();
        let persisted = persist_locked(&self.inner.store_path, &next);
        *state = next;
        self.bump_revision(user_id).await;
        drop(state);
        persisted
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

fn validate_token(token: &str) -> Result<(), FigmaConnectionError> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_LEN
        || token.trim() != token
        || token.bytes().any(|byte| !byte.is_ascii_graphic())
    {
        return Err(FigmaConnectionError::InvalidInput(
            "Personal access token is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_identity(identity: &FigmaVerifiedIdentity) -> Result<(), FigmaConnectionError> {
    if identity.account_id.is_empty()
        || identity.account_id.len() > 300
        || identity.account_name.is_empty()
        || identity.account_name.len() > 300
        || identity.account_email.as_ref().is_some_and(|email| email.len() > 320)
    {
        return Err(FigmaConnectionError::Probe(
            FigmaProbeFailure::InvalidIdentity.to_string(),
        ));
    }
    Ok(())
}

fn validate_connection_id(connection_id: &str) -> Result<(), FigmaConnectionError> {
    Uuid::parse_str(connection_id)
        .map(|_| ())
        .map_err(|_| FigmaConnectionError::NotFound)
}

fn find_connection_mut<'a>(
    state: &'a mut PersistedFigmaState,
    user_id: &str,
    connection_id: &str,
) -> Result<&'a mut PersistedFigmaConnection, FigmaConnectionError> {
    state
        .users
        .get_mut(user_id)
        .and_then(|user| user.connections.get_mut(connection_id))
        .ok_or(FigmaConnectionError::NotFound)
}

fn identity_label(identity: &FigmaVerifiedIdentity) -> String {
    match &identity.account_email {
        Some(email) => format!("{} · {email}", identity.account_name),
        None => identity.account_name.clone(),
    }
}

fn stable_namespace(connection: &FigmaConnectionResponse) -> String {
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
        "figma-{}-{}",
        if slug.is_empty() { "account" } else { &slug },
        &digest[..10]
    )
}

fn load_state(path: &Path) -> Result<PersistedFigmaState, FigmaConnectionError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| FigmaConnectionError::Persistence(format!("invalid state file: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PersistedFigmaState::default()),
        Err(error) => Err(FigmaConnectionError::Persistence(error.to_string())),
    }
}

fn persist_locked(path: &Path, state: &PersistedFigmaState) -> Result<(), FigmaConnectionError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let bytes =
        serde_json::to_vec_pretty(state).map_err(|error| FigmaConnectionError::Persistence(error.to_string()))?;
    std::fs::write(&tmp_path, bytes).map_err(|error| FigmaConnectionError::Persistence(error.to_string()))?;
    set_private_file_permissions(&tmp_path)?;
    crate::state::replace_state_file(&tmp_path, path)
        .map_err(|error| FigmaConnectionError::Persistence(error.to_string()))
}

fn create_private_dir(path: &Path) -> Result<(), FigmaConnectionError> {
    std::fs::create_dir_all(path).map_err(|error| FigmaConnectionError::Persistence(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| FigmaConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), FigmaConnectionError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| FigmaConnectionError::Persistence(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Probe(AtomicUsize);
    #[async_trait]
    impl FigmaConnectionProbe for Probe {
        async fn verify(&self, token: &str) -> Result<FigmaVerifiedIdentity, FigmaProbeFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let id = token
                .strip_prefix("fixture-token-")
                .ok_or(FigmaProbeFailure::Unauthorized)?;
            Ok(FigmaVerifiedIdentity {
                account_id: id.into(),
                account_name: format!("Account {id}"),
                account_email: Some(format!("{id}@example.test")),
            })
        }
    }
    fn manager(path: &Path, key: [u8; 32]) -> (FigmaConnectionManager, Arc<Probe>) {
        let probe = Arc::new(Probe(AtomicUsize::new(0)));
        (
            FigmaConnectionManager::new(path, key, "/fixture/aioncore".into(), probe.clone()).unwrap(),
            probe,
        )
    }
    #[tokio::test]
    async fn multiple_accounts_encrypt_tokens_isolate_users_and_restore_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(temp.path(), [7; 32]);
        let one = manager.connect_new("owner", "fixture-token-one".into()).await.unwrap();
        let two = manager.connect_new("owner", "fixture-token-two".into()).await.unwrap();
        assert_ne!(one.namespace, two.namespace);
        let bytes = std::fs::read_to_string(temp.path().join(STORE_FILE)).unwrap();
        assert!(!bytes.contains("fixture-token-"));
        assert!(
            !serde_json::to_string(&manager.list("owner").await.unwrap())
                .unwrap()
                .contains("token")
        );
        assert!(manager.list("foreign").await.unwrap().connections.is_empty());
        let (revision, servers) = manager.runtime_servers_snapshot("owner").await;
        assert_eq!(revision, 2);
        assert_eq!(servers.len(), 2);
        for server in &servers {
            assert!(!server.auto_approve_in_unattended);
            let SessionMcpTransport::Stdio { command, args, env } = &server.transport else {
                panic!("stdio required")
            };
            assert_eq!(command, "/fixture/aioncore");
            assert_eq!(args, &["mcp-figma-stdio"]);
            let expected = if server.id.ends_with(&one.id) { "one" } else { "two" };
            assert_eq!(env[FIGMA_TOKEN_ENV], format!("fixture-token-{expected}"));
            assert_eq!(env[FIGMA_ACCOUNT_ID_ENV], expected);
        }
        assert!(manager.runtime_servers_snapshot("foreign").await.1.is_empty());
        drop(manager);
        let (restored, _) = self::manager(temp.path(), [7; 32]);
        assert_eq!(restored.runtime_servers_snapshot("owner").await.1.len(), 2);
        drop(restored);
        let (wrong_key, _) = self::manager(temp.path(), [9; 32]);
        assert!(wrong_key.runtime_servers_snapshot("owner").await.1.is_empty());
        assert!(
            wrong_key
                .list("owner")
                .await
                .unwrap()
                .connections
                .iter()
                .all(|c| c.status == FigmaConnectionStatus::ReconnectRequired)
        );
    }
    #[tokio::test]
    async fn duplicate_concurrent_connect_and_foreign_reconnect_do_not_mix_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, probe) = manager(temp.path(), [7; 32]);
        let (a, b) = tokio::join!(
            manager.connect_new("owner", "fixture-token-one".into()),
            manager.connect_new("owner", "fixture-token-one".into())
        );
        assert!(a.is_ok());
        assert!(matches!(b, Err(FigmaConnectionError::InvalidInput(_))));
        let one = a.unwrap();
        let before = probe.0.load(Ordering::SeqCst);
        assert!(matches!(
            manager.reconnect("foreign", &one.id, "fixture-token-one".into()).await,
            Err(FigmaConnectionError::NotFound)
        ));
        assert_eq!(probe.0.load(Ordering::SeqCst), before);
        assert!(matches!(
            manager.disconnect("foreign", &one.id).await,
            Err(FigmaConnectionError::NotFound)
        ));
        let error = manager
            .reconnect("owner", &one.id, "fixture-token-two".into())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("same Figma account"));
        assert_eq!(
            manager.list("owner").await.unwrap().connections[0]
                .identity
                .as_ref()
                .unwrap()
                .account_id,
            "one"
        );
    }
    #[tokio::test]
    async fn failed_disconnect_persistence_revokes_live_tools_and_reports_incomplete_storage() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(temp.path(), [7; 32]);
        let one = manager.connect_new("owner", "fixture-token-one".into()).await.unwrap();
        let revision = manager.runtime_servers_snapshot("owner").await.0;
        std::fs::create_dir(temp.path().join(STORE_FILE).with_extension("json.tmp")).unwrap();
        assert!(matches!(
            manager.disconnect("owner", &one.id).await,
            Err(FigmaConnectionError::Persistence(_))
        ));
        let (next_revision, servers) = manager.runtime_servers_snapshot("owner").await;
        assert!(next_revision > revision);
        assert!(servers.is_empty());
        let state = manager.list("owner").await.unwrap();
        assert_eq!(state.connections[0].status, FigmaConnectionStatus::ReconnectRequired);
        assert!(
            state.connections[0]
                .last_error
                .as_ref()
                .unwrap()
                .contains("could not be saved")
        );
    }

    #[tokio::test]
    async fn failed_reconnect_revokes_runtime_and_disconnect_removes_encrypted_secret() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(temp.path(), [7; 32]);
        let one = manager.connect_new("owner", "fixture-token-one".into()).await.unwrap();
        let namespace = one.namespace;
        assert!(matches!(
            manager.reconnect("owner", &one.id, "expired".into()).await,
            Err(FigmaConnectionError::Probe(_))
        ));
        assert!(manager.runtime_servers_snapshot("owner").await.1.is_empty());
        let restored = manager
            .reconnect("owner", &one.id, "fixture-token-one".into())
            .await
            .unwrap();
        assert_eq!(restored.namespace, namespace);
        manager.disconnect("owner", &one.id).await.unwrap();
        assert!(manager.runtime_servers_snapshot("owner").await.1.is_empty());
        let stored = std::fs::read_to_string(temp.path().join(STORE_FILE)).unwrap();
        assert!(!stored.contains("encrypted_token"));
        for token in ["", "contains space", "token\n", "token\0"] {
            assert!(matches!(
                manager.connect_new("owner", token.into()).await,
                Err(FigmaConnectionError::InvalidInput(_))
            ));
        }
    }
}
