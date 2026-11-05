//! Request-scoped generation for TencentDB. Routes freeze the current model;
//! generation bypasses chat persistence, memory recall and the agent tool loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aion_providers::LlmProvider;
use aionui_process::Spawner;
use aionui_session::SessionConfig;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::{GenerationContext, GenerationRequest, GenerationResponse, cli, native};
use crate::session_context::AgentSessionKind;
use crate::types::BuildTaskOptions;
use crate::{AgentError, AgentInstance, IWorkerTaskManager};

#[derive(Clone)]
pub(crate) enum GenerationTarget {
    Native {
        provider: Arc<dyn LlmProvider>,
        model: String,
    },
    Cli {
        backend: String,
        config: Box<SessionConfig>,
        spawner: Arc<dyn Spawner>,
    },
}

impl GenerationTarget {
    pub(crate) fn model(&self) -> &str {
        match self {
            Self::Native { model, .. } => model,
            Self::Cli { config, .. } => config.model.as_deref().unwrap_or(""),
        }
    }
}

/// A private MCP capability belongs to a single worker slot. Only its digest is retained.
#[derive(Debug, Clone)]
pub struct MemoryCapability {
    token_hash: String,
    pub user: String,
    project_id: Option<String>,
    stored_workspace: String,
    conversation: String,
    provider: Option<String>,
}

impl MemoryCapability {
    /// Unregistered chats can derive project aliases from their persisted workspace.
    pub fn matches_binding(&self, user: &str, project_id: Option<&str>, stored_workspace: &str) -> bool {
        self.user == user
            && self.project_id.as_deref() == project_id
            && (project_id.is_some() || self.stored_workspace == stored_workspace)
    }

    pub(crate) fn matches(&self, token: &str, conversation_id: &str) -> bool {
        self.conversation == conversation_id && self.token_hash == hash(token)
    }
}

struct Route {
    owner: Option<MemoryCapability>,
    target: GenerationTarget,
    expires: Instant,
}

/// Private capabilities are never included in tool arguments or model prompts.
/// A callback route remains frozen for 24 hours, including queued extraction jobs.
pub struct GenerationService {
    usage_bus: Option<Arc<dyn aionui_realtime::EventBroadcaster>>,
    routes: DashMap<String, Route>,
    slots: Semaphore,
    base_url: String,
}

impl GenerationService {
    pub fn new(base_url: String) -> Self {
        Self {
            usage_bus: None,
            routes: DashMap::new(),
            slots: Semaphore::new(4),
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    /// Attribute auxiliary provider measurements through the private usage channel.
    pub fn with_usage_broadcaster(mut self, bus: Arc<dyn aionui_realtime::EventBroadcaster>) -> Self {
        self.usage_bus = Some(bus);
        self
    }

    /// Mint a conversation-bound capability for the private memory MCP.
    pub fn mcp_env(&self, options: &BuildTaskOptions) -> Result<[(String, String); 3], AgentError> {
        let context = &options.context;
        let conversation_id = context.conversation_id();
        let token = uuid::Uuid::new_v4().to_string();
        let provider = match &context.kind {
            AgentSessionKind::Aionrs(_) => Some(context.model.provider_id.clone()),
            AgentSessionKind::Acp(c) => c.config.agent_id.clone(),
            AgentSessionKind::Antigravity(c) => c.config.agent_id.clone(),
        };
        options
            .memory_capability
            .set(MemoryCapability {
                token_hash: hash(&token),
                user: context.conversation.user_id.clone(),
                conversation: conversation_id.into(),
                project_id: options.memory_recall.project_id.clone(),
                stored_workspace: context.workspace.stored_path.clone(),
                provider,
            })
            .map_err(|_| AgentError::internal("Memory capability already issued for this runtime"))?;
        Ok([
            (
                "AIONUI_GENERATION_CONTEXT_URL".into(),
                format!("{}/api/memory/generation/context", self.base_url),
            ),
            ("AIONUI_GENERATION_TOKEN".into(), token),
            ("AIONUI_CONVERSATION_ID".into(), conversation_id.to_owned()),
        ])
    }

    pub async fn issue_context(
        &self,
        token: &str,
        conversation_id: &str,
        tasks: &dyn IWorkerTaskManager,
    ) -> Result<GenerationContext, AgentError> {
        let (owner, task) = tasks
            .memory_runtime(conversation_id, token)
            .ok_or_else(|| AgentError::unauthorized("Invalid memory generation context capability"))?;
        let target = match task {
            AgentInstance::Aionrs(manager) => manager.generation_target(),
            AgentInstance::Session(manager) => manager.generation_target()?,
            _ => {
                return Err(AgentError::bad_request(
                    "Memory generation currently requires Codex, Claude or a native provider",
                ));
            }
        };
        let context = self.issue_target(target)?;
        if let Some(mut route) = self.routes.get_mut(&hash(&context.callback_token)) {
            route.owner = Some(owner);
        }
        Ok(context)
    }

    fn issue_target(&self, target: GenerationTarget) -> Result<GenerationContext, AgentError> {
        if target.model().is_empty() {
            return Err(AgentError::conflict(
                "Primary agent has not resolved its selected model yet",
            ));
        }
        self.routes.retain(|_, route| route.expires > Instant::now());
        if self.routes.len() >= 4096 {
            return Err(AgentError::RateLimited);
        }
        let token = uuid::Uuid::new_v4().to_string();
        let model = target.model().to_owned();
        self.routes.insert(
            hash(&token),
            Route {
                owner: None,
                target,
                expires: Instant::now() + Duration::from_secs(86400),
            },
        );
        // Docker reaches the host's existing listener. Deployments can override
        // this origin (e.g. http://aionui-core:25808); no request may select it.
        let public = std::env::var("AIONUI_GENERATION_CALLBACK_ORIGIN").unwrap_or_else(|_| {
            if cfg!(target_os = "macos") && std::env::var("AIONUI_MEMORY_NATIVE").as_deref() != Ok("1") {
                self.base_url.replace("127.0.0.1", "host.docker.internal")
            } else {
                self.base_url.clone()
            }
        });
        Ok(GenerationContext {
            base_url: format!("{}/api/runtime/tencent/v1", public.trim_end_matches('/')),
            model,
            callback_token: token,
        })
    }

    pub async fn complete(&self, token: &str, request: GenerationRequest) -> Result<GenerationResponse, AgentError> {
        request.validate()?;
        let (target, owner) = {
            let route = self.routes.get(&hash(token)).ok_or_else(|| {
                AgentError::unauthorized("Unknown or expired memory generation route; resubmit the memory operation")
            })?;
            if route.expires <= Instant::now() {
                return Err(AgentError::unauthorized(
                    "Memory generation route expired; resubmit the memory operation",
                ));
            }
            (route.target.clone(), route.owner.clone())
        };
        if request.model != target.model() {
            return Err(AgentError::bad_request(
                "Requested model differs from the captured primary-agent model",
            ));
        }
        let _permit = self.slots.try_acquire().map_err(|_| AgentError::RateLimited)?;
        tracing::info!(model = %request.model, "TencentDB generation through primary-agent snapshot");
        let result = tokio::time::timeout(Duration::from_secs(240), async {
            match target {
                GenerationTarget::Native { provider, .. } => native::complete(provider, &request).await,
                GenerationTarget::Cli {
                    backend,
                    config,
                    spawner,
                } => cli::complete(&backend, *config, spawner, &request).await,
            }
        })
        .await
        .map_err(|_| {
            AgentError::timeout("Selected agent memory generation timed out; no alternate model was used")
        })??;
        if let (Some(bus), Some(owner)) = (&self.usage_bus, owner)
            && (result.input_tokens > 0 || result.output_tokens > 0)
        {
            bus.broadcast(aionui_api_types::WebSocketMessage::new("usage.request",serde_json::json!({
                "user_id":owner.user,"conversation_id":owner.conversation,"data":{
                    "request_id":uuid::Uuid::new_v4().to_string(),"provider_id":owner.provider,
                    "model":request.model,"input_tokens":result.input_tokens,"output_tokens":result.output_tokens,
                    "cache_read_tokens":result.cache_read_tokens,"cache_creation_tokens":result.cache_creation_tokens,
                    "context_tokens":0,"context_window":0,"context_reported":false,"breakdown":{}
                }
            })));
        }
        result.validate(&request)?;
        Ok(GenerationResponse::new(request.model, result))
    }
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
#[path = "generation_test.rs"]
mod tests;
