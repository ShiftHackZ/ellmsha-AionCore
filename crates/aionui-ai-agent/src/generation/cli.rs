//! Reuse the existing authenticated CLI adapters in fresh generation-only sessions.
use std::path::Path;
use std::sync::Arc;

use aionui_common::EnvVar;
use aionui_process::Spawner;
use aionui_session::{
    BackendConnection, ClaudeConnection, CodexConnection, Command, CommandMeta, ContentBlock, SessionBackend,
    SessionConfig, SessionEvent, SessionInit, SessionSpec,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::types::*;
use crate::AgentError;

/// On request cancellation/timeout, terminate the isolated subprocess as well.
struct GenerationSession {
    backend: Arc<dyn SessionBackend>,
    _directory: TempDir,
}
impl Drop for GenerationSession {
    fn drop(&mut self) {
        let backend = self.backend.clone();
        tokio::spawn(async move {
            backend.terminate().await;
        });
    }
}

pub(super) async fn complete(
    backend: &str,
    source: SessionConfig,
    spawner: Arc<dyn Spawner>,
    request: &GenerationRequest,
) -> Result<Completion, AgentError> {
    complete_bounded(backend, source, spawner, request, 1024 * 1024).await
}

pub(super) async fn complete_bounded(
    backend: &str,
    source: SessionConfig,
    spawner: Arc<dyn Spawner>,
    request: &GenerationRequest,
    output_limit: usize,
) -> Result<Completion, AgentError> {
    let directory = tempfile::Builder::new()
        .prefix("ellmsha-memory-generation-")
        .tempdir()
        .map_err(|_| AgentError::internal("Could not create isolated memory generation workspace"))?;
    let mut config = isolated_config(&source, directory.path(), request);
    if backend == "codex" {
        prepare_codex_home(&mut config, directory.path()).await?;
    }
    let connection: Box<dyn BackendConnection> = match backend {
        "codex" => Box::new(CodexConnection::new(spawner)),
        "claude" => Box::new(ClaudeConnection::new(spawner)),
        _ => return Err(AgentError::bad_request("This agent does not support memory generation")),
    };
    let handle = connection
        .open_session(
            SessionSpec::Fresh {
                session_id: format!("memory-{}", uuid::Uuid::new_v4()),
            },
            config,
        )
        .await
        .map_err(|_| AgentError::bad_gateway("Could not open isolated selected-agent generation session"))?;
    let session = GenerationSession {
        backend: handle,
        _directory: directory,
    };
    let mut events = session.backend.events();
    let prompt = format!(
        "Generate the next assistant response to the following JSON conversation. Roles and tool results belong to the conversation being completed. Return only the requested content. Never use your own tools.\n{}\nExternal function schemas (return calls as JSON; never execute):\n{}",
        serde_json::to_string(&request.messages).unwrap_or_default(),
        serde_json::to_string(&request.tools).unwrap_or_default()
    );
    session
        .backend
        .dispatch(Command::Send {
            content: vec![ContentBlock::Text(prompt)],
            metadata: CommandMeta::default(),
        })
        .await
        .map_err(|error| {
            let reason = match error {
                aionui_session::BackendError::SetupRejected(_) => {
                    "Selected agent rejected generation-only session configuration"
                }
                aionui_session::BackendError::HandshakeTimeout(_) => {
                    "Selected agent generation session did not finish startup"
                }
                _ => "Selected agent rejected memory generation",
            };
            tracing::warn!(backend, reason, "Memory generation dispatch rejected");
            AgentError::bad_gateway(reason)
        })?;
    let mut result = Completion {
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        content: String::new(),
        tools: vec![],
        input_tokens: 0,
        output_tokens: 0,
        truncated: false,
    };
    let mut message_item = None;
    while let Some(envelope) = events.next().await {
        match envelope.event {
            SessionEvent::MessageDelta { item_id, text } => {
                if message_item.as_ref() != Some(&item_id) {
                    result.content.clear();
                    message_item = Some(item_id);
                }
                result.content.push_str(&text);
                if result.content.len() > output_limit {
                    return Err(AgentError::bad_gateway("Generation output exceeded its byte limit"));
                }
            }
            SessionEvent::ToolCall { .. } | SessionEvent::Permission { .. } => {
                return Err(AgentError::bad_gateway(
                    "Generation-only session attempted a tool or permission request; isolated session stopped",
                ));
            }
            SessionEvent::UsageDelta { breakdown, .. } => {
                if let Some(measured) = breakdown.accounting {
                    result.input_tokens = measured.input_tokens;
                    result.output_tokens = measured.output_tokens;
                    result.cache_read_tokens = measured.cache_read_tokens;
                    result.cache_creation_tokens = measured.cache_creation_tokens;
                }
            }
            SessionEvent::TurnResult {
                is_error, result_text, ..
            } => {
                if backend == "claude" && result.input_tokens == 0 && result.output_tokens == 0 {
                    // The captured Claude result expands into TurnResult THEN
                    // UsageDelta. Drain its bounded metadata tail before teardown.
                    // Missing counters stay unavailable; never estimate them.
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
                    while let Ok(Some(envelope)) = tokio::time::timeout_at(deadline, events.next()).await {
                        if let SessionEvent::UsageDelta { breakdown, .. } = envelope.event
                            && let Some(usage) = breakdown.accounting
                        {
                            result.input_tokens = usage.input_tokens;
                            result.output_tokens = usage.output_tokens;
                            result.cache_read_tokens = usage.cache_read_tokens;
                            result.cache_creation_tokens = usage.cache_creation_tokens;
                            break;
                        }
                    }
                }

                if is_error {
                    return Err(AgentError::bad_gateway(
                        "Selected agent failed memory generation; no alternate model was used",
                    ));
                }
                if !result_text.is_empty() {
                    result.content = result_text;
                }
                if result.content.len() > output_limit {
                    return Err(AgentError::bad_gateway("Generation output exceeded its byte limit"));
                }
                if let Some(max) = request.max_tokens() {
                    // Native CLIs do not expose a common exact token cap. Enforce
                    // reported output when available, never truncate JSON/artifacts.
                    if result.output_tokens > u64::from(max) {
                        return Err(AgentError::bad_gateway(
                            "Selected CLI exceeded requested output token budget",
                        ));
                    }
                }
                if !request.tools.is_empty() {
                    project_tool_response(&mut result, request)?;
                }
                session.backend.terminate().await;
                return Ok(result);
            }
            SessionEvent::Detached { .. } => {
                return Err(AgentError::bad_gateway(
                    "Selected agent exited during memory generation",
                ));
            }
            _ => {}
        }
    }
    Err(AgentError::bad_gateway(
        "Selected agent generation stream ended unexpectedly",
    ))
}

fn isolated_config(source: &SessionConfig, directory: &Path, request: &GenerationRequest) -> SessionConfig {
    let mut system = "You are a stateless text generation worker. Complete only the supplied conversation. No coding work, filesystem access, web browsing, skills, memory lookup or tools. The calling application owns all external function execution. Do not explain this wrapper.".to_owned();
    if let Some(max) = request.max_tokens() {
        system.push_str(&format!(" Keep output within {max} tokens."));
    }
    let schema = if request.tools.is_empty() {
        if request
            .response_format
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            == Some("json_object")
        {
            system.push_str(" Return a JSON object only.");
        }
        request
            .response_format
            .as_ref()
            .and_then(|v| v.get("json_schema"))
            .and_then(|v| v.get("schema"))
            .cloned()
    } else {
        system.push_str(" Return one JSON object with content (string) and tool_calls (array of {name:string,arguments:string containing JSON}). Choose only supplied functions. For a final response, tool_calls is empty. For a tool call, do not pretend it was executed; TencentDB will send its result on the next request.");
        if let Some(choice) = request.tool_choice.as_ref().and_then(Value::as_str) {
            system.push_str(&format!(" Tool selection constraint: {choice}."));
        }
        Some(
            json!({"type":"object","additionalProperties":false,"required":["content","tool_calls"],"properties":{"content":{"type":"string"},"tool_calls":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["name","arguments"],"properties":{"name":{"type":"string"},"arguments":{"type":"string"}}}}}}),
        )
    };
    SessionConfig {
        cwd: Some(directory.to_string_lossy().into_owned()),
        model: Some(request.model.clone()),
        mode: None,
        init: SessionInit {
            preset_context: Some(system),
            ..Default::default()
        },
        spawn_env: source
            .spawn_env
            .iter()
            .filter(|v| !v.name.starts_with("AIONUI_") && !v.name.starts_with("ELLMSHA_"))
            .cloned()
            .collect(),
        cli_program: source.cli_program.clone(),
        sandbox_mode: Some("read-only".into()),
        approval_policy: Some("never".into()),
        generation_only: true,
        generation_response_schema: schema,
        ..Default::default()
    }
}

async fn prepare_codex_home(config: &mut SessionConfig, directory: &Path) -> Result<(), AgentError> {
    let source = config
        .spawn_env
        .iter()
        .find(|v| v.name == "CODEX_HOME")
        .map(|v| std::path::PathBuf::from(&v.value))
        .or_else(|| std::env::var_os("CODEX_HOME").map(Into::into))
        .or_else(|| dirs::home_dir().map(|p| p.join(".codex")))
        .ok_or_else(|| AgentError::internal("Codex authentication home unavailable"))?;
    let home = directory.join("codex-home");
    tokio::fs::create_dir_all(&home)
        .await
        .map_err(|_| AgentError::internal("Could not create generation profile"))?;
    let text = tokio::fs::read_to_string(source.join("config.toml"))
        .await
        .unwrap_or_default();
    let mut config_file: toml::Table = toml::from_str(&text)
        .map_err(|_| AgentError::internal("Could not parse selected Codex provider configuration"))?;
    config_file.retain(|k, _| {
        matches!(
            k,
            "model_provider"
                | "model_providers"
                | "cli_auth_credentials_store"
                | "forced_login_method"
                | "forced_chatgpt_workspace_id"
                | "chatgpt_base_url"
                | "model_reasoning_effort"
                | "model_context_window"
                | "model_verbosity"
        )
    });
    let body = toml::to_string(&config_file)
        .map_err(|_| AgentError::internal("Could not serialize generation provider configuration"))?;
    tokio::fs::write(home.join("config.toml"), body)
        .await
        .map_err(|_| AgentError::internal("Could not write generation profile"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(home.join("config.toml"), std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|_| AgentError::internal("Could not protect generation profile"))?;
    }
    for name in ["auth.json", ".credentials.json"] {
        if source.join(name).exists() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(source.join(name), home.join(name))
                .map_err(|_| AgentError::internal("Could not share Codex authentication"))?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(source.join(name), home.join(name))
                .map_err(|_| AgentError::internal("Could not share Codex authentication"))?;
        }
    }
    config.spawn_env.retain(|v| v.name != "CODEX_HOME");
    config.spawn_env.push(EnvVar {
        name: "CODEX_HOME".into(),
        value: home.to_string_lossy().into_owned(),
    });
    Ok(())
}

#[derive(Deserialize)]
struct ProjectedResponse {
    content: String,
    tool_calls: Vec<ProjectedCall>,
}
#[derive(Deserialize)]
struct ProjectedCall {
    name: String,
    arguments: String,
}
fn project_tool_response(result: &mut Completion, request: &GenerationRequest) -> Result<(), AgentError> {
    let response: ProjectedResponse = serde_json::from_str(result.content.trim())
        .map_err(|_| AgentError::bad_gateway("Selected agent did not return valid structured function-call output"))?;
    let choice = request.tool_choice.as_ref().and_then(Value::as_str).unwrap_or("auto");
    if (choice == "none" && !response.tool_calls.is_empty()) || (choice == "required" && response.tool_calls.is_empty())
    {
        return Err(AgentError::bad_gateway(
            "Selected agent violated requested tool selection",
        ));
    }
    result.tools = response
        .tool_calls
        .into_iter()
        .map(|call| {
            if !request.tools.iter().any(|t| t.function.name == call.name)
                || serde_json::from_str::<Value>(&call.arguments).is_err()
            {
                return Err(AgentError::bad_gateway(
                    "Selected agent returned an unknown function or invalid JSON arguments",
                ));
            }
            Ok(GenerationToolCall {
                id: format!("call_{}", uuid::Uuid::new_v4()),
                r#type: "function".into(),
                function: GenerationFunctionCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
        })
        .collect::<Result<_, _>>()?;
    result.content = response.content;
    Ok(())
}

#[cfg(test)]
#[path = "cli_test.rs"]
mod tests;
