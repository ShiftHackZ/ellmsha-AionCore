//! Transient human-input rendezvous for MCP-capable workers without a native question wire.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::WebSocketMessage;
use aionui_common::{Confirmation, QuestionAnswer, UserQuestion, generate_id};
use aionui_realtime::EventBroadcaster;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::runtime_persistence::RuntimeWriteKind;
use crate::{ConversationError, ConversationService};

pub(crate) struct Pending {
    owner: String,
    chat: String,
    form: Confirmation,
    reply: Option<oneshot::Sender<(Value, oneshot::Sender<()>)>>,
}

impl Pending {
    fn awaiting(&self, owner: &str, chat: &str) -> bool {
        self.owner == owner && self.chat == chat && self.reply.is_some()
    }
}

pub(crate) type Registry = Arc<Mutex<HashMap<String, Pending>>>;

struct OpenForm<'a> {
    registry: &'a Registry,
    bus: &'a dyn EventBroadcaster,
    owner: &'a str,
    chat: &'a str,
    id: String,
}

impl Drop for OpenForm<'_> {
    fn drop(&mut self) {
        self.registry.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
        self.bus.broadcast(WebSocketMessage::new(
            "confirmation.remove",
            json!({"user_id":self.owner,"conversation_id":self.chat,"id":self.id}),
        ));
    }
}

impl ConversationService {
    /// Retain accepted answers that resume an existing turn instead of sending a
    /// new chat message. The transcript is display/history only, never a second
    /// provider send. Secret fields are not retained in this projection.
    pub(crate) async fn persist_question_reply(
        &self,
        owner: &str,
        chat: &str,
        form: &Confirmation,
        data: &Value,
        created_at: i64,
    ) -> Result<(), ConversationError> {
        if form.questions.is_empty() || data.get("answers").is_none() {
            return Ok(());
        }
        let answers: Vec<QuestionAnswer> = serde_json::from_value(data["answers"].clone())
            .map_err(|_| ConversationError::bad_request("Invalid answer set"))?;
        QuestionAnswer::validate(&form.questions, &answers).map_err(ConversationError::bad_request)?;
        let replies: Vec<_> = form
            .questions
            .iter()
            .map(|question| {
                let answer = answers.iter().find(|a| a.question == question.id);
                json!({
                    "questionItemId": question.id,
                    "question": question.question,
                    "answer": if question.is_secret { "••••••".into() } else {
                        answer.map(|a| a.labels.join(", ")).unwrap_or_default()
                    }
                })
            })
            .collect();
        let content = format!(
            "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
            json!(replies)
        );
        if !self.runtime_persistence().allows(chat, RuntimeWriteKind::UserMessage) {
            return Ok(());
        }
        let id = Self::mint_msg_id();
        let message = aionui_db::models::MessageRow {
            revision: 0,
            redacted: false,

            id: id.clone(),
            conversation_id: chat.into(),
            msg_id: Some(id.clone()),
            r#type: "text".into(),
            content: json!({"content":content}).to_string(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at,
            backend_turn_id: None,
        };
        self.conversation_repo
            .insert_message(owner, &message)
            .await
            .map_err(|error| {
                tracing::error!(
                    conversation_id = chat,
                    call_id = form.call_id,
                    "Accepted question reply could not be stored"
                );
                ConversationError::from(error)
            })?;
        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.userCreated",
            json!({
                "user_id":owner,"conversation_id":chat,"msg_id":id,"content":content,
                "position":"right","status":"finish","hidden":false,"created_at":created_at
            }),
        ));
        Ok(())
    }

    /// Questions are live only while the originating worker owns the same chat turn.
    pub async fn ask_user(
        &self,
        tasks: &dyn IWorkerTaskManager,
        token: &str,
        chat: &str,
        questions: Vec<UserQuestion>,
    ) -> Result<Value, ConversationError> {
        let (cap, _) = tasks
            .memory_runtime(chat, token)
            .ok_or_else(|| ConversationError::Unauthorized {
                reason: "Invalid worker capability".into(),
            })?;
        let turn = self
            .runtime_state()
            .active_turn_id_for(chat)
            .ok_or_else(|| ConversationError::Busy {
                reason: "The originating turn is no longer active".into(),
            })?;
        let valid = || async {
            let row = self
                .conversation_repo
                .get(&cap.user, chat)
                .await
                .map_err(|_| ConversationError::BadGateway {
                    reason: "Could not check chat owner".into(),
                })?
                .ok_or_else(|| ConversationError::Forbidden {
                    reason: "Chat is unavailable".into(),
                })?;
            let extra: Value = serde_json::from_str(&row.extra)
                .map_err(|_| ConversationError::bad_request("Invalid chat workspace"))?;
            if tasks.memory_runtime(chat, token).is_none()
                || !cap.matches_binding(
                    &row.user_id,
                    row.project_id.as_deref(),
                    extra["workspace"].as_str().unwrap_or_default(),
                )
                || self.runtime_state().active_turn_id_for(chat).as_ref() != Some(&turn)
            {
                return Err(ConversationError::Forbidden {
                    reason: "The originating worker, workspace or turn changed".into(),
                });
            }
            Ok(())
        };
        self.ask_user_authorized(&cap.user, chat, questions, valid).await
    }

    /// Neutral human-input boundary. The caller revalidates its dedicated runtime capability
    /// throughout the wait; no memory capability or second approval system is involved.
    pub async fn ask_user_authorized<F, Fut>(
        &self,
        owner: &str,
        chat: &str,
        questions: Vec<UserQuestion>,
        valid: F,
    ) -> Result<Value, ConversationError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), ConversationError>>,
    {
        let turn = self
            .runtime_state()
            .active_turn_id_for(chat)
            .ok_or_else(|| ConversationError::Busy {
                reason: "The originating turn is no longer active".into(),
            })?;
        valid().await?;
        if self.runtime_state().active_turn_id_for(chat).as_ref() != Some(&turn) {
            return Err(ConversationError::Forbidden {
                reason: "The originating turn changed".into(),
            });
        }
        if !UserQuestion::valid_form(&questions) {
            return Err(ConversationError::bad_request("Invalid question form"));
        }
        let id = format!("host-question:{}", generate_id());
        let form = Confirmation {
            id: id.clone(),
            call_id: id.clone(),
            title: None,
            action: None,
            description: String::new(),
            command_type: None,
            questions,
            options: vec![],
        };
        let (reply, mut rx) = oneshot::channel();
        {
            let mut pending = self.host_questions.lock().unwrap_or_else(|e| e.into_inner());
            if pending.len() >= 128 || pending.values().any(|p| p.chat == chat) {
                return Err(ConversationError::Busy {
                    reason: "A question is already pending".into(),
                });
            }
            pending.insert(
                id.clone(),
                Pending {
                    owner: owner.to_owned(),
                    chat: chat.to_owned(),
                    form: form.clone(),
                    reply: Some(reply),
                },
            );
        }
        let _form = OpenForm {
            registry: &self.host_questions,
            bus: self.broadcaster.as_ref(),
            owner,
            chat,
            id: id.clone(),
        };
        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.stream",
            json!({
                "user_id": owner,
                "conversation_id": chat,
                "turn_id": turn,
                "msg_id": id,
                "type": "permission",
                "data": form,
                "position": "left"
            }),
        ));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
        loop {
            tokio::select! {
                    _=tokio::time::sleep_until(deadline)=>return Err(ConversationError::Busy{reason:"Question timed out".into()}),
                    result = &mut rx => {
                        let (answer, accepted) = result.map_err(|_| ConversationError::Busy { reason: "Question closed".into() })?;
                        valid().await?;
            if self.runtime_state().active_turn_id_for(chat).as_ref()!=Some(&turn){return Err(ConversationError::Forbidden{reason:"The originating turn changed".into()});}
                        accepted.send(()).map_err(|_| ConversationError::Busy { reason: "Question response was cancelled".into() })?;
                        return Ok(answer);
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                        valid().await?;
            if self.runtime_state().active_turn_id_for(chat).as_ref()!=Some(&turn){return Err(ConversationError::Forbidden{reason:"The originating turn changed".into()});}
                    }
                }
        }
    }

    pub fn pending_host_question_count(&self, owner: &str, chat: &str) -> usize {
        self.host_questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|p| p.awaiting(owner, chat))
            .count()
    }

    pub(crate) fn host_forms(&self, owner: &str, chat: &str) -> Vec<Confirmation> {
        self.host_questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|p| p.awaiting(owner, chat))
            .map(|p| p.form.clone())
            .collect()
    }

    pub(crate) async fn answer_host_question(
        &self,
        owner: &str,
        chat: &str,
        id: &str,
        data: &Value,
    ) -> Result<(), ConversationError> {
        let accepted = {
            let mut pending = self.host_questions.lock().unwrap_or_else(|e| e.into_inner());
            let target = pending
                .get_mut(id)
                .filter(|p| p.awaiting(owner, chat))
                .ok_or_else(|| ConversationError::NotFound { id: id.into() })?;
            let response = if data["value"] == "reject" && data.get("answers").is_none() {
                json!({"declined":true})
            } else {
                let answers: Vec<aionui_common::QuestionAnswer> = serde_json::from_value(data["answers"].clone())
                    .map_err(|_| ConversationError::BadRequest {
                        reason: "Invalid answer set".into(),
                    })?;
                aionui_common::QuestionAnswer::validate(&target.form.questions, &answers)
                    .map_err(|reason| ConversationError::BadRequest { reason: reason.into() })?;
                json!({"answers":data["answers"]})
            };
            let (ack, accepted) = oneshot::channel();
            target
                .reply
                .take()
                .expect("pending reply")
                .send((response, ack))
                .map_err(|_| ConversationError::NotFound { id: id.into() })?;
            accepted
        };
        accepted
            .await
            .map_err(|_| ConversationError::NotFound { id: id.into() })
    }
}
