//! Validate frozen V2 execution identity before cached reuse or factory side effects.
use crate::{AgentError, session_context::AgentSessionKind, types::BuildTaskOptions};
use aionui_api_types::RouterTarget;
use aionui_common::AgentType;

pub(crate) const PIN_ERROR: &str = "Router execution target changed; start a new chat to change the frozen route";

pub(crate) fn frozen_target(options: &BuildTaskOptions) -> Result<Option<&RouterTarget>, AgentError> {
    let Some(snapshot) = options.routing.as_ref().filter(|s| s.config.schema_version == 2) else {
        return Ok(None);
    };
    let context = &options.context;
    let team_bound = context.team.is_some()
        || match &context.kind {
            AgentSessionKind::Aionrs(c) => c.belongs_to_team || c.team.is_some(),
            AgentSessionKind::Acp(c) => c.belongs_to_team || c.team.is_some(),
            AgentSessionKind::Antigravity(c) => c.belongs_to_team || c.team.is_some(),
        };
    if team_bound {
        return Ok(None);
    }
    let target = &snapshot.decision.target;
    let valid = match (target, &context.kind) {
        (RouterTarget::Provider { provider_id, model }, AgentSessionKind::Aionrs(_)) => {
            context.agent_type() == AgentType::Aionrs
                && context.model.provider_id == *provider_id
                && context
                    .model
                    .use_model
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .unwrap_or(&context.model.model)
                    == model
        }
        (RouterTarget::Agent { agent_id, model }, AgentSessionKind::Acp(c)) => {
            context.agent_type() == AgentType::Acp
                && c.config.agent_id.as_deref() == Some(agent_id)
                && c.config.current_model_id.as_deref() == Some(model)
                && c.session_snapshot
                    .as_ref()
                    .and_then(|s| s.current_model_id.as_ref())
                    .is_none_or(|current| current.as_str() == model)
        }
        (RouterTarget::Agent { agent_id, model }, AgentSessionKind::Antigravity(c)) => {
            context.agent_type() == AgentType::Antigravity
                && c.config.agent_id.as_deref() == Some(agent_id)
                && c.config.current_model_id.as_deref() == Some(model)
                && c.session_snapshot
                    .as_ref()
                    .and_then(|s| s.current_model_id.as_ref())
                    .is_none_or(|current| current.as_str() == model)
        }
        _ => false,
    };
    if !valid {
        return Err(AgentError::bad_request(PIN_ERROR));
    }
    Ok(Some(target))
}

pub(crate) fn validate_agent_identity(target: Option<&RouterTarget>, actual_id: &str) -> Result<(), AgentError> {
    if let Some(RouterTarget::Agent { agent_id, .. }) = target
        && agent_id != actual_id
    {
        return Err(AgentError::bad_request(PIN_ERROR));
    }
    Ok(())
}
