use super::*;
use serde_json::json;
fn event(value: serde_json::Value) -> AgentStreamEvent {
    serde_json::from_value(value).unwrap()
}
#[test]
fn codex_registered_identity_redacts_start_and_later_anonymous_result() {
    let mut projection = SensitiveProjection::default();
    let mut start = event(
        json!({"type":"tool_call","data":{"call_id":"s1","name":"mcpToolCall","args":{"type":"mcpToolCall","server":"ellmsha-secrets","tool":"ellmsha_global_secret_write","arguments":{"password":"SYNTHETIC_CANARY"}},"status":"running"}}),
    );
    projection.project(&mut start);
    let display = serde_json::to_string(&start).unwrap();
    assert!(!display.contains("SYNTHETIC_CANARY"));
    assert!(display.contains("ellmsha_global_secret_write"));
    let mut result = event(
        json!({"type":"tool_call","data":{"call_id":"s1","name":"","args":null,"status":"completed","output":"SYNTHETIC_CANARY"}}),
    );
    projection.project(&mut result);
    assert!(!serde_json::to_string(&result).unwrap().contains("SYNTHETIC_CANARY"));
}
#[test]
fn unrelated_tools_mentions_and_unattributed_empty_title_are_unchanged() {
    let mut projection = SensitiveProjection::default();
    for name in ["read_file", ""] {
        let value = json!({"type":"tool_call","data":{"call_id":"ordinary","name":name,"args":{"server":"ellmsha-secrets","tool":"ellmsha_global_secret_read","text":"mention ellmsha-secrets"},"status":"completed","output":"ordinary result"}});
        let mut frame = event(value);
        let before = serde_json::to_value(&frame).unwrap();
        projection.project(&mut frame);
        assert_eq!(serde_json::to_value(frame).unwrap(), before);
    }
}
#[test]
fn permission_projection_preserves_original_tool_identity_options_and_correlation() {
    let mut projection = SensitiveProjection::default();
    let mut request = event(
        json!({"type":"acp_permission","data":{"session_id":"session","tool_call":{"tool_call_id":"approval-123","title":"ellmsha-secrets · ellmsha_project_secret_write","raw_input":{"_meta":{"ellmsha_exact_tool":{"server":"ellmsha-secrets","tool":"ellmsha_project_secret_write"},"tool_params":{"password":"SYNTHETIC_CANARY"}},"description":"SYNTHETIC_CANARY"}},"options":[{"option_id":"once","name":"Allow once","kind":"allow_once"},{"option_id":"session","name":"Allow session","kind":"allow_always"},{"option_id":"always","name":"Always allow","kind":"allow_always"},{"option_id":"deny","name":"Reject","kind":"reject_once"}]}}),
    );
    projection.project(&mut request);
    let value = serde_json::to_value(request).unwrap();
    assert!(!value.to_string().contains("SYNTHETIC_CANARY"));
    assert_eq!(value["data"]["tool_call"]["tool_call_id"], "approval-123");
    assert_eq!(value["data"]["options"].as_array().unwrap().len(), 4);
    assert_eq!(
        value["data"]["tool_call"]["raw_input"]["_meta"]["ellmsha_exact_tool"]["tool"],
        "ellmsha_project_secret_write"
    );
}
#[test]
fn subagent_ids_are_isolated_and_new_turn_drops_old_attribution() {
    let mut projection = SensitiveProjection::default();
    let sensitive = json!({"type":"tool_call","data":{"call_id":"shared-id","name":"mcp__ellmsha-secrets__ellmsha_global_secret_read","args":{},"status":"running"}});
    let mut child = event(json!({"type":"subagent","data":{"ref":"child-a","event":sensitive}}));
    projection.project(&mut child);
    let ordinary = json!({"type":"tool_call","data":{"call_id":"shared-id","name":"","args":null,"output":"ordinary result","status":"completed"}});
    let mut other = event(json!({"type":"subagent","data":{"ref":"child-b","event":ordinary}}));
    let before = serde_json::to_value(&other).unwrap();
    projection.project(&mut other);
    assert_eq!(before, serde_json::to_value(other).unwrap());
    let mut start = event(sensitive);
    projection.project(&mut start);
    projection.project(&mut AgentStreamEvent::Start(Default::default()));
    let mut reused = event(ordinary);
    let before = serde_json::to_value(&reused).unwrap();
    projection.project(&mut reused);
    assert_eq!(before, serde_json::to_value(reused).unwrap());
}
#[test]
fn acp_sessions_do_not_share_call_identity() {
    let mut projection = SensitiveProjection::default();
    let mut first = event(
        json!({"type":"acp_tool_call","data":{"session_id":"a","update":{"sessionUpdate":"tool_call","tool_call_id":"same","title":"mcp__ellmsha-secrets__ellmsha_global_secret_read","rawInput":{"id":"fixture"}}}}),
    );
    projection.project(&mut first);
    let mut other = event(
        json!({"type":"acp_tool_call","data":{"session_id":"b","update":{"sessionUpdate":"tool_call_update","tool_call_id":"same","rawOutput":"ordinary"}}}),
    );
    let before = serde_json::to_value(&other).unwrap();
    projection.project(&mut other);
    assert_eq!(before, serde_json::to_value(other).unwrap());
    let mut same = event(
        json!({"type":"acp_tool_call","data":{"session_id":"a","update":{"sessionUpdate":"tool_call_update","tool_call_id":"same","rawOutput":"SYNTHETIC_CANARY"}}}),
    );
    projection.project(&mut same);
    assert!(!serde_json::to_string(&same).unwrap().contains("SYNTHETIC_CANARY"));
}

#[test]
fn sensitive_summary_keeps_only_structural_target_metadata() {
    let input = serde_json::json!({"id":"019a0000-0000-7000-8000-000000000001","expected_revision":3,"candidate_ids":["private handle", "second"],"title":"SYNTHETIC_RAW","fields":{"password":"SYNTHETIC_RAW"},"query":"SYNTHETIC_RAW"});
    let summary = super::sensitive_summary(&input);
    assert_eq!(summary["record_id"], input["id"]);
    assert_eq!(summary["candidate_count"], 2);
    assert_eq!(summary["expected_revision"], 3);
    assert!(!summary.to_string().contains("SYNTHETIC_RAW"));
    assert!(!summary.to_string().contains("private handle"));
    assert!(
        super::sensitive_summary(&serde_json::json!({"id":"SYNTHETIC_RAW"}))
            .get("record_id")
            .is_none()
    );
}

#[tokio::test]
async fn projection_precedes_broadcast_lag_and_preserves_terminal_and_next_turn() {
    let (raw, _) = broadcast::channel(2);
    let sender = EventSender::from(raw);
    sender.enable_secrets();
    let mut receiver = sender.subscribe();
    let sensitive = json!({"type":"tool_call","data":{"call_id":"secret","name":"mcp__ellmsha-secrets__ellmsha_global_secret_read","args":{},"status":"running"}});
    sender.send(event(sensitive.clone())).unwrap();
    for _ in 0..5 {
        sender
            .send(AgentStreamEvent::Text(crate::protocol::events::TextEventData {
                content: "ordinary".into(),
            }))
            .unwrap();
    }
    sender.send(event(json!({"type":"tool_call","data":{"call_id":"secret","name":"","args":null,"output":"SYNTHETIC_RAW","status":"completed"}}))).unwrap();
    assert!(matches!(
        receiver.recv().await,
        Err(broadcast::error::RecvError::Lagged(_))
    ));
    while let Ok(frame) = receiver.try_recv() {
        assert!(!serde_json::to_string(&frame).unwrap().contains("SYNTHETIC_RAW"));
    }
    sender.send(AgentStreamEvent::Finish(Default::default())).unwrap();
    assert!(matches!(receiver.recv().await.unwrap(), AgentStreamEvent::Finish(_)));
    sender.send(AgentStreamEvent::Start(Default::default())).unwrap();
    assert!(matches!(receiver.recv().await.unwrap(), AgentStreamEvent::Start(_)));
    let normal = event(
        json!({"type":"tool_call","data":{"call_id":"secret","name":"read_file","args":{},"output":"ordinary-result","status":"completed"}}),
    );
    sender.send(normal.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(receiver.recv().await.unwrap()).unwrap(),
        serde_json::to_value(normal).unwrap()
    );
    // A child identity emitted before parent completion still protects its later result.
    sender
        .send(event(
            json!({"type":"subagent","data":{"ref":"child","event":sensitive}}),
        ))
        .unwrap();
    sender.send(AgentStreamEvent::Start(Default::default())).unwrap();
    let mut child_result = event(
        json!({"type":"subagent","data":{"ref":"child","event":{"type":"tool_call","data":{"call_id":"secret","name":"","args":null,"output":"SYNTHETIC_RAW","status":"completed"}}}}),
    );
    let mut projection = sender.projection.lock().unwrap();
    projection.as_mut().unwrap().project(&mut child_result);
    assert!(!serde_json::to_string(&child_result).unwrap().contains("SYNTHETIC_RAW"));
}

#[test]
fn attribution_bound_masks_unknown_tool_details_until_own_next_start() {
    let mut projection = SensitiveProjection::default();
    for i in 0..8193 {
        let mut frame = event(
            json!({"type":"tool_call","data":{"call_id":i.to_string(),"name":"mcp__ellmsha-secrets__ellmsha_global_secret_read","args":{},"status":"running"}}),
        );
        projection.project(&mut frame);
    }
    assert_eq!(projection.calls.len(), 8192);
    let mut result = event(
        json!({"type":"tool_call","data":{"call_id":"8192","name":"","args":null,"output":"SYNTHETIC_RAW","status":"completed"}}),
    );
    projection.project(&mut result);
    assert!(!serde_json::to_string(&result).unwrap().contains("SYNTHETIC_RAW"));
    projection.project(&mut AgentStreamEvent::Start(Default::default()));
    assert!(!projection.failed);
}

#[test]
fn acp_typed_boundary_drops_unknown_nested_metadata_and_projection_clears_outer() {
    let mut frame = event(
        json!({"type":"acp_tool_call","data":{"session_id":"s","_meta":{"private":"OUTER_CANARY"},"update":{"sessionUpdate":"tool_call","tool_call_id":"id","title":"mcp__ellmsha-secrets__ellmsha_global_secret_read","rawInput":{"id":"fixture"},"_meta":{"private":"NESTED_CANARY"},"meta":{"private":"NESTED_CANARY"}}}}),
    );
    let canonical = serde_json::to_string(&frame).unwrap();
    assert!(!canonical.contains("NESTED_CANARY"));
    assert!(canonical.contains("OUTER_CANARY"));
    SensitiveProjection::default().project(&mut frame);
    let mut persisted = serde_json::to_value(&frame).unwrap();
    aionui_common::normalize_keys_to_snake_case(&mut persisted);
    assert!(!persisted.to_string().contains("CANARY"));
    assert_eq!(persisted["data"]["update"]["tool_call_id"], "id");
}
