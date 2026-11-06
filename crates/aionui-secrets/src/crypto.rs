//! Versioned authenticated encryption; provider/JWT encryption is deliberately independent.
use crate::service::SecretsError;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub type VaultKey = Zeroizing<[u8; 32]>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyEnvelope {
    pub format: u32,
    #[serde(with = "bytes_base64")]
    pub salt: Vec<u8>,
    #[serde(with = "bytes_base64")]
    pub wrapped_key: Vec<u8>,
}

pub fn random_key() -> Result<VaultKey, SecretsError> {
    let mut key = Zeroizing::new([0; 32]);
    getrandom::getrandom(key.as_mut()).map_err(|_| SecretsError::Crypto)?;
    Ok(key)
}

fn wrapping_key(passphrase: &str, salt: &[u8]) -> Result<VaultKey, SecretsError> {
    if !(12..=1024).contains(&passphrase.chars().count()) || salt.len() != 16 {
        return Err(SecretsError::InvalidInput);
    }
    let params = Params::new(65536, 3, 1, Some(32)).map_err(|_| SecretsError::Crypto)?;
    let mut key = Zeroizing::new([0; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|_| SecretsError::Crypto)?;
    Ok(key)
}

pub fn seal(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, SecretsError> {
    let mut nonce = [0; 12];
    getrandom::getrandom(&mut nonce).map_err(|_| SecretsError::Crypto)?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SecretsError::Crypto)?;
    let encrypted = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad })
        .map_err(|_| SecretsError::Crypto)?;
    Ok([nonce.as_slice(), encrypted.as_slice()].concat())
}

pub fn open(key: &[u8; 32], aad: &[u8], ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
    if ciphertext.len() < 28 {
        return Err(SecretsError::Crypto);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| SecretsError::Crypto)?;
    cipher
        .decrypt(
            Nonce::from_slice(&ciphertext[..12]),
            Payload {
                msg: &ciphertext[12..],
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| SecretsError::Crypto)
}

pub fn wrap(key: &[u8; 32], passphrase: &str, identity: &[u8]) -> Result<KeyEnvelope, SecretsError> {
    let mut salt = vec![0; 16];
    getrandom::getrandom(&mut salt).map_err(|_| SecretsError::Crypto)?;
    let wrapping = wrapping_key(passphrase, &salt)?;
    Ok(KeyEnvelope {
        format: 1,
        wrapped_key: seal(&wrapping, identity, key)?,
        salt,
    })
}

pub fn unwrap(envelope: &KeyEnvelope, passphrase: &str, identity: &[u8]) -> Result<VaultKey, SecretsError> {
    if envelope.format != 1 {
        return Err(SecretsError::Crypto);
    }
    let wrapping = wrapping_key(passphrase, &envelope.salt)?;
    let plaintext = open(&wrapping, identity, &envelope.wrapped_key)?;
    let key: [u8; 32] = plaintext.as_slice().try_into().map_err(|_| SecretsError::Crypto)?;
    Ok(Zeroizing::new(key))
}

/// Argon2 is deliberately memory-hard; never block Tokio's transport workers.
pub async fn wrap_async(
    key: VaultKey,
    passphrase: Zeroizing<String>,
    identity: Vec<u8>,
) -> Result<KeyEnvelope, SecretsError> {
    tokio::task::spawn_blocking(move || wrap(&key, &passphrase, &identity))
        .await
        .map_err(|_| SecretsError::Crypto)?
}
pub async fn unwrap_async(
    envelope: KeyEnvelope,
    passphrase: Zeroizing<String>,
    identity: Vec<u8>,
) -> Result<VaultKey, SecretsError> {
    tokio::task::spawn_blocking(move || unwrap(&envelope, &passphrase, &identity))
        .await
        .map_err(|_| SecretsError::Crypto)?
}

/// Base64 has a deterministic encoded length independent of random ciphertext bytes.
pub(crate) mod bytes_base64 {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
