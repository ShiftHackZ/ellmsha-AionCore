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

struct Route {
    target: GenerationTarget,
    expires: Instant,
}

/// Private capabilities are never included in tool arguments or model prompts.
/// A callback route remains frozen for 24 hours, including queued extraction jobs.
pub struct GenerationService {
    context_tokens: DashMap<String, String>,
    routes: DashMap<String, Route>,
    slots: Semaphore,
    base_url: String,
}

impl GenerationService {
    pub fn new(base_url: String) -> Self {
        Self {
            context_tokens: DashMap::new(),
            routes: DashMap::new(),
            slots: Semaphore::new(4),
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    /// Mint a conversation-bound capability for the private memory MCP.
    pub fn mcp_env(&self, conversation_id: &str) -> [(String, String); 3] {
        let token = uuid::Uuid::new_v4().to_string();
        self.context_tokens.insert(hash(&token), conversation_id.to_owned());
        [
            (
                "AIONUI_GENERATION_CONTEXT_URL".into(),
                format!("{}/api/memory/generation/context", self.base_url),
            ),
            ("AIONUI_GENERATION_TOKEN".into(), token),
            ("AIONUI_CONVERSATION_ID".into(), conversation_id.to_owned()),
        ]
    }

    pub async fn issue_context(
        &self,
        token: &str,
        conversation_id: &str,
        tasks: &dyn IWorkerTaskManager,
    ) -> Result<GenerationContext, AgentError> {
        self.validate_context_token(token, conversation_id)?;
        let task = tasks.get_task(conversation_id).ok_or_else(|| {
            AgentError::conflict("Primary agent is not running; start its conversation before memory generation")
        })?;
        let target = match task {
            AgentInstance::Aionrs(manager) => manager.generation_target(),
            AgentInstance::Session(manager) => manager.generation_target()?,
            _ => {
                return Err(AgentError::bad_request(
                    "Memory generation currently requires Codex, Claude or a native provider",
                ));
            }
        };
        self.issue_target(target)
    }

    pub fn validate_context_token(&self, token: &str, conversation_id: &str) -> Result<(), AgentError> {
        if self
            .context_tokens
            .get(&hash(token))
            .is_none_or(|entry| entry.as_str() != conversation_id)
        {
            return Err(AgentError::unauthorized("Invalid memory generation context capability"));
        }
        Ok(())
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
        let target = {
            let route = self.routes.get(&hash(token)).ok_or_else(|| {
                AgentError::unauthorized("Unknown or expired memory generation route; resubmit the memory operation")
            })?;
            if route.expires <= Instant::now() {
                return Err(AgentError::unauthorized(
                    "Memory generation route expired; resubmit the memory operation",
                ));
            }
            route.target.clone()
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
