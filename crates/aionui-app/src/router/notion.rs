//! Official Notion MCP OAuth, using the same pinned transport as Linear.
use super::state::{LinearOAuthProbeState, observe_remote_oauth_stderr};
use aionui_api_types::{NotionVerifiedCapabilities, NotionVerifiedIdentity};
use aionui_extension::{
    MCP_REMOTE_VERSION, NOTION_MCP_URL, NotionConnectionProbe, NotionProbeFailure, NotionProbeResult,
};
use aionui_mcp::{McpConnectionTestService, McpStderrObserver, McpToolProbeCall};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub(super) struct NotionMcpRemoteProbe {
    connection_test: McpConnectionTestService,
    oauth_attempt: Arc<tokio::sync::Mutex<()>>,
}

impl NotionMcpRemoteProbe {
    pub(super) fn new(connection_test: McpConnectionTestService, oauth_attempt: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self {
            connection_test,
            oauth_attempt,
        }
    }
}

#[async_trait::async_trait]
impl NotionConnectionProbe for NotionMcpRemoteProbe {
    async fn verify(
        &self,
        user_id: &str,
        connection_id: &str,
        auth_dir: &Path,
    ) -> Result<NotionProbeResult, NotionProbeFailure> {
        // Share the callback lease with Linear; credentials remain per connection.
        let _attempt = self.oauth_attempt.lock().await;
        tracing::info!(connection_id, stage = "oauth_start", "Notion authorization started");
        let args = vec![
            "-y".into(),
            format!("mcp-remote@{MCP_REMOTE_VERSION}"),
            NOTION_MCP_URL.into(),
            "--transport".into(),
            "http-only".into(),
            "--auth-timeout".into(),
            "300".into(),
        ];
        let env = std::collections::HashMap::from([(
            "MCP_REMOTE_CONFIG_DIR".into(),
            auth_dir.to_string_lossy().into_owned(),
        )]);
        let calls = vec![McpToolProbeCall {
            name: "notion-get-self".into(),
            arguments: serde_json::json!({}),
        }];
        let scope = format!("plugin:notion:{connection_id}:oauth");
        let state = Arc::new(Mutex::new(LinearOAuthProbeState::default()));
        let observed = state.clone();
        let observer: McpStderrObserver = Arc::new(move |line| {
            observe_remote_oauth_stderr(
                line,
                &observed,
                &aionui_shell::DefaultSystemOpener,
                "Notion",
                allowed_authorization_url,
            )
        });
        let probe = self
            .connection_test
            .probe_stdio_tools_observing_stderr("npx", &args, &env, Some(user_id), Some(&scope), &calls, observer)
            .await
            .map_err(|error| {
                let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let (failure, stage) = if state.browser_launch_failed {
                    (NotionProbeFailure::BrowserLaunch, "browser_launch")
                } else if state.authorization_url_seen && error.contains("timed out") {
                    (NotionProbeFailure::AuthorizationTimeout, "authorization_timeout")
                } else {
                    (NotionProbeFailure::Unavailable, "oauth_transport")
                };
                tracing::warn!(connection_id, stage, "Notion authorization failed");
                failure
            })?;
        let identity = probe
            .results
            .get("notion-get-self")
            .and_then(parse_identity)
            .ok_or_else(|| {
                tracing::warn!(
                    connection_id,
                    stage = "identity_verification",
                    "Notion identity could not be verified"
                );
                NotionProbeFailure::IdentityUnavailable
            })?;
        let has = |name: &str| probe.tools.iter().any(|tool| tool == name);
        tracing::info!(
            connection_id,
            stage = "identity_verified",
            "Notion authorization verified"
        );
        Ok(NotionProbeResult {
            identity,
            capabilities: NotionVerifiedCapabilities {
                search: has("notion-search") || has("notion-ai-search"),
                read: has("notion-fetch"),
                write: has("notion-create-pages") || has("notion-update-page"),
            },
        })
    }
}

fn allowed_authorization_url(value: &str) -> Option<url::Url> {
    let url = url::Url::parse(value).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && (host == "notion.com" || host.ends_with(".notion.com")))
    .then_some(url)
}

/// Parse only the documented notion-get-self response. Never invent an identity
/// from a display label, email or a previous connection's OAuth cache.
fn parse_identity(result: &Value) -> Option<NotionVerifiedIdentity> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let value = result.get("structuredContent").cloned().or_else(|| {
        result.get("content")?.as_array()?.iter().find_map(|part| {
            (part.get("type")?.as_str()? == "text")
                .then(|| serde_json::from_str::<Value>(part.get("text")?.as_str()?).ok())
                .flatten()
        })
    })?;
    let user = value.get("user")?;
    let workspace = value.get("workspace")?;
    if !matches!(user.get("type").and_then(Value::as_str), Some("person" | "bot")) {
        return None;
    }
    let text = |object: &Value, key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.len() <= 512)
            .map(str::to_owned)
    };
    let account_id = text(user, "id")?;
    let workspace_id = text(workspace, "id")?;
    Some(NotionVerifiedIdentity {
        account_name: text(user, "name").unwrap_or_else(|| account_id.clone()),
        account_email: text(user, "email"),
        account_id,
        workspace_name: text(workspace, "name"),
        workspace_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn documented_identity_envelopes_and_missing_optional_labels() {
        let identity = json!({"user":{"id":"person-1","type":"person","name":"","email":"a@example.test"},"workspace":{"id":"workspace-1","name":""}});
        let structured = parse_identity(&json!({"structuredContent":identity})).unwrap();
        let text = parse_identity(&json!({"content":[{"type":"text","text":identity.to_string()}]})).unwrap();
        assert_eq!(structured, text);
        assert_eq!(structured.workspace_id, "workspace-1");
        assert_eq!(structured.account_name, "person-1");
        assert!(structured.workspace_name.is_none());
    }
    #[test]
    fn rejects_errors_partial_and_untrusted_identity_shapes() {
        for value in [
            json!({"isError":true,"structuredContent":{"user":{"id":"x"},"workspace":{"id":"y"}}}),
            json!({"structuredContent":{"user":{"id":"x"}}}),
            json!({"structuredContent":{"workspace":{"id":"x"}}}),
            json!({"content":[{"type":"text","text":"private error"}]}),
            json!({"structuredContent":{"user":{"id":""},"workspace":{"id":"y"}}}),
        ] {
            assert!(parse_identity(&value).is_none());
        }
    }
    #[test]
    fn authorization_urls_are_notion_only() {
        assert!(allowed_authorization_url("https://www.notion.com/oauth/authorize?state=secret").is_some());
        for url in [
            "http://notion.com",
            "https://notion.com.evil.test",
            "https://user:pass@notion.com",
            "https://notion.com:444",
            "file:///tmp/private",
        ] {
            assert!(allowed_authorization_url(url).is_none());
        }
    }
    #[test]
    fn observer_accepts_only_notion_authorization_and_never_opens_untrusted_urls() {
        let state = Mutex::new(LinearOAuthProbeState::default());
        let opener = aionui_shell::DefaultSystemOpener;
        let observe = |line| observe_remote_oauth_stderr(line, &state, &opener, "Notion", allowed_authorization_url);
        observe("Please authorize this client by visiting:").unwrap();
        assert!(observe("https://notion.com.evil.test/authorize?token=private").is_err());
        assert!(state.lock().unwrap().browser_launch_failed);
        let state = Mutex::new(LinearOAuthProbeState::default());
        let observe = |line| observe_remote_oauth_stderr(line, &state, &opener, "Notion", allowed_authorization_url);
        observe("Please authorize this client by visiting:").unwrap();
        observe("https://mcp.notion.com/authorize?state=private").unwrap();
        observe("Browser opened automatically.").unwrap();
        assert!(state.lock().unwrap().authorization_url_seen);
        assert!(!state.lock().unwrap().browser_launch_failed);
    }
}
