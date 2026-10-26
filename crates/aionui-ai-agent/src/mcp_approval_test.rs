use super::*;
use serde_json::json;

#[test]
fn serialized_or_client_snapshot_cannot_claim_account_provenance() {
    let mut server: SessionMcpServer = serde_json::from_value(json!({"id":"plugin:linear:account","name":"linear-fixture","transport":{"type":"streamable_http","url":"http://127.0.0.1"},"approval_identity":"linear:account","auto_approve_in_unattended":true})).unwrap();
    assert!(server.approval_identity.is_none());
    let untrusted = trusted_bindings(std::slice::from_ref(&server));
    server.approval_identity = Some("linear:account".into());
    assert_ne!(trusted_bindings(std::slice::from_ref(&server)), untrusted);
    let roundtrip: SessionMcpServer = serde_json::from_value(serde_json::to_value(server).unwrap()).unwrap();
    assert!(roundtrip.approval_identity.is_none());
}

#[test]
fn original_transport_binding_is_adapter_neutral_and_invalidates_changed_credentials() {
    let mut server: SessionMcpServer = serde_json::from_value(json!({
        "id":"configured-account", "name":"fixture",
        "transport":{"type":"streamable_http","url":"https://example.invalid/mcp","headers":{"Authorization":"account-a"}}
    })).unwrap();
    let first = trusted_bindings(std::slice::from_ref(&server));
    let restored: SessionMcpServer = serde_json::from_value(serde_json::to_value(&server).unwrap()).unwrap();
    assert_eq!(trusted_bindings(&[restored]), first);
    if let aionui_api_types::SessionMcpTransport::StreamableHttp { headers, .. } = &mut server.transport {
        headers.insert("Authorization".into(), "account-b".into());
    }
    assert_ne!(trusted_bindings(&[server]), first);
}

#[test]
fn owned_browser_grant_survives_new_chat_and_restart_but_never_matches_a_spoof() {
    use aionui_common::mcp_approvals::McpApprovalStore;

    // This key is not used by other tests in this crate. Restore it after the test.
    const KEY: &str = "AIONUI_BUILTIN_BROWSER_MCP_PATH";
    let old = std::env::var_os(KEY);
    unsafe { std::env::set_var(KEY, "/fixture/builtin-mcp-browser.js") };
    let server = |token: &str| -> SessionMcpServer {
        serde_json::from_value(json!({"id":"browser", "name":"aionui-browser", "transport":{
            "type":"stdio", "command":"node", "args":["/fixture/builtin-mcp-browser.js"],
            "env":{"AIONUI_BROWSER_SCOPE_TOKEN":token}
        }}))
        .unwrap()
    };
    let first = trusted_bindings(&[server("chat-one")]);
    let second = trusted_bindings(&[server("chat-two")]);
    let mut registered = server("chat-two");
    registered.approval_identity = Some(registration_identity(
        &registered.id,
        &registered.name,
        "stdio",
        &json!({"command":"node", "args":["/fixture/builtin-mcp-browser.js"]}),
    ));
    // Registry resolution may replace node with a versioned bundled runtime.
    if let aionui_api_types::SessionMcpTransport::Stdio { command, .. } = &mut registered.transport {
        *command = "/new-install/runtime/node-vNext/bin/node".into();
    }
    let registered = trusted_bindings(&[registered]);
    let mut spoof = server("chat-two");
    if let aionui_api_types::SessionMcpTransport::Stdio { env, .. } = &mut spoof.transport {
        env.insert("NODE_OPTIONS".into(), "--require evil.js".into());
    }
    let spoof = trusted_bindings(&[spoof]);
    unsafe {
        match old {
            Some(value) => std::env::set_var(KEY, value),
            None => std::env::remove_var(KEY),
        }
    }
    assert_eq!(first, second);
    assert_eq!(first, registered);
    assert_ne!(first, spoof);
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "owner");
    store
        .grant("aionui-browser", &first["aionui-browser"], "click")
        .unwrap();
    let restored = McpApprovalStore::new(dir.path(), "owner");
    assert!(
        restored
            .allows("aionui-browser", &second["aionui-browser"], "click")
            .unwrap()
    );
    assert!(
        !restored
            .allows("aionui-browser", &second["aionui-browser"], "fill")
            .unwrap()
    );
    assert!(
        !restored
            .allows("aionui-browser", &spoof["aionui-browser"], "click")
            .unwrap()
    );
    store.revoke(&store.list().unwrap()[0].id).unwrap();
    assert!(
        !restored
            .allows("aionui-browser", &second["aionui-browser"], "click")
            .unwrap()
    );
}
