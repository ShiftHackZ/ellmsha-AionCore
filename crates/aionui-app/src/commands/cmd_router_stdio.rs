//! Session-scoped MCP façade for the host's authenticated router child tools.
use aionui_api_types::RouterChildCall;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars,
    service::ServiceExt,
    tool, tool_router, transport,
};
use serde::Deserialize;
use std::{process::ExitCode, time::Duration};

// A 32 KiB child result can expand sixfold when JSON escapes control bytes.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

pub async fn run_router_stdio() -> ExitCode {
    let Ok(server) = RouterStdio::from_env() else {
        eprintln!("ROUTER_MCP_CONTEXT_INVALID");
        return ExitCode::from(2);
    };
    match server.serve(transport::io::stdio()).await {
        Ok(peer) => {
            if peer.waiting().await.is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        _ => ExitCode::from(1),
    }
}
#[derive(Clone)]
struct RouterStdio {
    client: reqwest::Client,
    url: String,
    user: String,
    conversation: String,
    token: String,
}
impl RouterStdio {
    fn from_env() -> Result<Self, ()> {
        let get = |key| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty() && v.len() < 8192)
                .ok_or(())
        };
        let base = get("AIONUI_BASE_URL")?;
        let parsed = reqwest::Url::parse(&base).map_err(|_| ())?;
        if parsed.scheme() != "http"
            || !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(());
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(55))
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| ())?,
            url: format!("{}/api/runtime/router-tools/call", base.trim_end_matches('/')),
            user: get("AIONUI_USER_ID")?,
            conversation: get("AIONUI_CONVERSATION_ID")?,
            token: get("AIONUI_RUNTIME_TOKEN")?,
        })
    }
    async fn forward(&self, request: RouterChildCall) -> CallToolResult {
        let result = self
            .client
            .post(&self.url)
            .header("x-aionui-user-id", &self.user)
            .header("x-aionui-conversation-id", &self.conversation)
            .header("x-aionui-runtime-token", &self.token)
            .json(&request)
            .send()
            .await;
        let Ok(mut response) = result else {
            return CallToolResult::error(vec![Content::text(
                "Router host unavailable; do not assume the child completed.",
            )]);
        };
        let success = response.status().is_success();
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                        return CallToolResult::error(vec![Content::text("Router result exceeds output limit")]);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => {
                    return CallToolResult::error(vec![Content::text(
                        "Router response interrupted; status is unknown",
                    )]);
                }
            }
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if success {
            CallToolResult::success(vec![Content::text(text)])
        } else {
            CallToolResult::error(vec![Content::text(text)])
        }
    }
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Spawn {
    /// Complete delegated task; the host applies the saved classifier/default policy.
    task: String,
    /// Optional short child label.
    name: Option<String>,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Wait {
    /// Opaque task ID returned by router_spawn.
    task_id: String,
    /// Wait up to 50,000 milliseconds; zero returns immediate status.
    #[serde(default)]
    timeout_ms: u64,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Cancel {
    /// Opaque task ID returned by router_spawn in this parent turn.
    task_id: String,
}
#[tool_router]
impl RouterStdio {
    #[tool(
        name = "router_spawn",
        description = "Delegate one task through the user's frozen Auto Router policy. The host classifies custom task classes or uses the default child route, selects the configured provider/model, preserves workspace and permissions, and starts a visible child chat. Use router_wait for its result before finishing your turn. Only these host-managed children are controlled by Auto Router; provider-internal children are outside this policy."
    )]
    async fn spawn(&self, Parameters(p): Parameters<Spawn>) -> CallToolResult {
        self.forward(RouterChildCall::Spawn {
            task: p.task,
            name: p.name,
        })
        .await
    }
    #[tool(
        name = "router_wait",
        description = "Wait for one child spawned in this parent turn, at most 50 seconds. Returns pending_init/running/waiting_for_approval/cancelling/completed/errored/interrupted plus bounded result and child chat ID. Waiting for approval requires the user in that child chat; never auto-approve. A timeout is not success."
    )]
    async fn wait(&self, Parameters(p): Parameters<Wait>) -> CallToolResult {
        self.forward(RouterChildCall::Wait {
            task_id: p.task_id,
            timeout_ms: p.timeout_ms,
        })
        .await
    }
    #[tool(
        name = "router_cancel",
        description = "Cancel one host-managed child spawned in this parent turn, including pending provisioning. Finished results remain available until this parent turn finishes."
    )]
    async fn cancel(&self, Parameters(p): Parameters<Cancel>) -> CallToolResult {
        self.forward(RouterChildCall::Cancel { task_id: p.task_id }).await
    }
}
#[rmcp::tool_handler(router=Self::tool_router())]
impl rmcp::ServerHandler for RouterStdio {}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{RouterChildResponse, RouterChildStatus};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    fn client(url: String) -> RouterStdio {
        RouterStdio {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            url,
            user: "fixture".into(),
            conversation: "parent".into(),
            token: "fixture-only".into(),
        }
    }
    fn wait() -> RouterChildCall {
        RouterChildCall::Wait {
            task_id: "task".into(),
            timeout_ms: 0,
        }
    }

    #[tokio::test]
    async fn router_stdio_roundtrips_maximum_json_escaped_child_result() {
        let server = MockServer::start().await;
        let expected = RouterChildResponse {
            task_id: "task".into(),
            conversation_id: Some("child".into()),
            status: RouterChildStatus::Completed,
            result: Some("\0".repeat(32 * 1024)),
            truncated: false,
            error: None,
        };
        let body = serde_json::to_string(&expected).unwrap();
        assert!(body.len() > 64 * 1024);
        assert!(body.len() < MAX_RESPONSE_BYTES);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let result = client(server.uri()).forward(wait()).await;
        assert_ne!(result.is_error, Some(true));
        let actual: RouterChildResponse = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[tokio::test]
    async fn router_stdio_rejects_oversized_wire_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(MAX_RESPONSE_BYTES + 1)))
            .mount(&server)
            .await;
        let result = client(server.uri()).forward(wait()).await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "Router result exceeds output limit"
        );
    }

    #[tokio::test]
    async fn router_stdio_rejects_interrupted_response_instead_of_reporting_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"task_id\":")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });
        let result = client(format!("http://{address}/")).forward(wait()).await;
        server.await.unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "Router response interrupted; status is unknown"
        );
    }
}
