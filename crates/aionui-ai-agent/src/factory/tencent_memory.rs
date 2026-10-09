//! Keep the runtime memory MCP available even in older chats and delegated sessions.
use aionui_api_types::{SessionMcpServer, SessionMcpTransport};

use crate::session_context::{AgentSessionContext, AgentSessionKind};

pub(super) async fn attach_runtime_memory(
    context: &mut AgentSessionContext,
    generation: Option<&crate::generation::GenerationService>,
) {
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
    };
    let servers = match &mut context.kind {
        AgentSessionKind::Acp(ctx) => &mut ctx.config.session_mcp_servers,
        AgentSessionKind::Antigravity(ctx) => &mut ctx.config.session_mcp_servers,
        AgentSessionKind::Aionrs(ctx) => &mut ctx.config.session_mcp_servers,
    };
    replace_memory_server(servers, server);
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
}
