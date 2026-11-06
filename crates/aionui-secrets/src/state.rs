use crate::{
    history::SecretHistoryService,
    service::{SecretsError, SecretsService},
};
use aionui_api_types::secrets::SecretScope;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait SecretsAccess: Send + Sync {
    /// Validates current ownership and effective plugin state, not a cached launch snapshot.
    async fn groups(
        &self,
        owner: &str,
        conversation: Option<&str>,
    ) -> Result<Vec<aionui_api_types::secrets::SecretGroup>, SecretsError>;
    async fn authorize(&self, owner: &str, scope: &SecretScope) -> Result<(), SecretsError>;
    async fn authorize_chat(
        &self,
        owner: &str,
        conversation: &str,
        expected_project: Option<&str>,
        scope: &SecretScope,
    ) -> Result<(), SecretsError>;
    /// Non-database enablement check, safe while the history writer transaction is held.
    async fn authorize_binding(&self, owner: &str, project: Option<&str>) -> Result<(), SecretsError>;
}

#[derive(Clone)]
pub struct SecretsRouterState {
    pub service: Arc<SecretsService>,
    pub history: Arc<SecretHistoryService>,
    pub access: Arc<dyn SecretsAccess>,
}

/// Installed by the composition boundary only after human transport authentication.
#[derive(Clone)]
pub struct HumanSecretsAccess;

/// Present only for the launch capability authenticated desktop main process.
#[derive(Clone)]
pub struct DesktopSecretsAccess;
