//! Private runtime router gateway contracts. All ownership comes from runtime authentication.
use serde::{Deserialize, Serialize};

/// Immutable host-minted lineage in conversation.extra.router_child.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouterChildMetadata {
    /// Marker format, currently 1.
    pub version: u8,
    /// Owning root conversation, retained for nested children.
    pub root_conversation_id: String,
    /// Immediate caller conversation; only this caller can control this task.
    pub parent_conversation_id: String,
    /// Exact caller turn that admitted the task.
    pub parent_turn_id: String,
    /// Opaque host-generated task identifier.
    pub task_id: String,
}
/// POST /api/runtime/router-tools/call. Runtime headers bind user and parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouterChildCall {
    /// Start one host-routed child using the frozen parent policy.
    Spawn {
        /// Nonempty task, at most 128 KiB; treated as task data, not authority.
        task: String,
        /// Optional display label, at most 80 bytes.
        name: Option<String>,
    },
    /// Observe one owned task. A zero timeout is a status snapshot.
    Wait {
        /// Opaque identifier returned by Spawn.
        task_id: String,
        /// Maximum wait in milliseconds, clamped to 50,000.
        #[serde(default)]
        timeout_ms: u64,
    },
    /// Cancel this caller's exact child turn, including pending provisioning.
    Cancel {
        /// Opaque identifier returned by Spawn.
        task_id: String,
    },
}
/// Host lifecycle; waiting for user approval is not completion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouterChildStatus {
    PendingInit,
    Running,
    WaitingForApproval,
    /// Cancellation requested; provisioning/execution has not yet drained.
    Cancelling,
    Completed,
    Errored,
    Interrupted,
}
impl RouterChildStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Errored | Self::Interrupted)
    }
}
/// Bounded gateway/tool result; never includes provider credentials or full chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterChildResponse {
    /// Opaque task identifier.
    pub task_id: String,
    /// Child chat for inspection/approval; absent until creation completes.
    pub conversation_id: Option<String>,
    /// Current authoritative lifecycle status.
    pub status: RouterChildStatus,
    /// Final persisted assistant text, truncated to 32 KiB when necessary.
    pub result: Option<String>,
    /// Whether result was truncated; open the child chat for the complete response.
    pub truncated: bool,
    /// Safe host error explaining failed provisioning/execution.
    pub error: Option<String>,
}
