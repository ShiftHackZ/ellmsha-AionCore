//! Conversation working brief: user-source references and separately labelled evidence.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskConstraintKind {
    Branch,
    Environment,
    Connector,
    BrowserTarget,
    Receiver,
    Acceptance,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskConstraint {
    /// Stable key within this brief (1–64 ASCII alphanumeric, '_' or '-').
    pub key: String,
    pub kind: TaskConstraintKind,
    /// Agent summary of the user's requirement, at most 512 UTF-8 bytes. No secrets.
    pub value: String,
    /// Visible user message in this conversation, never a tool/assistant message.
    pub source_message_id: String,
    /// Exact nonempty excerpt (at most 512 bytes), validated against that message.
    pub source_quote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskEvidenceLevel {
    Unit,
    Integration,
    Ui,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskReportedResult {
    Passed,
    Failed,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskEvidence {
    /// Key of an acceptance constraint in the same replacement brief.
    pub criterion_key: String,
    /// Unit, integration, UI and manual evidence are not interchangeable.
    pub level: TaskEvidenceLevel,
    /// Model-reported interpretation; reference existence is the only server verification.
    pub reported_result: TaskReportedResult,
    /// One to eight existing tool-call message IDs in this conversation.
    pub message_ids: Vec<String>,
    /// Bounded factual observation (512 bytes), not a copy of private tool output.
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateTaskContextRequest {
    /// Revision returned by GET; 0 for a conversation without a brief.
    pub expected_revision: u64,
    /// Latest visible user-message ID returned by GET; stale updates return 409.
    pub based_on_user_message_id: String,
    /// Full replacement, at most 16 entries. Retain still-applicable earlier constraints.
    pub constraints: Vec<TaskConstraint>,
    /// Full replacement, at most 16 entries. Empty means no recorded acceptance evidence.
    pub evidence: Vec<TaskEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskContext {
    pub revision: u64,
    /// Server timestamp in milliseconds; content remains agent-recorded working data.
    pub updated_at: i64,
    pub based_on_user_message_id: String,
    pub constraints: Vec<TaskConstraint>,
    pub evidence: Vec<TaskEvidence>,
}

/// GET/PUT /api/conversations/{id}/task-context response, inside ApiResponse.data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskContextResponse {
    pub conversation_id: String,
    pub context: Option<TaskContext>,
    /// None before the first visible user message.
    pub latest_user_message_id: Option<String>,
    /// A newer user message exists: reconcile it before treating this brief as current.
    pub needs_reconciliation: bool,
}
