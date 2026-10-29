use crate::shared_kernel::PersistedSessionState;
use agent_client_protocol::schema::v1::{EnvVariable, McpServer, McpServerStdio, NewSessionRequest};
use aionui_api_types::AgentMetadata;
use aionui_api_types::{AcpBuildExtra, TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig};
use aionui_common::CommandSpec;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

/// Pre-computed workspace information.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub path: String,
    pub is_custom: bool,
}

/// All pre-computed parameters needed to create and drive an ACP session.
///
/// Assembled once by `assemble_acp_params` in the factory layer; the
/// `AcpAgentManager` reads from this but never mutates it. By front-loading
/// the decision logic (which MCP servers to inject, what preset context to
/// compose) we keep the manager focused on execution + state.
#[derive(Debug, Clone)]
pub struct AcpSessionParams {
    pub conversation_id: String,
    pub user_id: String,
    pub workspace: WorkspaceInfo,
    pub metadata: AgentMetadata,
    pub command_spec: CommandSpec,
    pub config: AcpBuildExtra,
    pub mcp_servers: Vec<McpServer>,
    pub preset_context: Option<String>,
    pub session_snapshot: Option<PersistedSessionState>,
    /// Backend data directory (`AppConfig.data_dir`) used for process
    /// registration and optional prompt diagnostics.
    pub data_dir: PathBuf,
    /// Whether prompt diagnostics should be dumped under `data_dir/prompt-dumps`.
    pub dump_prompts: bool,
    /// Private launch configuration lives as long as the session, including resume/retry.
    launch_mcp_config: Option<Arc<tempfile::NamedTempFile>>,
}

impl AcpSessionParams {
    /// Copilot CLI 1.0.105 ignores stdio entries on the ACP wire, but accepts
    /// them through its documented session-local --additional-mcp-config flag.
    /// Verified against the real CLI with isolated local MCP/model fixtures.
    pub(crate) fn prepare_mcp_launch(&mut self) -> Result<(), crate::AgentError> {
        if self.metadata.backend.as_deref() != Some("copilot") || self.launch_mcp_config.is_some() {
            return Ok(());
        }
        let mut servers = serde_json::Map::new();
        for server in &self.mcp_servers {
            let McpServer::Stdio(server) = server else { continue };
            let env: std::collections::HashMap<_, _> =
                server.env.iter().map(|entry| (&entry.name, &entry.value)).collect();
            let value = serde_json::json!({
                "type": "local", "command": server.command, "args": server.args,
                "env": env, "tools": ["*"]
            });
            if servers.insert(server.name.clone(), value).is_some() {
                return Err(crate::AgentError::bad_request("Duplicate Copilot MCP server name"));
            }
        }
        if servers.is_empty() {
            return Ok(());
        }
        // NamedTempFile creates an exclusive 0600 file on Unix. Only its path
        // reaches argv/process diagnostics; credentials stay inside the file.
        let mut file = tempfile::NamedTempFile::new()
            .map_err(|_| crate::AgentError::internal("Could not create private Copilot MCP configuration"))?;
        let content = serde_json::to_vec(&serde_json::json!({"mcpServers": servers}))
            .map_err(|_| crate::AgentError::internal("Could not encode Copilot MCP configuration"))?;
        file.write_all(&content)
            .and_then(|()| file.flush())
            .map_err(|_| crate::AgentError::internal("Could not write private Copilot MCP configuration"))?;
        self.command_spec
            .args
            .extend(["--additional-mcp-config".into(), format!("@{}", file.path().display())]);
        let count = self
            .mcp_servers
            .iter()
            .filter(|s| matches!(s, McpServer::Stdio(_)))
            .count();
        self.mcp_servers.retain(|s| !matches!(s, McpServer::Stdio(_)));
        self.launch_mcp_config = Some(Arc::new(file));
        tracing::info!(
            servers = count,
            "Copilot stdio MCP servers prepared through private launch configuration"
        );
        Ok(())
    }

    /// Build a `NewSessionRequest` using the pre-computed MCP servers.
    pub fn new_session_request(&self) -> NewSessionRequest {
        let req = NewSessionRequest::new(&self.workspace.path);
        if self.mcp_servers.is_empty() {
            req
        } else {
            req.mcp_servers(self.mcp_servers.clone())
        }
    }
}

/// Assemble fully-resolved ACP session params from factory inputs.
///
/// This front-loads all decision logic that was previously scattered across
/// `build_new_session_request`, preset context normalization,
/// and the factory's ACP match arm.
///
/// `user_mcp_servers` are operator-configured MCP servers loaded from the DB
/// by the factory layer; they are appended after the team injection so
/// the agent gets *all* the user's tools on `session/new` (ELECTRON-1JG fix).
#[allow(clippy::too_many_arguments)]
pub async fn assemble_acp_params(
    conversation_id: String,
    user_id: String,
    workspace: WorkspaceInfo,
    metadata: AgentMetadata,
    command_spec: CommandSpec,
    config: AcpBuildExtra,
    user_mcp_servers: Vec<McpServer>,
    session_snapshot: Option<PersistedSessionState>,
    data_dir: PathBuf,
    dump_prompts: bool,
) -> AcpSessionParams {
    let mcp_servers = resolve_mcp_servers(&config, user_mcp_servers);
    let preset_context = compose_preset_context(config.preset_context.as_deref());

    AcpSessionParams {
        conversation_id,
        user_id,
        workspace,
        metadata,
        command_spec,
        config,
        mcp_servers,
        preset_context,
        session_snapshot,
        data_dir,
        dump_prompts,
        launch_mcp_config: None,
    }
}

/// Determine which MCP servers to inject into `session/new`.
///
/// Layout: `[team?, ...user_mcp_servers]`. The user's
/// own enabled MCP servers are always appended on top so a team
/// session still gets the operator's tools.
fn resolve_mcp_servers(config: &AcpBuildExtra, user_mcp_servers: Vec<McpServer>) -> Vec<McpServer> {
    let mut servers: Vec<McpServer> = Vec::new();
    if let Some(cfg) = config.team_mcp_stdio_config.as_ref() {
        servers.push(team_mcp_server(cfg));
    }
    servers.extend(user_mcp_servers);
    servers
}

/// Compose first-message preset context.
fn compose_preset_context(base_preset_context: Option<&str>) -> Option<String> {
    base_preset_context
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn team_mcp_server(cfg: &TeamMcpStdioConfig) -> McpServer {
    let env = vec![
        EnvVariable::new(TeamMcpStdioConfig::ENV_PORT.to_owned(), cfg.port.to_string()),
        EnvVariable::new(TeamMcpStdioConfig::ENV_TOKEN.to_owned(), cfg.token.clone()),
        EnvVariable::new(TeamMcpStdioConfig::ENV_SLOT_ID.to_owned(), cfg.slot_id.clone()),
    ];
    let stdio = McpServerStdio::new(TEAM_MCP_SERVER_NAME, &cfg.binary_path)
        .args(vec!["mcp-team-stdio".to_owned()])
        .env(env);
    McpServer::Stdio(stdio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_preset_context_returns_trimmed_base_only() {
        assert_eq!(
            compose_preset_context(Some("  frozen rules  ")),
            Some("frozen rules".to_owned())
        );
        let result = compose_preset_context(Some("  "));
        assert_eq!(result, None);
        assert_eq!(compose_preset_context(None), None);
    }

    fn user_stdio(name: &str) -> McpServer {
        McpServer::Stdio(McpServerStdio::new(name, "/bin/sh"))
    }

    async fn launch_params(backend: &str, servers: Vec<McpServer>) -> AcpSessionParams {
        let mut metadata = test_metadata();
        metadata.backend = Some(backend.into());
        assemble_acp_params(
            "conv-launch".into(),
            "user-launch".into(),
            WorkspaceInfo {
                path: "/tmp/fixture".into(),
                is_custom: false,
            },
            metadata,
            CommandSpec::default(),
            AcpBuildExtra::default(),
            servers,
            None,
            PathBuf::from("/tmp/data"),
            false,
        )
        .await
    }

    #[tokio::test]
    async fn copilot_launch_preserves_stdio_tools_env_and_private_lifetime() {
        use agent_client_protocol::schema::v1::{HttpHeader, McpServerHttp};
        let private = "fixture-private-value";
        let stdio = McpServer::Stdio(
            McpServerStdio::new("ellmsha-memory", "/fixture/node")
                .args(vec!["discovery.js".into()])
                .env(vec![EnvVariable::new("PRIVATE", private)]),
        );
        let http = McpServer::Http(
            McpServerHttp::new("remote", "http://127.0.0.1:9/mcp")
                .headers(vec![HttpHeader::new("X-Fixture", "fixture-header")]),
        );
        let mut params = launch_params("copilot", vec![stdio, http]).await;
        params.prepare_mcp_launch().unwrap();
        let args = params.command_spec.args.clone();
        assert_eq!(args[0], "--additional-mcp-config");
        assert!(!args.join(" ").contains(private));
        let path = PathBuf::from(args[1].strip_prefix('@').unwrap());
        let content: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            content["mcpServers"]["ellmsha-memory"],
            serde_json::json!({
                "type":"local", "command":"/fixture/node", "args":["discovery.js"],
                "env":{"PRIVATE":private}, "tools":["*"]
            })
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let wire = serde_json::to_value(params.new_session_request()).unwrap();
        assert_eq!(wire["mcpServers"].as_array().unwrap().len(), 1);
        assert_eq!(wire["mcpServers"][0]["name"], "remote");
        assert!(!wire.to_string().contains(private));
        params.prepare_mcp_launch().unwrap();
        assert_eq!(params.command_spec.args, args);
        let retained = params.clone();
        drop(params);
        assert!(
            path.exists(),
            "resume/startup retry must retain the launch configuration"
        );
        drop(retained);
        assert!(
            !path.exists(),
            "dropping the last session owner must remove private configuration"
        );
    }

    #[tokio::test]
    async fn copilot_launch_rejects_duplicate_names_without_partial_mutation() {
        let mut params = launch_params("copilot", vec![user_stdio("duplicate"), user_stdio("duplicate")]).await;
        assert!(
            params
                .prepare_mcp_launch()
                .unwrap_err()
                .to_string()
                .contains("Duplicate Copilot MCP")
        );
        assert!(params.command_spec.args.is_empty());
        assert_eq!(params.mcp_servers.len(), 2);
        assert!(params.launch_mcp_config.is_none());
    }

    #[tokio::test]
    async fn other_agents_and_empty_copilot_sessions_keep_their_existing_wire() {
        for backend in ["claude", "codex", "gemini", "custom"] {
            let mut params = launch_params(backend, vec![user_stdio("fixture")]).await;
            params.prepare_mcp_launch().unwrap();
            assert_eq!(params.mcp_servers.len(), 1);
            assert!(params.command_spec.args.is_empty());
            assert!(params.launch_mcp_config.is_none());
        }
        let mut params = launch_params("copilot", vec![]).await;
        params.prepare_mcp_launch().unwrap();
        assert!(params.launch_mcp_config.is_none());
        assert!(params.command_spec.args.is_empty());
    }

    fn team_cfg() -> TeamMcpStdioConfig {
        TeamMcpStdioConfig {
            team_id: "team-1".into(),
            port: 9999,
            token: "tok".into(),
            slot_id: "slot-lead".into(),
            binary_path: "/bin/backend".into(),
        }
    }

    fn test_metadata() -> AgentMetadata {
        AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude".into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Builtin,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: Some("claude".into()),
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: true,
            last_check_status: None,
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            handshake: aionui_api_types::AgentHandshake::default(),
            has_command_override: false,
            env_override_key_count: 0,
        }
    }

    #[tokio::test]
    async fn assemble_acp_params_uses_frozen_preset_context_and_snapshot_seeds() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            preset_context: Some("frozen rules".into()),
            skills: vec!["pdf".into()],
            mcp_server_ids: Some(vec!["mcp-docs".into()]),
            team_mcp_stdio_config: Some(team_cfg()),
            ..Default::default()
        };

        let params = assemble_acp_params(
            "conv-1".into(),
            "user-1".into(),
            WorkspaceInfo {
                path: "/tmp/workspace".into(),
                is_custom: false,
            },
            test_metadata(),
            CommandSpec::default(),
            config,
            vec![user_stdio("mcp-docs")],
            None,
            PathBuf::from("/tmp/data"),
            true,
        )
        .await;

        assert!(params.dump_prompts);
        assert_eq!(params.preset_context.as_deref(), Some("frozen rules"));
        assert_eq!(params.config.skills, vec!["pdf"]);
        assert_eq!(
            params.config.mcp_server_ids.as_deref(),
            Some(&["mcp-docs".to_owned()][..])
        );
        assert_eq!(params.mcp_servers.len(), 2);
    }

    #[test]
    fn resolve_mcp_servers_solo_only_gets_user_mcp_servers() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            ..Default::default()
        };
        let servers = resolve_mcp_servers(&config, vec![user_stdio("mcp-docs")]);
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "mcp-docs"),
            _ => panic!("expected user stdio server"),
        }
    }

    #[test]
    fn resolve_mcp_servers_team_session_keeps_team_mcp_before_user_mcp() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            team_mcp_stdio_config: Some(team_cfg()),
            ..Default::default()
        };
        let servers = resolve_mcp_servers(&config, vec![user_stdio("mcp-docs")]);
        assert_eq!(servers.len(), 2);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, TEAM_MCP_SERVER_NAME);
                assert_eq!(s.args, vec!["mcp-team-stdio".to_owned()]);
            }
            _ => panic!("expected stdio"),
        }
        match &servers[1] {
            McpServer::Stdio(s) => assert_eq!(s.name, "mcp-docs"),
            _ => panic!("expected stdio"),
        }
    }

    /// The pre-fix bug: with no team configured and an empty
    /// user-server list, the payload is empty. This is the *no-fix*
    /// scenario and remains valid (no MCP configured anywhere).
    #[test]
    fn resolve_mcp_servers_empty_when_nothing_configured() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            ..Default::default()
        };
        let servers = resolve_mcp_servers(&config, Vec::new());
        assert!(servers.is_empty());
    }
}
