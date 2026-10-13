use super::*;
use serde_json::json;

#[test]
fn serialized_or_client_snapshot_cannot_claim_account_provenance() {
    let mut server: SessionMcpServer = serde_json::from_value(json!({"id":"plugin:linear:account","name":"linear-fixture","transport":{"type":"streamable_http","url":"http://127.0.0.1"},"approval_identity":"linear:account","auto_approve_in_unattended":true})).unwrap();
    assert!(server.approval_identity.is_none());
    assert!(trusted_bindings(std::slice::from_ref(&server)).is_empty());
    server.approval_identity = Some("linear:account".into());
    assert_eq!(trusted_bindings(std::slice::from_ref(&server)).len(), 1);
    let roundtrip: SessionMcpServer = serde_json::from_value(serde_json::to_value(server).unwrap()).unwrap();
    assert!(roundtrip.approval_identity.is_none());
}
