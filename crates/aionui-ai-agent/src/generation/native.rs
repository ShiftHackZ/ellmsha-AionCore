use std::sync::Arc;

use aion_providers::LlmProvider;
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{ContentBlock, Message, Role, StopReason};
use aion_types::tool::ToolDef;

use super::types::*;
use crate::AgentError;

pub(super) async fn complete(
    provider: Arc<dyn LlmProvider>,
    request: &GenerationRequest,
) -> Result<Completion, AgentError> {
    let mut messages = Vec::new();
    let mut system = String::new();
    for message in &request.messages {
        if matches!(message.role.as_str(), "system" | "developer") {
            system.push_str(message.content.as_deref().unwrap_or_default());
            system.push('\n');
            continue;
        }
        let mut content = Vec::new();
        if message.role == "tool" {
            content.push(ContentBlock::ToolResult {
                tool_use_id: message.tool_call_id.clone().unwrap_or_default(),
                content: message.content.clone().unwrap_or_default(),
                is_error: false,
            });
        } else if let Some(text) = &message.content {
            content.push(ContentBlock::Text { text: text.clone() });
        }
        for call in &message.tool_calls {
            content.push(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input: serde_json::from_str(&call.function.arguments)
                    .map_err(|_| AgentError::bad_request("Tool call arguments must be JSON"))?,
                extra: None,
            });
        }
        messages.push(Message::new(
            if message.role == "assistant" {
                Role::Assistant
            } else {
                Role::User
            },
            content,
        ));
    }
    if request.tool_choice.as_ref().and_then(|v| v.as_str()) == Some("required") {
        system.push_str("\nReturn at least one tool call from the provided functions.");
    }
    if let Some(format) = &request.response_format {
        match format.get("type").and_then(serde_json::Value::as_str) {
            Some("json_object") => system.push_str("\nReturn only a valid JSON object, without code fences."),
            Some("json_schema") => {
                system.push_str("\nReturn only valid JSON matching this schema: ");
                system.push_str(
                    &format
                        .get("json_schema")
                        .and_then(|v| v.get("schema"))
                        .unwrap_or(&serde_json::Value::Null)
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    let tools = if request.tool_choice.as_ref().and_then(|v| v.as_str()) == Some("none") {
        vec![]
    } else {
        request
            .tools
            .iter()
            .map(|t| ToolDef {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                input_schema: t.function.parameters.clone(),
                deferred: false,
            })
            .collect()
    };
    let mut stream = provider
        .stream(&LlmRequest {
            model: request.model.clone(),
            system,
            messages,
            tools,
            max_tokens: request.max_tokens(),
            thinking: None,
            reasoning_effort: None,
        })
        .await
        .map_err(|_| AgentError::bad_gateway("Selected provider could not start memory generation"))?;
    let mut result = Completion {
        content: String::new(),
        tools: vec![],
        input_tokens: 0,
        output_tokens: 0,
        truncated: false,
    };
    while let Some(event) = stream.recv().await {
        match event {
            LlmEvent::TextDelta(text) => result.content.push_str(&text),
            LlmEvent::ToolUse { id, name, input, .. } => result.tools.push(GenerationToolCall {
                id,
                r#type: "function".into(),
                function: GenerationFunctionCall {
                    name,
                    arguments: input.to_string(),
                },
            }),
            LlmEvent::Done { stop_reason, usage } => {
                result.input_tokens = usage.input_tokens;
                result.output_tokens = usage.output_tokens;
                result.truncated = stop_reason == StopReason::MaxTokens;
                return Ok(result);
            }
            LlmEvent::Error(_) => {
                return Err(AgentError::bad_gateway(
                    "Selected provider failed memory generation; no alternate model was used",
                ));
            }
            _ => {}
        }
    }
    Err(AgentError::bad_gateway(
        "Selected provider ended before completing memory generation",
    ))
}
