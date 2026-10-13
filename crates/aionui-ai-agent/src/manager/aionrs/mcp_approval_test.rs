use super::*;
use aion_config::config::TransportType;
use aion_protocol::{ToolApprovalManager, ToolApprovalResult};
use aion_protocol::{commands::ApprovalScope, events::ToolCategory};
use std::sync::Arc;

fn config(token: &str) -> HashMap<String, McpServerConfig> {
    HashMap::from([(
        "linear-fixture".into(),
        McpServerConfig {
            transport: TransportType::StreamableHttp,
            command: None,
            args: None,
            env: Some(HashMap::from([("AIONUI_RUNTIME_TOKEN".into(), token.into())])),
            url: Some(format!("http://127.0.0.1:1234/{token}")),
            headers: None,
            deferred: Some(true),
            startup_timeout_ms: None,
        },
    )])
}
fn identity(tool: &str) -> McpApprovalIdentity {
    McpApprovalIdentity {
        server: "linear-fixture".into(),
        tool: tool.into(),
    }
}

#[tokio::test]
async fn native_exact_grant_survives_fresh_manager_and_rotated_runtime_then_revokes() {
    let dir = tempfile::tempdir().unwrap();
    let store = McpApprovalStore::new(dir.path(), "user");
    let trusted = HashMap::from([("linear-fixture".into(), "account-a".into())]);
    let first = ToolApprovalManager::new().with_persistence(Arc::new(NativeMcpApprovals::new(
        store.clone(),
        &config("chat-1"),
        &trusted,
    )));
    let pending = first.request_tool_approval("call", &ToolCategory::Mcp, Some(identity("get_issue")));
    first.approve_checked("call", ApprovalScope::Always).unwrap();
    assert!(matches!(pending.await.unwrap(), ToolApprovalResult::Approved));
    let resumed = ToolApprovalManager::new().with_persistence(Arc::new(NativeMcpApprovals::new(
        store.clone(),
        &config("chat-2-new-token"),
        &trusted,
    )));
    assert!(resumed.is_mcp_approved(&identity("get_issue")));
    assert!(!resumed.is_mcp_approved(&identity("delete_issue")));
    assert!(!resumed.is_auto_approved("mcp"));
    let other = ToolApprovalManager::new().with_persistence(Arc::new(NativeMcpApprovals::new(
        store.clone(),
        &config("chat-2"),
        &HashMap::from([("linear-fixture".into(), "account-b".into())]),
    )));
    assert!(!other.is_mcp_approved(&identity("get_issue")));
    store.revoke(&store.list().unwrap()[0].id).unwrap();
    assert!(!resumed.is_mcp_approved(&identity("get_issue")));
}

#[tokio::test]
async fn native_session_exact_scope_and_unmanaged_always_fail_closed() {
    let manager = ToolApprovalManager::new();
    let pending = manager.request_tool_approval("call", &ToolCategory::Mcp, Some(identity("get_issue")));
    assert!(manager.approve_checked("call", ApprovalScope::Always).is_err());
    manager.approve_checked("call", ApprovalScope::Session).unwrap();
    assert!(matches!(pending.await.unwrap(), ToolApprovalResult::Approved));
    assert!(manager.is_mcp_approved(&identity("get_issue")));
    assert!(!manager.is_mcp_approved(&identity("delete_issue")));
    assert!(!ToolApprovalManager::new().is_mcp_approved(&identity("get_issue")));
    manager.add_auto_approve("mcp");
    assert!(!manager.is_auto_approved("mcp"));
}
