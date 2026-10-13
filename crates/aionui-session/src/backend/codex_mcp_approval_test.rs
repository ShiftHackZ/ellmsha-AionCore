use super::*;

fn servers() -> Vec<McpServerSpec> {
    vec![McpServerSpec {
        name: "fixture".into(),
        transport: McpTransport::Http {
            url: "http://127.0.0.1:1234/mcp".into(),
            headers: vec![],
        },
        auto_approve_in_unattended: false,
    }]
}
fn started(id: &str, tool: &str) -> Value {
    json!({"method":"item/started","params":{"threadId":"root","turnId":"turn","item":{"type":"mcpToolCall","id":id,"server":"fixture","tool":tool,"arguments":{}}}})
}
// Exact shape captured from installed Codex 0.162.1 + a localhost fake Responses/MCP fixture.
fn request() -> Value {
    json!({"method":"mcpServer/elicitation/request","id":0,"params":{"threadId":"root","turnId":"turn","serverName":"fixture","mode":"form","_meta":{"codex_approval_kind":"mcp_tool_call","tool_params":{}},"requestedSchema":{"type":"object","properties":{}}}})
}

#[test]
fn captured_wire_grant_survives_fresh_backend_and_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    let mut backend = CodexMcpApprovals::new(Some(store.clone()), &servers(), &HashMap::new());
    backend.observe(&mut started("call", "probe"));
    let mut prompt = request();
    assert!(!backend.observe(&mut prompt));
    assert_eq!(prompt["params"]["_meta"]["ellmsha_exact_tool"]["tool"], "probe");
    assert!(backend.approve("0", Some("ellmsha_always")).unwrap());
    let mut fresh = CodexMcpApprovals::new(Some(store.clone()), &servers(), &HashMap::new());
    fresh.observe(&mut started("fresh", "probe"));
    assert!(fresh.observe(&mut request()));
    let grant = store.list().unwrap().remove(0);
    store.revoke(&grant.id).unwrap();
    assert!(!fresh.observe(&mut request()));
}

#[test]
fn ambiguous_unmanaged_changed_account_and_other_tool_never_reuse_grant() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    let mut backend = CodexMcpApprovals::new(Some(store.clone()), &servers(), &HashMap::new());
    backend.observe(&mut started("call", "probe"));
    backend.observe(&mut request());
    backend.approve("0", Some("ellmsha_always")).unwrap();
    backend.observe(&mut started("other", "delete"));
    assert!(!backend.observe(&mut request())); // two calls with identical args; cannot correlate safely
    let mut other = CodexMcpApprovals::new(Some(store.clone()), &servers(), &HashMap::new());
    other.observe(&mut started("call", "delete"));
    assert!(!other.observe(&mut request()));
    let mut changed = servers();
    changed[0].transport = McpTransport::Http {
        url: "http://127.0.0.1:5678/other-account".into(),
        headers: vec![],
    };
    let mut changed = CodexMcpApprovals::new(Some(store.clone()), &changed, &HashMap::new());
    changed.observe(&mut started("call", "probe"));
    assert!(!changed.observe(&mut request()));
    let mut unmanaged = CodexMcpApprovals::new(Some(store), &[], &HashMap::new());
    unmanaged.observe(&mut started("call", "probe"));
    let mut spoof = request();
    spoof["params"]["_meta"]["ellmsha_exact_tool"] = json!({"server":"fixture","tool":"probe"});
    assert!(!unmanaged.observe(&mut spoof));
    assert!(spoof["params"]["_meta"].get("ellmsha_exact_tool").is_none());
    assert!(unmanaged.approve("0", Some("ellmsha_always")).is_err());
}

#[test]
fn session_grant_does_not_survive_new_backend_and_completed_calls_cannot_grant() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    let mut backend = CodexMcpApprovals::new(Some(store.clone()), &servers(), &HashMap::new());
    backend.observe(&mut started("call", "probe"));
    backend.observe(&mut request());
    backend.approve("0", Some("ellmsha_session")).unwrap();
    assert!(backend.observe(&mut request()));
    assert!(store.list().unwrap().is_empty());
    let mut fresh = CodexMcpApprovals::new(Some(store), &servers(), &HashMap::new());
    fresh.observe(&mut started("call", "probe"));
    assert!(!fresh.observe(&mut request()));
    fresh.observe(&mut json!({"method":"item/completed","params":{"item":{"id":"call"}}}));
    let mut prompt = request();
    fresh.observe(&mut prompt);
    assert!(prompt["params"]["_meta"].get("ellmsha_exact_tool").is_none());
}

#[test]
fn trusted_account_binding_survives_conversation_tokens_but_not_account_change() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    let trusted = HashMap::from([("fixture".into(), "host-account-a".into())]);
    let mut first = CodexMcpApprovals::new(Some(store.clone()), &servers(), &trusted);
    first.observe(&mut started("call", "probe"));
    first.observe(&mut request());
    first.approve("0", Some("ellmsha_always")).unwrap();
    let mut rotated = servers();
    rotated[0].transport = McpTransport::Stdio {
        command: "new-runtime-path".into(),
        args: vec!["new-port".into()],
        env: vec![("CONVERSATION_TOKEN".into(), "rotated-secret".into())],
    };
    let mut same = CodexMcpApprovals::new(Some(store.clone()), &rotated, &trusted);
    same.observe(&mut started("call", "probe"));
    assert!(same.observe(&mut request()));
    let other = HashMap::from([("fixture".into(), "host-account-b".into())]);
    let mut different = CodexMcpApprovals::new(Some(store), &rotated, &other);
    different.observe(&mut started("call", "probe"));
    assert!(!different.observe(&mut request()));
}

#[test]
fn malformed_notification_cannot_auto_approve_and_answers_clear_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut backend = CodexMcpApprovals::new(
        Some(McpApprovalStore::new(dir.path(), "user")),
        &servers(),
        &HashMap::new(),
    );
    backend.observe(&mut started("call", "probe"));
    backend.observe(&mut request());
    backend.approve("0", Some("ellmsha_session")).unwrap();
    let mut malformed = request();
    malformed.as_object_mut().unwrap().remove("id");
    assert!(!backend.observe(&mut malformed));
    backend.answered("0");
    assert!(backend.approve("0", Some("ellmsha_always")).is_err());
}

#[test]
fn app_catalog_uses_actual_linear_ids_and_preserves_other_apps() {
    let (ids, cursor) = linear_app_page(&json!({"data":[{"id":"actual-linear-id","name":"Linear"},{"id":"github","name":"GitHub"},{"id":"unrelated","name":"Linear notes"}],"nextCursor":"next"})).unwrap();
    assert_eq!(ids, vec!["actual-linear-id"]);
    assert_eq!(cursor.as_deref(), Some("next"));
    assert!(linear_app_page(&json!({"data":[{"name":"Linear"}]})).is_err());
}

#[test]
fn prepared_permission_does_not_access_store_again_after_rest_ack() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    let mut backend = CodexMcpApprovals::new(Some(store.clone()), &servers(), &HashMap::new());
    backend.observe(&mut started("call", "probe"));
    backend.observe(&mut request());
    backend.approve("0", Some("ellmsha_always")).unwrap();
    // Simulate disk failure after synchronous prepare but before async wire send.
    std::fs::remove_dir_all(dir.path().join("mcp-approvals")).unwrap();
    std::fs::write(dir.path().join("mcp-approvals"), "unavailable").unwrap();
    assert!(backend.approve("0", Some("ellmsha_always")).unwrap());
    backend.answered("0");
    assert!(backend.approve("0", Some("ellmsha_always")).is_err());
}
