//! Same bounded, loopback-only transport as the existing router host façade.
use super::cmd_router_stdio::RouterStdio;
use aionui_api_types::secrets::*;
use rmcp::{
    RoleServer, ServerHandler,
    model::*,
    service::{RequestContext, ServiceExt},
};
use serde_json::{Value, json};
use std::{process::ExitCode, sync::Arc};

#[derive(Clone)]
struct SecretsStdio {
    transport: RouterStdio,
    project_scope: bool,
}

fn schema<T: rmcp::schemars::JsonSchema>(remove_scope: bool) -> serde_json::Map<String, Value> {
    let mut value = serde_json::to_value(rmcp::schemars::schema_for!(T)).unwrap_or_else(|_| json!({"type":"object"}));
    if remove_scope && let Some(properties) = value.get_mut("properties").and_then(Value::as_object_mut) {
        properties.remove("scope");
    }
    if remove_scope && let Some(required) = value.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|v| v != "scope");
    }
    value.as_object().cloned().unwrap_or_default()
}
fn tools(project_scope: bool) -> Vec<Tool> {
    let mut tools = vec![];
    for scope in ["global", "project"]
        .into_iter()
        .filter(|scope| *scope != "project" || project_scope)
    {
        for (suffix, description, input) in [
            (
                "secrets_list",
                "List unlocked metadata only; select a record ID and exact environment/tenant binding.",
                schema::<SecretListRequest>(true),
            ),
            (
                "secret_read",
                "Return RAW automation credentials to the model. Never put them in memory. Required/unknown MFA needs ellmsha_ask_user before authentication.",
                schema::<SecretReadRequest>(true),
            ),
            (
                "secret_write",
                "Create a credential or patch with its current metadata expected_incarnation and expected_revision. Omitted fields remain; null removes. UUID operation_id makes retries idempotent. Never save OTP, seed or recovery material.",
                schema::<SecretWriteRequest>(true),
            ),
            (
                "secret_delete",
                "Delete exactly one record with its metadata expected_incarnation, expected_revision and UUID operation_id. Re-read and re-approve after restore.",
                schema::<SecretDeleteRequest>(true),
            ),
        ] {
            tools.push(Tool::new(
                format!("ellmsha_{scope}_{suffix}"),
                description,
                Arc::new(input),
            ));
        }
    }
    tools.push(Tool::new("ellmsha_secret_history_search","Find unverified credential candidates in canonical owned chat history; RAW values reach the model. Scope defaults to this chat. No external transcript or attachment search.",Arc::new(schema::<SecretHistorySearchRequest>(false))));
    tools.push(Tool::new("ellmsha_secret_history_mask","Atomically replace selected candidate spans with <masked> in canonical history. Save separately first if needed. Does NOT erase active model context, provider/native replay, old exports/backups, memory or existing forks.",Arc::new(schema::<SecretHistoryMaskRequest>(false))));
    tools.push(Tool::new("ellmsha_ask_user","Ask the human through the existing question UI. For MFA prefer direct device approval; relay OTP only as isSecret and never persist it.",Arc::new(json!({"type":"object","properties":{"questions":{"type":"array","minItems":1,"maxItems":3,"items":{"type":"object","properties":{"id":{"type":"string"},"header":{"type":"string"},"question":{"type":"string"},"options":{"type":"array","items":{"type":"object","properties":{"label":{"type":"string"},"description":{"type":"string"}},"required":["label","description"]}},"isOther":{"type":"boolean"},"multiSelect":{"type":"boolean"},"isSecret":{"type":"boolean"}},"required":["id","header","question","options","isOther","multiSelect"]}}},"required":["questions"],"additionalProperties":false}).as_object().cloned().unwrap_or_default())));
    tools
}
impl ServerHandler for SecretsStdio {
    fn get_info(&self) -> ServerInfo {
        {
            let mut info = ServerInfo::default();
            info.capabilities = ServerCapabilities::builder().enable_tools().build();
            info
        }
    }
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult::with_all_items(tools(self.project_scope)))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if !tools(self.project_scope).iter().any(|t| t.name == request.name) {
            return Err(rmcp::ErrorData::invalid_params("Unknown Secrets tool", None));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        tokio::select! {
            result = self.transport.forward_secret(&request.name, &arguments, Some(&id)) => Ok(result),
            _ = context.ct.cancelled() => {
                if request.name == "ellmsha_ask_user" { self.transport.cancel_secret_question(&id).await; }
                Ok(CallToolResult::error(vec![Content::text("QUESTION_CANCELLED")]))
            }
        }
    }
}
pub(crate) async fn run() -> ExitCode {
    let Ok(transport) = RouterStdio::from_env() else {
        eprintln!("CONTEXT_REQUIRED");
        return ExitCode::from(2);
    };
    let project_scope = std::env::var("AIONUI_SECRETS_PROJECT_ENABLED").ok().as_deref() == Some("1");
    match (SecretsStdio {
        transport,
        project_scope,
    })
    .serve(rmcp::transport::io::stdio())
    .await
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temporary_scope_does_not_advertise_project_tools() {
        let tools = tools(false);
        assert_eq!(tools.len(), 7);
        assert!(tools.iter().all(|tool| !tool.name.starts_with("ellmsha_project_")));
    }
    #[test]
    fn scoped_tools_hide_only_server_owned_scope_and_history_keeps_selector() {
        let tools = tools(true);
        assert_eq!(tools.len(), 11);
        let names: std::collections::HashSet<_> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names.len(), 11);
        for tool in &tools {
            let properties = tool.input_schema.get("properties").unwrap().as_object().unwrap();
            if tool.name.starts_with("ellmsha_global_") || tool.name.starts_with("ellmsha_project_") {
                assert!(!properties.contains_key("scope"));
                assert!(
                    !tool
                        .input_schema
                        .get("required")
                        .and_then(Value::as_array)
                        .is_some_and(|v| v.contains(&json!("scope")))
                );
            }
            if tool.name.ends_with("_secret_write") || tool.name.ends_with("_secret_delete") {
                assert!(properties.contains_key("expected_incarnation"));
                assert!(properties.contains_key("expected_revision"));
            }
            if tool.name.ends_with("_secret_delete") {
                assert!(
                    tool.input_schema["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("expected_incarnation"))
                );
            }
            if tool.name == "ellmsha_secret_history_search" {
                assert!(
                    properties.contains_key("scope"),
                    "history search must advertise its caller-selected owned-history scope"
                );
            }
        }
    }
}
