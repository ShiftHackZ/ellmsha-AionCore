use super::{RouterService, native};
use crate::types::MemoryRecallScope;
use aion_agent::spawner::{ChildProvider, ChildRouter};
use aionui_api_types::{RouterConfig, RouterTarget};
use std::sync::Arc;

/// Private host-bound resolver: descendants cannot choose another user or policy.
pub struct RoutedChildren {
    pub service: Arc<RouterService>,
    pub user_id: String,
    pub config: RouterConfig,
    pub scope: MemoryRecallScope,
}
#[async_trait::async_trait]
impl ChildRouter for RoutedChildren {
    async fn resolve(&self, prompt: &str) -> Result<ChildProvider, String> {
        let (decision, _) = self
            .service
            .select(
                &self.user_id,
                &self.config,
                prompt,
                &self.scope,
                true,
                true,
                false,
                &Default::default(),
            )
            .await
            .map_err(|e| e.to_string())?;
        let RouterTarget::Provider { provider_id, model } = &decision.target else {
            return Err(
                "This native child requires a provider target; external CLI delegation must use a managed Team".into(),
            );
        };
        let row = self
            .service
            .providers
            .find_by_id(&self.user_id, provider_id)
            .await
            .map_err(|_| "Could not resolve child provider")?
            .ok_or("Child provider disappeared")?;
        let (provider, config) = native::configured_provider(&row, model, &self.service.key, &self.scope.workspace)
            .map_err(|e| e.to_string())?;
        Ok(ChildProvider {
            provider,
            config,
            decision: serde_json::to_string(&decision).map_err(|_| "Invalid child decision")?,
        })
    }
}
