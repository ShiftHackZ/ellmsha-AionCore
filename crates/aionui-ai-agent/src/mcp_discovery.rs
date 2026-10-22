//! One on-demand MCP surface for CLI/ACP runtimes, preserving original names.
use std::collections::HashMap;
use std::path::Path;

use agent_client_protocol::schema::v1::{EnvVariable, McpServer, McpServerStdio};
use aionui_api_types::SessionMcpTransport;
use aionui_runtime::ensure_runtime_command;
use aionui_session::{McpServerSpec, McpTransport, builtin_browser_launch_env};
use serde_json::{Value, json};

const PATH_ENV: &str = "AIONUI_MCP_DISCOVERY_PATH";
const CONFIG_ENV: &str = "AIONUI_MCP_DISCOVERY_CONFIG";

pub(crate) fn available() -> bool {
    std::env::var_os(PATH_ENV).is_some()
}

async fn wrap(
    name: &str,
    transport: SessionMcpTransport,
    runtime_env: &[(String, String)],
) -> Result<SessionMcpTransport, String> {
    let Some(script) = std::env::var_os(PATH_ENV) else {
        // Standalone Core/test deployments without the desktop resource bundle
        // retain their transport; native aionrs implements discovery itself.
        return Ok(transport);
    };
    if !Path::new(&script).is_file() {
        return Err("Packaged MCP discovery launcher is missing".into());
    }
    let mut config: Value = match transport {
        SessionMcpTransport::Stdio { command, args, mut env } => {
            if env.contains_key(CONFIG_ENV) {
                return Err("Recursive MCP discovery configuration".into());
            }
            // Verify the original browser launcher before wrapping; never trust
            // a server's name alone to grant the live CDP bridge credentials.
            let spec = McpServerSpec {
                name: name.into(),
                auto_approve_in_unattended: false,
                transport: McpTransport::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.clone().into_iter().collect(),
                },
            };
            let launch = builtin_browser_launch_env(&spec);
            let owned = aionui_session::is_owned_browser_launcher(&spec);
            env.extend(launch);
            if owned {
                env.remove("AIONUI_BROWSER_SCOPE_TOKEN");
                if let Some((name, value)) = runtime_env
                    .iter()
                    .find(|(name, _)| name == "AIONUI_BROWSER_SCOPE_TOKEN")
                {
                    env.insert(name.clone(), value.clone());
                }
            }
            let runtime = ensure_runtime_command(&command)
                .await
                .map_err(|_| "MCP upstream runtime unavailable")?;
            let mut launch_args: Vec<String> = runtime
                .args_prefix
                .into_iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            launch_args.extend(args);
            env.extend(
                runtime
                    .env
                    .into_iter()
                    .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())),
            );
            json!({"type":"stdio", "command":runtime.program, "args":launch_args, "env":env})
        }
        SessionMcpTransport::Http { url, headers } | SessionMcpTransport::StreamableHttp { url, headers } => {
            json!({"type":"streamable_http", "url":url, "headers":headers})
        }
        SessionMcpTransport::Sse { url, headers } => json!({"type":"sse", "url":url, "headers":headers}),
    };
    // Stable launch fingerprints keep unchanged account/tool grants reusable.
    config.sort_all_objects();
    let runtime = ensure_runtime_command("node")
        .await
        .map_err(|_| "MCP discovery runtime unavailable")?;
    let mut args: Vec<String> = runtime
        .args_prefix
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    args.push(script.to_string_lossy().into_owned());
    let mut env: HashMap<String, String> = runtime
        .env
        .into_iter()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect();
    env.insert(CONFIG_ENV.into(), config.to_string());
    Ok(SessionMcpTransport::Stdio {
        command: runtime.program.to_string_lossy().into_owned(),
        args,
        env,
    })
}

pub(crate) async fn prepare_specs(
    servers: &mut [McpServerSpec],
    runtime_env: &[(String, String)],
) -> Result<(), String> {
    if !available() {
        return Ok(());
    }
    for server in servers.iter_mut() {
        let transport = match &server.transport {
            McpTransport::Stdio { command, args, env } => SessionMcpTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env.iter().cloned().collect(),
            },
            McpTransport::Http { url, headers } => SessionMcpTransport::StreamableHttp {
                url: url.clone(),
                headers: headers.iter().cloned().collect(),
            },
            McpTransport::Sse { url, headers } => SessionMcpTransport::Sse {
                url: url.clone(),
                headers: headers.iter().cloned().collect(),
            },
        };
        let SessionMcpTransport::Stdio { command, args, env } = wrap(&server.name, transport, runtime_env).await?
        else {
            return Err("MCP discovery configuration changed during preparation".into());
        };
        server.transport = McpTransport::Stdio {
            command,
            args,
            env: env.into_iter().collect(),
        };
    }
    tracing::info!(
        servers = servers.len(),
        "CLI MCP catalogs prepared for on-demand discovery"
    );
    Ok(())
}

pub(crate) async fn prepare_sdk(servers: &mut [McpServer], runtime_env: &[(String, String)]) -> Result<(), String> {
    if !available() {
        return Ok(());
    }
    for server in servers.iter_mut() {
        let (name, transport) = match &*server {
            McpServer::Stdio(s) => (
                s.name.clone(),
                SessionMcpTransport::Stdio {
                    command: s.command.to_string_lossy().into_owned(),
                    args: s.args.clone(),
                    env: s.env.iter().map(|e| (e.name.clone(), e.value.clone())).collect(),
                },
            ),
            McpServer::Http(s) => (
                s.name.clone(),
                SessionMcpTransport::StreamableHttp {
                    url: s.url.clone(),
                    headers: s.headers.iter().map(|h| (h.name.clone(), h.value.clone())).collect(),
                },
            ),
            McpServer::Sse(s) => (
                s.name.clone(),
                SessionMcpTransport::Sse {
                    url: s.url.clone(),
                    headers: s.headers.iter().map(|h| (h.name.clone(), h.value.clone())).collect(),
                },
            ),
            _ => return Err("Unsupported MCP transport for discovery".into()),
        };
        let SessionMcpTransport::Stdio { command, args, env } = wrap(&name, transport, runtime_env).await? else {
            return Err("MCP discovery configuration changed during preparation".into());
        };
        *server = McpServer::Stdio(
            McpServerStdio::new(name, command)
                .args(args)
                .env(env.into_iter().map(|(k, v)| EnvVariable::new(k, v)).collect()),
        );
    }
    tracing::info!(
        servers = servers.len(),
        "ACP MCP catalogs prepared for on-demand discovery"
    );
    Ok(())
}
