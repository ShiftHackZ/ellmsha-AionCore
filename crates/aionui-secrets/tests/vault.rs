use aionui_api_types::secrets::*;
use aionui_db::{ISecretsRepository, SqliteSecretsRepository};
use aionui_secrets::{
    crypto,
    service::{SecretsError, SecretsService},
};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;
use zeroize::Zeroizing;

fn phrase() -> Zeroizing<String> {
    Zeroizing::new("synthetic test passphrase only".into())
}
fn scope(project: Option<&str>) -> SecretScope {
    SecretScope {
        project_id: project.map(str::to_owned),
    }
}
fn write(project: Option<&str>) -> SecretWriteRequest {
    SecretWriteRequest {
        scope: scope(project),
        operation_id: Uuid::now_v7().to_string(),
        id: None,
        expected_revision: None,
        expected_incarnation: None,
        patch: None,
        record: Some(SecretRecordInput {
            title: "Synthetic portal".into(),
            kind: SecretKind::Login,
            fields: BTreeMap::from([(
                "password".into(),
                SecretField {
                    kind: FieldKind::Password,
                    value: "SYNTHETIC_SECRET_123".into(),
                },
            )]),
            bindings: vec![],
        }),
    }
}
fn read(id: &str, project: Option<&str>) -> SecretReadRequest {
    SecretReadRequest {
        scope: scope(project),
        id: id.into(),
        fields: None,
        binding: None,
    }
}

#[tokio::test]
async fn encrypted_persistence_scope_cas_idempotency_backup_restore() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.sqlite");
    let repository = Arc::new(SqliteSecretsRepository::open(&path).await.unwrap());
    let service = SecretsService::new(repository.clone());
    service.initialize("alice", phrase()).await.unwrap();
    let request = write(Some("project-a"));
    let receipt = service.write("alice", request.clone()).await.unwrap();
    assert_eq!(receipt, service.write("alice", request.clone()).await.unwrap());
    let mut conflicting = request;
    conflicting.record.as_mut().unwrap().title = "different".into();
    assert!(matches!(
        service.write("alice", conflicting).await,
        Err(SecretsError::Conflict)
    ));
    assert_eq!(
        service
            .read("alice", read(&receipt.id, Some("project-a")))
            .await
            .unwrap()
            .fields["password"]
            .value,
        "SYNTHETIC_SECRET_123"
    );
    assert!(matches!(
        service.read("alice", read(&receipt.id, None)).await,
        Err(SecretsError::NotFound)
    ));
    assert!(matches!(
        service.read("alice", read(&receipt.id, Some("project-b"))).await,
        Err(SecretsError::NotFound)
    ));
    service.initialize("bob", phrase()).await.unwrap();
    assert!(matches!(
        service.read("bob", read(&receipt.id, Some("project-a"))).await,
        Err(SecretsError::NotFound)
    ));
    let stored = repository.load("alice").await.unwrap().unwrap();
    let serialized = String::from_utf8(stored.ciphertext).unwrap();
    assert!(!serialized.contains("SYNTHETIC_SECRET_123"));
    assert!(!serialized.contains("Synthetic portal"));
    let backup = service.backup("alice").await.unwrap();
    assert!(!backup.encrypted_data.contains("SYNTHETIC_SECRET_123"));
    let stale = SecretDeleteRequest {
        scope: scope(Some("project-a")),
        id: receipt.id.clone(),
        expected_revision: 2,
        expected_incarnation: service.status("alice").await.unwrap().incarnation.unwrap(),
        operation_id: Uuid::now_v7().to_string(),
    };
    assert!(matches!(
        service.delete("alice", stale).await,
        Err(SecretsError::Conflict)
    ));
    service.lock("alice").await;
    assert!(matches!(
        service.read("alice", read(&receipt.id, Some("project-a"))).await,
        Err(SecretsError::Locked)
    ));
    assert!(matches!(
        service
            .unlock("alice", Zeroizing::new("incorrect synthetic password".into()))
            .await,
        Err(SecretsError::Crypto)
    ));
    let restarted = SecretsService::new(repository);
    restarted.unlock("alice", phrase()).await.unwrap();
    assert_eq!(
        restarted
            .read("alice", read(&receipt.id, Some("project-a")))
            .await
            .unwrap()
            .metadata
            .revision,
        1
    );
    assert!(matches!(
        restarted.restore("bob", backup, phrase()).await,
        Err(SecretsError::Crypto)
    ));
}

#[test]
fn ciphertext_rejects_wrong_owner_scope_revision_and_tamper() {
    let key = crypto::random_key().unwrap();
    let encrypted = crypto::seal(&key, b"owner/project/id/1", b"fixture").unwrap();
    assert_eq!(
        crypto::open(&key, b"owner/project/id/1", &encrypted)
            .unwrap()
            .as_slice(),
        b"fixture"
    );
    for aad in [
        b"other/project/id/1".as_slice(),
        b"owner/global/id/1",
        b"owner/project/id/2",
    ] {
        assert!(matches!(crypto::open(&key, aad, &encrypted), Err(SecretsError::Crypto)));
    }
    let mut tampered = encrypted.clone();
    tampered[15] ^= 1;
    assert!(matches!(
        crypto::open(&key, b"owner/project/id/1", &tampered),
        Err(SecretsError::Crypto)
    ));
    assert_ne!(
        encrypted,
        crypto::seal(&key, b"owner/project/id/1", b"fixture").unwrap()
    );
}

#[tokio::test]
async fn restore_is_atomic_patch_removes_fields_and_concurrent_cas_has_one_winner() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vault.sqlite");
    let repository = Arc::new(SqliteSecretsRepository::open(&path).await.unwrap());
    let service = Arc::new(SecretsService::new(repository.clone()));
    service.initialize("alice", phrase()).await.unwrap();
    let created = service.write("alice", write(None)).await.unwrap();
    let backup = service.backup("alice").await.unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&backup.encrypted_data)
        .unwrap();
    for marker in [
        "SYNTHETIC_SECRET_123",
        "Synthetic portal",
        "synthetic test passphrase only",
    ] {
        assert!(!decoded.windows(marker.len()).any(|w| w == marker.as_bytes()));
    }
    let request = SecretWriteRequest {
        scope: scope(None),
        id: Some(created.id.clone()),
        expected_revision: Some(1),
        expected_incarnation: service.status("alice").await.unwrap().incarnation,
        operation_id: Uuid::now_v7().to_string(),
        record: None,
        patch: Some(SecretPatch {
            title: Some("Changed".into()),
            kind: None,
            fields: BTreeMap::from([("password".into(), None)]),
            bindings: None,
        }),
    };
    let mut other = request.clone();
    other.operation_id = Uuid::now_v7().to_string();
    let (a, b) = tokio::join!(service.write("alice", request), service.write("alice", other));
    assert_ne!(a.is_ok(), b.is_ok());
    assert!(
        service
            .read("alice", read(&created.id, None))
            .await
            .unwrap()
            .fields
            .is_empty()
    );
    let corrupt = VaultBackup {
        format: 1,
        encrypted_data: "invalid".into(),
    };
    assert!(matches!(
        service.restore("alice", corrupt, phrase()).await,
        Err(SecretsError::Crypto)
    ));
    assert!(
        service
            .read("alice", read(&created.id, None))
            .await
            .unwrap()
            .fields
            .is_empty()
    );
    service.restore("alice", backup, phrase()).await.unwrap();
    assert!(!service.status("alice").await.unwrap().unlocked);
    service.unlock("alice", phrase()).await.unwrap();
    assert_eq!(
        service.read("alice", read(&created.id, None)).await.unwrap().fields["password"].value,
        "SYNTHETIC_SECRET_123"
    );
    // Inspect actual disk files, including any SQLite journals/WAL, not just object serialization.
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        for marker in [
            "SYNTHETIC_SECRET_123",
            "Synthetic portal",
            "synthetic test passphrase only",
        ] {
            assert!(!bytes.windows(marker.len()).any(|w| w == marker.as_bytes()));
        }
    }
}

#[tokio::test]
async fn device_key_is_invalidated_by_rotation_and_mfa_material_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let repository = Arc::new(
        SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = SecretsService::new(repository);
    service.initialize("alice", phrase()).await.unwrap();
    let key = service.desktop_key("alice").await.unwrap();
    service.lock("alice").await;
    service.unlock_desktop("alice", key).await.unwrap();
    let old = service.desktop_key("alice").await.unwrap();
    service
        .change_passphrase(
            "alice",
            phrase(),
            Zeroizing::new("different synthetic passphrase".into()),
        )
        .await
        .unwrap();
    assert!(matches!(
        service.unlock_desktop("alice", old).await,
        Err(SecretsError::Crypto)
    ));
    service
        .unlock("alice", Zeroizing::new("different synthetic passphrase".into()))
        .await
        .unwrap();
    let mut request = write(None);
    request.record.as_mut().unwrap().title = "otpauth://synthetic".into();
    assert!(matches!(
        service.write("alice", request).await,
        Err(SecretsError::InvalidInput)
    ));
}

#[tokio::test]
async fn selected_binding_controls_mfa_and_revocation_prevents_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repository = Arc::new(
        SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = SecretsService::new(repository);
    service.initialize("alice", phrase()).await.unwrap();
    let mut request = write(None);
    request.record.as_mut().unwrap().bindings =
        [MfaRequirement::None, MfaRequirement::Required, MfaRequirement::Unknown]
            .into_iter()
            .map(|mfa| SecretBinding {
                service: "portal".into(),
                environment: "test".into(),
                tenant: "synthetic".into(),
                role: "QA".into(),
                target: "https://example.test".into(),
                mfa,
                mfa_method: "human".into(),
                mfa_instructions: "Complete the sign-in prompt".into(),
            })
            .collect();
    let created = service.write("alice", request).await.unwrap();
    for (binding, expected) in [(Some(0), false), (Some(1), true), (Some(2), true), (None, true)] {
        let mut request = read(&created.id, None);
        request.binding = binding;
        let result = service.read("alice", request).await.unwrap();
        assert_eq!(result.human_action_required, expected);
        assert_eq!(result.fields["password"].value, "SYNTHETIC_SECRET_123");
    }
    let count = std::sync::atomic::AtomicUsize::new(0);
    let result = service
        .write_checked("alice", write(None), || async {
            if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err(SecretsError::Forbidden)
            }
        })
        .await;
    assert!(matches!(result, Err(SecretsError::Forbidden)));
    let records = service
        .list(
            "alice",
            SecretListRequest {
                scope: scope(None),
                query: String::new(),
                kind: None,
                service: None,
                environment: None,
                tenant: None,
                cursor: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(records.records.len(), 1);
}

#[tokio::test]
async fn inventory_rejects_deleted_substituted_records_and_envelope_receipt_tampering() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = SecretsService::new(repo.clone());
    service.initialize("alice", phrase()).await.unwrap();
    service.write("alice", write(None)).await.unwrap();
    service.write("alice", write(None)).await.unwrap();
    let backup = service.backup("alice").await.unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&backup.encrypted_data)
        .unwrap();
    let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<String> = original["records"].as_object().unwrap().keys().cloned().collect();
    for mode in 0..4 {
        let mut altered = original.clone();
        match mode {
            0 => {
                altered["records"].as_object_mut().unwrap().remove(&ids[0]);
            }
            1 => {
                altered["records"][&ids[0]] = altered["records"][&ids[1]].clone();
            }
            2 => {
                altered["envelope"]["salt"] =
                    serde_json::json!(base64::engine::general_purpose::STANDARD.encode([0u8; 16]));
            }
            _ => {
                altered["receipts"] = serde_json::json!([]);
            }
        }
        let mut invalid: VaultBackup = serde_json::from_value(serde_json::to_value(&backup).unwrap()).unwrap();
        invalid.encrypted_data =
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&altered).unwrap());
        let before = repo.load("alice").await.unwrap().unwrap();
        assert!(
            matches!(
                service.restore("alice", invalid, phrase()).await,
                Err(SecretsError::Crypto)
            ),
            "mode {mode}"
        );
        assert_eq!(repo.load("alice").await.unwrap().unwrap().ciphertext, before.ciphertext);
    }
    let mut altered = original;
    altered["records"].as_object_mut().unwrap().remove(&ids[0]);
    let before = repo.load("alice").await.unwrap().unwrap();
    repo.replace("alice", Some(before.generation), &serde_json::to_vec(&altered).unwrap())
        .await
        .unwrap();
    assert!(matches!(service.status("alice").await, Err(SecretsError::Crypto)));
}

#[test]
fn passphrase_policy_counts_unicode_scalars_not_utf8_bytes() {
    assert!(matches!(
        crypto::wrap(&[1; 32], &"密".repeat(4), b"fixture"),
        Err(SecretsError::InvalidInput)
    ));
    let phrase = "密".repeat(12);
    let envelope = crypto::wrap(&[1; 32], &phrase, b"fixture").unwrap();
    assert_eq!(*crypto::unwrap(&envelope, &phrase, b"fixture").unwrap(), [1; 32]);
}
