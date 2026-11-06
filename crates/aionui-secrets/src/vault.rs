use crate::{
    crypto::{self, KeyEnvelope},
    service::SecretsError,
};
use aionui_api_types::secrets::{SecretMutationReceipt, SecretRecordInput, SecretScope};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRecord {
    pub id: String,
    pub scope: SecretScope,
    pub revision: u64,
    #[serde(with = "crypto::bytes_base64")]
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Vault {
    pub incarnation: String,
    pub format: u32,
    pub id: String,
    pub owner: String,
    pub envelope: KeyEnvelope,
    pub records: BTreeMap<String, StoredRecord>,
    /// The operation fingerprints and receipts are encrypted, too.
    #[serde(with = "crypto::bytes_base64")]
    pub receipts: Vec<u8>,
    /// AEAD-authenticated complete inventory, including the key envelope and receipts.
    #[serde(with = "crypto::bytes_base64")]
    pub inventory: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct OperationReceipt {
    pub fingerprint: String,
    pub receipt: SecretMutationReceipt,
}

impl Vault {
    fn inventory_digest(&self) -> Result<Vec<u8>, SecretsError> {
        let bytes = serde_json::to_vec(&(
            self.format,
            &self.incarnation,
            &self.id,
            &self.owner,
            &self.envelope,
            &self.records,
            &self.receipts,
        ))
        .map_err(|_| SecretsError::Crypto)?;
        Ok(Sha256::digest(bytes).to_vec())
    }
    pub fn authenticate_inventory(&mut self, key: &[u8; 32]) -> Result<(), SecretsError> {
        self.inventory = crypto::seal(
            key,
            &[self.identity(), b"inventory-v1".to_vec()].concat(),
            &self.inventory_digest()?,
        )?;
        Ok(())
    }
    pub fn verify_inventory(&self, key: &[u8; 32]) -> Result<(), SecretsError> {
        let digest = crypto::open(
            key,
            &[self.identity(), b"inventory-v1".to_vec()].concat(),
            &self.inventory,
        )?;
        if digest.as_slice() != self.inventory_digest()? {
            return Err(SecretsError::Crypto);
        }
        Ok(())
    }

    pub fn identity(&self) -> Vec<u8> {
        serde_json::to_vec(&(self.format, &self.id, &self.owner)).unwrap_or_default()
    }
    fn aad(&self, row: &StoredRecord) -> Result<Vec<u8>, SecretsError> {
        serde_json::to_vec(&(self.format, &self.id, &self.owner, &row.scope, &row.id, row.revision))
            .map_err(|_| SecretsError::Crypto)
    }
    pub fn decrypt(&self, row: &StoredRecord, key: &[u8; 32]) -> Result<SecretRecordInput, SecretsError> {
        let data = crypto::open(key, &self.aad(row)?, &row.ciphertext)?;
        serde_json::from_slice(&data).map_err(|_| SecretsError::Crypto)
    }
    pub fn encrypt(
        &self,
        row: &mut StoredRecord,
        key: &[u8; 32],
        record: &SecretRecordInput,
    ) -> Result<(), SecretsError> {
        let bytes = Zeroizing::new(serde_json::to_vec(record).map_err(|_| SecretsError::InvalidInput)?);
        row.ciphertext = crypto::seal(key, &self.aad(row)?, &bytes)?;
        Ok(())
    }
    pub fn read_receipts(&self, key: &[u8; 32]) -> Result<BTreeMap<String, OperationReceipt>, SecretsError> {
        let bytes = crypto::open(key, &[self.identity(), b"receipts".to_vec()].concat(), &self.receipts)?;
        serde_json::from_slice(&bytes).map_err(|_| SecretsError::Crypto)
    }
    pub fn write_receipts(
        &mut self,
        key: &[u8; 32],
        receipts: &BTreeMap<String, OperationReceipt>,
    ) -> Result<(), SecretsError> {
        let bytes = Zeroizing::new(serde_json::to_vec(receipts).map_err(|_| SecretsError::Crypto)?);
        self.receipts = crypto::seal(key, &[self.identity(), b"receipts".to_vec()].concat(), &bytes)?;
        Ok(())
    }
    pub fn validate(&self, key: &[u8; 32], owner: &str) -> Result<(), SecretsError> {
        if self.format != 1 || self.owner != owner || !canonical_uuid(&self.incarnation) || self.records.len() > 10000 {
            return Err(SecretsError::Crypto);
        }
        self.verify_inventory(key)?;
        let receipts = self.read_receipts(key)?;
        for (operation_id, value) in receipts {
            if !canonical_uuid(&operation_id)
                || !canonical_uuid(&value.receipt.id)
                || value.receipt.revision == 0
                || value.fingerprint.len() != 64
                || !value.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(SecretsError::Crypto);
            }
        }
        for (id, row) in &self.records {
            if id != &row.id || !canonical_uuid(id) || row.revision == 0 {
                return Err(SecretsError::Crypto);
            }
            crate::service::validate_record(&self.decrypt(row, key)?)?;
        }
        Ok(())
    }
}

pub(crate) fn canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}
