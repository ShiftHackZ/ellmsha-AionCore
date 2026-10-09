use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AgentError;

/// Immutable OpenAI-compatible callback route captured for one memory job.
#[derive(Serialize)]
pub struct GenerationContext {
    /// Server-configured, Docker-reachable OpenAI API base URL.
    pub base_url: String,
    /// Exact primary-agent model when the job was submitted.
    pub model: String,
    /// Opaque expiring capability; never a provider credential.
    pub callback_token: String,
}

/// Supported non-streaming Chat Completions request used by TencentDB.
#[derive(Deserialize)]
pub struct GenerationRequest {
    /// Must exactly match the immutable callback route's selected model.
    pub model: String,
    /// Ordered text messages, including tool-result round trips.
    pub messages: Vec<GenerationMessage>,
    /// External TencentDB tool schemas. This bridge never executes them.
    #[serde(default)]
    pub tools: Vec<GenerationTool>,
    /// Optional provider output token budget (native) / CLI output bound.
    pub max_tokens: Option<u32>,
    /// Alternative OpenAI spelling of the output token budget.
    pub max_completion_tokens: Option<u32>,
    /// Streaming is not supported; must be absent or false.
    #[serde(default)]
    pub stream: bool,
    /// Tool selection constraint: auto, none or required.
    pub tool_choice: Option<Value>,
    /// Accepted for Tencent compatibility; sampling uses the selected backend defaults.
    pub temperature: Option<f64>,
    /// Optional OpenAI JSON output request.
    pub response_format: Option<Value>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct GenerationMessage {
    /// system, developer, user, assistant, or tool.
    pub role: String,
    /// Text payload. Null is allowed on assistant tool-call messages.
    #[serde(default)]
    pub content: Option<String>,
    /// Assistant tool invocations, executed by TencentDB outside this bridge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<GenerationToolCall>,
    /// Matching invocation identifier for tool-role messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct GenerationTool {
    /// Only function tools are supported.
    pub r#type: String,
    /// Function definition and JSON Schema input parameters.
    pub function: GenerationFunction,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct GenerationFunction {
    /// External function name.
    pub name: String,
    /// Function purpose.
    #[serde(default)]
    pub description: String,
    /// JSON Schema describing its arguments.
    pub parameters: Value,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct GenerationToolCall {
    /// Invocation identifier used by subsequent tool-role messages.
    pub id: String,
    /// Always function.
    pub r#type: String,
    /// Function name and serialized JSON arguments.
    pub function: GenerationFunctionCall,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct GenerationFunctionCall {
    /// Name from the request's external function schemas.
    pub name: String,
    /// JSON-encoded function arguments, passed back to TencentDB.
    pub arguments: String,
}

impl GenerationRequest {
    pub(crate) fn max_tokens(&self) -> Option<u32> {
        self.max_completion_tokens.or(self.max_tokens)
    }
    pub(crate) fn validate(&self) -> Result<(), AgentError> {
        if self.stream {
            return Err(AgentError::bad_request(
                "Memory generation supports non-streaming responses only",
            ));
        }
        if self.messages.is_empty() || self.messages.len() > 200 {
            return Err(AgentError::bad_request("Memory generation requires 1..200 messages"));
        }
        if self.max_tokens().is_some_and(|n| n == 0 || n > 65536) {
            return Err(AgentError::bad_request("max_tokens must be within 1..65536"));
        }
        if self.tools.len() > 32 || self.tools.iter().any(|t| t.r#type != "function") {
            return Err(AgentError::bad_request("At most 32 function tools are supported"));
        }
        if self.messages.iter().any(|m| {
            !matches!(m.role.as_str(), "system" | "developer" | "user" | "assistant" | "tool")
                || (m.role == "tool" && m.tool_call_id.is_none())
        }) {
            return Err(AgentError::bad_request(
                "Invalid memory generation message role or tool_call_id",
            ));
        }
        if self
            .tool_choice
            .as_ref()
            .is_some_and(|c| !matches!(c.as_str(), Some("auto" | "none" | "required")))
        {
            return Err(AgentError::bad_request(
                "Only auto, none and required tool_choice are supported",
            ));
        }
        if self.response_format.as_ref().is_some_and(|v| {
            !matches!(
                v.get("type").and_then(Value::as_str),
                Some("text" | "json_object" | "json_schema")
            )
        }) {
            return Err(AgentError::bad_request("Unsupported response_format"));
        }
        Ok(())
    }
}

pub(crate) struct Completion {
    pub content: String,
    pub tools: Vec<GenerationToolCall>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub truncated: bool,
}

/// Standard non-streaming OpenAI completion. Provider credentials are absent.
#[derive(Serialize)]
pub struct GenerationResponse {
    /// Unique OpenAI-compatible completion identifier.
    pub id: String,
    /// Always chat.completion.
    pub object: &'static str,
    /// Creation timestamp in Unix seconds.
    pub created: u64,
    /// Exact selected model captured by the immutable route.
    pub model: String,
    /// One assistant choice; multiple candidates are unsupported.
    pub choices: Vec<GenerationChoice>,
    /// Usage reported by the selected provider or CLI.
    pub usage: GenerationUsage,
}
#[derive(Serialize)]
pub struct GenerationChoice {
    /// Always zero for the single supported choice.
    pub index: u32,
    /// Assistant text or external function calls.
    pub message: GenerationMessage,
    /// stop, tool_calls, or length.
    pub finish_reason: &'static str,
}
#[derive(Serialize)]
pub struct GenerationUsage {
    /// Provider-reported input tokens; zero if unavailable.
    pub prompt_tokens: u64,
    /// Provider-reported output tokens; zero if unavailable.
    pub completion_tokens: u64,
    /// Sum of input and output tokens.
    pub total_tokens: u64,
}
impl GenerationResponse {
    pub(crate) fn new(model: String, result: Completion) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion",
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            model,
            choices: vec![GenerationChoice {
                index: 0,
                finish_reason: if result.truncated {
                    "length"
                } else if result.tools.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                },
                message: GenerationMessage {
                    role: "assistant".into(),
                    content: Some(result.content),
                    tool_calls: result.tools,
                    tool_call_id: None,
                },
            }],
            usage: GenerationUsage {
                prompt_tokens: result.input_tokens,
                completion_tokens: result.output_tokens,
                total_tokens: result.input_tokens + result.output_tokens,
            },
        }
    }
}

impl Completion {
    pub(crate) fn validate(&self, request: &GenerationRequest) -> Result<(), AgentError> {
        let choice = request.tool_choice.as_ref().and_then(Value::as_str).unwrap_or("auto");
        if (choice == "none" && !self.tools.is_empty()) || (choice == "required" && self.tools.is_empty()) {
            return Err(AgentError::bad_gateway(
                "Selected model violated requested tool selection",
            ));
        }
        for call in &self.tools {
            if !request.tools.iter().any(|t| t.function.name == call.function.name)
                || serde_json::from_str::<Value>(&call.function.arguments).is_err()
            {
                return Err(AgentError::bad_gateway(
                    "Selected model returned an unknown function or invalid JSON arguments",
                ));
            }
        }
        if self.tools.is_empty() {
            let format = request
                .response_format
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str);
            if matches!(format, Some("json_object" | "json_schema")) {
                let output: Value = serde_json::from_str(&self.content)
                    .map_err(|_| AgentError::bad_gateway("Selected model did not return requested JSON output"))?;
                if format == Some("json_object") && !output.is_object() {
                    return Err(AgentError::bad_gateway(
                        "Selected model returned JSON that is not an object",
                    ));
                }
            }
        }
        Ok(())
    }
}
