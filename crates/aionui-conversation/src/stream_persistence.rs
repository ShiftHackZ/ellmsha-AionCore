use std::{collections::HashMap, sync::Arc};

use aionui_ai_agent::protocol::events::{
    AgentStreamEvent, ErrorEventData, SubagentEventData, TipType, TipsEventData,
    tool_call::{AcpToolCallStatus, ToolCallStatus},
};
use aionui_api_types::{ConversationRuntimeSummary, WebSocketMessage};
use aionui_common::{ErrorChain, normalize_keys_to_snake_case, now_ms};
use aionui_db::models::{ConversationRow, MessageRow};
use aionui_db::{ConversationRowUpdate, DbError, IConversationRepository, MessageRowUpdate};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tracing::{debug, error, warn};

use crate::runtime_completion::RuntimeCompletionPublisher;
use crate::runtime_persistence::{RuntimePersistenceCoordinator, RuntimeWriteKind};
use crate::service::ConversationService;

fn is_not_found(err: &DbError) -> bool {
    matches!(err, DbError::NotFound(_))
}

fn is_foreign_key_constraint(err: &DbError) -> bool {
    err.to_string().contains("FOREIGN KEY constraint failed")
}

fn is_deleted_during_stream_persistence(err: &DbError) -> bool {
    is_not_found(err) || is_foreign_key_constraint(err)
}

fn log_persist_error(err: &DbError, message: &'static str) {
    if is_deleted_during_stream_persistence(err) {
        debug!(error = %ErrorChain(err), "{message}; conversation was likely deleted during stream finalization");
    } else {
        error!(error = %ErrorChain(err), "{message}");
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TextSegmentState {
    pub id: String,
    pub buffer: String,
    pub created_at: i64,
    pub record_created: bool,
    pub flush_counter: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedTextSegment {
    pub id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ThinkingSegmentState {
    pub id: String,
    pub buffer: String,
    pub started_at: i64,
}

struct SubagentTranscriptState {
    conversation_id: String,
    adapter: StreamPersistenceAdapter,
    active_text: Option<TextSegmentState>,
    active_thinking: Option<ThinkingSegmentState>,
}

/// Stores attributed child-thread output under a read-only conversation. The
/// relation is persisted in `extra`; rows are created only from live child
/// events, so historical mixed transcripts are never guessed.
pub(crate) struct SubagentTranscriptStore {
    user_id: String,
    parent_conversation_id: String,
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    states: HashMap<String, SubagentTranscriptState>,
}

impl SubagentTranscriptStore {
    pub(crate) fn new(
        user_id: String,
        parent_conversation_id: String,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
    ) -> Self {
        Self {
            user_id,
            parent_conversation_id,
            repo,
            broadcaster,
            states: HashMap::new(),
        }
    }

    fn child_id(&self, child_ref: &str) -> String {
        format!("subagent:{}:{}", self.parent_conversation_id, child_ref)
    }

    async fn ensure_conversation(&self, data: &SubagentEventData) -> Option<String> {
        let child_id = self.child_id(&data.r#ref);
        let existing = match self.repo.get(&self.user_id, &child_id).await {
            Ok(row) => row,
            Err(err) => {
                log_persist_error(&err, "Failed to load subagent conversation");
                return None;
            }
        };
        let row_status = data.status.as_deref().map(|status| {
            if matches!(status, "pending_init" | "running") {
                "running"
            } else {
                "finished"
            }
        });
        if let Some(row) = existing {
            if data.label.is_some() || data.status.is_some() || data.parent_ref.is_some() {
                let mut extra = serde_json::from_str::<serde_json::Value>(&row.extra).unwrap_or_else(|_| json!({}));
                merge_subagent_extra(&mut extra, data, &self.parent_conversation_id);
                let update = ConversationRowUpdate {
                    name: data.label.clone(),
                    name_source: data.label.as_ref().map(|_| "agent".to_owned()),
                    status: row_status.map(str::to_owned),
                    extra: Some(extra.to_string()),
                    updated_at: Some(now_ms()),
                    ..Default::default()
                };
                if let Err(err) = self.repo.update(&self.user_id, &child_id, &update).await {
                    log_persist_error(&err, "Failed to update subagent conversation");
                }
                self.broadcast_list_change(&child_id, "updated");
            }
            return Some(child_id);
        }

        let parent = match self.repo.get(&self.user_id, &self.parent_conversation_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return None,
            Err(err) => {
                log_persist_error(&err, "Failed to load parent conversation for subagent");
                return None;
            }
        };
        let mut extra = serde_json::from_str::<serde_json::Value>(&parent.extra).unwrap_or_else(|_| json!({}));
        if let Some(object) = extra.as_object_mut() {
            for key in [
                "backend_session_id",
                "acp_session_id",
                "acp_session_updated_at",
                "session_id",
                "cron_job_id",
                "pinned",
                "pinned_at",
                "fork",
            ] {
                object.remove(key);
            }
        }
        merge_subagent_extra(&mut extra, data, &self.parent_conversation_id);
        let now = now_ms();
        let row = ConversationRow {
            id: child_id.clone(),
            user_id: self.user_id.clone(),
            name: data.label.clone().unwrap_or_else(|| data.r#ref.clone()),
            r#type: parent.r#type,
            extra: extra.to_string(),
            model: parent.model,
            status: Some(row_status.unwrap_or("running").to_owned()),
            source: parent.source,
            channel_chat_id: parent.channel_chat_id,
            pinned: false,
            pinned_at: None,
            created_at: now,
            updated_at: now,
            project_id: parent.project_id,
            folder_id: parent.folder_id,
            name_source: Some("agent".to_owned()),
        };
        if let Err(err) = self.repo.create(&row).await
            && !matches!(err, DbError::Conflict(_))
        {
            log_persist_error(&err, "Failed to create subagent conversation");
            return None;
        }
        self.broadcast_list_change(&child_id, "created");
        Some(child_id)
    }

    fn broadcast_list_change(&self, child_id: &str, action: &str) {
        self.broadcaster.broadcast(WebSocketMessage::new(
            "conversation.listChanged",
            json!({
                "user_id": self.user_id,
                "conversation_id": child_id,
                "parent_conversation_id": self.parent_conversation_id,
                "action": action,
                "source": "subagent",
            }),
        ));
    }

    fn forward(
        broadcaster: &Arc<dyn EventBroadcaster>,
        user_id: &str,
        child_id: &str,
        msg_id: &str,
        child_ref: &str,
        event: &AgentStreamEvent,
    ) {
        let Ok(mut serialized) = serde_json::to_value(event) else {
            return;
        };
        normalize_keys_to_snake_case(&mut serialized);
        broadcaster.broadcast(WebSocketMessage::new(
            "message.stream",
            json!({
                "user_id": user_id,
                "conversation_id": child_id,
                "msg_id": msg_id,
                "turn_id": child_ref,
                "type": serialized.get("type").cloned().unwrap_or(json!("unknown")),
                "data": serialized.get("data").cloned().unwrap_or(json!({})),
                "hidden": false,
            }),
        ));
    }

    pub(crate) async fn apply(&mut self, data: &SubagentEventData) {
        let mut normalized = data.clone();
        if normalized.status.is_none() {
            normalized.status = match normalized.event.as_deref() {
                Some(AgentStreamEvent::Finish(_)) => Some("completed".to_owned()),
                Some(AgentStreamEvent::Error(_)) => Some("errored".to_owned()),
                _ => None,
            };
        }
        let data = &normalized;
        let Some(child_id) = self.ensure_conversation(data).await else {
            return;
        };
        let Some(event) = data.event.as_deref() else { return };
        let broadcaster = self.broadcaster.clone();
        let user_id = self.user_id.clone();
        let state = self
            .states
            .entry(data.r#ref.clone())
            .or_insert_with(|| SubagentTranscriptState {
                conversation_id: child_id.clone(),
                adapter: StreamPersistenceAdapter::new(
                    self.user_id.clone(),
                    child_id.clone(),
                    ConversationService::mint_msg_id(),
                    self.repo.clone(),
                    None,
                ),
                active_text: None,
                active_thinking: None,
            });
        let conversation_id = state.conversation_id.clone();
        match event {
            AgentStreamEvent::Text(text) => {
                if let Some(thinking) = state.active_thinking.take() {
                    let elapsed = now_ms().saturating_sub(thinking.started_at) as u64;
                    state.adapter.persist_thinking_segment(thinking, elapsed).await;
                }
                let segment = state.active_text.get_or_insert_with(|| TextSegmentState {
                    id: ConversationService::mint_msg_id(),
                    buffer: String::new(),
                    created_at: now_ms(),
                    record_created: false,
                    flush_counter: 0,
                });
                Self::forward(
                    &broadcaster,
                    &user_id,
                    &conversation_id,
                    &segment.id,
                    &data.r#ref,
                    event,
                );
                segment.buffer.push_str(&text.content);
                segment.flush_counter += 1;
                if segment.flush_counter >= 5 {
                    state.adapter.flush_text_segment(segment).await;
                    segment.flush_counter = 0;
                }
            }
            AgentStreamEvent::Thinking(thinking) => {
                if let Some(text) = state.active_text.take() {
                    state.adapter.finalize_text_segment(text, "finish").await;
                }
                if thinking.status.as_deref() == Some("done") {
                    if let Some(segment) = state.active_thinking.take() {
                        let elapsed = now_ms().saturating_sub(segment.started_at) as u64;
                        state.adapter.persist_thinking_segment(segment, elapsed).await;
                    }
                } else if !thinking.content.is_empty() || thinking.subject.is_some() {
                    let segment = state.active_thinking.get_or_insert_with(|| ThinkingSegmentState {
                        id: ConversationService::mint_msg_id(),
                        buffer: String::new(),
                        started_at: now_ms(),
                    });
                    segment.buffer.push_str(&thinking.content);
                    Self::forward(
                        &broadcaster,
                        &user_id,
                        &conversation_id,
                        &segment.id,
                        &data.r#ref,
                        event,
                    );
                }
            }
            AgentStreamEvent::ToolCall(tool) => {
                if let Some(text) = state.active_text.take() {
                    state.adapter.finalize_text_segment(text, "finish").await;
                }
                let mut tool = tool.clone();
                tool.call_id = format!("{}:{}", conversation_id, tool.call_id);
                let wrapped = AgentStreamEvent::ToolCall(tool.clone());
                Self::forward(
                    &broadcaster,
                    &user_id,
                    &conversation_id,
                    &tool.call_id,
                    &data.r#ref,
                    &wrapped,
                );
                state.adapter.persist_tool_call(&tool).await;
            }
            AgentStreamEvent::Tips(tip) => {
                Self::forward(&broadcaster, &user_id, &conversation_id, "", &data.r#ref, event);
                state.adapter.persist_tip(tip).await;
            }
            AgentStreamEvent::Error(error) => {
                if let Some(text) = state.active_text.take() {
                    state.adapter.finalize_text_segment(text, "error").await;
                }
                Self::forward(&broadcaster, &user_id, &conversation_id, "", &data.r#ref, event);
                state.adapter.persist_error_tip(error).await;
            }
            AgentStreamEvent::Finish(_) => {
                if let Some(thinking) = state.active_thinking.take() {
                    let elapsed = now_ms().saturating_sub(thinking.started_at) as u64;
                    state.adapter.persist_thinking_segment(thinking, elapsed).await;
                }
                if let Some(text) = state.active_text.take() {
                    state.adapter.finalize_text_segment(text, "finish").await;
                }
            }
            AgentStreamEvent::SegmentBreak => {
                if let Some(thinking) = state.active_thinking.take() {
                    let elapsed = now_ms().saturating_sub(thinking.started_at) as u64;
                    state.adapter.persist_thinking_segment(thinking, elapsed).await;
                }
                if let Some(text) = state.active_text.take() {
                    state.adapter.finalize_text_segment(text, "finish").await;
                }
            }
            AgentStreamEvent::Permission(_) | AgentStreamEvent::AcpPermission(_) => {}
            other => Self::forward(&broadcaster, &user_id, &conversation_id, "", &data.r#ref, other),
        }
    }
}

fn merge_subagent_extra(extra: &mut serde_json::Value, data: &SubagentEventData, parent_id: &str) {
    if !extra.is_object() {
        *extra = json!({});
    }
    let object = extra.as_object_mut().expect("object normalized above");
    let mut metadata = object.remove("subagent").unwrap_or_else(|| json!({}));
    if !metadata.is_object() {
        metadata = json!({});
    }
    let metadata = metadata.as_object_mut().expect("object normalized above");
    metadata.insert("parent_conversation_id".to_owned(), json!(parent_id));
    metadata.insert("ref".to_owned(), json!(data.r#ref));
    if let Some(label) = &data.label {
        metadata.insert("label".to_owned(), json!(label));
    }
    if let Some(parent_ref) = &data.parent_ref {
        metadata.insert("parent_ref".to_owned(), json!(parent_ref));
    }
    if let Some(status) = &data.status {
        metadata.insert("status".to_owned(), json!(status));
    }
    object.insert("subagent".to_owned(), serde_json::Value::Object(metadata.clone()));
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalTextOverride {
    pub msg_id: String,
    pub text: String,
    pub hidden: bool,
}

#[derive(Clone)]
pub(crate) struct StreamPersistenceAdapter {
    user_id: String,
    conversation_id: String,
    msg_id: String,
    repo: Arc<dyn IConversationRepository>,
    persistence: Option<RuntimePersistenceCoordinator>,
    /// The backend's own id for the in-flight turn (codex `Turn.id`), stamped
    /// onto every message row this adapter persists — the lookup key for
    /// `thread/fork`'s `lastTurnId`. Set by the relay on the internal
    /// `BackendTurnBound` frame; `None` for backends without one (claude/ACP).
    /// Shared across clones (the relay and its helpers clone the adapter).
    backend_turn_id: Arc<std::sync::Mutex<Option<String>>>,
}

impl StreamPersistenceAdapter {
    pub fn new(
        user_id: String,
        conversation_id: String,
        msg_id: String,
        repo: Arc<dyn IConversationRepository>,
        persistence: Option<RuntimePersistenceCoordinator>,
    ) -> Self {
        Self {
            user_id,
            conversation_id,
            msg_id,
            repo,
            persistence,
            backend_turn_id: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Record the backend's turn id for the in-flight turn (relay-only).
    pub(crate) fn set_backend_turn_id(&self, backend_turn_id: String) {
        *self.backend_turn_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(backend_turn_id);
    }

    /// The stamp every persisted message row of the current turn carries.
    /// Also read by the relay so live `message.stream` frames carry the anchor
    /// (without it, mid-history fork entries only appear after a reload).
    pub(crate) fn current_backend_turn_id(&self) -> Option<String> {
        self.backend_turn_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn with_persistence(mut self, persistence: RuntimePersistenceCoordinator) -> Self {
        self.persistence = Some(persistence);
        self
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %self.conversation_id))]
    pub async fn complete_conversation(
        &self,
        broadcaster: &Arc<dyn EventBroadcaster>,
        turn_id: &str,
        runtime: Option<ConversationRuntimeSummary>,
    ) {
        if let Some(persistence) = &self.persistence {
            RuntimeCompletionPublisher::new(
                self.user_id.clone(),
                self.repo.clone(),
                broadcaster.clone(),
                persistence.clone(),
            )
            .publish(&self.conversation_id, turn_id, runtime)
            .await;
            return;
        }

        let update = ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(&self.user_id, &self.conversation_id, &update).await {
            log_persist_error(&e, "Failed to update conversation status");
        }

        let payload = json!({
            "user_id": self.user_id,
            "conversation_id": self.conversation_id,
            "session_id": self.conversation_id,
            "turn_id": turn_id,
            "status": "finished",
            "canSendMessage": true,
            "runtime": runtime,
        });
        broadcaster.broadcast(WebSocketMessage::new("turn.completed", payload));

        debug!(conversation_id = %self.conversation_id, turn_id, status = "finished", "Turn completed");
    }

    fn allows_write(&self, kind: RuntimeWriteKind) -> bool {
        self.persistence
            .as_ref()
            .is_none_or(|persistence| persistence.allows(&self.conversation_id, kind))
    }

    #[tracing::instrument(skip_all)]
    pub async fn flush_text_segment(&self, segment: &mut TextSegmentState) {
        if !self.allows_write(RuntimeWriteKind::AssistantTextFlush) {
            return;
        }
        if segment.buffer.is_empty() {
            return;
        }

        let content = json!({ "content": segment.buffer }).to_string();

        if segment.record_created {
            let update = MessageRowUpdate {
                content: Some(content),
                status: Some(Some("work".into())),
                hidden: None,
            };
            if let Err(e) = self
                .repo
                .update_message(&self.user_id, &self.conversation_id, &segment.id, &update)
                .await
            {
                log_persist_error(&e, "Failed to update streaming text segment");
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some("work".into()),
                hidden: false,
                created_at: segment.created_at,
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                log_persist_error(&e, "Failed to create streaming text segment");
            }
            segment.record_created = true;
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn finalize_text_segment(&self, segment: TextSegmentState, status: &str) -> Option<PersistedTextSegment> {
        if !self.allows_write(RuntimeWriteKind::AssistantTextFinalize) {
            return None;
        }
        if segment.buffer.is_empty() {
            return None;
        }

        let content = json!({ "content": segment.buffer }).to_string();
        if segment.record_created {
            let update = MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            if let Err(e) = self
                .repo
                .update_message(&self.user_id, &self.conversation_id, &segment.id, &update)
                .await
            {
                log_persist_error(&e, "Failed to finalize text segment");
                return None;
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: segment.created_at,
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                log_persist_error(&e, "Failed to create finalized text segment");
                return None;
            }
        }

        Some(PersistedTextSegment { id: segment.id })
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_final_text(
        &self,
        text_segments: &[PersistedTextSegment],
        status: &str,
        final_text: &str,
        hidden: bool,
        rewrite_segments: bool,
    ) -> Vec<FinalTextOverride> {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return Vec::new();
        }

        let mut overrides = Vec::new();
        if let Some(primary_segment) = text_segments.first() {
            if rewrite_segments {
                let content = json!({ "content": final_text }).to_string();
                let update = MessageRowUpdate {
                    content: Some(content),
                    status: Some(Some(status.to_owned())),
                    hidden: Some(hidden),
                };
                if let Err(e) = self
                    .repo
                    .update_message(&self.user_id, &self.conversation_id, &primary_segment.id, &update)
                    .await
                {
                    log_persist_error(&e, "Failed to rewrite finalized text segment");
                }
                overrides.push(FinalTextOverride {
                    msg_id: primary_segment.id.clone(),
                    text: final_text.to_owned(),
                    hidden,
                });

                for segment in text_segments.iter().skip(1) {
                    let hide_update = MessageRowUpdate {
                        content: None,
                        status: Some(Some(status.to_owned())),
                        hidden: Some(true),
                    };
                    if let Err(e) = self
                        .repo
                        .update_message(&self.user_id, &self.conversation_id, &segment.id, &hide_update)
                        .await
                    {
                        log_persist_error(&e, "Failed to hide superseded text segment");
                    }
                    overrides.push(FinalTextOverride {
                        msg_id: segment.id.clone(),
                        text: String::new(),
                        hidden: true,
                    });
                }
            } else {
                for segment in text_segments {
                    let status_update = MessageRowUpdate {
                        content: None,
                        status: Some(Some(status.to_owned())),
                        hidden: Some(false),
                    };
                    if let Err(e) = self
                        .repo
                        .update_message(&self.user_id, &self.conversation_id, &segment.id, &status_update)
                        .await
                    {
                        log_persist_error(&e, "Failed to finalize text segment status");
                    }
                }
            }
        } else if !hidden {
            let row = MessageRow {
                id: self.msg_id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(self.msg_id.clone()),
                r#type: "text".into(),
                content: json!({ "content": final_text }).to_string(),
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                log_persist_error(&e, "Failed to create final fallback message");
            }
        }

        overrides
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_error_tip(&self, data: &ErrorEventData) {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return;
        }

        let content = json!({ "content": &data.message, "type": "error", "error": &data }).to_string();
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: self.conversation_id.clone(),
            msg_id: None,
            r#type: "tips".into(),
            content,
            position: Some("left".into()),
            status: Some("error".into()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
            log_persist_error(&e, "Failed to store error message");
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_tip(&self, data: &TipsEventData) {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return;
        }

        let status = match data.tip_type {
            TipType::Error => "error",
            TipType::Success | TipType::Warning | TipType::Info => "finish",
        };
        let content = json!({
            "content": &data.content,
            "type": &data.tip_type,
            "code": &data.code,
            "params": &data.params,
        })
        .to_string();
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: self.conversation_id.clone(),
            msg_id: None,
            r#type: "tips".into(),
            content,
            position: Some("left".into()),
            status: Some(status.into()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
            log_persist_error(&e, "Failed to store tip message");
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_thinking_segment(&self, segment: ThinkingSegmentState, duration_ms: u64) {
        // An empty segment should no longer be reachable: StreamRelay drops a
        // thinking chunk that has no text before it can open one (see the POLICY
        // note there). This stays as the second line of defense — persisting a
        // contentless row is what put a column of blank "thinking done · 0s" cards
        // into the reloaded view, so the storage layer refuses it too rather than
        // trusting every future caller to have filtered upstream.
        if segment.buffer.is_empty() {
            return;
        }
        if !self.allows_write(RuntimeWriteKind::AssistantThinkingFinalize) {
            return;
        }
        let content = json!({
            "content": segment.buffer,
            "status": "done",
            "duration_ms": duration_ms,
        })
        .to_string();
        let row = MessageRow {
            id: segment.id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(segment.id),
            r#type: "thinking".into(),
            content,
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: segment.started_at,
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
            log_persist_error(&e, "Failed to persist thinking message");
        }
    }

    /// Persist a Gemini-style tool_call event.
    #[tracing::instrument(skip_all)]
    pub async fn persist_tool_call(&self, data: &aionui_ai_agent::protocol::events::tool_call::ToolCallEventData) {
        if !self.allows_write(RuntimeWriteKind::ToolCallPersist) {
            return;
        }
        if data.call_id.trim().is_empty() {
            warn!(
                tool = %data.name,
                status = ?data.status,
                "Skipping tool_call persistence because call_id is empty"
            );
            return;
        }

        let status = match data.status {
            ToolCallStatus::Running => "work",
            ToolCallStatus::Completed => "finish",
            ToolCallStatus::Error => "error",
            // A cancelled call is terminal: the row must leave "work" so the
            // frontend spinner (hasRunningToolMessages) stops after interrupt.
            ToolCallStatus::Canceled => "finish",
        };
        let content = serde_json::to_string(data).unwrap_or_default();

        let row = MessageRow {
            id: data.call_id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(data.call_id.clone()),
            r#type: "tool_call".into(),
            content,
            position: Some("left".into()),
            status: Some(status.to_owned()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.upsert_message(&self.user_id, &row).await {
            error!(
                call_id = %data.call_id,
                tool = %data.name,
                status,
                error = %ErrorChain(&e),
                "Failed to upsert tool_call message"
            );
        } else {
            debug!(
                call_id = %data.call_id,
                tool = %data.name,
                status,
                "Upserted tool_call message"
            );
        }
    }

    /// Persist an ACP (Claude CLI) tool call event.
    #[tracing::instrument(skip_all)]
    pub async fn persist_acp_tool_call(
        &self,
        data: &aionui_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
    ) {
        if !self.allows_write(RuntimeWriteKind::AcpToolCallPersist) {
            return;
        }
        let tool_call_id = &data.update.tool_call_id;
        let status = match data.update.status {
            Some(AcpToolCallStatus::Pending) | None => "work",
            Some(AcpToolCallStatus::InProgress) => "work",
            Some(AcpToolCallStatus::Completed) => "finish",
            Some(AcpToolCallStatus::Failed) => "error",
        };

        let mut value = serde_json::to_value(data).unwrap_or_default();
        normalize_keys_to_snake_case(&mut value);
        let content = value.to_string();

        let row = MessageRow {
            id: tool_call_id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(tool_call_id.clone()),
            r#type: "acp_tool_call".into(),
            content,
            position: Some("left".into()),
            status: Some(status.to_owned()),
            hidden: false,
            created_at: now_ms(),
            backend_turn_id: self.current_backend_turn_id(),
        };
        if let Err(e) = self.repo.upsert_message(&self.user_id, &row).await {
            error!(error = %ErrorChain(&e), "Failed to upsert acp_tool_call message");
        }
    }

    /// Apply a STATUS-ONLY settle to an EXISTING tool_call row, if any.
    ///
    /// Returns whether the row existed. Never inserts: a `settle_only` frame is
    /// the pump settling a card it has no memory of (post-resume), and the same
    /// unknown-terminal shape also fires for workflow-internal refs that never
    /// had a row — inserting for those would conjure junk cards.
    #[tracing::instrument(skip_all)]
    pub async fn settle_tool_call_if_present(
        &self,
        data: &aionui_ai_agent::protocol::events::tool_call::ToolCallEventData,
    ) -> bool {
        let existing = self
            .repo
            .get_message_by_msg_id(&self.user_id, &self.conversation_id, &data.call_id, "tool_call")
            .await
            .unwrap_or(None);
        if existing.is_none() {
            debug!(call_id = %data.call_id, "settle-only frame for a row that does not exist; dropped");
            return false;
        }
        self.persist_tool_call(data).await;
        true
    }

    /// Persist a tool_group event (array of tool summaries).
    #[tracing::instrument(skip_all)]
    pub async fn persist_tool_group(&self, entries: &[aionui_ai_agent::protocol::events::tool_call::ToolGroupEntry]) {
        if !self.allows_write(RuntimeWriteKind::ToolGroupPersist) {
            return;
        }
        let all_done = entries.iter().all(|e| e.status.is_terminal());
        let status = if all_done { "finish" } else { "work" };
        let content = serde_json::to_string(entries).unwrap_or_default();

        let group_id = entries
            .first()
            .map(|e| e.call_id.clone())
            .unwrap_or_else(ConversationService::mint_msg_id);

        let existing = self
            .repo
            .get_message_by_msg_id(&self.user_id, &self.conversation_id, &group_id, "tool_group")
            .await
            .unwrap_or(None);

        if existing.is_some() {
            let update = MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(e) = self
                .repo
                .update_message(&self.user_id, &self.conversation_id, &group_id, &update)
                .await
            {
                error!(error = %ErrorChain(&e), "Failed to update tool_group message");
            }
        } else {
            let row = MessageRow {
                id: group_id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(group_id),
                r#type: "tool_group".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
                backend_turn_id: self.current_backend_turn_id(),
            };
            if let Err(e) = self.repo.insert_message(&self.user_id, &row).await {
                error!(error = %ErrorChain(&e), "Failed to persist tool_group message");
            }
        }
    }
}
