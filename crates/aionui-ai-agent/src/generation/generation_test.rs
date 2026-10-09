use super::*;
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{ContentBlock, StopReason, TokenUsage};
use serde_json::json;
use std::sync::Mutex;

#[derive(Default)]
struct FakeProvider {
    requests: Mutex<Vec<LlmRequest>>,
}
#[async_trait::async_trait]
impl LlmProvider for FakeProvider {
    async fn stream(
        &self,
        request: &LlmRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, aion_providers::ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let (tx, rx) = tokio::sync::mpsc::channel(3);
        if request.tools.is_empty() {
            tx.send(LlmEvent::TextDelta(request.model.clone())).await.unwrap();
        } else {
            tx.send(LlmEvent::ToolUse {
                id: "call_1".into(),
                name: request.tools[0].name.clone(),
                input: json!({"path":"scene.md"}),
                extra: None,
            })
            .await
            .unwrap();
        }
        tx.send(LlmEvent::Done {
            stop_reason: if request.tools.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            },
            usage: TokenUsage {
                input_tokens: 12,
                output_tokens: 8,
                ..Default::default()
            },
        })
        .await
        .unwrap();
        Ok(rx)
    }
}
fn request(model: &str) -> GenerationRequest {
    serde_json::from_value(json!({"model":model,"messages":[{"role":"system","content":"Extract facts"},{"role":"user","content":"hello"}],"max_tokens":99})).unwrap()
}
fn service() -> GenerationService {
    GenerationService::new("http://127.0.0.1:1234".into())
}

#[tokio::test]
async fn generation_capabilities_reject_missing_unknown_expired_and_model_mismatch() {
    let service = service();
    let provider = Arc::new(FakeProvider::default());
    let ctx = service
        .issue_target(GenerationTarget::Native {
            provider: provider.clone(),
            model: "chosen".into(),
        })
        .unwrap();
    for token in ["", "unknown"] {
        assert!(matches!(
            service.complete(token, request("chosen")).await,
            Err(AgentError::Unauthorized(_))
        ));
    }
    assert!(matches!(
        service.complete(&ctx.callback_token, request("other")).await,
        Err(AgentError::BadRequest(_))
    ));
    service.routes.get_mut(&hash(&ctx.callback_token)).unwrap().expires = Instant::now() - Duration::from_secs(1);
    assert!(matches!(
        service.complete(&ctx.callback_token, request("chosen")).await,
        Err(AgentError::Unauthorized(_))
    ));
    assert!(provider.requests.lock().unwrap().is_empty());
}
#[test]
fn generation_context_capability_is_bound_to_conversation() {
    let service = service();
    let env = service.mcp_env("chat-a");
    let token = &env[1].1;
    assert!(service.validate_context_token(token, "chat-a").is_ok());
    assert!(service.validate_context_token(token, "chat-b").is_err());
    assert!(service.validate_context_token("", "chat-a").is_err());
    assert!(service.validate_context_token("unknown", "chat-a").is_err());
    assert_eq!(env[0].1, "http://127.0.0.1:1234/api/memory/generation/context");
}
#[tokio::test]
async fn generation_routes_freeze_model_and_keep_concurrent_snapshots_separate() {
    let service = service();
    let provider = Arc::new(FakeProvider::default());
    let a = service
        .issue_target(GenerationTarget::Native {
            provider: provider.clone(),
            model: "model-a".into(),
        })
        .unwrap();
    let b = service
        .issue_target(GenerationTarget::Native {
            provider: provider.clone(),
            model: "model-b".into(),
        })
        .unwrap();
    let (a, b) = tokio::join!(
        service.complete(&a.callback_token, request("model-a")),
        service.complete(&b.callback_token, request("model-b"))
    );
    assert_eq!(a.unwrap().choices[0].message.content.as_deref(), Some("model-a"));
    assert_eq!(b.unwrap().choices[0].message.content.as_deref(), Some("model-b"));
    assert!(
        provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.max_tokens == Some(99))
    );
}
#[tokio::test]
async fn generation_native_maps_function_calls_and_tool_result_roundtrip_without_execution() {
    let service = service();
    let provider = Arc::new(FakeProvider::default());
    let ctx = service
        .issue_target(GenerationTarget::Native {
            provider: provider.clone(),
            model: "native".into(),
        })
        .unwrap();
    let req=serde_json::from_value(json!({"model":"native","messages":[{"role":"system","content":"Extract facts"},{"role":"assistant","content":null,"tool_calls":[{"id":"prior","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"scene.md\"}"}}]},{"role":"tool","tool_call_id":"prior","content":"old scene"}],"tools":[{"type":"function","function":{"name":"read_file","description":"Read memory","parameters":{"type":"object"}}}],"tool_choice":"required","response_format":{"type":"json_object"}})).unwrap();
    let response = service.complete(&ctx.callback_token, req).await.unwrap();
    assert_eq!(response.choices[0].finish_reason, "tool_calls");
    assert_eq!(response.choices[0].message.tool_calls[0].function.name, "read_file");
    assert_eq!(response.usage.total_tokens, 20);
    let calls = provider.requests.lock().unwrap();
    assert!(
        matches!(&calls[0].messages[1].content[0],ContentBlock::ToolResult {tool_use_id,content,..} if tool_use_id=="prior"&&content=="old scene")
    );
    assert!(calls[0].system.contains("valid JSON object"));
}

#[test]
fn generation_json_object_rejects_arrays_and_scalars() {
    let mut request = request("native");
    request.response_format = Some(json!({"type":"json_object"}));
    for content in ["[]", "123", "\"text\""] {
        let result = super::super::types::Completion {
            content: content.into(),
            tools: vec![],
            input_tokens: 0,
            output_tokens: 0,
            truncated: false,
        };
        assert!(result.validate(&request).is_err());
    }
}
