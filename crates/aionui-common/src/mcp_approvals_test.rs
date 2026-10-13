use super::*;

#[test]
fn exact_grants_survive_reopen_and_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let first = McpApprovalStore::new(dir.path(), "user-a");
    first.grant("linear-account-a", "binding-a", "get_issue").unwrap();
    let reopened = McpApprovalStore::new(dir.path(), "user-a");
    assert!(reopened.allows("linear-account-a", "binding-a", "get_issue").unwrap());
    for (server, binding, tool) in [
        ("linear-account-b", "binding-a", "get_issue"),
        ("linear-account-a", "binding-b", "get_issue"),
        ("linear-account-a", "binding-a", "delete_issue"),
    ] {
        assert!(!reopened.allows(server, binding, tool).unwrap());
    }
    assert!(
        !McpApprovalStore::new(dir.path(), "user-b")
            .allows("linear-account-a", "binding-a", "get_issue")
            .unwrap()
    );
    let grant = reopened.list().unwrap().remove(0);
    assert!(reopened.revoke(&grant.id).unwrap());
    assert!(!first.allows("linear-account-a", "binding-a", "get_issue").unwrap());
}

#[test]
fn corrupted_store_fails_closed_and_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    store.grant("server", "binding", "tool").unwrap();
    fs::write(&store.path, "not-json").unwrap();
    assert!(store.allows("server", "binding", "tool").is_err());
    assert!(store.grant("server", "binding", "other").is_err());
    assert_eq!(fs::read_to_string(store.path).unwrap(), "not-json");
}

#[test]
fn fingerprints_are_order_independent_and_never_store_transport_secrets() {
    let left: serde_json::Value = serde_json::from_str(r#"{"a":{"c":2,"b":1},"token":"fixture-secret"}"#).unwrap();
    let right: serde_json::Value = serde_json::from_str(r#"{"token":"fixture-secret","a":{"b":1,"c":2}}"#).unwrap();
    assert_eq!(mcp_binding(&left), mcp_binding(&right));
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    store.grant("fixture", &mcp_binding(&left), "tool").unwrap();
    assert!(!fs::read_to_string(store.path).unwrap().contains("fixture-secret"));
}
