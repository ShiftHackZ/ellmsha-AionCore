//! Keep eLLMsha's runtime MCPs available in older chats and delegated sessions.
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};

use crate::session_context::{AgentSessionContext, AgentSessionKind};

pub(super) async fn attach_runtime_memory(
    context: &mut AgentSessionContext,
    generation: Option<&crate::generation::GenerationService>,
) {
    attach_chat_history(context).await;

    let (Some(script), Some(config)) = (
        std::env::var_os("AIONUI_TDAI_MEMORY_MCP_PATH"),
        std::env::var_os("AIONUI_TDAI_MEMORY_CONFIG"),
    ) else {
        return;
    };
    let runtime = match aionui_runtime::ensure_runtime_command("node").await {
        Ok(runtime) => runtime,
        Err(_) => {
            tracing::warn!(
                conversation_id = context.conversation_id(),
                "TencentDB memory MCP node runtime unavailable"
            );
            return;
        }
    };
    let mut env = runtime
        .env
        .into_iter()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()))
        .collect::<std::collections::HashMap<_, _>>();
    env.insert(
        "AIONUI_TDAI_MEMORY_CONFIG".into(),
        config.to_string_lossy().into_owned(),
    );
    env.insert("AIONUI_TDAI_MEMORY_WORKSPACE".into(), context.workspace.path.clone());
    if let Some(generation) = generation {
        env.extend(generation.mcp_env(context.conversation_id()));
    }
    let mut args = runtime
        .args_prefix
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    args.push(script.to_string_lossy().into_owned());
    let server = SessionMcpServer {
        id: "ellmsha-tencentdb-memory".into(),
        name: "ellmsha-tencentdb-memory".into(),
        transport: SessionMcpTransport::Stdio {
            command: runtime.program.to_string_lossy().into_owned(),
            args,
            env,
        },
        approval_identity: None,
        auto_approve_in_unattended: false,
    };
    let servers = match &mut context.kind {
        AgentSessionKind::Acp(ctx) => &mut ctx.config.session_mcp_servers,
        AgentSessionKind::Antigravity(ctx) => &mut ctx.config.session_mcp_servers,
        AgentSessionKind::Aionrs(ctx) => &mut ctx.config.session_mcp_servers,
    };
    replace_memory_server(servers, server);
}

async fn attach_chat_history(context: &mut AgentSessionContext) {
    let Some(script) = std::env::var_os("AIONUI_CHAT_HISTORY_MCP_PATH") else {
        return;
    };
    let Some(runtime_context) = history_runtime_context(&context.runtime_env) else {
        tracing::warn!(
            conversation_id = context.conversation_id(),
            "Chat history MCP runtime context unavailable"
        );
        return;
    };
    let runtime = match aionui_runtime::ensure_runtime_command("node").await {
        Ok(runtime) => runtime,
        Err(_) => {
            tracing::warn!(
                conversation_id = context.conversation_id(),
                "Chat history MCP node runtime unavailable"
            );
            return;
        }
    };
    let mut env = runtime
        .env
        .into_iter()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()))
        .collect::<std::collections::HashMap<_, _>>();
    env.extend(runtime_context);
    let mut args = runtime
        .args_prefix
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    args.push(script.to_string_lossy().into_owned());
    let server = SessionMcpServer {
        id: "ellmsha-chat-history".into(),
        name: "ellmsha-chat-history".into(),
        transport: SessionMcpTransport::Stdio {
            command: runtime.program.to_string_lossy().into_owned(),
            args,
            env,
        },
        // The server exposes only bounded, authenticated GET requests and every
        // registered tool carries the MCP readOnlyHint.
        approval_identity: None,
        auto_approve_in_unattended: true,
    };
    let servers = match &mut context.kind {
        AgentSessionKind::Acp(ctx) => &mut ctx.config.session_mcp_servers,
        AgentSessionKind::Antigravity(ctx) => &mut ctx.config.session_mcp_servers,
        AgentSessionKind::Aionrs(ctx) => &mut ctx.config.session_mcp_servers,
    };
    replace_memory_server(servers, server);
}

fn history_runtime_context(runtime_env: &[(String, String)]) -> Option<std::collections::HashMap<String, String>> {
    const REQUIRED: [&str; 4] = [
        "AIONUI_BASE_URL",
        "AIONUI_RUNTIME_TOKEN",
        "AIONUI_USER_ID",
        "AIONUI_CONVERSATION_ID",
    ];
    let mut selected = std::collections::HashMap::new();
    for key in REQUIRED {
        let value = runtime_env
            .iter()
            .rev()
            .find_map(|(candidate, value)| (candidate == key && !value.trim().is_empty()).then(|| value.clone()))?;
        selected.insert(key.to_owned(), value);
    }
    Some(selected)
}

fn replace_memory_server(servers: &mut Vec<SessionMcpServer>, server: SessionMcpServer) {
    servers.retain(|existing| existing.id != server.id && existing.name != server.name);
    servers.push(server);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn refresh_replaces_stale_memory_snapshot_and_keeps_user_tools() {
        let server = |id: &str, workspace: &str| SessionMcpServer {
            id: id.into(),
            name: id.into(),
            transport: SessionMcpTransport::Stdio {
                command: "node".into(),
                args: vec![],
                env: HashMap::from([("AIONUI_TDAI_MEMORY_WORKSPACE".into(), workspace.into())]),
            },
            approval_identity: None,
            auto_approve_in_unattended: false,
        };
        let mut servers = vec![server("ellmsha-tencentdb-memory", "/old"), server("user-tool", "/user")];
        replace_memory_server(&mut servers, server("ellmsha-tencentdb-memory", "/new"));
        replace_memory_server(&mut servers, server("ellmsha-tencentdb-memory", "/new"));
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "user-tool");
        let SessionMcpTransport::Stdio { env, .. } = &servers[1].transport else {
            panic!("stdio expected")
        };
        assert_eq!(env["AIONUI_TDAI_MEMORY_WORKSPACE"], "/new");
    }

    #[test]
    fn history_runtime_context_keeps_only_required_session_values() {
        let env = history_runtime_context(&[
            ("AIONUI_BASE_URL".into(), "http://127.0.0.1:25808".into()),
            ("AIONUI_RUNTIME_TOKEN".into(), "runtime-token".into()),
            ("AIONUI_USER_ID".into(), "user-1".into()),
            ("AIONUI_CONVERSATION_ID".into(), "conversation-1".into()),
            ("UNRELATED_SECRET".into(), "do-not-forward".into()),
        ])
        .expect("complete runtime context");

        assert_eq!(env.len(), 4);
        assert_eq!(env["AIONUI_CONVERSATION_ID"], "conversation-1");
        assert!(!env.contains_key("UNRELATED_SECRET"));
    }

    #[test]
    fn history_runtime_context_rejects_incomplete_context() {
        assert!(history_runtime_context(&[("AIONUI_CONVERSATION_ID".into(), "conversation-1".into())]).is_none());
    }
}
