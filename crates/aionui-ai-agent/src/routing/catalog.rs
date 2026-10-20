//! One credential-free catalog for every graph role; unavailable entries stay visible.
use super::*;
use std::collections::BTreeSet;

fn eligibility(reason: Option<&str>) -> RouterEligibility {
    RouterEligibility {
        supported: reason.is_none(),
        reason: reason.map(str::to_owned),
    }
}
fn role_reason(unavailable: Option<&str>, local_only: bool, local: bool) -> Option<&str> {
    unavailable.or(if local_only && !local {
        Some("local_inference_unverified")
    } else {
        None
    })
}

impl RouterService {
    /// Catalog reads never run inference, alter health, or reveal account credentials.
    /// Role support describes the host adapters, not provider-internal subagents.
    pub async fn catalog(&self, user: &str, config: &RouterConfig) -> Result<Vec<RouterCatalogEntry>, AgentError> {
        // A graph can be incomplete while its picker is open. Bound references/hints
        // without requiring a root, child or classifier to have been chosen yet.
        if config.targets().count() > 420
            || config.hints.len() > 128
            || config.classes.len() > 32
            || config
                .targets()
                .any(|target| target.id().len() > 256 || target.model().len() > 256)
        {
            return Err(AgentError::bad_request("Router catalog request exceeds its bounds"));
        }
        // Permit an incomplete graph while editing, but validate numeric hints and
        // reference syntax. Matching descriptions are not used by this endpoint.
        let mut validation = config.clone();
        validation.mode = RouterMode::Auto;
        validation.classes.clear();
        validation.schema_version = 2;
        validation.validate().map_err(AgentError::bad_request)?;
        let mut entries = Vec::new();
        for row in self
            .providers
            .list(user)
            .await
            .map_err(|_| AgentError::internal("Could not read router provider catalog"))?
        {
            let models: Vec<String> = serde_json::from_str(&row.models).unwrap_or_default();
            let enabled: Value = row
                .model_enabled
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let health: Value = row
                .model_health
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            for model in models {
                let transport =
                    crate::factory::aionrs::map_aionrs_provider(&row.platform, &model, row.model_protocols.as_deref());
                let target = RouterTarget::Provider {
                    provider_id: row.id.clone(),
                    model,
                };
                let local = config
                    .hints
                    .iter()
                    .find(|h| h.target.as_ref() == Some(&target))
                    .is_some_and(|h| h.local_inference)
                    && loopback_url(&row.base_url)
                    && transport
                        .as_ref()
                        .is_ok_and(|t| !matches!(t.as_str(), "bedrock" | "vertex"));
                let unavailable = if !row.enabled {
                    Some("account_disabled")
                } else if enabled.get(target.model()).and_then(Value::as_bool) == Some(false) {
                    Some("model_disabled")
                } else if health
                    .get(target.model())
                    .and_then(|h| h.get("status"))
                    .and_then(Value::as_str)
                    == Some("unhealthy")
                {
                    Some("model_unhealthy")
                } else if transport.is_err() {
                    Some("provider_transport_unsupported")
                } else {
                    None
                };
                let tool_disabled = config
                    .hints
                    .iter()
                    .find(|h| h.target.as_ref() == Some(&target))
                    .is_some_and(|h| h.supports_tools == Some(false));
                let role = eligibility(role_reason(
                    unavailable.or(if tool_disabled {
                        Some("tool_calling_disabled")
                    } else {
                        None
                    }),
                    config.local_only,
                    local,
                ));
                entries.push(RouterCatalogEntry {
                    target,
                    label: row.name.clone(),
                    backend: "aionrs".into(),
                    local,
                    root: role.clone(),
                    child: role.clone(),
                    classifier: eligibility(role_reason(unavailable, config.local_only, local)),
                });
            }
        }
        for row in self.registry.list_management_rows_for_user(user).await? {
            if row.id == ROUTER_AGENT_ID {
                continue;
            }
            let backend = row.backend.unwrap_or_default();
            let unavailable = if !row.enabled {
                Some("agent_disabled")
            } else if !row.installed {
                Some("agent_not_installed")
            } else if !matches!(
                row.status,
                AgentManagementStatus::Online | AgentManagementStatus::Unchecked
            ) {
                Some("agent_unavailable")
            } else {
                None
            };
            // Root/child execution uses the same factory and inline host MCP
            // as ordinary conversations, independently from stateless generation.
            // In particular, Team membership / a shell fallback does not prove
            // that the router MCP server can reach an ACP session.
            let known_adapter = matches!(backend.as_str(), "claude" | "codex") && row.agent_type == AgentType::Acp;
            let capabilities = self
                .registry
                .repo_handle()
                .get_for_user(user, &row.id)
                .await
                .map_err(|_| AgentError::internal("Could not read router adapter capabilities"))?
                .and_then(|meta| meta.agent_capabilities)
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            let transport = match row.agent_type {
                AgentType::Acp if known_adapter => true,
                AgentType::Acp => {
                    capabilities
                        .as_ref()
                        .map(aionui_mcp::parse_acp_mcp_capabilities)
                        .unwrap_or_default()
                        .stdio
                }
                AgentType::Antigravity => true,
                _ => false,
            };
            let delegated = unavailable.or(if !transport {
                Some("router_mcp_transport_unsupported")
            } else {
                None
            });
            let classifier = unavailable.or(if !known_adapter {
                Some("stateless_generation_unsupported")
            } else if self.session_spawner.is_none() {
                Some("stateless_generation_unavailable")
            } else {
                None
            });
            for model in advertised_models(row.config_options.as_ref(), row.available_models.as_ref()) {
                let target = RouterTarget::Agent {
                    agent_id: row.id.clone(),
                    model,
                };
                let tool_disabled = config
                    .hints
                    .iter()
                    .find(|h| h.target.as_ref() == Some(&target))
                    .is_some_and(|h| h.supports_tools == Some(false));
                let delegated = delegated.or(if tool_disabled {
                    Some("tool_calling_disabled")
                } else {
                    None
                });
                entries.push(RouterCatalogEntry {
                    target,
                    label: row.name.clone(),
                    backend: backend.clone(),
                    local: false,
                    root: eligibility(role_reason(delegated, config.local_only, false)),
                    child: eligibility(role_reason(delegated, config.local_only, false)),
                    classifier: eligibility(role_reason(classifier, config.local_only, false)),
                });
            }
        }
        // Never resolve foreign IDs for display. Missing saved assignments are
        // represented only by the credential-free reference the caller supplied.
        let mut seen: BTreeSet<_> = entries.iter().map(|e| e.target.clone()).collect();
        for target in config.targets() {
            if seen.insert(target.clone()) {
                entries.push(RouterCatalogEntry {
                    target: target.clone(),
                    label: target.id().into(),
                    backend: String::new(),
                    local: false,
                    root: eligibility(Some("target_missing")),
                    child: eligibility(Some("target_missing")),
                    classifier: eligibility(Some("target_missing")),
                });
            }
        }
        entries.sort_by(|a, b| a.target.cmp(&b.target));
        entries.dedup_by(|a, b| a.target == b.target);
        Ok(entries)
    }

    /// Preserve stale saved selections, but never let a newly submitted reference
    /// name another user's account or a model that was never in this user's catalog.
    pub(super) async fn validate_graph_references(&self, user: &str, config: &RouterConfig) -> Result<(), AgentError> {
        let owned_catalog = self.catalog(user, &RouterConfig::default()).await?;
        let owned: BTreeSet<_> = owned_catalog.into_iter().map(|e| e.target).collect();
        let rows = self
            .preferences
            .get_by_keys(user, &[ROUTER_CONFIG_KEY])
            .await
            .map_err(|_| AgentError::internal("Could not read saved router references"))?;
        let previous: BTreeSet<RouterTarget> = rows
            .first()
            .and_then(|r| serde_json::from_str(&r.value).ok())
            .and_then(|v| RouterSettings::from_value(v).ok())
            .map(|s| s.policies().flat_map(|p| p.targets().cloned()).collect())
            .unwrap_or_default();
        if config.targets().any(|t| !owned.contains(t) && !previous.contains(t)) {
            return Err(AgentError::bad_request(
                "Router configuration references an unknown or unowned target",
            ));
        }
        Ok(())
    }
}
