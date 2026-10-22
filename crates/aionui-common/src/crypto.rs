use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

const NONCE_SIZE: usize = 12;
const KEY_SIZE: usize = 32;

/// Crypto helper error independent of HTTP/API boundaries.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("AES-256 key must be exactly {expected} bytes, got {actual}")]
    InvalidKeySize { expected: usize, actual: usize },

    #[error("Failed to create cipher: {0}")]
    CipherInit(String),

    #[error("RNG failure: {0}")]
    Random(String),

    #[error("Encryption failed: {0}")]
    Encryption(String),

    #[error("Invalid base64: {0}")]
    InvalidBase64(String),

    #[error("Ciphertext too short")]
    CiphertextTooShort,

    #[error("Decryption failed: invalid key or corrupted data")]
    DecryptionFailed,

    #[error("Invalid UTF-8 in decrypted data: {0}")]
    InvalidUtf8(String),
}

impl CryptoError {
    /// Returns true for caller/data problems that API boundaries should map to 400.
    pub fn is_bad_request(&self) -> bool {
        matches!(
            self,
            Self::InvalidKeySize { .. } | Self::InvalidBase64(_) | Self::CiphertextTooShort | Self::DecryptionFailed
        )
    }
}

/// Encrypt a string value using AES-256-GCM.
///
/// The key must be exactly 32 bytes. Output is base64-encoded (nonce + ciphertext + tag).
pub fn encrypt_string(plaintext: &str, key: &[u8]) -> Result<String, CryptoError> {
    validate_key_size(key)?;

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| CryptoError::CipherInit(e.to_string()))?;

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    getrandom::getrandom(&mut nonce_bytes).map_err(|e| CryptoError::Random(e.to_string()))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| CryptoError::Encryption(e.to_string()))?;

    let mut combined = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(combined))
}

/// Decrypt an AES-256-GCM encrypted string.
///
/// The key must be exactly 32 bytes. Input is base64-encoded (nonce + ciphertext + tag).
pub fn decrypt_string(ciphertext: &str, key: &[u8]) -> Result<String, CryptoError> {
    validate_key_size(key)?;

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| CryptoError::CipherInit(e.to_string()))?;

    let combined = BASE64
        .decode(ciphertext)
        .map_err(|e| CryptoError::InvalidBase64(e.to_string()))?;

    if combined.len() < NONCE_SIZE {
        return Err(CryptoError::CiphertextTooShort);
    }

    let (nonce_bytes, encrypted) = combined.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, encrypted)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    String::from_utf8(plaintext).map_err(|e| CryptoError::InvalidUtf8(e.to_string()))
}

fn validate_key_size(key: &[u8]) -> Result<(), CryptoError> {
    if key.len() != KEY_SIZE {
        return Err(CryptoError::InvalidKeySize {
            expected: KEY_SIZE,
            actual: key.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42; 32]
    }

    #[test]
    fn test_roundtrip() {
        let key = test_key();
        let encrypted = encrypt_string("hello", &key).unwrap();
        let decrypted = decrypt_string(&encrypted, &key).unwrap();
        assert_eq!(decrypted, "hello");
    }

    #[test]
    fn test_empty_string() {
        let key = test_key();
        let encrypted = encrypt_string("", &key).unwrap();
        let decrypted = decrypt_string(&encrypted, &key).unwrap();
        assert_eq!(decrypted, "");
    }

    #[test]
    fn test_unicode() {
        let key = test_key();
        let encrypted = encrypt_string("你好世界", &key).unwrap();
        let decrypted = decrypt_string(&encrypted, &key).unwrap();
        assert_eq!(decrypted, "你好世界");
    }

    #[test]
    fn test_wrong_key_fails() {
        let key = test_key();
        let encrypted = encrypt_string("hello", &key).unwrap();
        let wrong_key = [0x99; 32];
        assert!(matches!(
            decrypt_string(&encrypted, &wrong_key),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn test_nonce_randomness() {
        let key = test_key();
        let enc1 = encrypt_string("hello", &key).unwrap();
        let enc2 = encrypt_string("hello", &key).unwrap();
        assert_ne!(enc1, enc2);
    }

    #[test]
    fn test_invalid_key_size() {
        let short_key = [0u8; 16];
        assert!(matches!(
            encrypt_string("hello", &short_key),
            Err(CryptoError::InvalidKeySize {
                expected: KEY_SIZE,
                actual: 16
            })
        ));
        assert!(matches!(
            decrypt_string("dGVzdA==", &short_key),
            Err(CryptoError::InvalidKeySize {
                expected: KEY_SIZE,
                actual: 16
            })
        ));
    }

    #[test]
    fn test_invalid_base64() {
        let key = test_key();
        assert!(matches!(
            decrypt_string("not-valid-base64!!!", &key),
            Err(CryptoError::InvalidBase64(_))
        ));
    }

    #[test]
    fn test_ciphertext_too_short() {
        let key = test_key();
        // Base64 of less than 12 bytes
        let short = BASE64.encode([0u8; 5]);
        assert!(matches!(
            decrypt_string(&short, &key),
            Err(CryptoError::CiphertextTooShort)
        ));
    }
}

/// Sign a canonical browser ownership snapshot. The signing key is backend-only;
/// subprocesses receive only this scoped capability, never its issuer key.
pub fn browser_scope_token(key: &str, user_id: &str, conversation_id: &str, project_id: Option<&str>) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let payload = serde_json::json!({"user_id": user_id, "conversation_id": conversation_id, "project_id": project_id})
        .to_string();
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(encoded.as_bytes());
    format!(
        "{encoded}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

#[cfg(test)]
mod browser_scope_tests {
    use super::browser_scope_token;
    #[test]
    fn capability_binds_owner_conversation_and_project() {
        let token = browser_scope_token("issuer-key", "owner-a", "chat", Some("project-a"));
        assert_eq!(
            token,
            browser_scope_token("issuer-key", "owner-a", "chat", Some("project-a"))
        );
        for changed in [
            browser_scope_token("other-key", "owner-a", "chat", Some("project-a")),
            browser_scope_token("issuer-key", "owner-b", "chat", Some("project-a")),
            browser_scope_token("issuer-key", "owner-a", "child", Some("project-a")),
            browser_scope_token("issuer-key", "owner-a", "chat", None),
        ] {
            assert_ne!(token, changed);
        }
    }
}

static BROWSER_SCOPE_SIGNING_KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// Install once at process startup, before agent runtimes exist.
pub fn initialize_browser_scope_signing_key(key: String) {
    if key.len() >= 32 {
        let _ = BROWSER_SCOPE_SIGNING_KEY.set(key);
    }
}
/// Issuer key remains in Core memory only, never inherited by native tools/MCP.
pub fn browser_scope_signing_key() -> Option<&'static str> {
    BROWSER_SCOPE_SIGNING_KEY.get().map(String::as_str)
}

/// Domain-separated main-process proof for the browser identity endpoint.
pub fn browser_scope_request_proof(key: &str, conversation_id: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"ellmsha-browser-context-v1:");
    mac.update(conversation_id.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}
/// Verify the exact request without exposing timing-dependent signature prefixes.
pub fn verify_browser_scope_request_proof(key: &str, conversation_id: &str, proof: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Ok(proof) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(proof) else {
        return false;
    };
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"ellmsha-browser-context-v1:");
    mac.update(conversation_id.as_bytes());
    mac.verify_slice(&proof).is_ok()
}
