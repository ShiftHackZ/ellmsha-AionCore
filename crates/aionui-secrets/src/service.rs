use crate::{
    crypto::{self, VaultKey},
    vault::{OperationReceipt, StoredRecord, Vault},
};
use aionui_api_types::secrets::*;
use aionui_db::{DbError, ISecretsRepository};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SecretsError {
    #[error("CONTEXT_REQUIRED")]
    ContextRequired,
    #[error("PROJECT_SCOPE_REQUIRED")]
    ProjectScopeRequired,
    #[error("PLUGIN_DISABLED")]
    PluginDisabled,
    #[error("MESSAGE_BUSY")]
    MessageBusy,
    #[error("VAULT_LOCKED")]
    Locked,
    #[error("VAULT_NOT_INITIALIZED")]
    NotInitialized,
    #[error("INVALID_INPUT")]
    InvalidInput,
    #[error("AUTHENTICATION_FAILED")]
    Crypto,
    #[error("REVISION_CONFLICT")]
    Conflict,
    #[error("NOT_FOUND")]
    NotFound,
    #[error("FORBIDDEN")]
    Forbidden,
    #[error("STORAGE_UNAVAILABLE")]
    Storage,
}
impl From<DbError> for SecretsError {
    fn from(value: DbError) -> Self {
        if let DbError::Conflict(code) = &value {
            match code.as_str() {
                "VAULT_LOCKED" => return Self::Locked,
                "PLUGIN_DISABLED" => return Self::PluginDisabled,
                "CONTEXT_REQUIRED" => return Self::ContextRequired,
                "PROJECT_SCOPE_REQUIRED" => return Self::ProjectScopeRequired,
                "MESSAGE_BUSY" => return Self::MessageBusy,
                _ => {}
            }
        }
        if matches!(value, DbError::Conflict(_)) {
            Self::Conflict
        } else {
            Self::Storage
        }
    }
}

struct Unlocked {
    key: VaultKey,
    last_used: Instant,
    vault_id: String,
}

/// Owns keys in memory only. Callers must authorize owner/project before entering this service.
pub struct SecretsService {
    repository: Arc<dyn ISecretsRepository>,
    unlocked: Arc<Mutex<BTreeMap<String, Unlocked>>>,
    /// Serializes lifecycle and writes; the repository also CAS-protects against another process.
    writer: Mutex<()>,
    // Base64 export is <43 MiB, leaving room for its JSON and passphrase under 64 MiB HTTP/UI limits.
    max_snapshot_bytes: usize,
}

pub(crate) fn validate_record(record: &SecretRecordInput) -> Result<(), SecretsError> {
    let bytes = Zeroizing::new(serde_json::to_vec(record).map_err(|_| SecretsError::InvalidInput)?);
    if bytes.len() > 65536
        || record.title.trim().is_empty()
        || record.title.len() > 512
        || record.fields.len() > 64
        || record.bindings.len() > 32
    {
        return Err(SecretsError::InvalidInput);
    }
    if String::from_utf8_lossy(&bytes)
        .to_ascii_lowercase()
        .contains("otpauth:")
    {
        return Err(SecretsError::InvalidInput);
    }
    for binding in &record.bindings {
        if binding
            .target
            .contains(|c: char| c.is_whitespace() || c.is_control() || c == '*')
            || binding.target.len() > 2048
        {
            return Err(SecretsError::InvalidInput);
        }
        if binding.target.contains("://") {
            let url = url::Url::parse(&binding.target).map_err(|_| SecretsError::InvalidInput)?;
            if !matches!(url.scheme(), "https" | "http")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(SecretsError::InvalidInput);
            }
        }
    }
    if record.fields.keys().any(|name| {
        name.is_empty()
            || name.len() > 128
            || ["otp", "totp", "recovery_code", "recovery_codes", "seed", "passkey"]
                .contains(&name.to_ascii_lowercase().as_str())
    }) || record
        .fields
        .values()
        .any(|f| f.value.to_ascii_lowercase().contains("otpauth:"))
    {
        return Err(SecretsError::InvalidInput);
    }
    Ok(())
}

fn fingerprint(value: &impl Serialize) -> Result<String, SecretsError> {
    let bytes = Zeroizing::new(serde_json::to_vec(value).map_err(|_| SecretsError::InvalidInput)?);
    Ok(format!("{:x}", Sha256::digest(bytes.as_slice())))
}

fn metadata(row: &StoredRecord, record: SecretRecordInput, incarnation: &str) -> SecretMetadata {
    SecretMetadata {
        incarnation: incarnation.into(),
        id: row.id.clone(),
        revision: row.revision,
        scope: row.scope.clone(),
        title: record.title,
        kind: record.kind,
        bindings: record.bindings,
    }
}

impl SecretsService {
    pub fn new(repository: Arc<dyn ISecretsRepository>) -> Self {
        let unlocked = Arc::new(Mutex::new(BTreeMap::<String, Unlocked>::new()));
        let weak = Arc::downgrade(&unlocked);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(keys) = weak.upgrade() else { break };
                keys.lock()
                    .await
                    .retain(|_, entry| entry.last_used.elapsed() < Duration::from_secs(900));
            }
        });
        Self {
            repository,
            unlocked,
            writer: Mutex::new(()),
            max_snapshot_bytes: 32 * 1024 * 1024,
        }
    }
    async fn load(&self, owner: &str) -> Result<(i64, Vault), SecretsError> {
        let snapshot = self.repository.load(owner).await?.ok_or(SecretsError::NotInitialized)?;
        if snapshot.ciphertext.len() > self.max_snapshot_bytes {
            return Err(SecretsError::InvalidInput);
        }
        let vault: Vault = serde_json::from_slice(&snapshot.ciphertext).map_err(|_| SecretsError::Crypto)?;
        if vault.format != 1 || vault.owner != owner {
            return Err(SecretsError::Crypto);
        }
        if let Some(unlocked) = self.unlocked.lock().await.get(owner)
            && unlocked.vault_id == vault.id
        {
            vault.verify_inventory(&unlocked.key)?;
        }
        Ok((snapshot.generation, vault))
    }
    async fn key(&self, owner: &str, vault: &Vault, touch: bool) -> Result<VaultKey, SecretsError> {
        let mut keys = self.unlocked.lock().await;
        keys.retain(|_, v| v.last_used.elapsed() < Duration::from_secs(900));
        let value = keys.get_mut(owner).ok_or(SecretsError::Locked)?;
        if value.vault_id != vault.id {
            keys.remove(owner);
            return Err(SecretsError::Locked);
        }
        if touch {
            value.last_used = Instant::now();
        }
        Ok(value.key.clone())
    }
    async fn save(
        &self,
        owner: &str,
        generation: Option<i64>,
        vault: &Vault,
        key: &[u8; 32],
    ) -> Result<(), SecretsError> {
        let mut vault = vault.clone();
        vault.authenticate_inventory(key)?;
        let ciphertext = serde_json::to_vec(&vault).map_err(|_| SecretsError::Crypto)?;
        // Every live record reserves enough encoded space for one maximum-sized
        // permanent deletion receipt. Base64 sizes do not vary with fresh nonces.
        if ciphertext
            .len()
            .saturating_add(vault.records.len() * DELETE_RECEIPT_RESERVE)
            > self.max_snapshot_bytes
        {
            return Err(SecretsError::InvalidInput);
        }
        self.repository.replace(owner, generation, &ciphertext).await?;
        Ok(())
    }
    pub async fn status(&self, owner: &str) -> Result<VaultStatus, SecretsError> {
        let Some(snapshot) = self.repository.load(owner).await? else {
            return Ok(VaultStatus {
                incarnation: None,
                initialized: false,
                unlocked: false,
                vault_id: None,
            });
        };
        if snapshot.ciphertext.len() > self.max_snapshot_bytes {
            return Err(SecretsError::InvalidInput);
        }
        let vault: Vault = serde_json::from_slice(&snapshot.ciphertext).map_err(|_| SecretsError::Crypto)?;
        if vault.owner != owner {
            return Err(SecretsError::Crypto);
        }
        let mut keys = self.unlocked.lock().await;
        keys.retain(|_, v| v.last_used.elapsed() < Duration::from_secs(900));
        if let Some(unlocked) = keys.get(owner)
            && unlocked.vault_id == vault.id
        {
            vault.verify_inventory(&unlocked.key)?;
        }
        Ok(VaultStatus {
            incarnation: Some(vault.incarnation),
            initialized: true,
            unlocked: keys.get(owner).is_some_and(|v| v.vault_id == vault.id),
            vault_id: Some(vault.id),
        })
    }
    pub async fn initialize(&self, owner: &str, passphrase: Zeroizing<String>) -> Result<VaultStatus, SecretsError> {
        let _write = self.writer.lock().await;
        if self.repository.load(owner).await?.is_some() {
            return Err(SecretsError::Conflict);
        }
        let key = crypto::random_key()?;
        let id = Uuid::now_v7().to_string();
        let identity = serde_json::to_vec(&(1u32, &id, owner)).map_err(|_| SecretsError::Crypto)?;
        let mut vault = Vault {
            incarnation: Uuid::now_v7().to_string(),
            format: 1,
            id: id.clone(),
            owner: owner.into(),
            envelope: crypto::wrap_async(key.clone(), passphrase, identity).await?,
            records: BTreeMap::new(),
            receipts: vec![],
            inventory: vec![],
        };
        vault.write_receipts(&key, &BTreeMap::new())?;
        self.save(owner, None, &vault, &key).await?;
        self.unlocked.lock().await.insert(
            owner.into(),
            Unlocked {
                key,
                last_used: Instant::now(),
                vault_id: id,
            },
        );
        self.status(owner).await
    }
    pub async fn unlock(&self, owner: &str, passphrase: Zeroizing<String>) -> Result<VaultStatus, SecretsError> {
        let _write = self.writer.lock().await;
        let (_, vault) = self.load(owner).await?;
        let key = crypto::unwrap_async(vault.envelope.clone(), passphrase, vault.identity()).await?;
        vault.validate(&key, owner)?;
        self.unlocked.lock().await.insert(
            owner.into(),
            Unlocked {
                key,
                last_used: Instant::now(),
                vault_id: vault.id,
            },
        );
        self.status(owner).await
    }
    /// Desktop main-process capability only; never exposed to the renderer or agent routes.
    pub async fn desktop_key(&self, owner: &str) -> Result<DesktopVaultKey, SecretsError> {
        let _write = self.writer.lock().await;
        let (_, vault) = self.load(owner).await?;
        let key = self.key(owner, &vault, true).await?;
        Ok(DesktopVaultKey {
            vault_id: vault.id.clone(),
            envelope_id: fingerprint(&vault.envelope)?,
            key: STANDARD.encode(key.as_slice()),
        })
    }
    pub async fn unlock_desktop(&self, owner: &str, request: DesktopVaultKey) -> Result<VaultStatus, SecretsError> {
        let _write = self.writer.lock().await;
        let (_, vault) = self.load(owner).await?;
        if request.vault_id != vault.id || request.envelope_id != fingerprint(&vault.envelope)? {
            return Err(SecretsError::Crypto);
        }
        let encoded = Zeroizing::new(request.key);
        let bytes = Zeroizing::new(STANDARD.decode(encoded.as_bytes()).map_err(|_| SecretsError::Crypto)?);
        let key = Zeroizing::new(bytes.as_slice().try_into().map_err(|_| SecretsError::Crypto)?);
        vault.validate(&key, owner)?;
        self.unlocked.lock().await.insert(
            owner.into(),
            Unlocked {
                key,
                last_used: Instant::now(),
                vault_id: vault.id,
            },
        );
        self.status(owner).await
    }
    pub async fn lock(&self, owner: &str) {
        let _write = self.writer.lock().await;
        self.unlocked.lock().await.remove(owner);
    }
    pub async fn lock_all(&self) {
        let _write = self.writer.lock().await;
        self.unlocked.lock().await.clear();
    }
    pub async fn change_passphrase(
        &self,
        owner: &str,
        current: Zeroizing<String>,
        next: Zeroizing<String>,
    ) -> Result<(), SecretsError> {
        let _write = self.writer.lock().await;
        let (generation, mut vault) = self.load(owner).await?;
        let key = crypto::unwrap_async(vault.envelope.clone(), current, vault.identity()).await?;
        vault.validate(&key, owner)?;
        vault.envelope = crypto::wrap_async(key.clone(), next, vault.identity()).await?;
        self.save(owner, Some(generation), &vault, &key).await?;
        self.unlocked.lock().await.remove(owner);
        Ok(())
    }
    pub async fn backup(&self, owner: &str) -> Result<VaultBackup, SecretsError> {
        let (_, vault) = self.load(owner).await?;
        let key = self.key(owner, &vault, true).await?;
        vault.validate(&key, owner)?;
        Ok(VaultBackup {
            format: 1,
            encrypted_data: STANDARD.encode(serde_json::to_vec(&vault).map_err(|_| SecretsError::Crypto)?),
        })
    }
    pub async fn restore(
        &self,
        owner: &str,
        backup: VaultBackup,
        passphrase: Zeroizing<String>,
    ) -> Result<(), SecretsError> {
        let _write = self.writer.lock().await;
        if backup.format != 1 || backup.encrypted_data.len() > 64 * 1024 * 1024 {
            return Err(SecretsError::InvalidInput);
        }
        let bytes = STANDARD
            .decode(backup.encrypted_data)
            .map_err(|_| SecretsError::Crypto)?;
        if bytes.len() > self.max_snapshot_bytes {
            return Err(SecretsError::InvalidInput);
        }
        let mut vault: Vault = serde_json::from_slice(&bytes).map_err(|_| SecretsError::Crypto)?;
        let key = crypto::unwrap_async(vault.envelope.clone(), passphrase, vault.identity()).await?;
        vault.validate(&key, owner)?;
        vault.incarnation = Uuid::now_v7().to_string();
        let generation = self.repository.load(owner).await?.map(|v| v.generation);
        self.save(owner, generation, &vault, &key).await?;
        self.unlocked.lock().await.remove(owner);
        Ok(())
    }
    pub async fn read(&self, owner: &str, request: SecretReadRequest) -> Result<SecretReadResponse, SecretsError> {
        let _write = self.writer.lock().await;
        let (_, vault) = self.load(owner).await?;
        let key = self.key(owner, &vault, true).await?;
        let row = vault
            .records
            .get(&request.id)
            .filter(|r| r.scope == request.scope)
            .ok_or(SecretsError::NotFound)?;
        let mut record = vault.decrypt(row, &key)?;
        if let Some(fields) = request.fields {
            record.fields.retain(|k, _| fields.contains(k));
        }
        if let Some(binding) = request.binding {
            record.bindings = vec![
                record
                    .bindings
                    .get(binding)
                    .cloned()
                    .ok_or(SecretsError::InvalidInput)?,
            ];
        }
        let fields = std::mem::take(&mut record.fields);
        let human_action_required = record
            .bindings
            .iter()
            .any(|binding| !matches!(binding.mfa, MfaRequirement::None));
        Ok(SecretReadResponse {
            metadata: metadata(row, record, &vault.incarnation),
            fields,
            human_action_required,
        })
    }
    /// Human settings can recover retained project records without granting runtime access.
    pub async fn retained_project_ids(&self, owner: &str) -> Result<Vec<String>, SecretsError> {
        // One atomic repository snapshot; safe inside a write authorization callback.
        let (_, vault) = self.load(owner).await?;
        self.key(owner, &vault, false).await?;
        Ok(vault
            .records
            .values()
            .filter_map(|row| row.scope.project_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect())
    }
    pub async fn list(&self, owner: &str, request: SecretListRequest) -> Result<SecretListResponse, SecretsError> {
        let _write = self.writer.lock().await;
        if request.query.len() > 512 {
            return Err(SecretsError::InvalidInput);
        }
        let (generation, vault) = self.load(owner).await?;
        let key = self.key(owner, &vault, false).await?;
        let mut binding_request = request.clone();
        binding_request.cursor = None;
        let binding = fingerprint(&binding_request)?;
        let start = match request.cursor {
            Some(cursor) => {
                let bytes = STANDARD.decode(cursor).map_err(|_| SecretsError::InvalidInput)?;
                let data = crypto::open(&key, &vault.identity(), &bytes)?;
                let (g, id, cursor_binding): (i64, String, String) =
                    serde_json::from_slice(&data).map_err(|_| SecretsError::InvalidInput)?;
                if g != generation || cursor_binding != binding {
                    return Err(SecretsError::Conflict);
                }
                id
            }
            None => String::new(),
        };
        let query = request.query.to_lowercase();
        let mut records = vec![];
        for row in vault
            .records
            .values()
            .filter(|r| r.scope == request.scope && r.id > start)
        {
            let record = vault.decrypt(row, &key)?;
            if request.kind.as_ref().is_some_and(|kind| kind != &record.kind)
                || !record.title.to_lowercase().contains(&query)
            {
                continue;
            }
            if (request.service.is_some() || request.environment.is_some() || request.tenant.is_some())
                && !record.bindings.iter().any(|b| {
                    request.service.as_ref().is_none_or(|v| v == &b.service)
                        && request.environment.as_ref().is_none_or(|v| v == &b.environment)
                        && request.tenant.as_ref().is_none_or(|v| v == &b.tenant)
                })
            {
                continue;
            }
            records.push(metadata(row, record, &vault.incarnation));
            if records.len() == 51 {
                break;
            }
        }
        let cursor = if records.len() > 50 {
            records.pop();
            let id = records.last().ok_or(SecretsError::Crypto)?.id.clone();
            Some(STANDARD.encode(crypto::seal(
                &key,
                &vault.identity(),
                &serde_json::to_vec(&(generation, id, binding)).map_err(|_| SecretsError::Crypto)?,
            )?))
        } else {
            None
        };
        Ok(SecretListResponse { records, cursor })
    }
    pub async fn write(&self, owner: &str, request: SecretWriteRequest) -> Result<SecretMutationReceipt, SecretsError> {
        self.write_checked(owner, request, || async { Ok(()) }).await
    }
    /// Revalidates the live capability after lock acquisition and immediately before commit.
    pub async fn write_checked<F, Fut>(
        &self,
        owner: &str,
        mut request: SecretWriteRequest,
        authorize: F,
    ) -> Result<SecretMutationReceipt, SecretsError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), SecretsError>>,
    {
        request.operation_id = Uuid::parse_str(&request.operation_id)
            .map_err(|_| SecretsError::InvalidInput)?
            .to_string();
        let _write = self.writer.lock().await;
        authorize().await?;
        let (generation, mut vault) = self.load(owner).await?;
        let key = self.key(owner, &vault, true).await?;
        if (request.id.is_some() || request.expected_incarnation.is_some())
            && request.expected_incarnation.as_deref() != Some(vault.incarnation.as_str())
        {
            return Err(SecretsError::Conflict);
        }
        let fingerprint = fingerprint(&request)?;
        let mut receipts = vault.read_receipts(&key)?;
        if let Some(receipt) = retry(&request.operation_id, &fingerprint, &receipts)? {
            return Ok(receipt);
        }
        let (mut row, record) = match request.id {
            None => {
                if vault.records.len() >= 10000 || request.expected_revision.is_some() || request.patch.is_some() {
                    return Err(SecretsError::InvalidInput);
                }
                (
                    StoredRecord {
                        id: Uuid::now_v7().to_string(),
                        scope: request.scope,
                        revision: 1,
                        ciphertext: vec![],
                    },
                    request.record.ok_or(SecretsError::InvalidInput)?,
                )
            }
            Some(id) => {
                if request.record.is_some() {
                    return Err(SecretsError::InvalidInput);
                }
                let mut row = vault
                    .records
                    .get(&id)
                    .filter(|r| r.scope == request.scope)
                    .cloned()
                    .ok_or(SecretsError::NotFound)?;
                if request.expected_revision != Some(row.revision) {
                    return Err(SecretsError::Conflict);
                }
                let mut record = vault.decrypt(&row, &key)?;
                let patch = request.patch.ok_or(SecretsError::InvalidInput)?;
                if let Some(title) = patch.title {
                    record.title = title;
                }
                if let Some(kind) = patch.kind {
                    record.kind = kind;
                }
                if let Some(bindings) = patch.bindings {
                    record.bindings = bindings;
                }
                for (name, value) in patch.fields {
                    match value {
                        Some(value) => {
                            record.fields.insert(name, value);
                        }
                        None => {
                            record.fields.remove(&name);
                        }
                    }
                }
                row.revision = row.revision.checked_add(1).ok_or(SecretsError::Conflict)?;
                (row, record)
            }
        };
        validate_record(&record)?;
        vault.encrypt(&mut row, &key, &record)?;
        let receipt = SecretMutationReceipt {
            id: row.id.clone(),
            revision: row.revision,
            deleted: false,
        };
        vault.records.insert(row.id.clone(), row);
        receipts.insert(
            request.operation_id,
            OperationReceipt {
                fingerprint,
                receipt: receipt.clone(),
            },
        );
        vault.write_receipts(&key, &receipts)?;
        authorize().await?;
        self.key(owner, &vault, false).await?;
        self.save(owner, Some(generation), &vault, &key).await?;
        Ok(receipt)
    }
    /// Receipt-only recovery for authenticated Settings after its last retained
    /// project record was deleted. This never authorizes or performs a mutation.
    pub(crate) async fn recover_settings_delete<F, Fut>(
        &self,
        owner: &str,
        mut request: SecretDeleteRequest,
        authorize: F,
    ) -> Result<Option<SecretMutationReceipt>, SecretsError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), SecretsError>>,
    {
        request.operation_id = Uuid::parse_str(&request.operation_id)
            .map_err(|_| SecretsError::InvalidInput)?
            .to_string();
        let _write = self.writer.lock().await;
        authorize().await?;
        let (_, vault) = self.load(owner).await?;
        let key = self.key(owner, &vault, true).await?;
        if request.expected_incarnation != vault.incarnation {
            return Err(SecretsError::Conflict);
        }
        let receipt = retry(
            &request.operation_id,
            &fingerprint(&request)?,
            &vault.read_receipts(&key)?,
        )?
        .filter(|receipt| receipt.deleted);
        authorize().await?;
        self.key(owner, &vault, false).await?;
        Ok(receipt)
    }
    pub async fn delete(
        &self,
        owner: &str,
        request: SecretDeleteRequest,
    ) -> Result<SecretMutationReceipt, SecretsError> {
        self.delete_checked(owner, request, || async { Ok(()) }).await
    }
    /// Revalidates the live capability after lock acquisition and immediately before commit.
    pub async fn delete_checked<F, Fut>(
        &self,
        owner: &str,
        mut request: SecretDeleteRequest,
        authorize: F,
    ) -> Result<SecretMutationReceipt, SecretsError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), SecretsError>>,
    {
        request.operation_id = Uuid::parse_str(&request.operation_id)
            .map_err(|_| SecretsError::InvalidInput)?
            .to_string();
        let _write = self.writer.lock().await;
        authorize().await?;
        let (generation, mut vault) = self.load(owner).await?;
        let key = self.key(owner, &vault, true).await?;
        if request.expected_incarnation != vault.incarnation {
            return Err(SecretsError::Conflict);
        }
        let fingerprint = fingerprint(&request)?;
        let mut receipts = vault.read_receipts(&key)?;
        if let Some(receipt) = retry(&request.operation_id, &fingerprint, &receipts)? {
            return Ok(receipt);
        }
        let row = vault
            .records
            .get(&request.id)
            .filter(|r| r.scope == request.scope)
            .ok_or(SecretsError::NotFound)?;
        if row.revision != request.expected_revision {
            return Err(SecretsError::Conflict);
        }
        let receipt = SecretMutationReceipt {
            id: row.id.clone(),
            revision: row.revision,
            deleted: true,
        };
        vault.records.remove(&request.id);
        receipts.insert(
            request.operation_id,
            OperationReceipt {
                fingerprint,
                receipt: receipt.clone(),
            },
        );
        vault.write_receipts(&key, &receipts)?;
        authorize().await?;
        self.key(owner, &vault, false).await?;
        self.save(owner, Some(generation), &vault, &key).await?;
        Ok(receipt)
    }
}

// Maximum deletion entry: canonical operation/record UUIDs (36 each), SHA256
// fingerprint (64), u64 revision (20), fixed JSON keys and punctuation. A test
// proves its base64 expansion (including a comma and rounding) is < 1024.
const DELETE_RECEIPT_RESERVE: usize = 1024;

fn retry(
    id: &str,
    fingerprint: &str,
    receipts: &BTreeMap<String, OperationReceipt>,
) -> Result<Option<SecretMutationReceipt>, SecretsError> {
    if !crate::vault::canonical_uuid(id) {
        return Err(SecretsError::InvalidInput);
    }
    match receipts.get(id) {
        Some(value) if value.fingerprint != fingerprint => Err(SecretsError::Conflict),
        Some(value) => Ok(Some(value.receipt.clone())),
        None => Ok(None),
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
