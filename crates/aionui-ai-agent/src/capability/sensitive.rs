//! Display/persistence projection for host-registered Secrets tools. Model transport is unchanged.
use crate::protocol::events::{AcpPermissionEventData, AgentStreamEvent};
use std::collections::BTreeMap;
use tokio::sync::broadcast;

const TOOLS: &[&str] = &[
    "ellmsha_global_secrets_list",
    "ellmsha_global_secret_read",
    "ellmsha_global_secret_write",
    "ellmsha_global_secret_delete",
    "ellmsha_project_secrets_list",
    "ellmsha_project_secret_read",
    "ellmsha_project_secret_write",
    "ellmsha_project_secret_delete",
    "ellmsha_secret_history_search",
    "ellmsha_secret_history_mask",
    "ellmsha_ask_user",
];
fn operation(name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    if let Some(tool) = name
        .strip_prefix("mcp__ellmsha-secrets__")
        .filter(|tool| TOOLS.contains(tool))
    {
        return Some(tool.into());
    }
    let input = input?;
    // This marker is injected by the host after correlating the MCP item and request.
    if let Some(exact) = input.pointer("/_meta/ellmsha_exact_tool")
        && exact.get("server").and_then(serde_json::Value::as_str) == Some("ellmsha-secrets")
    {
        return exact
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .filter(|tool| TOOLS.contains(tool) && name == format!("ellmsha-secrets · {tool}"))
            .map(str::to_owned);
    }
    // Codex's typed item is the identity source; arbitrary ordinary tool arguments are not.
    if name == "mcpToolCall"
        && input.get("type").and_then(serde_json::Value::as_str) == Some("mcpToolCall")
        && input.get("server").and_then(serde_json::Value::as_str) == Some("ellmsha-secrets")
    {
        return input
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .filter(|tool| TOOLS.contains(tool))
            .map(str::to_owned);
    }
    // ACP adapter identity, only when its exact tool title also agrees.
    if input.get("server_name").and_then(serde_json::Value::as_str) == Some("ellmsha-secrets") && TOOLS.contains(&name)
    {
        return Some(name.into());
    }
    None
}
pub(crate) fn registered(name: &str, input: Option<&serde_json::Value>) -> bool {
    operation(name, input).is_some()
}
/// Metadata allowed in human approvals; never titles, queries, field names or values.
pub(crate) fn sensitive_summary(input: &serde_json::Value) -> serde_json::Value {
    let mut summary = serde_json::json!({"sensitive":true});
    if let Some(id) = input.get("id").and_then(serde_json::Value::as_str)
        && id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        summary["record_id"] = serde_json::json!(id);
    }
    if let Some(revision) = input.get("expected_revision").and_then(serde_json::Value::as_u64) {
        summary["expected_revision"] = revision.into();
    }
    if let Some(candidates) = input.get("candidate_ids").and_then(serde_json::Value::as_array) {
        summary["candidate_count"] = candidates.len().into();
    }
    summary
}
fn safe_input(tool: &str, raw: Option<&serde_json::Value>) -> serde_json::Value {
    let args = raw.map(|v| {
        v.get("arguments")
            .or_else(|| v.get("args"))
            .or_else(|| v.pointer("/_meta/tool_params"))
            .unwrap_or(v)
    });
    let mut summary = sensitive_summary(args.unwrap_or(&serde_json::Value::Null));
    if TOOLS.contains(&tool) {
        summary["_meta"] = serde_json::json!({"ellmsha_exact_tool":{"server":"ellmsha-secrets","tool":tool}});
    }
    summary
}
#[derive(Default)]
pub(crate) struct SensitiveProjection {
    calls: BTreeMap<String, String>,
    children: BTreeMap<String, Box<SensitiveProjection>>,
    failed: bool,
}
impl SensitiveProjection {
    fn tool(&mut self, id: &str, name: &str, input: Option<&serde_json::Value>) -> Option<String> {
        if self.failed {
            return operation(name, input)
                .or_else(|| self.calls.get(id).cloned())
                .or_else(|| Some("sensitive-operation".into()));
        }
        if let Some(tool) = operation(name, input) {
            if self.calls.len() >= 8192 && !self.calls.contains_key(id) {
                self.failed = true;
                return Some(tool);
            }
            self.calls.insert(id.into(), tool);
        }
        self.calls.get(id).cloned()
    }
    pub(crate) fn project(&mut self, event: &mut AgentStreamEvent) {
        match event {
            AgentStreamEvent::ToolCall(call) => {
                if let Some(tool) = self.tool(&call.call_id, &call.name, Some(&call.args)) {
                    call.name = tool.clone();
                    call.args = safe_input(&tool, Some(&call.args));
                    call.input = None;
                    call.output = call.output.as_ref().map(|_| "<sensitive result hidden>".into());
                    call.description = Some("Sensitive tool operation".into());
                }
            }
            AgentStreamEvent::AcpToolCall(call) => {
                if let Some(tool) = self.tool(
                    &format!("{}:{}", call.session_id, call.update.tool_call_id),
                    call.update.title.as_deref().unwrap_or(""),
                    call.update.raw_input.as_ref(),
                ) {
                    call.update.title = Some(tool.clone());
                    call.update.raw_input = Some(safe_input(&tool, call.update.raw_input.as_ref()));
                    call.update.raw_output = None;
                    call.update.content = None;
                    call.update.locations = None;
                    call.meta = None;
                }
            }
            AgentStreamEvent::AcpPermission(AcpPermissionEventData::Request(request)) => {
                if let Some(tool) = self.tool(
                    &format!("{}:{}", request.session_id, request.tool_call.tool_call_id),
                    request.tool_call.title.as_deref().unwrap_or(""),
                    request.tool_call.raw_input.as_ref(),
                ) {
                    request.tool_call.title = Some(tool.clone());
                    request.tool_call.raw_input = Some(safe_input(&tool, request.tool_call.raw_input.as_ref()));
                    request.tool_call.raw_output = None;
                    request.tool_call.content = None;
                    request.tool_call.locations = None;
                    // Grant options and correlation IDs stay intact; arbitrary provider metadata does not.
                    request.meta = None;
                    request.tool_call.meta = None;
                }
            }
            AgentStreamEvent::Permission(value) => {
                let name = value
                    .get("tool_name")
                    .or_else(|| value.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let id = value
                    .get("id")
                    .or_else(|| value.get("call_id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if let Some(tool) = self.tool(id, name, value.get("input").or_else(|| value.get("args"))) {
                    let summary = safe_input(&tool, value.get("input").or_else(|| value.get("args")));
                    if let Some(object) = value.as_object_mut() {
                        for key in [
                            "args",
                            "arguments",
                            "input",
                            "output",
                            "raw_input",
                            "raw_output",
                            "rawInput",
                            "rawOutput",
                            "description",
                        ] {
                            if object.contains_key(key) {
                                object.insert(key.into(), summary.clone());
                            }
                        }
                    }
                }
            }
            AgentStreamEvent::ToolGroup(entries) => {
                for entry in entries {
                    if let Some(tool) = self.tool(&entry.call_id, &entry.name, None) {
                        entry.name = tool;
                        entry.description = Some("Sensitive tool operation".into());
                    }
                }
            }
            AgentStreamEvent::Start(_) => {
                self.calls.clear();
                self.failed = false;
            }
            AgentStreamEvent::Subagent(child) => {
                if let Some(event) = &mut child.event {
                    if self.failed || (self.children.len() >= 4096 && !self.children.contains_key(&child.r#ref)) {
                        // No unbounded child registry: project this entire child frame
                        // without attribution, keeping approval correlation/options intact.
                        let mut conservative = SensitiveProjection {
                            failed: true,
                            ..Default::default()
                        };
                        conservative.project(event);
                    } else {
                        let projection = self.children.entry(child.r#ref.clone()).or_default();
                        projection.project(event);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Project synchronously BEFORE broadcasting. Subscriber lag can drop display frames,
/// but can never drop identity before projecting later deltas. Clones share attribution.
#[derive(Clone)]
pub(crate) struct EventSender {
    tx: broadcast::Sender<AgentStreamEvent>,
    projection: std::sync::Arc<std::sync::Mutex<Option<SensitiveProjection>>>,
}
impl From<broadcast::Sender<AgentStreamEvent>> for EventSender {
    fn from(tx: broadcast::Sender<AgentStreamEvent>) -> Self {
        Self {
            tx,
            projection: Default::default(),
        }
    }
}
impl EventSender {
    pub(crate) fn enable_secrets(&self) {
        self.projection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Default::default);
    }
    pub(crate) fn secrets_registered(&self) -> bool {
        self.projection.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.tx.subscribe()
    }
    pub(crate) fn send(
        &self,
        mut event: AgentStreamEvent,
    ) -> Result<usize, Box<broadcast::error::SendError<AgentStreamEvent>>> {
        // Keep send inside the lock: concurrent producers must share identity order.
        let mut projection = self.projection.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(projection) = projection.as_mut() {
            projection.project(&mut event);
        }
        self.tx.send(event).map_err(Box::new)
    }
}

#[cfg(test)]
#[path = "sensitive_tests.rs"]
mod tests;
