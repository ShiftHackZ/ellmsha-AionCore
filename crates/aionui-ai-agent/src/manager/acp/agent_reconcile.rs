use crate::manager::acp::AcpAgentManager;

use crate::manager::acp::error_mapping::is_acp_session_not_found;
use crate::manager::acp::mode_normalize::normalize_requested_mode_for_available_values;
use crate::manager::acp::session::PendingStartupConfigSeedResult;
use crate::protocol::error::AcpError;
use crate::shared_kernel::{ConfigKey, ConfigValue, ModeId, ModelId};
use agent_client_protocol::schema::v1::{SessionId, SetSessionConfigOptionRequest, SetSessionModeRequest};
use std::collections::VecDeque;
use tracing::{error, info, warn};

const MAX_RECONCILE_ACTIONS: usize = 8;

/// Actions the session driver must execute to align CLI state with user intent.
///
/// Produced by `AcpSession::plan_reconcile` — a pure function that compares
/// desired vs observed and returns a list of idempotent, order-independent ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    SetMode { mode: ModeId },
    SetModel { model: ModelId },
    SetConfigOption { key: ConfigKey, value: ConfigValue },
}

impl AcpAgentManager {
    /// Execute reconcile actions produced by `AcpSession::plan_reconcile`.
    ///
    /// Compares the aggregate's desired state against what the CLI has
    /// reported as current, then issues the minimal set of SDK calls
    /// (set_mode, set_model, set_config_option) to bring the CLI into
    /// alignment.
    ///
    /// Failure handling:
    /// - `SessionNotFound`: returned as structured `AcpError::SessionNotFound` so callers
    ///   (e.g. `open_session_resume`) can drop the stale sid and rebuild
    ///   the session. ELECTRON-1HQ regressed because we silently swallowed
    ///   this case during warmup, leaving downstream `session/prompt` to
    ///   surface the same error to the user every turn.
    /// - Any other error: logged and skipped (best-effort), so a failed
    ///   `set_config_option` doesn't block a successful `set_mode`.
    pub(super) async fn reconcile_session(&self, session_id: &str) -> Result<(), AcpError> {
        use crate::manager::acp::ReconcileAction;

        let (startup_config_seed_results, invalid_mode, invalid_model, actions) = {
            let mut session = self.session.write().await;
            let startup_config_seed_results =
                session.resolve_pending_startup_config_seeds_with_mode_normalizer(|requested, available_values| {
                    normalize_requested_mode_for_available_values(
                        &self.params.metadata,
                        requested,
                        available_values.iter().copied(),
                    )
                });
            let invalid_mode = session.clear_invalid_desired_mode();
            let invalid_model = session.clear_invalid_desired_model();
            let actions = session.plan_reconcile();
            (startup_config_seed_results, invalid_mode, invalid_model, actions)
        };
        self.log_reconcile_session_plan_results(startup_config_seed_results, invalid_mode, invalid_model);
        let mut actions: VecDeque<_> = actions.into();
        let mut executed_actions = 0usize;
        while let Some(action) = actions.pop_front() {
            let executed_action = action.clone();
            executed_actions += 1;
            if executed_actions > MAX_RECONCILE_ACTIONS {
                warn!(
                    conversation_id = %self.params.conversation_id,
                    max_actions = MAX_RECONCILE_ACTIONS,
                    "reconcile_session: stopping after action limit"
                );
                break;
            }
            match action {
                ReconcileAction::SetMode { mode } => {
                    let normalized = {
                        let session = self.session.read().await;
                        let snapshot = session.config_snapshot();
                        normalize_requested_mode_for_available_values(
                            &self.params.metadata,
                            mode.as_str(),
                            snapshot.selectable_values("mode"),
                        )
                    };
                    if normalized.is_empty() {
                        continue;
                    }
                    if let Err(e) = self
                        .protocol
                        .set_mode(SetSessionModeRequest::new(
                            SessionId::new(session_id),
                            normalized.clone(),
                        ))
                        .await
                    {
                        if is_acp_session_not_found(&e) {
                            warn!(
                                conversation_id = %self.params.conversation_id,
                                mode_id = %normalized,
                                error = %e,
                                "reconcile_session: set_mode hit SessionNotFound; aborting reconcile"
                            );
                            return Err(e);
                        }
                        error!(
                            conversation_id = %self.params.conversation_id,
                            mode_id = %normalized,
                            error = %e,
                            "reconcile_session: set_mode failed"
                        );
                        continue;
                    }
                }

                ReconcileAction::SetModel { model } => {
                    if let Err(e) = self.protocol.set_model(session_id, model.as_str()).await {
                        if self.session.read().await.has_required_routed_model() {
                            return Err(e);
                        }
                        if is_acp_session_not_found(&e) {
                            warn!(
                                conversation_id = %self.params.conversation_id,
                                model_id = %model,
                                error = %e,
                                "reconcile_session: set_model hit SessionNotFound; aborting reconcile"
                            );
                            return Err(e);
                        }
                        error!(
                            conversation_id = %self.params.conversation_id,
                            model_id = %model,
                            error = %e,
                            "reconcile_session: set_model failed"
                        );
                        continue;
                    }
                }

                ReconcileAction::SetConfigOption { key, value } => {
                    let resolved_value = if key.as_str() == "mode" {
                        let session = self.session.read().await;
                        let snapshot = session.config_snapshot();
                        normalize_requested_mode_for_available_values(
                            &self.params.metadata,
                            value.as_str(),
                            snapshot.selectable_values("mode"),
                        )
                    } else {
                        value.as_str().trim().to_owned()
                    };
                    if key.as_str() == "mode" && resolved_value != value.as_str() {
                        let mut session = self.session.write().await;
                        session.set_desired_config(key.clone(), ConfigValue::new(resolved_value.clone()));
                    }
                    info!(
                        conversation_id = %self.params.conversation_id,
                        agent_backend = ?self.params.metadata.backend,
                        config_id = %key,
                        desired = %resolved_value,
                        "acp_reconcile_config_option_requested"
                    );
                    match self
                        .protocol
                        .set_config_option(SetSessionConfigOptionRequest::new(
                            SessionId::new(session_id),
                            key.as_str().to_owned(),
                            resolved_value.as_str(),
                        ))
                        .await
                    {
                        Ok(response) => {
                            info!(
                                conversation_id = %self.params.conversation_id,
                                agent_backend = ?self.params.metadata.backend,
                                config_id = %key,
                                desired = %resolved_value,
                                "acp_reconcile_config_option_ack"
                            );
                            let (startup_config_seed_results, invalid_mode, invalid_model, followup_actions) = {
                                let mut session = self.session.write().await;
                                session.apply_advertised_config_options(response.config_options);
                                let startup_config_seed_results = session
                                    .resolve_pending_startup_config_seeds_with_mode_normalizer(
                                        |requested, available_values| {
                                            normalize_requested_mode_for_available_values(
                                                &self.params.metadata,
                                                requested,
                                                available_values.iter().copied(),
                                            )
                                        },
                                    );
                                let invalid_mode = session.clear_invalid_desired_mode();
                                let invalid_model = session.clear_invalid_desired_model();
                                let followup_actions = session.plan_reconcile();
                                self.commit_session_changes(&mut session).await;
                                (
                                    startup_config_seed_results,
                                    invalid_mode,
                                    invalid_model,
                                    followup_actions,
                                )
                            };
                            self.log_reconcile_session_plan_results(
                                startup_config_seed_results,
                                invalid_mode,
                                invalid_model,
                            );
                            let mut followup_actions = followup_actions;
                            followup_actions.retain(|candidate| candidate != &executed_action);
                            actions = followup_actions.into();
                        }
                        Err(err) => {
                            if self.session.read().await.has_required_routed_model() {
                                return Err(err);
                            }
                            if is_acp_session_not_found(&err) {
                                warn!(
                                    conversation_id = %self.params.conversation_id,
                                    config_id = %key,
                                    desired = %resolved_value,
                                    error = %err,
                                    "reconcile_session: set_config_option hit SessionNotFound; aborting reconcile"
                                );
                                return Err(err);
                            }
                            info!(
                                conversation_id = %self.params.conversation_id,
                                config_id = %key,
                                desired = %resolved_value,
                                error = %err,
                                "reconcile_session: set_config_option failed; skipping"
                            );
                            continue;
                        }
                    }
                }
            }
        }
        self.session.read().await.verify_routed_model()
    }

    fn log_reconcile_session_plan_results(
        &self,
        startup_config_seed_results: Vec<PendingStartupConfigSeedResult>,
        invalid_mode: Option<ModeId>,
        invalid_model: Option<ModelId>,
    ) {
        if let Some(mode) = invalid_mode {
            warn!(
                conversation_id = %self.params.conversation_id,
                mode_id = %mode,
                "reconcile_session: dropped unavailable desired mode"
            );
        }
        if let Some(model) = invalid_model {
            warn!(
                conversation_id = %self.params.conversation_id,
                model_id = %model,
                "reconcile_session: dropped unavailable desired model"
            );
        }
        for result in startup_config_seed_results {
            match result {
                PendingStartupConfigSeedResult::Applied { .. } => {}
                PendingStartupConfigSeedResult::OptionNotAdvertised { category } => {
                    warn!(
                        conversation_id = %self.params.conversation_id,
                        agent_backend = ?self.params.metadata.backend,
                        category = ?category,
                        reason = "option_not_advertised",
                        "reconcile_session: startup config seed not applied"
                    );
                }
                PendingStartupConfigSeedResult::ValueNotSelectable { category } => {
                    warn!(
                        conversation_id = %self.params.conversation_id,
                        agent_backend = ?self.params.metadata.backend,
                        category = ?category,
                        reason = "value_not_selectable",
                        "reconcile_session: startup config seed not applied"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::acp::AcpSession;
    use std::collections::HashMap;

    #[cfg(unix)]
    #[tokio::test]
    async fn routed_acp_reconcile_fails_closed_on_catalog_mismatch_and_selection_errors() {
        use crate::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
        use crate::registry::AgentRegistry;
        use std::sync::Arc;
        for (scenario, routed, should_pass) in [
            ("missing", true, false),
            ("config-fails", true, false),
            ("legacy-fails", true, false),
            ("success", true, true),
            ("missing", false, true),
            ("config-fails", false, true),
            ("legacy-fails", false, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("agent.py");
            let calls = dir.path().join("calls");
            std::fs::write(&script, r#"import json, sys
scenario, path = sys.argv[1:]
def options(current):
    models = ['other'] if scenario == 'missing' else ['other', 'wanted']
    return [{'id':'model','name':'Model','category':'model','type':'select','currentValue':current,'options':[{'value':m,'name':m} for m in models]}]
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    with open(path, 'a') as log: log.write(str(method) + '\n')
    if 'id' not in message: continue
    response = {'jsonrpc':'2.0','id':message['id']}
    if method == 'initialize': response['result'] = {'protocolVersion':1,'agentCapabilities':{},'authMethods':[]}
    elif method == 'session/new':
        response['result'] = {'sessionId':'fixture'}
        if scenario == 'legacy-fails': response['result']['models'] = {'currentModelId':'other','availableModels':[{'modelId':'other','name':'Other'},{'modelId':'wanted','name':'Wanted'}]}
        else: response['result']['configOptions'] = options('other')
    elif method in ['session/set_config_option', 'session/set_model']:
        if scenario == 'success': response['result'] = {'configOptions':options('wanted')}
        else: response['error'] = {'code':-32602,'message':'fixture rejected model'}
    elif method == 'session/prompt': response['result'] = {'stopReason':'end_turn'}
    else: response['result'] = {}
    print(json.dumps(response), flush=True)
"#).unwrap();
            let db = aionui_db::init_database_memory().await.unwrap();
            let registry = AgentRegistry::new(Arc::new(aionui_db::SqliteAgentMetadataRepository::new(
                db.pool().clone(),
            )));
            registry.hydrate().await.unwrap();
            let metadata = registry.find_builtin_by_backend("gemini").await.unwrap();
            let params = Arc::new(
                assemble_acp_params(
                    "route-fixture".into(),
                    "fixture-user".into(),
                    WorkspaceInfo {
                        path: dir.path().to_string_lossy().into_owned(),
                        is_custom: true,
                    },
                    metadata,
                    aionui_common::CommandSpec {
                        command: "python3".into(),
                        args: vec![
                            script.to_string_lossy().into_owned(),
                            scenario.into(),
                            calls.to_string_lossy().into_owned(),
                        ],
                        env: vec![],
                        cwd: None,
                    },
                    aionui_api_types::AcpBuildExtra {
                        backend: Some("gemini".into()),
                        current_model_id: Some("wanted".into()),
                        ..Default::default()
                    },
                    vec![],
                    None,
                    dir.path().into(),
                    false,
                )
                .await,
            );
            let paths = Arc::new(aionui_extension::resolve_skill_paths(dir.path(), dir.path()));
            let (manager, _events, notifications) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                AcpAgentManager::build(params, crate::AcpSkillManager::new(paths), &registry.catalog_sender()),
            )
            .await
            .unwrap()
            .unwrap();
            if routed {
                manager.require_routed_model("wanted".into()).await;
            }
            let manager = Arc::new(manager);
            manager.start_session_event_tracker(notifications);
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), manager.warmup_session())
                .await
                .unwrap();
            if routed && scenario == "success" {
                manager
                    .session
                    .write()
                    .await
                    .apply_observed_model(ModelId::new("other"));
                let data = serde_json::from_value(serde_json::json!({"content":"must not be sent","msg_id":"fixture"}))
                    .unwrap();
                assert!(
                    manager
                        .prompt_existing_session(&data, Some("fixture"), None)
                        .await
                        .is_err(),
                    "drift after warmup must be rejected before sending a prompt"
                );
            }
            manager.kill_and_wait(None).await;
            assert_eq!(result.is_ok(), should_pass, "{scenario}, routed={routed}: {result:?}");
            let calls = std::fs::read_to_string(calls).unwrap();
            assert!(!calls.lines().any(|method| method == "session/prompt"));
            if scenario == "config-fails" {
                assert!(calls.contains("session/set_config_option"));
            }
            if scenario == "legacy-fails" {
                assert!(calls.contains("session/set_model"));
            }
        }
    }

    #[test]
    fn reconcile_action_equality() {
        let a = ReconcileAction::SetMode {
            mode: ModeId::new("plan"),
        };
        let b = ReconcileAction::SetMode {
            mode: ModeId::new("plan"),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn reconcile_set_config_option_ack_must_not_be_modeled_as_observed_event() {
        let mut session = AcpSession::new(
            None,
            None,
            HashMap::from([(ConfigKey::new("reasoning_effort"), ConfigValue::new("high"))]),
        );

        session.apply_observed_config(ConfigKey::new("reasoning_effort"), ConfigValue::new("medium"));
        session.drain_events();

        let actions = session.plan_reconcile();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], ReconcileAction::SetConfigOption { .. }));

        let events = session.drain_events();
        assert!(events.is_empty());
    }
}
