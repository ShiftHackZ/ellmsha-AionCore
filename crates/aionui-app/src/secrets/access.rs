#![allow(clippy::disallowed_types)]
use aionui_api_types::secrets::SecretScope;
use aionui_auth::middleware::VerifiedHumanSession;
use aionui_common::ApiError;
use aionui_db::IProjectStore;
use aionui_extension::ExtensionRegistry;
use aionui_secrets::{
    service::SecretsError,
    state::{HumanSecretsAccess, SecretsAccess},
};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    sync::{Arc, OnceLock},
};
use zeroize::Zeroize;

static HUMAN_CAPABILITY: OnceLock<[u8; 32]> = OnceLock::new();
static TLS_PROXY_CAPABILITY: OnceLock<[u8; 32]> = OnceLock::new();
/// Explicit deployment contract: a TLS-only reverse proxy injects this private
/// capability and the origin binds only to loopback. Forwarded headers are never proof.
pub fn initialize_tls_proxy_capability(mut capability: String) {
    if capability.len() >= 64 {
        let _ = TLS_PROXY_CAPABILITY.set(Sha256::digest(capability.as_bytes()).into());
    }
    capability.zeroize();
}
pub fn tls_proxy_configured() -> bool {
    TLS_PROXY_CAPABILITY.get().is_some()
}
fn matches_capability(supplied: Option<&str>, expected: Option<&[u8; 32]>) -> bool {
    supplied.zip(expected).is_some_and(|(supplied, expected)| {
        let actual: [u8; 32] = Sha256::digest(supplied.as_bytes()).into();
        actual.iter().zip(expected).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    })
}
fn loopback_authority(value: &str) -> bool {
    value
        .parse::<axum::http::uri::Authority>()
        .ok()
        .is_some_and(|authority| {
            let host = authority.host().trim_start_matches('[').trim_end_matches(']');
            host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        })
}
pub(super) fn web_transport_allowed(request: &Request, proxy_capability: Option<&[u8; 32]>) -> bool {
    // ConnectInfo is supplied by our TCP listener, never a client header.
    if !request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| peer.0.ip().is_loopback())
    {
        return false;
    }
    if matches_capability(
        request
            .headers()
            .get("x-ellmsha-secrets-tls-proxy")
            .and_then(|h| h.to_str().ok()),
        proxy_capability,
    ) {
        return true;
    }
    // With proxy mode configured, every browser request must traverse its TLS listener.
    if proxy_capability.is_some() {
        return false;
    }
    if request
        .headers()
        .keys()
        .any(|key| key.as_str() == "forwarded" || key.as_str().starts_with("x-forwarded-"))
    {
        return false;
    }
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .is_some_and(loopback_authority);
    let origin = request.headers().get("origin").is_none_or(|header| {
        header
            .to_str()
            .ok()
            .and_then(|s| s.parse::<axum::http::Uri>().ok())
            .and_then(|uri| uri.authority().cloned())
            .is_some_and(|authority| loopback_authority(authority.as_str()))
    });
    host && origin
}
/// Called at process entry, before the environment credential is removed and workers start.
pub fn initialize_human_capability(mut capability: String) {
    if capability.len() >= 64 {
        let _ = HUMAN_CAPABILITY.set(Sha256::digest(capability.as_bytes()).into());
    }
    // This credential is never returned through a renderer API or inherited by agents.
    capability.zeroize();
}

pub(crate) struct SecretsAccessAdapter {
    pub vault: Arc<aionui_secrets::service::SecretsService>,
    pub registry: Arc<ExtensionRegistry>,
    pub projects: Arc<dyn IProjectStore>,
    pub conversations: Arc<dyn aionui_db::IConversationRepository>,
}
#[async_trait::async_trait]
impl SecretsAccess for SecretsAccessAdapter {
    async fn groups(
        &self,
        owner: &str,
        conversation: Option<&str>,
    ) -> Result<Vec<aionui_api_types::secrets::SecretGroup>, SecretsError> {
        use aionui_api_types::secrets::SecretGroup;
        let mut groups = vec![SecretGroup {
            scope: SecretScope { project_id: None },
            name: String::new(),
            retained: false,
        }];
        if let Some(chat) = conversation {
            let row = self
                .conversations
                .get(owner, chat)
                .await?
                .ok_or(SecretsError::Forbidden)?;
            if let Some(id) = row.project_id
                && let Some(project) = self.projects.get_project(owner, &id).await?
                && project.kind == "standard"
            {
                groups.push(SecretGroup {
                    scope: SecretScope { project_id: Some(id) },
                    name: project.name,
                    retained: false,
                });
            }
        } else {
            let visible = self.projects.list_standard_projects_with_conversations(owner).await?;
            for project in self.projects.list_standard_projects(owner).await? {
                let retained = !visible.iter().any(|p| p.project_id == project.project_id);
                groups.push(SecretGroup {
                    scope: SecretScope {
                        project_id: Some(project.project_id),
                    },
                    name: project.name,
                    retained,
                });
            }
            for id in self.vault.retained_project_ids(owner).await? {
                if !groups
                    .iter()
                    .any(|group| group.scope.project_id.as_deref() == Some(&id))
                {
                    groups.push(SecretGroup {
                        scope: SecretScope {
                            project_id: Some(id.clone()),
                        },
                        name: id,
                        retained: true,
                    });
                }
            }
        }
        Ok(groups)
    }

    async fn authorize_binding(&self, owner: &str, project: Option<&str>) -> Result<(), SecretsError> {
        if self.registry.secrets_enabled_for_binding(owner, project).await {
            Ok(())
        } else {
            Err(SecretsError::PluginDisabled)
        }
    }
    async fn authorize_chat(
        &self,
        owner: &str,
        conversation: &str,
        expected_project: Option<&str>,
        scope: &SecretScope,
    ) -> Result<(), SecretsError> {
        let row = self
            .conversations
            .get(owner, conversation)
            .await?
            .ok_or(SecretsError::ContextRequired)?;
        let project = if let Some(id) = row.project_id {
            let row = self
                .projects
                .get_project(owner, &id)
                .await?
                .ok_or(SecretsError::ContextRequired)?;
            (row.kind == "standard").then_some(id)
        } else {
            None
        };
        if project.as_deref() != expected_project {
            return Err(SecretsError::ContextRequired);
        }
        if scope.project_id.is_some() && scope.project_id != project {
            return Err(SecretsError::ProjectScopeRequired);
        }
        self.authorize_binding(owner, project.as_deref()).await
    }
    async fn authorize(&self, owner: &str, scope: &SecretScope) -> Result<(), SecretsError> {
        if let Some(project) = scope.project_id.as_deref() {
            match self.projects.get_project(owner, project).await? {
                Some(row) if row.kind == "standard" => {}
                None if self
                    .vault
                    .retained_project_ids(owner)
                    .await?
                    .iter()
                    .any(|id| id == project) =>
                {
                    // This adapter belongs only to human management routes. Runtime routes
                    // independently require a currently owned Standard project binding.
                    if self.registry.secrets_enabled_for_binding(owner, None).await {
                        return Ok(());
                    }
                    return Err(SecretsError::PluginDisabled);
                }
                _ => return Err(SecretsError::Forbidden),
            }
        }
        if !self
            .registry
            .secrets_enabled_for_binding(owner, scope.project_id.as_deref())
            .await
        {
            return Err(SecretsError::PluginDisabled);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct HumanGuardState {
    pub local: bool,
}

pub(crate) async fn human_guard(
    State(state): State<HumanGuardState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let runtime = request.headers().contains_key("x-aionui-runtime-token");
    let authorized = if state.local {
        matches_capability(
            request
                .headers()
                .get("x-ellmsha-secrets-human")
                .and_then(|h| h.to_str().ok()),
            HUMAN_CAPABILITY.get(),
        )
    } else {
        web_transport_allowed(&request, TLS_PROXY_CAPABILITY.get())
            && request.extensions().get::<VerifiedHumanSession>().is_some()
    };
    if runtime || !authorized {
        return Err(ApiError::Unauthorized("HUMAN_CONTEXT_REQUIRED".into()));
    }
    request.extensions_mut().insert(HumanSecretsAccess);
    if state.local {
        request
            .extensions_mut()
            .insert(aionui_secrets::state::DesktopSecretsAccess);
    }
    Ok(next.run(request).await)
}

pub(crate) struct SecretsDisableObserver(
    pub Arc<aionui_secrets::service::SecretsService>,
    pub Arc<aionui_ai_agent::RuntimeTokenService>,
);
#[async_trait::async_trait]
impl aionui_extension::registry::ExtensionDisableObserver for SecretsDisableObserver {
    async fn disabled(&self, owner: &str, name: &str) {
        if name == "secrets" {
            self.1.invalidate_user_secrets(owner);
            self.0.lock(owner).await;
        }
    }
}
