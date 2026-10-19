//! Bounded internal completion used by task classification. No chat capability,
//! persistence or external tool execution is created for this operation.
use super::{GenerationRequest, GenerationTarget, cli, native, types::Completion};
use crate::AgentError;
use std::time::Duration;
static CLASSIFIER_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(crate) async fn complete_stateless(
    target: GenerationTarget,
    request: &GenerationRequest,
    timeout: Duration,
    output_limit: usize,
) -> Result<Completion, AgentError> {
    request.validate()?;
    if target.model() != request.model
        || !request.tools.is_empty()
        || request.tool_choice.as_ref().and_then(|v| v.as_str()) != Some("none")
        || request
            .messages
            .iter()
            .any(|m| !m.tool_calls.is_empty() || m.role == "tool")
    {
        return Err(AgentError::bad_request(
            "Stateless classification requires a pinned model and no tools",
        ));
    }
    let completion = tokio::time::timeout(timeout, async {
        let _permit = CLASSIFIER_SLOTS
            .acquire()
            .await
            .map_err(|_| AgentError::internal("Classifier service unavailable"))?;
        match target {
            GenerationTarget::Native { provider, .. } => {
                native::complete_bounded(provider, request, output_limit, true).await
            }
            GenerationTarget::Cli {
                backend,
                config,
                spawner,
            } => cli::complete_bounded(&backend, *config, spawner, request, output_limit).await,
        }
    })
    .await
    .map_err(|_| AgentError::timeout("Router classifier timed out"))??;
    if completion.truncated || completion.content.len() > output_limit || !completion.tools.is_empty() {
        return Err(AgentError::bad_gateway("Invalid classifier output"));
    }
    Ok(completion)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aion_providers::LlmProvider;
    use aion_types::llm::{LlmEvent, LlmRequest};
    use aion_types::message::{StopReason, TokenUsage};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    struct Provider {
        requests: Mutex<Vec<LlmRequest>>,
        kind: &'static str,
    }
    #[async_trait::async_trait]
    impl LlmProvider for Provider {
        async fn stream(
            &self,
            request: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, aion_providers::ProviderError> {
            self.requests.lock().unwrap().push(request.clone());
            if self.kind == "timeout" {
                std::future::pending::<()>().await;
            }
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let event = if self.kind == "tool" {
                LlmEvent::ToolUse {
                    id: "x".into(),
                    name: "shell".into(),
                    input: json!({}),
                    extra: None,
                }
            } else {
                LlmEvent::TextDelta(if self.kind == "large" {
                    "x".repeat(5000)
                } else {
                    r#"{"class_id":null}"#.into()
                })
            };
            tx.send(event).await.unwrap();
            tx.send(LlmEvent::Done {
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
            .await
            .unwrap();
            Ok(rx)
        }
    }
    fn request() -> GenerationRequest {
        serde_json::from_value(json!({"model":"exact","messages":[{"role":"user","content":"Classify"}],"tool_choice":"none","max_tokens":128})).unwrap()
    }
    #[tokio::test]
    async fn stateless_generation_enforces_no_tools_output_bound_and_timeout() {
        for (kind, expected) in [
            ("normal", ""),
            ("tool", "attempted a tool call"),
            ("large", "byte limit"),
            ("timeout", "timed out"),
        ] {
            let provider = Arc::new(Provider {
                requests: Mutex::new(vec![]),
                kind,
            });
            let result = complete_stateless(
                GenerationTarget::Native {
                    provider: provider.clone(),
                    model: "exact".into(),
                },
                &request(),
                Duration::from_millis(20),
                4096,
            )
            .await;
            if expected.is_empty() {
                assert_eq!(result.unwrap().content, r#"{"class_id":null}"#);
            } else {
                assert!(result.err().unwrap().to_string().contains(expected));
            }
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].tools.is_empty());
            assert_eq!(requests[0].model, "exact");
            assert_eq!(requests[0].max_tokens, Some(128));
        }
    }
    #[tokio::test]
    async fn stateless_generation_rejects_model_drift_and_tool_context_before_inference() {
        let provider = Arc::new(Provider {
            requests: Mutex::new(vec![]),
            kind: "normal",
        });
        let target = GenerationTarget::Native {
            provider: provider.clone(),
            model: "other".into(),
        };
        assert!(matches!(
            complete_stateless(target, &request(), Duration::from_millis(20), 4096).await,
            Err(AgentError::BadRequest(_))
        ));
        let mut tools = request();
        tools.messages[0].role = "tool".into();
        tools.messages[0].tool_call_id = Some("bad".into());
        let target = GenerationTarget::Native {
            provider: provider.clone(),
            model: "exact".into(),
        };
        assert!(matches!(
            complete_stateless(target, &tools, Duration::from_millis(20), 4096).await,
            Err(AgentError::BadRequest(_))
        ));
        assert!(provider.requests.lock().unwrap().is_empty());
    }
}
