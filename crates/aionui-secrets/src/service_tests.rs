use super::*;
#[tokio::test]
async fn idle_keys_expire_without_a_later_api_call_and_metadata_poll_does_not_renew() {
    let dir = tempfile::tempdir().unwrap();
    let repository = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = SecretsService::new(repository);
    service
        .initialize("owner", Zeroizing::new("synthetic test passphrase".into()))
        .await
        .unwrap();
    let earlier = service.unlocked.lock().await["owner"].last_used;
    // Advance the runtime clock instead of subtracting from a young Windows clock.
    // Resume before SQLite awaits so its worker cannot auto-advance idle time.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::time::resume();
    service.status("owner").await.unwrap();
    service
        .list(
            "owner",
            SecretListRequest {
                scope: SecretScope { project_id: None },
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
    assert_eq!(service.unlocked.lock().await["owner"].last_used, earlier);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(781)).await;
    tokio::time::resume();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // Inspect key ownership directly: no status/key request is allowed to trigger cleanup.
    assert!(service.unlocked.lock().await.is_empty());
}

#[tokio::test]
async fn committed_vault_always_exports_and_restores_within_bound() {
    let dir = tempfile::tempdir().unwrap();
    let repository = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let mut service = SecretsService::new(repository.clone());
    service.max_snapshot_bytes = 8192;
    service
        .initialize("owner", Zeroizing::new("synthetic test passphrase".into()))
        .await
        .unwrap();
    let request = |size| SecretWriteRequest {
        scope: SecretScope { project_id: None },
        operation_id: Uuid::now_v7().to_string(),
        id: None,
        expected_revision: None,
        expected_incarnation: None,
        patch: None,
        record: Some(SecretRecordInput {
            title: "Synthetic size fixture".into(),
            kind: SecretKind::SecureNote,
            fields: BTreeMap::from([(
                "note".into(),
                SecretField {
                    kind: FieldKind::Note,
                    value: "x".repeat(size),
                },
            )]),
            bindings: vec![],
        }),
    };
    service.write("owner", request(16)).await.unwrap();
    let before = repository.load("owner").await.unwrap().unwrap();
    assert!(matches!(
        service.write("owner", request(8192)).await,
        Err(SecretsError::InvalidInput)
    ));
    let after = repository.load("owner").await.unwrap().unwrap();
    assert_eq!(before.ciphertext, after.ciphertext);
    assert_eq!(before.generation, after.generation);
    let backup = service.backup("owner").await.unwrap();
    assert!(serde_json::to_vec(&backup).unwrap().len() < 64 * 1024 * 1024 - 2048);
    service
        .restore("owner", backup, Zeroizing::new("synthetic test passphrase".into()))
        .await
        .unwrap();
    service
        .unlock("owner", Zeroizing::new("synthetic test passphrase".into()))
        .await
        .unwrap();
    let restored: Vault = serde_json::from_slice(&repository.load("owner").await.unwrap().unwrap().ciphertext).unwrap();
    let original: Vault = serde_json::from_slice(&before.ciphertext).unwrap();
    assert_eq!(
        serde_json::to_value(&original.records).unwrap(),
        serde_json::to_value(&restored.records).unwrap()
    );
    assert_eq!(original.receipts, restored.receipts);
}

#[tokio::test]
async fn receipt_quota_does_not_prevent_deleting_a_minimal_record() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let mut service = SecretsService::new(repo);
    service.max_snapshot_bytes = 8192;
    service
        .initialize("owner", Zeroizing::new("synthetic test passphrase".into()))
        .await
        .unwrap();
    let request:SecretWriteRequest=serde_json::from_value(serde_json::json!({"scope":{"project_id":null},"operation_id":Uuid::now_v7().to_string(),"record":{"title":"x","kind":"secure_note","fields":{},"bindings":[]}})).unwrap();
    let mut receipt = service.write("owner", request).await.unwrap();
    let mut exhausted = false;
    for _ in 0..100 {
        let request=serde_json::from_value(serde_json::json!({"scope":{"project_id":null},"operation_id":Uuid::now_v7().to_string(),"id":receipt.id,"expected_revision":receipt.revision,"expected_incarnation":service.status("owner").await.unwrap().incarnation,"patch":{"title":"x"}})).unwrap();
        match service.write("owner", request).await {
            Ok(next) => receipt = next,
            Err(SecretsError::InvalidInput) => {
                exhausted = true;
                break;
            }
            Err(other) => panic!("unexpected {other}"),
        }
    }
    assert!(exhausted);
    service
        .delete(
            "owner",
            SecretDeleteRequest {
                scope: SecretScope { project_id: None },
                id: receipt.id,
                expected_revision: receipt.revision,
                expected_incarnation: service.status("owner").await.unwrap().incarnation.unwrap(),
                operation_id: Uuid::now_v7().to_string(),
            },
        )
        .await
        .unwrap();
}

#[test]
fn maximum_deletion_receipt_fits_reserved_encoded_capacity() {
    let entry = BTreeMap::from([(
        "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned(),
        OperationReceipt {
            fingerprint: "f".repeat(64),
            receipt: SecretMutationReceipt {
                id: "ffffffff-ffff-ffff-ffff-ffffffffffff".into(),
                revision: u64::MAX,
                deleted: true,
            },
        },
    )]);
    // Replacing the empty braces with one entry plus the worst-case separator.
    let added = serde_json::to_vec(&entry).unwrap().len() - 2 + 1;
    assert!(4 * added.div_ceil(3) + 4 < DELETE_RECEIPT_RESERVE);
}

#[tokio::test]
async fn exact_quota_restore_and_max_revision_delete_have_deterministic_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let mut service = SecretsService::new(repo.clone());
    let phrase = || Zeroizing::new("synthetic test passphrase".into());
    service.initialize("owner", phrase()).await.unwrap();
    let create: SecretWriteRequest = serde_json::from_value(serde_json::json!({"scope":{"project_id":null},"operation_id":Uuid::now_v7().to_string(),"record":{"title":"x","kind":"secure_note","fields":{},"bindings":[]}})).unwrap();
    let created = service.write("owner", create).await.unwrap();
    let (generation, mut vault) = service.load("owner").await.unwrap();
    let key = service.key("owner", &vault, false).await.unwrap();
    let mut row = vault.records[&created.id].clone();
    let record = vault.decrypt(&row, &key).unwrap();
    row.revision = u64::MAX;
    vault.encrypt(&mut row, &key, &record).unwrap();
    vault.records.insert(row.id.clone(), row);
    vault.authenticate_inventory(&key).unwrap();
    service.max_snapshot_bytes = serde_json::to_vec(&vault).unwrap().len() + DELETE_RECEIPT_RESERVE;
    service.save("owner", Some(generation), &vault, &key).await.unwrap();
    // At exactly the reserve boundary, every fresh random inventory nonce has
    // identical encoded size. Repeated valid restores cannot randomly overflow.
    for _ in 0..8 {
        let backup = service.backup("owner").await.unwrap();
        service.restore("owner", backup, phrase()).await.unwrap();
        service.unlock("owner", phrase()).await.unwrap();
        assert_eq!(
            repo.load("owner").await.unwrap().unwrap().ciphertext.len() + DELETE_RECEIPT_RESERVE,
            service.max_snapshot_bytes
        );
    }
    let operation = "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned();
    let incarnation = service.status("owner").await.unwrap().incarnation.unwrap();
    let deletion = || SecretDeleteRequest {
        scope: SecretScope { project_id: None },
        id: created.id.clone(),
        expected_revision: u64::MAX,
        expected_incarnation: incarnation.clone(),
        operation_id: operation.clone(),
    };
    let receipt = service.delete("owner", deletion()).await.unwrap();
    assert!(receipt.deleted);
    assert_eq!(service.delete("owner", deletion()).await.unwrap().revision, u64::MAX);
    assert!(repo.load("owner").await.unwrap().unwrap().ciphertext.len() < service.max_snapshot_bytes);
}

#[tokio::test]
async fn authenticated_restore_rejects_unbounded_receipt_identity() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = SecretsService::new(repo.clone());
    service
        .initialize("owner", Zeroizing::new("synthetic test passphrase".into()))
        .await
        .unwrap();
    let (_, mut vault) = service.load("owner").await.unwrap();
    let key = service.key("owner", &vault, false).await.unwrap();
    let receipts = BTreeMap::from([(
        "x".repeat(1024),
        OperationReceipt {
            fingerprint: "f".repeat(64),
            receipt: SecretMutationReceipt {
                id: Uuid::now_v7().to_string(),
                revision: 1,
                deleted: true,
            },
        },
    )]);
    vault.write_receipts(&key, &receipts).unwrap();
    vault.authenticate_inventory(&key).unwrap();
    let backup = VaultBackup {
        format: 1,
        encrypted_data: STANDARD.encode(serde_json::to_vec(&vault).unwrap()),
    };
    let before = repo.load("owner").await.unwrap().unwrap();
    assert!(matches!(
        service
            .restore("owner", backup, Zeroizing::new("synthetic test passphrase".into()))
            .await,
        Err(SecretsError::Crypto)
    ));
    assert_eq!(repo.load("owner").await.unwrap().unwrap().ciphertext, before.ciphertext);
}

#[tokio::test]
async fn alternate_operation_uuid_spellings_retry_the_same_create() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = SecretsService::new(repo);
    service
        .initialize("owner", Zeroizing::new("synthetic test passphrase".into()))
        .await
        .unwrap();
    let operation = Uuid::now_v7();
    let request = |id: String| {
        serde_json::from_value(serde_json::json!({"scope":{"project_id":null},"operation_id":id,"record":{"title":"x","kind":"secure_note","fields":{},"bindings":[]}})).unwrap()
    };
    let initial = service.write("owner", request(operation.to_string())).await.unwrap();
    for alternate in [
        operation.to_string().to_uppercase(),
        operation.urn().to_string(),
        operation.simple().to_string(),
    ] {
        assert_eq!(service.write("owner", request(alternate)).await.unwrap().id, initial.id);
    }
}

#[tokio::test]
async fn restore_must_not_reuse_a_revision_approved_before_restore() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = SecretsService::new(repo);
    let phrase = || Zeroizing::new("synthetic test passphrase".into());
    service.initialize("owner", phrase()).await.unwrap();
    let request = serde_json::from_value(serde_json::json!({"scope":{"project_id":null},"operation_id":Uuid::now_v7().to_string(),"record":{"title":"r1","kind":"secure_note","fields":{},"bindings":[]}})).unwrap();
    let record = service.write("owner", request).await.unwrap();
    let backup = service.backup("owner").await.unwrap();
    let patch = |revision: u64, title: &str, incarnation: &str| {
        serde_json::from_value(serde_json::json!({"scope":{"project_id":null},"operation_id":Uuid::now_v7().to_string(),"id":record.id,"expected_revision":revision,"expected_incarnation":incarnation,"patch":{"title":title}})).unwrap()
    };
    let original = service.status("owner").await.unwrap().incarnation.unwrap();
    let before = service.write("owner", patch(1, "r2-B", &original)).await.unwrap();
    let stale = patch(before.revision, "stale-approved-write", &original);
    service.restore("owner", backup, phrase()).await.unwrap();
    service.unlock("owner", phrase()).await.unwrap();
    let restored = service.status("owner").await.unwrap().incarnation.unwrap();
    assert_ne!(original, restored);
    service.write("owner", patch(1, "r2-C", &restored)).await.unwrap();
    assert!(matches!(
        service.write("owner", stale).await,
        Err(SecretsError::Conflict)
    ));
    let delete = SecretDeleteRequest {
        scope: SecretScope { project_id: None },
        id: record.id,
        expected_revision: 2,
        expected_incarnation: original,
        operation_id: Uuid::now_v7().to_string(),
    };
    assert!(matches!(
        service.delete("owner", delete).await,
        Err(SecretsError::Conflict)
    ));
}

#[tokio::test]
async fn settings_receipt_recovery_rechecks_enablement_after_writer_wait() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        aionui_db::SqliteSecretsRepository::open(&dir.path().join("vault.sqlite"))
            .await
            .unwrap(),
    );
    let service = Arc::new(SecretsService::new(repo));
    service
        .initialize("owner", Zeroizing::new("synthetic fixture passphrase".into()))
        .await
        .unwrap();
    let create=serde_json::from_value(serde_json::json!({"scope":{"project_id":"removed"},"operation_id":Uuid::now_v7().to_string(),"record":{"title":"fixture","kind":"secure_note","fields":{},"bindings":[]}})).unwrap();
    let record = service.write("owner", create).await.unwrap();
    let request = SecretDeleteRequest {
        scope: SecretScope {
            project_id: Some("removed".into()),
        },
        id: record.id,
        expected_revision: record.revision,
        expected_incarnation: service.status("owner").await.unwrap().incarnation.unwrap(),
        operation_id: Uuid::now_v7().to_string(),
    };
    service.delete("owner", request.clone()).await.unwrap();
    let writer = service.writer.lock().await;
    let enabled = Arc::new(AtomicBool::new(true));
    let task_service = service.clone();
    let task_enabled = enabled.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let task_calls = calls.clone();
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        ready.send(()).unwrap();
        task_service
            .recover_settings_delete("owner", request, || async {
                task_calls.fetch_add(1, Ordering::SeqCst);
                if task_enabled.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(SecretsError::PluginDisabled)
                }
            })
            .await
    });
    waiting.await.unwrap();
    assert!(!pending.is_finished());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    enabled.store(false, Ordering::SeqCst);
    drop(writer);
    assert!(matches!(pending.await.unwrap(), Err(SecretsError::PluginDisabled)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
