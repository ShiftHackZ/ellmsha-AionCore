//! Host-owned grants for exactly correlated app-injected MCP calls. Provider app
//! approvals keep their original semantics; ambiguous or unknown calls prompt.
use std::collections::{HashMap, HashSet};

use aionui_common::mcp_approvals::{McpApprovalStore, mcp_binding};
use serde_json::{Value, json};

use super::{McpServerSpec, McpTransport};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Identity {
    server: String,
    tool: String,
    binding: String,
}

struct ActiveCall {
    thread: String,
    turn: String,
    server: String,
    tool: String,
    arguments: Value,
}

pub(super) struct CodexMcpApprovals {
    store: Option<McpApprovalStore>,
    bindings: HashMap<String, String>,
    active: HashMap<String, ActiveCall>,
    pending: HashMap<String, Identity>,
    session: HashSet<Identity>,
    prepared: HashMap<String, String>,
}

impl CodexMcpApprovals {
    pub(super) fn new(
        store: Option<McpApprovalStore>,
        servers: &[McpServerSpec],
        trusted: &HashMap<String, String>,
    ) -> Self {
        let mut bindings = HashMap::new();
        let mut duplicates = HashSet::new();
        for server in servers {
            let transport = match &server.transport {
                McpTransport::Stdio { command, args, env } => {
                    let env: std::collections::BTreeMap<_, _> = env.iter().cloned().collect();
                    json!({"command":command,"args":args,"env":env})
                }
                McpTransport::Http { url, headers } | McpTransport::Sse { url, headers } => {
                    let headers: std::collections::BTreeMap<_, _> = headers.iter().cloned().collect();
                    json!({"url":url,"headers":headers})
                }
            };
            if bindings
                .insert(
                    server.name.clone(),
                    trusted
                        .get(&server.name)
                        .cloned()
                        .unwrap_or_else(|| mcp_binding(&json!(["codex", server.name, transport]))),
                )
                .is_some()
            {
                duplicates.insert(server.name.clone());
            }
        }
        for duplicate in duplicates {
            bindings.remove(&duplicate);
        }
        Self {
            store,
            bindings,
            active: HashMap::new(),
            pending: HashMap::new(),
            session: HashSet::new(),
            prepared: HashMap::new(),
        }
    }

    /// Returns true only when this exact incoming approval has an existing grant.
    /// Host scope metadata is stripped first, so the provider cannot forge it.
    pub(super) fn observe(&mut self, frame: &mut Value) -> bool {
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("").to_owned();
        let params = &frame["params"];
        let thread = params["threadId"].as_str().unwrap_or("");
        let turn = params["turnId"].as_str().unwrap_or("");
        if method == "item/started" && params["item"]["type"] == "mcpToolCall" {
            let item = &params["item"];
            if let (Some(id), Some(server), Some(tool), Some(arguments)) = (
                item["id"].as_str(),
                item["server"].as_str(),
                item["tool"].as_str(),
                item.get("arguments"),
            ) && !thread.is_empty()
                && !turn.is_empty()
                && self.active.len() < 1024
            {
                self.active.insert(
                    id.into(),
                    ActiveCall {
                        thread: thread.into(),
                        turn: turn.into(),
                        server: server.into(),
                        tool: tool.into(),
                        arguments: arguments.clone(),
                    },
                );
            }
        } else if method == "item/completed" {
            if let Some(id) = params["item"]["id"].as_str() {
                self.active.remove(id);
            }
        } else if method == "turn/completed" {
            let turn = params["turn"]["id"].as_str().unwrap_or(turn);
            self.active.retain(|_, call| call.thread != thread || call.turn != turn);
        } else if method == "serverRequest/resolved"
            && let Some(id) = params.get("requestId").or_else(|| params.get("request_id"))
        {
            self.answered(&id.to_string());
        }
        if method != "mcpServer/elicitation/request" {
            return false;
        }
        if let Some(meta) = frame["params"]["_meta"].as_object_mut() {
            meta.remove("ellmsha_exact_tool");
        }
        let Some(id) = frame.get("id").filter(|id| !id.is_null()).map(Value::to_string) else {
            return false;
        };
        self.answered(&id);
        let params = &frame["params"];
        if params["_meta"]["codex_approval_kind"] != "mcp_tool_call" || self.store.is_none() {
            return false;
        }
        let Some(server) = params["serverName"].as_str() else {
            return false;
        };
        let Some(binding) = self.bindings.get(server) else {
            return false;
        };
        let Some(arguments) = params["_meta"].get("tool_params") else {
            return false;
        };
        let mut candidates = self.active.values().filter(|call| {
            call.server == server
                && params["threadId"] == call.thread
                && params["turnId"] == call.turn
                && &call.arguments == arguments
        });
        let Some(call) = candidates.next() else {
            return false;
        };
        if candidates.next().is_some() {
            return false;
        }
        let identity = Identity {
            server: server.into(),
            tool: call.tool.clone(),
            binding: binding.clone(),
        };
        let allowed = self.session.contains(&identity)
            || self.store.as_ref().is_some_and(|store| {
                store
                    .allows(&identity.server, &identity.binding, &identity.tool)
                    .unwrap_or(false)
            });
        if allowed {
            return true;
        }
        if self.pending.len() >= 1024 {
            return false;
        }
        self.pending.insert(id, identity.clone());
        frame["params"]["_meta"]["ellmsha_exact_tool"] = json!({"server":identity.server,"tool":identity.tool});
        false
    }

    /// The selection is accepted only for a pending identity captured from the
    /// actual MCP item; neither client input nor provider text can choose a grant.
    pub(super) fn answered(&mut self, raw_id: &str) {
        self.pending.remove(raw_id);
        self.prepared.remove(raw_id);
    }

    pub(super) fn approve(&mut self, raw_id: &str, selected: Option<&str>) -> Result<bool, String> {
        if !matches!(selected, Some("ellmsha_session" | "ellmsha_always")) {
            return Ok(false);
        }
        let identity = self
            .pending
            .get(raw_id)
            .ok_or("Exact MCP approval no longer available")?
            .clone();
        if self.prepared.get(raw_id).map(String::as_str) == selected {
            return Ok(true);
        }
        if selected == Some("ellmsha_always") {
            self.store
                .as_ref()
                .ok_or("MCP approval store unavailable")?
                .grant(&identity.server, &identity.binding, &identity.tool)
                .map_err(|_| "MCP permission could not be saved")?;
        } else {
            self.session.insert(identity);
        }
        self.prepared.insert(raw_id.into(), selected.unwrap_or_default().into());
        Ok(true)
    }
}

#[cfg(test)]
#[path = "codex_mcp_approval_test.rs"]
mod tests;

/// app/list returns the account's stable app IDs. Match the exact provider
/// catalog name, never a model-supplied tool title or a guessed connector ID.
pub(super) fn linear_app_page(result: &Value) -> Result<(Vec<String>, Option<String>), String> {
    let rows = result["data"]
        .as_array()
        .filter(|rows| rows.len() <= 100)
        .ok_or("Provider app catalog has an invalid page")?;
    let mut ids = Vec::new();
    for row in rows {
        let name = row["name"].as_str().ok_or("Provider app name unavailable")?;
        let id = row["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Provider app identity unavailable")?;
        if name.eq_ignore_ascii_case("linear") {
            ids.push(id.to_owned());
        }
    }
    let cursor = match result.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) if !cursor.is_empty() => Some(cursor.clone()),
        _ => return Err("Provider app catalog has an invalid cursor".into()),
    };
    Ok((ids, cursor))
}
