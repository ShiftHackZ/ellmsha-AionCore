//! Ordinary V2 router children. Scheduling remains in ConversationService's turn orchestrator.
use super::*;
use aionui_api_types::{RouterChildCall, RouterChildMetadata, RouterChildResponse, RouterChildStatus, RouterSnapshot};
use std::sync::Mutex;

const SERVER: &str = "ellmsha-router";
const MAX_ACTIVE_CHILDREN: usize = 8;
const MAX_TASKS: usize = 1024;
const MAX_DEPTH: usize = 4;

#[derive(Default)]
pub(super) struct RouterChildren {
    state: Mutex<ChildrenState>,
    status_writes: tokio::sync::Mutex<()>,
}
#[derive(Default)]
struct ChildrenState {
    tasks: HashMap<String, ChildTask>,
    closed: HashMap<String, String>,
    // Survives runtime turn release while a pending child creation drains during deletion.
    deleting: std::collections::HashSet<String>,
}
#[derive(Clone)]
struct ChildTask {
    user: String,
    lineage: RouterChildMetadata,
    response: RouterChildResponse,
    turn: Option<String>,
    cancelled: bool,
    // Terminal status alone does not mean a cancelled provisioning future has stopped.
    settled: bool,
}

impl ChildrenState {
    fn owned(&self, user: &str, parent: &str, parent_turn: &str, task: &str) -> Result<&ChildTask, ConversationError> {
        self.tasks
            .get(task)
            .filter(|t| {
                t.user == user && t.lineage.parent_conversation_id == parent && t.lineage.parent_turn_id == parent_turn
            })
            .ok_or_else(|| ConversationError::bad_request("Unknown task for this parent turn"))
    }
    fn reclaim_finished(&mut self, active: impl Fn(&str, &str) -> bool) {
        self.tasks.retain(|_, task| {
            !task.settled || active(&task.lineage.parent_conversation_id, &task.lineage.parent_turn_id)
        });
        self.closed.retain(|parent, turn| {
            active(parent, turn)
                || self
                    .tasks
                    .values()
                    .any(|task| task.lineage.parent_conversation_id == *parent)
        });
        self.deleting.retain(|parent| {
            self.tasks
                .values()
                .any(|task| task.lineage.parent_conversation_id == *parent)
        });
    }
    fn admit(&mut self, task: ChildTask) -> Result<(), ConversationError> {
        let parent = &task.lineage.parent_conversation_id;
        if self.deleting.contains(parent) || self.closed.get(parent) == Some(&task.lineage.parent_turn_id) {
            return Err(ConversationError::bad_request("Parent turn is closed"));
        }
        // Retain current-turn results, discard only terminal results from old turns.
        self.tasks.retain(|_, t| {
            t.lineage.parent_conversation_id != *parent
                || t.lineage.parent_turn_id == task.lineage.parent_turn_id
                || !t.settled
        });
        if self.tasks.contains_key(&task.lineage.task_id) {
            return Err(ConversationError::internal(
                "Router task identity collision; retry delegation",
            ));
        }
        if self.tasks.len() >= MAX_TASKS
            || self
                .tasks
                .values()
                .filter(|t| t.lineage.root_conversation_id == task.lineage.root_conversation_id && !t.settled)
                .count()
                >= MAX_ACTIVE_CHILDREN
        {
            return Err(ConversationError::bad_request(
                "Router child concurrency limit reached; wait for a child",
            ));
        }
        self.tasks.insert(task.lineage.task_id.clone(), task);
        Ok(())
    }
}

impl ConversationService {
    /// Authenticated gateway entry. Capture turn before the first await; callbacks never adopt a newer turn.
    pub async fn router_child_call(
        &self,
        user: &str,
        parent: &str,
        call: RouterChildCall,
    ) -> Result<RouterChildResponse, ConversationError> {
        let parent_turn = self
            .runtime_state
            .active_turn_id_for(parent)
            .ok_or_else(|| ConversationError::bad_request("Router delegation requires an active parent turn"))?;
        let row = self
            .conversation_repo
            .get(user, parent)
            .await?
            .ok_or_else(|| ConversationError::NotFound { id: parent.into() })?;
        let extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|_| ConversationError::bad_request("Invalid parent context"))?;
        let tool_scope = aionui_api_types::ConversationToolScope::from_extra(&extra)
            .map_err(|_| ConversationError::bad_request("Invalid parent tool scope"))?;
        if tool_scope.is_session_only() {
            return Err(ConversationError::bad_request(
                "Host delegation is unavailable for session_only tool scope",
            ));
        }
        let snapshot: RouterSnapshot = serde_json::from_value(extra.get("routing").cloned().unwrap_or_default())
            .map_err(|_| ConversationError::bad_request("Host delegation requires a V2 router chat"))?;
        if snapshot.config.schema_version != 2 || team_id_from_extra(&row.extra).is_some() {
            return Err(ConversationError::bad_request(
                "Host delegation requires an ordinary V2 router chat",
            ));
        }
        self.check_parent_turn(parent, &parent_turn)?;
        if let Some(metadata) = extra.get("router_child") {
            let metadata: RouterChildMetadata = serde_json::from_value(metadata.clone())
                .map_err(|_| ConversationError::bad_request("Invalid child lineage"))?;
            let owns_turn = self
                .router_children
                .state
                .lock()
                .map_err(|_| ConversationError::internal("Router state unavailable"))?
                .tasks
                .get(&metadata.task_id)
                .is_some_and(|entry| {
                    entry.user == user
                        && entry.lineage == metadata
                        && entry.response.conversation_id.as_deref() == Some(parent)
                        && entry.turn.as_deref() == Some(parent_turn.as_str())
                });
            if !owns_turn || !self.child_admitted(&metadata.task_id) {
                return Err(ConversationError::bad_request(
                    "Managed child parent turn is no longer active",
                ));
            }
        }
        match call {
            RouterChildCall::Spawn { task, name } => {
                if task.trim().is_empty()
                    || task.len() > 128 * 1024
                    || name.as_ref().is_some_and(|n| n.trim().is_empty() || n.len() > 80)
                {
                    return Err(ConversationError::bad_request(
                        "Child requires a task up to 128 KiB and label up to 80 bytes",
                    ));
                }
                let mut ancestor = row.clone();
                let mut root = parent.to_owned();
                for depth in 0..MAX_DEPTH {
                    let value: serde_json::Value = serde_json::from_str(&ancestor.extra).unwrap_or_default();
                    let Some(metadata) = value.get("router_child") else {
                        break;
                    };
                    if depth + 1 == MAX_DEPTH {
                        return Err(ConversationError::bad_request("Router child depth limit reached"));
                    }
                    let metadata: RouterChildMetadata = serde_json::from_value(metadata.clone())
                        .map_err(|_| ConversationError::bad_request("Invalid child lineage"))?;
                    root = metadata.root_conversation_id;
                    ancestor = self
                        .conversation_repo
                        .get(user, &metadata.parent_conversation_id)
                        .await?
                        .ok_or_else(|| ConversationError::bad_request("Router parent no longer exists"))?;
                }
                self.check_parent_turn(parent, &parent_turn)?;
                let id = generate_short_id();
                let lineage = RouterChildMetadata {
                    version: 1,
                    root_conversation_id: root,
                    parent_conversation_id: parent.into(),
                    parent_turn_id: parent_turn.clone(),
                    task_id: id.clone(),
                };
                let response = RouterChildResponse {
                    task_id: id.clone(),
                    conversation_id: None,
                    status: RouterChildStatus::PendingInit,
                    result: None,
                    truncated: false,
                    error: None,
                };
                let mut state = self
                    .router_children
                    .state
                    .lock()
                    .map_err(|_| ConversationError::internal("Router state unavailable"))?;
                state.reclaim_finished(|parent, turn| {
                    self.runtime_state.active_turn_id_for(parent).as_deref() == Some(turn)
                });
                state.admit(ChildTask {
                    user: user.into(),
                    lineage: lineage.clone(),
                    response: response.clone(),
                    turn: None,
                    cancelled: false,
                    settled: false,
                })?;
                drop(state);
                let service = self.clone();
                let user = user.to_owned();
                tokio::spawn(async move {
                    service.run_router_child(user, row, snapshot, lineage, task, name).await;
                });
                Ok(response)
            }
            RouterChildCall::Wait { task_id, timeout_ms } => {
                let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms.min(50_000));
                loop {
                    self.check_parent_turn(parent, &parent_turn)?;
                    let mut result = self
                        .router_children
                        .state
                        .lock()
                        .map_err(|_| ConversationError::internal("Router state unavailable"))?
                        .owned(user, parent, &parent_turn, &task_id)?
                        .response
                        .clone();
                    if let Some(id) = result.conversation_id.as_deref() {
                        let runtime = self.runtime_summary_for(id).await;
                        if !result.status.terminal()
                            && result.status != RouterChildStatus::Cancelling
                            && runtime.pending_confirmations > 0
                        {
                            result.status = RouterChildStatus::WaitingForApproval;
                        }
                    }
                    self.check_parent_turn(parent, &parent_turn)?;
                    if result.status.terminal()
                        || result.status == RouterChildStatus::WaitingForApproval
                        || tokio::time::Instant::now() >= deadline
                    {
                        return Ok(result);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            RouterChildCall::Cancel { task_id } => {
                let task = {
                    let mut state = self
                        .router_children
                        .state
                        .lock()
                        .map_err(|_| ConversationError::internal("Router state unavailable"))?;
                    state.owned(user, parent, &parent_turn, &task_id)?;
                    let task = state.tasks.get_mut(&task_id).expect("validated task");
                    if !task.response.status.terminal() {
                        task.cancelled = true;
                        task.response.status = RouterChildStatus::Cancelling;
                    }
                    task.clone()
                };
                if !task.response.status.terminal() {
                    self.stop_router_task(&task).await?;
                }
                self.check_parent_turn(parent, &parent_turn)?;
                Ok(task.response)
            }
        }
    }

    fn check_parent_turn(&self, parent: &str, turn: &str) -> Result<(), ConversationError> {
        if self.runtime_state.active_turn_id_for(parent).as_deref() != Some(turn)
            || self.runtime_state.is_deleting(parent)
            || self
                .router_children
                .state
                .lock()
                .map_err(|_| ConversationError::internal("Router state unavailable"))?
                .closed
                .get(parent)
                .is_some_and(|t| t == turn)
        {
            return Err(ConversationError::bad_request("Parent turn is no longer active"));
        }
        Ok(())
    }
    fn child_admitted(&self, id: &str) -> bool {
        let task = self
            .router_children
            .state
            .lock()
            .ok()
            .and_then(|s| s.tasks.get(id).cloned());
        task.is_some_and(|t| {
            !t.cancelled
                && self
                    .check_parent_turn(&t.lineage.parent_conversation_id, &t.lineage.parent_turn_id)
                    .is_ok()
        })
    }

    async fn run_router_child(
        &self,
        user: String,
        parent: ConversationRow,
        snapshot: RouterSnapshot,
        lineage: RouterChildMetadata,
        task: String,
        name: Option<String>,
    ) {
        let outcome = self
            .provision_router_child(&user, &parent, snapshot, &lineage, &task, name)
            .await;
        let (status, error, result, truncated) = match outcome {
            Ok((result, truncated)) => (RouterChildStatus::Completed, None, result, truncated),
            Err(error) => (
                RouterChildStatus::Errored,
                Some(match error {
                    ConversationError::BadRequest { reason } | ConversationError::Forbidden { reason } => reason,
                    _ => "Child execution failed; inspect the child conversation".into(),
                }),
                None,
                false,
            ),
        };
        let child = if let Ok(mut state) = self.router_children.state.lock() {
            state.tasks.get_mut(&lineage.task_id).map(|entry| {
                if entry.cancelled {
                    entry.response.status = RouterChildStatus::Interrupted;
                    entry.response.result = None;
                    entry.response.truncated = false;
                } else {
                    entry.response.status = status;
                    entry.response.error = error;
                    entry.response.result = result;
                    entry.response.truncated = truncated;
                }
                entry.clone()
            })
        } else {
            None
        };
        if let Some(child) = child {
            self.persist_router_child_status(&child).await;
            if let Ok(mut state) = self.router_children.state.lock()
                && let Some(entry) = state.tasks.get_mut(&lineage.task_id)
            {
                entry.settled = true;
            }
        }
    }

    async fn provision_router_child(
        &self,
        user: &str,
        parent: &ConversationRow,
        snapshot: RouterSnapshot,
        lineage: &RouterChildMetadata,
        task: &str,
        name: Option<String>,
    ) -> Result<(Option<String>, bool), ConversationError> {
        if !self.child_admitted(&lineage.task_id) {
            return Err(ConversationError::bad_request("Child cancelled before provisioning"));
        }
        let stored = self
            .conversation_repo
            .get_assistant_snapshot(user, &parent.id)
            .await?
            .ok_or_else(|| ConversationError::bad_request("Parent assistant snapshot is unavailable"))?;
        let parent_extra: serde_json::Value = serde_json::from_str(&parent.extra).unwrap_or_default();
        let agent = self
            .task_manager
            .get_task(&parent.id)
            .ok_or_else(|| ConversationError::bad_request("Parent runtime is unavailable"))?;
        let mode = agent.get_mode().await?;
        if !mode.initialized {
            return Err(ConversationError::bad_request(
                "Parent permission mode is not initialized",
            ));
        }
        let assistant = AssistantSnapshot {
            assistant_definition_id: stored.assistant_definition_id,
            assistant_id: stored.assistant_id,
            assistant_source: stored.assistant_source,
            name: parent.name.clone(),
            avatar_type: String::new(),
            avatar: None,
            agent_id: stored.agent_id,
            agent_source: parent_extra["agent_source"].as_str().unwrap_or("internal").into(),
            runtime_backend: parent_extra["backend"].as_str().unwrap_or("aionrs").into(),
            agent_type: string_to_enum(&parent.r#type)?,
            rules: AssistantSnapshotRules {
                content: stored.rules_content,
            },
            default_modes: AssistantSnapshotDefaultModes {
                model: stored.default_model_mode,
                permission: stored.default_permission_mode,
                thought_level: stored.default_thought_level_mode,
                skills: stored.default_skills_mode,
                mcps: stored.default_mcps_mode,
            },
            resolved_defaults: AssistantSnapshotResolvedDefaults {
                model: stored.resolved_model_id,
                permission: Some(mode.mode),
                thought_level: stored.resolved_thought_level_value,
                skill_ids: serde_json::from_str(&stored.resolved_skill_ids).unwrap_or_default(),
                disabled_builtin_skill_ids: serde_json::from_str(&stored.resolved_disabled_builtin_skill_ids)
                    .unwrap_or_default(),
                mcp_ids: serde_json::from_str(&stored.resolved_mcp_ids).unwrap_or_default(),
            },
            created_at: now_ms(),
        };
        // Copy selected durable capabilities only. Never inherit session IDs, forks, team transport or runtime tokens.
        let mut extra = serde_json::json!({"workspace":snapshot.decision.scope_workspace,"custom_workspace":true});
        for field in ["skills", "exclude_auto_inject_skills"] {
            if let Some(value) = parent_extra.get(field) {
                extra[field] = value.clone();
            }
        }
        if let Some(value) = parent_extra.get("mcp_server_ids") {
            extra["selected_mcp_server_ids"] = value.clone();
        }
        if let Some(value) = parent_extra.get("session_mcp_servers") {
            extra["selected_session_mcp_servers"] = value.clone();
        }
        if !self.child_admitted(&lineage.task_id) {
            return Err(ConversationError::bad_request("Child cancelled before creation"));
        }
        let child = self
            .create_inner(
                user,
                CreateConversationRequest {
                    routing: Some(aionui_api_types::RouterCreateRequest {
                        initial_prompt: task.into(),
                        config: None,
                        has_images: false,
                    }),
                    tool_scope: Default::default(),
                    r#type: None,
                    name: Some(name.unwrap_or_else(|| "Router subagent".into())),
                    model: None,
                    assistant: None,
                    source: None,
                    channel_chat_id: None,
                    extra,
                },
                Some(snapshot.config),
                true,
                false,
                None,
                Some((assistant, lineage.clone())),
            )
            .await?;
        {
            let mut state = self
                .router_children
                .state
                .lock()
                .map_err(|_| ConversationError::internal("Router state unavailable"))?;
            if let Some(entry) = state.tasks.get_mut(&lineage.task_id) {
                entry.response.conversation_id = Some(child.id.clone());
            }
        }
        if !self.child_admitted(&lineage.task_id) {
            if self.router_parent_is_deleting(&parent.id)
                || self.conversation_repo.get(user, &parent.id).await?.is_none()
            {
                Box::pin(self.delete(user, &child.id)).await?;
            }
            return Err(ConversationError::bad_request("Child cancelled during creation"));
        }
        let service = self.clone();
        let id = lineage.task_id.clone();
        let outcome = self
            .run_agent_turn(ConversationAgentTurnRequest {
                user_id: user.into(),
                conversation_id: child.id.clone(),
                content: task.into(),
                files: vec![],
                inject_skills: vec![],
                required_runtime_mode: None,
                persist_user_message: true,
                user_message_hidden: false,
                on_started: Some(Arc::new(move |started| {
                    let service = service.clone();
                    let id = id.clone();
                    Box::pin(async move {
                        let entry = service.record_router_child_started(&id, &started.turn_id);
                        if !service.child_admitted(&id) {
                            // Apply the existing deferred-cancel path before the child runtime is built.
                            service
                                .runtime_state
                                .defer_cancel(&started.conversation_id, &started.turn_id);
                            if let Some(entry) = entry.as_ref()
                                && let Err(error) = service.stop_router_task(entry).await
                            {
                                warn!(error = %ErrorChain(&error), "Router child cancellation failed");
                            }
                        }
                        if let Some(entry) = entry {
                            service.persist_router_child_status(&entry).await;
                        }
                    })
                })),
            })
            .await?;
        if outcome.status == ConversationAgentTurnStatus::Failed {
            return Err(ConversationError::bad_request(
                "Child turn failed; inspect the child conversation",
            ));
        }
        let messages = self
            .conversation_repo
            .list_messages_page(
                user,
                &child.id,
                &MessagePageParams {
                    limit: 30,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await?;
        Ok(final_child_text(&messages.items))
    }

    async fn persist_router_child_status(&self, task: &ChildTask) {
        // A caller may have waited for cancellation since taking its snapshot.
        // Serialize the writes and only read the current status after admission.
        let _write = self.router_children.status_writes.lock().await;
        let current = self
            .router_children
            .state
            .lock()
            .ok()
            .and_then(|state| state.tasks.get(&task.lineage.task_id).cloned());
        let Some(task) = current else { return };
        if let Some(child) = task.response.conversation_id.as_deref() {
            if let Err(error) = self
                .update_extra(
                    &task.user,
                    child,
                    serde_json::json!({"subagent":{"status":task.response.status}}),
                )
                .await
            {
                warn!(task_id = %task.lineage.task_id, error = %ErrorChain(&error), "Router child status persistence failed");
                return;
            }
            self.broadcast_list_changed(&task.user, child, "updated", None);
        }
    }
    fn record_router_child_started(&self, task: &str, turn: &str) -> Option<ChildTask> {
        let mut state = self.router_children.state.lock().ok()?;
        let entry = state.tasks.get_mut(task)?;
        entry.turn = Some(turn.into());
        if !entry.cancelled {
            entry.response.status = RouterChildStatus::Running;
        }
        Some(entry.clone())
    }
    pub(super) fn mark_router_child_cancelled(&self, user: &str, child: &str, turn: &str) {
        if let Ok(mut state) = self.router_children.state.lock() {
            for task in state.tasks.values_mut().filter(|task| {
                task.user == user
                    && task.response.conversation_id.as_deref() == Some(child)
                    && task.turn.as_deref() == Some(turn)
                    && !task.settled
                    && !task.response.status.terminal()
            }) {
                task.cancelled = true;
                task.response.status = RouterChildStatus::Cancelling;
            }
        }
    }
    async fn stop_router_task(&self, task: &ChildTask) -> Result<(), ConversationError> {
        if let (Some(child), Some(turn)) = (task.response.conversation_id.as_deref(), task.turn.as_deref()) {
            Box::pin(self.cancel(&task.user, child, turn, &self.task_manager)).await?;
        }
        self.persist_router_child_status(task).await;
        Ok(())
    }
    pub(super) async fn close_router_children(&self, user: &str, parent: &str, turn: Option<&str>) {
        let tasks = {
            let Ok(mut state) = self.router_children.state.lock() else {
                return;
            };
            if let Some(turn) = turn
                .map(str::to_owned)
                .or_else(|| self.runtime_state.active_turn_id_for(parent))
            {
                state.closed.insert(parent.into(), turn);
            }
            state
                .tasks
                .values_mut()
                .filter(|t| {
                    t.user == user
                        && t.lineage.parent_conversation_id == parent
                        && turn.is_none_or(|turn| t.lineage.parent_turn_id == turn)
                        && !t.response.status.terminal()
                })
                .map(|t| {
                    t.cancelled = true;
                    t.response.status = RouterChildStatus::Cancelling;
                    t.clone()
                })
                .collect::<Vec<_>>()
        };
        for task in tasks {
            if let Err(error) = self.stop_router_task(&task).await {
                warn!(error = %ErrorChain(&error), "Router child cancellation failed during parent cleanup");
            }
        }
    }
    pub(super) fn mark_router_parent_deleting(&self, parent: &str) {
        if let Ok(mut state) = self.router_children.state.lock() {
            state.deleting.insert(parent.into());
        }
    }
    pub(super) fn clear_router_parent_deleting(&self, parent: &str) {
        if let Ok(mut state) = self.router_children.state.lock() {
            state.deleting.remove(parent);
        }
    }
    fn router_parent_is_deleting(&self, parent: &str) -> bool {
        self.router_children
            .state
            .lock()
            .map(|s| s.deleting.contains(parent))
            .unwrap_or(true)
    }
    pub(super) async fn delete_router_child_rows(&self, user: &str, parent: &str) -> Result<(), ConversationError> {
        for row in self.conversation_repo.list_associated(user, parent).await? {
            let metadata = serde_json::from_str::<serde_json::Value>(&row.extra)
                .ok()
                .and_then(|v| v.get("router_child").cloned())
                .and_then(|v| serde_json::from_value::<RouterChildMetadata>(v).ok());
            if metadata.is_some_and(|m| m.parent_conversation_id == parent) {
                Box::pin(self.delete(user, &row.id)).await?;
            }
        }
        Ok(())
    }

    pub(super) fn apply_router_child_tools(&self, opts: &mut BuildTaskOptions) -> Result<(), ConversationError> {
        if opts.context.team.is_some()
            || opts.context.is_session_only()
            || opts.routing.as_ref().is_none_or(|r| r.config.schema_version != 2)
        {
            return Ok(());
        }
        let (Some(helper), Some(base), Some(token)) = (
            self.runtime_helper_bin.as_ref(),
            self.runtime_base_url.as_ref(),
            self.runtime_token_for_build(opts, &opts.context.conversation.user_id, opts.context.conversation_id()),
        ) else {
            return Err(ConversationError::internal("Router runtime helper is unavailable"));
        };
        let server = SessionMcpServer {
            id: SERVER.into(),
            name: SERVER.into(),
            transport: SessionMcpTransport::Stdio {
                command: helper.clone(),
                args: vec!["mcp-router-stdio".into()],
                env: HashMap::from([
                    ("AIONUI_BASE_URL".into(), base.clone()),
                    ("AIONUI_RUNTIME_TOKEN".into(), token),
                    ("AIONUI_USER_ID".into(), opts.context.conversation.user_id.clone()),
                    ("AIONUI_CONVERSATION_ID".into(), opts.context.conversation_id().into()),
                ]),
            },
            auto_approve_in_unattended: false,
            approval_identity: None,
        };
        let guidance = "[Auto Router delegation] For delegated tasks use ellmsha-router router_spawn/router_wait/router_cancel so the saved classifier/default routes, workspace and permissions apply. Wait for child results before completing your turn; unfinished children are cancelled when it ends. Approval requests stay in the child chat. Provider-internal children are outside Auto Router control.";
        let prompt = match &mut opts.context.kind {
            AgentSessionKind::Aionrs(ctx) => &mut ctx.config.preset_rules,
            AgentSessionKind::Acp(ctx) => &mut ctx.config.preset_context,
            AgentSessionKind::Antigravity(ctx) => &mut ctx.config.preset_context,
        };
        *prompt = Some(format!("{}\n\n{}", prompt.take().unwrap_or_default(), guidance));
        let servers = match &mut opts.context.kind {
            AgentSessionKind::Aionrs(ctx) => &mut ctx.config.session_mcp_servers,
            AgentSessionKind::Acp(ctx) => &mut ctx.config.session_mcp_servers,
            AgentSessionKind::Antigravity(ctx) => &mut ctx.config.session_mcp_servers,
        };
        servers.retain(|s| s.name != SERVER && s.id != SERVER);
        servers.push(server);
        Ok(())
    }
}

/// Conservative cross-runtime mode translation. Unknown custom profiles never become full-auto.
pub(super) fn inherited_permission(mode: &str, backend: &str) -> Result<&'static str, ConversationError> {
    match mode {
        "yolo" | "full-access" | ":full-access" | "agent-full-access" | "bypassPermissions" => Ok("yolo"),
        "default" | "auto" | "auto_edit" | "autoEdit" | "acceptEdits" | ":workspace" => {
            Ok(if backend == "codex" { "read-only" } else { "default" })
        }
        "plan" | "read-only" | ":read-only" => match backend {
            "codex" => Ok("read-only"),
            "claude" => Ok("plan"),
            _ => Err(ConversationError::bad_request(
                "Read-only parent requires a read-only-capable child runtime",
            )),
        },
        _ => Err(ConversationError::bad_request(
            "Parent permission profile cannot be safely inherited",
        )),
    }
}

fn final_child_text(messages: &[MessageRow]) -> (Option<String>, bool) {
    // Repository pages are chronological, including InitialLatest; reverse picks the latest final text.
    let result = messages
        .iter()
        .rev()
        .find(|m| {
            m.r#type == "text"
                && m.position.as_deref() == Some("left")
                && m.status.as_deref() == Some("finish")
                && !m.hidden
        })
        .and_then(|m| serde_json::from_str::<serde_json::Value>(&m.content).ok())
        .and_then(|v| v.get("content").and_then(|s| s.as_str()).map(str::to_owned));
    match result {
        Some(mut text) => {
            let mut end = text.len().min(32 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let truncated = end < text.len();
            text.truncate(end);
            (Some(text), truncated)
        }
        None => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn insert_row(service: &ConversationService, id: &str, extra: serde_json::Value) {
        service
            .conversation_repo
            .create(&ConversationRow {
                id: id.into(),
                user_id: "owner".into(),
                name: "Router fixture".into(),
                r#type: "aionrs".into(),
                extra: extra.to_string(),
                model: None,
                status: Some("finished".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: 1,
                updated_at: 1,
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn router_child_gateway_rejects_session_only_before_admission() {
        let service = crate::service_test::router_test_service();
        let snapshot = RouterSnapshot {
            config: aionui_api_types::RouterConfig {
                schema_version: 2,
                ..Default::default()
            },
            decision: aionui_api_types::RouterDecision {
                id: "decision".into(),
                target: aionui_api_types::RouterTarget::Provider {
                    provider_id: "fixture".into(),
                    model: "model".into(),
                },
                requirements: Default::default(),
                reasons: vec![],
                evidence_count: 0,
                elapsed_ms: 0,
                managed_children: true,
                scope_workspace: String::new(),
            },
        };
        insert_row(
            &service,
            "parent",
            serde_json::json!({"tool_scope":"session_only", "routing":snapshot}),
        )
        .await;
        let _claim = service.runtime_state.try_claim_turn("parent", "turn").unwrap();
        for call in [
            RouterChildCall::Spawn {
                task: "Read fixture".into(),
                name: None,
            },
            RouterChildCall::Wait {
                task_id: "unknown".into(),
                timeout_ms: 0,
            },
            RouterChildCall::Cancel {
                task_id: "unknown".into(),
            },
        ] {
            assert!(matches!(service.router_child_call("owner", "parent", call).await,
                Err(ConversationError::BadRequest { reason }) if reason.contains("session_only tool scope")));
        }
        assert!(service.router_children.state.lock().unwrap().tasks.is_empty());
        insert_row(&service, "ordinary", serde_json::json!({"routing":snapshot})).await;
        let _ordinary_claim = service.runtime_state.try_claim_turn("ordinary", "turn").unwrap();
        assert!(
            matches!(service.router_child_call("owner", "ordinary", RouterChildCall::Wait {
            task_id: "unknown".into(), timeout_ms: 0,
        }).await, Err(ConversationError::BadRequest { reason }) if reason == "Unknown task for this parent turn")
        );
    }

    #[tokio::test]
    async fn router_child_status_queued_stale_writes_cannot_regress_terminal_state() {
        let service = crate::service_test::router_test_service();
        insert_row(&service, "child-chat", serde_json::json!({"preserved":"value"})).await;
        let mut running = task("task", "parent-turn");
        running.response.conversation_id = Some("child-chat".into());
        running.response.status = RouterChildStatus::Running;
        let mut cancelling = running.clone();
        cancelling.response.status = RouterChildStatus::Cancelling;
        service
            .router_children
            .state
            .lock()
            .unwrap()
            .admit(running.clone())
            .unwrap();
        // Both start/cancel callbacks have old snapshots and queue behind an I/O
        // writer. Completion occurs before they can enter the persistence gate.
        let gate = service.router_children.status_writes.lock().await;
        let first = service.persist_router_child_status(&running);
        let second = service.persist_router_child_status(&cancelling);
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        {
            let mut state = service.router_children.state.lock().unwrap();
            let current = state.tasks.get_mut("task").unwrap();
            current.cancelled = true;
            current.response.status = RouterChildStatus::Interrupted;
        }
        drop(gate);
        tokio::join!(first, second);
        let row = service
            .conversation_repo
            .get("owner", "child-chat")
            .await
            .unwrap()
            .unwrap();
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
        assert_eq!(extra["subagent"]["status"], "interrupted");
        assert_eq!(extra["preserved"], "value");
    }

    #[tokio::test]
    async fn router_child_late_cancel_cannot_reopen_terminal_status_awaiting_persistence() {
        for status in [
            RouterChildStatus::Completed,
            RouterChildStatus::Errored,
            RouterChildStatus::Interrupted,
        ] {
            let service = crate::service_test::router_test_service();
            insert_row(&service, "child-chat", serde_json::json!({})).await;
            let mut finished = task("task", "parent-turn");
            finished.response.conversation_id = Some("child-chat".into());
            finished.response.status = status;
            finished.turn = Some("child-turn".into());
            assert!(!finished.settled, "terminal outcome is awaiting its final write");
            service
                .router_children
                .state
                .lock()
                .unwrap()
                .admit(finished.clone())
                .unwrap();
            let gate = service.router_children.status_writes.lock().await;
            let persist = service.persist_router_child_status(&finished);
            tokio::pin!(persist);
            assert!(futures_util::poll!(&mut persist).is_pending());
            service.mark_router_child_cancelled("owner", "child-chat", "child-turn");
            {
                let state = service.router_children.state.lock().unwrap();
                assert_eq!(state.tasks["task"].response.status, status);
                assert!(!state.tasks["task"].cancelled);
            }
            drop(gate);
            persist.await;
            let row = service
                .conversation_repo
                .get("owner", "child-chat")
                .await
                .unwrap()
                .unwrap();
            let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
            assert_eq!(extra["subagent"]["status"], serde_json::to_value(status).unwrap());
        }
    }

    fn task(id: &str, turn: &str) -> ChildTask {
        ChildTask {
            user: "owner".into(),
            lineage: RouterChildMetadata {
                version: 1,
                root_conversation_id: "parent".into(),
                parent_conversation_id: "parent".into(),
                parent_turn_id: turn.into(),
                task_id: id.into(),
            },
            response: RouterChildResponse {
                task_id: id.into(),
                conversation_id: None,
                status: RouterChildStatus::PendingInit,
                result: None,
                truncated: false,
                error: None,
            },
            turn: None,
            cancelled: false,
            settled: false,
        }
    }
    #[test]
    fn router_child_ownership_and_turn_are_exact() {
        let mut state = ChildrenState::default();
        state.admit(task("a", "old")).unwrap();
        assert!(state.owned("owner", "parent", "old", "a").is_ok());
        for (user, parent, turn) in [
            ("foreign", "parent", "old"),
            ("owner", "other", "old"),
            ("owner", "parent", "new"),
        ] {
            assert!(state.owned(user, parent, turn, "a").is_err());
        }
    }
    #[tokio::test]
    async fn router_child_cancel_before_turn_creation_cannot_resume_in_new_parent_turn() {
        let service = crate::service_test::router_test_service();
        let mut claim = service.runtime_state.try_claim_turn("parent", "old").unwrap();
        service
            .router_children
            .state
            .lock()
            .unwrap()
            .admit(task("child", "old"))
            .unwrap();
        assert!(service.child_admitted("child"));
        service.close_router_children("owner", "parent", Some("old")).await;
        assert!(!service.child_admitted("child"));
        assert!(
            service
                .router_children
                .state
                .lock()
                .unwrap()
                .admit(task("late", "old"))
                .is_err()
        );
        claim.release();
        let _new = service.runtime_state.try_claim_turn("parent", "new").unwrap();
        assert!(!service.child_admitted("child"));
        assert!(service.check_parent_turn("parent", "old").is_err());
        service
            .router_children
            .state
            .lock()
            .unwrap()
            .admit(task("new-child", "new"))
            .unwrap();
        assert!(service.child_admitted("new-child"));
        // The cancelled old future may finish create after the next parent turn has spawned.
        // It must remain tracked until settled and its late on_started must not resurrect it.
        let late = service.record_router_child_started("child", "late-child-turn").unwrap();
        assert!(late.cancelled);
        assert_eq!(late.response.status, RouterChildStatus::Cancelling);
        assert!(!service.child_admitted("child"));
        assert!(service.record_router_child_started("missing", "turn").is_none());
        assert!(
            service
                .router_children
                .state
                .lock()
                .unwrap()
                .owned("owner", "parent", "new", "child")
                .is_err()
        );
    }
    #[test]
    fn router_child_limits_admission_before_provisioning() {
        let mut state = ChildrenState::default();
        for n in 0..MAX_ACTIVE_CHILDREN {
            state.admit(task(&n.to_string(), "turn")).unwrap();
        }
        assert!(state.admit(task("overflow", "turn")).is_err());
    }
    #[tokio::test]
    async fn router_child_delete_intent_survives_parent_turn_release_during_creation() {
        let service = crate::service_test::router_test_service();
        let mut claim = service.runtime_state.try_claim_turn("parent", "turn").unwrap();
        service
            .router_children
            .state
            .lock()
            .unwrap()
            .admit(task("pending", "turn"))
            .unwrap();
        service.close_router_children("owner", "parent", Some("turn")).await;
        assert!(
            !service.router_parent_is_deleting("parent"),
            "ordinary cancel retains child evidence"
        );
        service.mark_router_parent_deleting("parent");
        service.runtime_state.mark_deleting("parent");
        claim.release();
        assert!(!service.runtime_state.is_deleting("parent"));
        assert!(
            service.router_parent_is_deleting("parent"),
            "late create must delete its child even before parent SQL delete completes"
        );
        let mut state = service.router_children.state.lock().unwrap();
        state.reclaim_finished(|_, _| false);
        assert!(
            state.deleting.contains("parent"),
            "pending future retains the tombstone"
        );
        state.tasks.get_mut("pending").unwrap().settled = true;
        state.reclaim_finished(|_, _| false);
        assert!(!state.deleting.contains("parent"));
    }
    #[test]
    fn router_child_reclaims_finished_old_parents_but_keeps_live_results_and_provisioning() {
        let mut state = ChildrenState::default();
        for n in 0..MAX_TASKS {
            let mut entry = task(&n.to_string(), "old");
            entry.lineage.parent_conversation_id = format!("parent-{n}");
            entry.response.status = RouterChildStatus::Completed;
            entry.settled = true;
            state.admit(entry).unwrap();
        }
        let entry = state.tasks.get_mut("0").unwrap();
        entry.cancelled = true;
        entry.settled = false;
        entry.response.status = RouterChildStatus::Cancelling;
        state.reclaim_finished(|parent, _| parent == "parent-1");
        assert_eq!(state.tasks.len(), 2);
        assert!(state.tasks.contains_key("0"));
        assert!(state.tasks.contains_key("1"));
        state.admit(task("new", "new")).unwrap();
    }
    #[test]
    fn router_child_direct_cancel_is_scoped_and_cannot_report_completed() {
        let service = crate::service_test::router_test_service();
        let mut entry = task("task", "parent-turn");
        entry.turn = Some("child-turn".into());
        entry.response.conversation_id = Some("child-chat".into());
        service.router_children.state.lock().unwrap().admit(entry).unwrap();
        service.mark_router_child_cancelled("foreign", "child-chat", "child-turn");
        service.mark_router_child_cancelled("owner", "child-chat", "new-turn");
        assert!(!service.router_children.state.lock().unwrap().tasks["task"].cancelled);
        service.mark_router_child_cancelled("owner", "child-chat", "child-turn");
        let state = service.router_children.state.lock().unwrap();
        assert!(state.tasks["task"].cancelled);
        assert_eq!(state.tasks["task"].response.status, RouterChildStatus::Cancelling);
        assert!(!state.tasks["task"].response.status.terminal());
    }
    #[test]
    fn router_child_final_result_uses_latest_visible_finish_and_utf8_byte_limit() {
        let row = |content: &str, status: &str, hidden| MessageRow {
            id: "msg".into(),
            conversation_id: "child".into(),
            msg_id: None,
            r#type: "text".into(),
            content: serde_json::json!({"content":content}).to_string(),
            position: Some("left".into()),
            status: Some(status.into()),
            hidden,
            created_at: 0,
            backend_turn_id: None,
        };
        let rows = vec![
            row("older", "finish", false),
            row("last", "finish", false),
            row("secret", "finish", true),
            row("partial", "pending", false),
        ];
        assert_eq!(final_child_text(&rows), (Some("last".into()), false));
        let (text, truncated) = final_child_text(&[row(&"猫".repeat(12000), "finish", false)]);
        assert!(truncated);
        assert_eq!(text.unwrap().len(), 32766);
    }
    #[test]
    fn router_child_permissions_never_upgrade_unknown_or_restricted_modes() {
        for mode in ["default", "auto", "auto_edit", "acceptEdits"] {
            assert_eq!(inherited_permission(mode, "aionrs").unwrap(), "default");
            assert_eq!(inherited_permission(mode, "claude").unwrap(), "default");
            assert_eq!(inherited_permission(mode, "codex").unwrap(), "read-only");
        }
        assert_eq!(inherited_permission("read-only", "claude").unwrap(), "plan");
        assert!(inherited_permission("read-only", "aionrs").is_err());
        assert!(inherited_permission("custom", "claude").is_err());
        assert_eq!(inherited_permission("full-access", "aionrs").unwrap(), "yolo");
    }
    #[test]
    fn router_child_request_rejects_forged_authority_fields() {
        for field in ["user_id", "workspace", "parent_turn_id", "provider_id", "permission"] {
            let value = serde_json::json!({"tool":"spawn","task":"task","name":null,field:"forged"});
            assert!(serde_json::from_value::<RouterChildCall>(value).is_err());
        }
    }
}
