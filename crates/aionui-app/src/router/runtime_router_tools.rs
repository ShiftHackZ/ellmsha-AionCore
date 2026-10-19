#![allow(clippy::disallowed_types)] // HTTP boundary maps domain/auth errors to ApiError.
//! Conversation-scoped host delegation. Ordinary session authentication cannot call this route.
use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{RouterChildCall, RouterChildResponse};
use aionui_common::ApiError;
use aionui_conversation::ConversationService;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use std::sync::Arc;

#[derive(Clone)]
pub struct RuntimeRouterToolsState {
    pub conversation: ConversationService,
    pub tokens: Arc<RuntimeTokenService>,
}
pub fn runtime_router_tools_routes(state: RuntimeRouterToolsState) -> Router {
    Router::new()
        .route("/api/runtime/router-tools/call", post(call))
        .with_state(state)
}
async fn call(
    State(state): State<RuntimeRouterToolsState>,
    headers: HeaderMap,
    Json(request): Json<RouterChildCall>,
) -> Result<Json<RouterChildResponse>, ApiError> {
    let (user, conversation) = runtime_headers(&headers, &state.tokens)?;
    Ok(Json(
        state
            .conversation
            .router_child_call(user, conversation, request)
            .await?,
    ))
}
fn runtime_headers<'a>(headers: &'a HeaderMap, tokens: &RuntimeTokenService) -> Result<(&'a str, &'a str), ApiError> {
    let header = |name| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ApiError::Unauthorized("Runtime authentication required".into()))
    };
    let user = header("x-aionui-user-id")?;
    let conversation = header("x-aionui-conversation-id")?;
    let token = header("x-aionui-runtime-token")?;
    tokens
        .validate(
            Some(token),
            user,
            conversation,
            RuntimeTokenScope::RouterCall,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .map_err(|_| ApiError::Unauthorized("Invalid router runtime capability".into()))?;
    Ok((user, conversation))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn router_runtime_auth_rejects_foreign_owner_conversation_scope_and_revoked_tokens() {
        let tokens = RuntimeTokenService::new();
        let mut headers = HeaderMap::new();
        assert!(runtime_headers(&headers, &tokens).is_err());
        let issued = tokens.issue("owner", "parent", "default", [RuntimeTokenScope::RouterCall]);
        headers.insert("x-aionui-user-id", "owner".parse().unwrap());
        headers.insert("x-aionui-conversation-id", "parent".parse().unwrap());
        headers.insert("x-aionui-runtime-token", issued.token.parse().unwrap());
        assert_eq!(runtime_headers(&headers, &tokens).unwrap(), ("owner", "parent"));
        headers.insert("x-aionui-user-id", "foreign".parse().unwrap());
        assert!(runtime_headers(&headers, &tokens).is_err());
        headers.insert("x-aionui-user-id", "owner".parse().unwrap());
        headers.insert("x-aionui-conversation-id", "foreign-chat".parse().unwrap());
        assert!(runtime_headers(&headers, &tokens).is_err());
        headers.insert("x-aionui-conversation-id", "parent".parse().unwrap());
        tokens.invalidate_conversation("owner", "parent");
        assert!(runtime_headers(&headers, &tokens).is_err());
        for scope in [RuntimeTokenScope::ConversationHelper, RuntimeTokenScope::TeamCall] {
            let issued = tokens.issue("owner", "parent", "default", [scope]);
            headers.insert("x-aionui-runtime-token", issued.token.parse().unwrap());
            assert!(runtime_headers(&headers, &tokens).is_err());
        }
    }
}
