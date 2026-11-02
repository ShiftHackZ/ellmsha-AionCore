//! Account-bound, approval-gated Figma REST tools; compiled into every Core target.
use super::FigmaApi;
use aionui_extension::figma::{FIGMA_ACCOUNT_ID_ENV, FIGMA_CONNECTION_ID_ENV, FIGMA_TOKEN_ENV};
use reqwest::Method;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    schemars,
    service::ServiceExt,
    tool, tool_router, transport,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{process::ExitCode, sync::Arc};

pub async fn run_figma_stdio() -> ExitCode {
    let server = match FigmaTools::from_env() {
        Ok(server) => server,
        Err(_) => {
            eprintln!("FIGMA_MCP_CONFIG_INVALID: missing or invalid private account configuration");
            return ExitCode::from(2);
        }
    };
    match server.serve(transport::io::stdio()).await {
        Ok(peer) => {
            if peer.waiting().await.is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(_) => ExitCode::from(1),
    }
}

#[derive(Clone)]
struct FigmaTools {
    api: FigmaApi,
    token: Arc<String>,
    account_id: String,
    connection_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FileParams {
    /// Figma file or branch key from a user-provided Figma URL, not a URL itself.
    file_key: String,
    /// Optional document depth from 1 to 10. Defaults to 2 to bound responses.
    depth: Option<u8>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NodesParams {
    /// Figma file or branch key.
    file_key: String,
    /// Between 1 and 50 exact node IDs, e.g. 1:2. URL node-id separators must be decoded.
    node_ids: Vec<String>,
    /// Optional subtree depth, 1 to 10. Defaults to 2.
    depth: Option<u8>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ImageFormat {
    Png,
    Svg,
    Jpg,
    Pdf,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ImagesParams {
    /// Figma file or branch key.
    file_key: String,
    /// Between 1 and 50 exact node IDs.
    node_ids: Vec<String>,
    /// Export format. Defaults to png. Null image URLs mean a node failed to render.
    format: Option<ImageFormat>,
    /// Export scale from 0.01 to 4. Defaults to 1.
    scale: Option<f64>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TeamParams {
    /// Team ID from the user's Figma team URL. There is no API to discover all team IDs.
    team_id: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FolderParams {
    /// Folder ID returned by the team folders or subfolders tool.
    folder_id: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CommentsParams {
    /// Figma file or branch key whose comments should be inspected.
    file_key: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddCommentParams {
    /// Target Figma file or branch key explicitly authorized by the user.
    file_key: String,
    /// Exact authorized comment text, between 1 and 10,000 characters.
    message: String,
    /// Optional root comment ID to reply to. Omit to create a file-level comment.
    comment_id: Option<String>,
}

impl FigmaTools {
    fn from_env() -> Result<Self, String> {
        let read = |name| {
            std::env::var(name)
                .ok()
                .filter(|value| {
                    !value.is_empty() && value.len() <= 16_384 && value.bytes().all(|byte| byte.is_ascii_graphic())
                })
                .ok_or_else(|| "private configuration unavailable".to_owned())
        };
        Ok(Self {
            api: FigmaApi::new()?,
            token: Arc::new(read(FIGMA_TOKEN_ENV)?),
            account_id: read(FIGMA_ACCOUNT_ID_ENV)?,
            connection_id: read(FIGMA_CONNECTION_ID_ENV)?,
        })
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value, String> {
        let identity = self.api.identity(&self.token).await?;
        if identity.account_id != self.account_id {
            return Err("Figma account identity changed. Reconnect the intended account before continuing.".into());
        }
        let result = if path == "/v1/me" {
            serde_json::to_value(&identity).map_err(|_| "Figma identity unavailable".to_owned())?
        } else {
            self.api.request(&self.token, method, path, query, body).await?
        };
        let value = json!({"connection_id":self.connection_id,"account":identity,"data":result});
        if value.to_string().len() > 1024 * 1024 {
            return Err("Figma tool output exceeds 1 MiB. Request fewer nodes or a smaller depth; this result was not returned as complete.".into());
        }
        // Provider-controlled content must never echo this process's credential.
        if value.to_string().contains(self.token.as_str()) {
            return Err("Figma returned a response containing private account data; output withheld.".into());
        }
        Ok(value)
    }
}

fn result(value: Result<Value, String>) -> CallToolResult {
    match value {
        Ok(value) => CallToolResult::success(vec![Content::text(value.to_string())]),
        Err(error) => CallToolResult::error(vec![Content::text(error)]),
    }
}
fn key(value: &str) -> Result<&str, String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err("Invalid Figma resource key: use an exact provider ID, not a URL or path.".into());
    }
    Ok(value)
}
fn nodes(values: &[String]) -> Result<String, String> {
    if values.is_empty()
        || values.len() > 50
        || values.iter().any(|value| {
            value.is_empty()
                || value.len() > 200
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b":;_-".contains(&byte))
        })
    {
        return Err("Provide 1 to 50 valid Figma node IDs.".into());
    }
    Ok(values.join(","))
}
fn depth(value: Option<u8>) -> Result<String, String> {
    let value = value.unwrap_or(2);
    if !(1..=10).contains(&value) {
        return Err("Figma depth must be between 1 and 10.".into());
    }
    Ok(value.to_string())
}

#[tool_router]
impl FigmaTools {
    #[tool(
        name = "figma_whoami",
        description = "Verify the Figma identity for this specific account connection.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn whoami(&self) -> CallToolResult {
        result(self.request(Method::GET, "/v1/me", &[], None).await)
    }

    #[tool(
        name = "figma_get_file",
        description = "Read a bounded Figma document tree. Requires file_content:read. Start at depth 2, then inspect specific nodes.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn get_file(&self, Parameters(p): Parameters<FileParams>) -> CallToolResult {
        result(
            async {
                let path = format!("/v1/files/{}", key(&p.file_key)?);
                self.request(Method::GET, &path, &[("depth", depth(p.depth)?)], None)
                    .await
            }
            .await,
        )
    }
    #[tool(
        name = "figma_get_nodes",
        description = "Read up to 50 selected Figma node subtrees. Requires file_content:read.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn get_nodes(&self, Parameters(p): Parameters<NodesParams>) -> CallToolResult {
        result(
            async {
                let path = format!("/v1/files/{}/nodes", key(&p.file_key)?);
                self.request(
                    Method::GET,
                    &path,
                    &[("ids", nodes(&p.node_ids)?), ("depth", depth(p.depth)?)],
                    None,
                )
                .await
            }
            .await,
        )
    }
    #[tool(
        name = "figma_export_images",
        description = "Export selected nodes to expiring image URLs. Null URLs indicate failed renders. Requires file_content:read; does not fetch external asset URLs.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn export_images(&self, Parameters(p): Parameters<ImagesParams>) -> CallToolResult {
        result(
            async {
                let scale = p.scale.unwrap_or(1.0);
                if !scale.is_finite() || !(0.01..=4.0).contains(&scale) {
                    return Err("Figma image scale must be between 0.01 and 4.".into());
                }
                let format = match p.format.unwrap_or(ImageFormat::Png) {
                    ImageFormat::Png => "png",
                    ImageFormat::Svg => "svg",
                    ImageFormat::Jpg => "jpg",
                    ImageFormat::Pdf => "pdf",
                };
                let path = format!("/v1/images/{}", key(&p.file_key)?);
                self.request(
                    Method::GET,
                    &path,
                    &[
                        ("ids", nodes(&p.node_ids)?),
                        ("scale", scale.to_string()),
                        ("format", format.into()),
                    ],
                    None,
                )
                .await
            }
            .await,
        )
    }
    #[tool(
        name = "figma_list_team_folders",
        description = "List a team's top-level folders (formerly projects). Supply a team ID from its URL; requires folders:read. Traverse subfolders separately.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_team_folders(&self, Parameters(p): Parameters<TeamParams>) -> CallToolResult {
        result(
            async {
                let path = format!("/v2/teams/{}/folders", key(&p.team_id)?);
                self.request(Method::GET, &path, &[], None).await
            }
            .await,
        )
    }
    #[tool(
        name = "figma_list_subfolders",
        description = "List direct child folders only. Requires folders:read.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_subfolders(&self, Parameters(p): Parameters<FolderParams>) -> CallToolResult {
        result(
            async {
                let path = format!("/v2/folders/{}/folders", key(&p.folder_id)?);
                self.request(Method::GET, &path, &[], None).await
            }
            .await,
        )
    }
    #[tool(
        name = "figma_list_folder_files",
        description = "List files directly inside one folder; does not recurse. Requires folders:read.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_folder_files(&self, Parameters(p): Parameters<FolderParams>) -> CallToolResult {
        result(
            async {
                let path = format!("/v2/folders/{}/files", key(&p.folder_id)?);
                self.request(Method::GET, &path, &[], None).await
            }
            .await,
        )
    }
    #[tool(
        name = "figma_list_comments",
        description = "Read comments on one Figma file. Requires file_comments:read. Inspect before repeating a comment write with unknown outcome.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_comments(&self, Parameters(p): Parameters<CommentsParams>) -> CallToolResult {
        result(
            async {
                let path = format!("/v1/files/{}/comments", key(&p.file_key)?);
                self.request(Method::GET, &path, &[], None).await
            }
            .await,
        )
    }
    #[tool(
        name = "figma_add_comment",
        description = "Post exactly one user-authorized comment or reply to a Figma file. Requires file_comments:write. This external write is not automatically retried; inspect comments if the result is unknown.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn add_comment(&self, Parameters(p): Parameters<AddCommentParams>) -> CallToolResult {
        result(
            async {
                if p.message.trim().is_empty() || p.message.chars().count() > 10_000 {
                    return Err("Comment must contain 1 to 10,000 characters.".into());
                }
                let mut body = json!({"message":p.message});
                if let Some(id) = p.comment_id {
                    key(&id)?;
                    body["comment_id"] = json!(id);
                }
                let path = format!("/v1/files/{}/comments", key(&p.file_key)?);
                self.request(Method::POST, &path, &[], Some(body)).await
            }
            .await,
        )
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for FigmaTools {}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn real_mcp_transport_routes_requests_to_exact_account_and_rejects_identity_drift() {
        let (api, requests, server) = super::super::tests::fixture().await;
        let tools = FigmaTools {
            api: api.clone(),
            token: Arc::new("fixture-token-one".into()),
            account_id: "account-one".into(),
            connection_id: "connection-one".into(),
        };
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let service = tokio::spawn(async move {
            tools.serve(server_io).await.unwrap().waiting().await.unwrap();
        });
        let client = ().serve(client_io).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 9);
        let write = tools.iter().find(|tool| tool.name == "figma_add_comment").unwrap();
        assert_eq!(write.annotations.as_ref().unwrap().read_only_hint, Some(false));
        let response = client
            .call_tool(
                rmcp::model::CallToolRequestParams::new("figma_get_nodes").with_arguments(
                    json!({"file_key":"FileOne","node_ids":["1:2"],"depth":1})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true));
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, path, token)| path.starts_with("/v1/files/FileOne/nodes?")
                    && path.contains("depth=1")
                    && token == "fixture-token-one")
        );
        for (name, arguments) in [
            ("figma_whoami", json!({})),
            ("figma_get_file", json!({"file_key":"FileOne"})),
            (
                "figma_export_images",
                json!({"file_key":"FileOne","node_ids":["1:2"],"format":"svg","scale":2}),
            ),
            ("figma_list_team_folders", json!({"team_id":"123"})),
            ("figma_list_subfolders", json!({"folder_id":"234"})),
            ("figma_list_folder_files", json!({"folder_id":"234"})),
            ("figma_list_comments", json!({"file_key":"FileOne"})),
            (
                "figma_add_comment",
                json!({"file_key":"FileOne","message":"Authorized fixture comment"}),
            ),
        ] {
            let response = client
                .call_tool(
                    rmcp::model::CallToolRequestParams::new(name)
                        .with_arguments(arguments.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            assert_ne!(response.is_error, Some(true), "{name}: {response:?}");
        }
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(method, _, _)| method == "POST")
                .count(),
            1
        );
        let previous_calls = requests.lock().unwrap().len();
        let response = client
            .call_tool(
                rmcp::model::CallToolRequestParams::new("figma_get_file")
                    .with_arguments(json!({"file_key":"../me"}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(response.is_error, Some(true));
        assert_eq!(requests.lock().unwrap().len(), previous_calls);
        let changed = FigmaTools {
            api,
            token: Arc::new("fixture-token-two".into()),
            account_id: "account-one".into(),
            connection_id: "one".into(),
        };
        let error = changed
            .add_comment(Parameters(AddCommentParams {
                file_key: "FileOne".into(),
                message: "Authorized comment".into(),
                comment_id: None,
            }))
            .await;
        assert_eq!(error.is_error, Some(true));
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(method, _, _)| method == "POST")
                .count(),
            1
        );
        client.cancel().await.unwrap();
        service.await.unwrap();
        server.abort();
    }
    #[test]
    fn rejects_path_injection_and_unbounded_queries() {
        for value in ["../me", "x?token=secret", "https://evil.test", "x/y", ""] {
            assert!(key(value).is_err());
        }
        assert!(nodes(&[]).is_err());
        assert!(nodes(&vec!["1:2".into(); 51]).is_err());
        assert!(depth(Some(0)).is_err());
        assert!(depth(Some(11)).is_err());
    }
}
