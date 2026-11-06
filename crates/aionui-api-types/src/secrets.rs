//! Secrets management contracts. Raw fields are intentionally returned only by explicit reads.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    Login,
    ApiKey,
    SecureNote,
    Structured,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    Text,
    Password,
    Secret,
    Note,
}

#[derive(schemars::JsonSchema, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretField {
    /// Record or field presentation type; never changes authorization.
    pub kind: FieldKind,
    /// Plaintext field value, delivered only by explicit authorized reads.
    pub value: String,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MfaRequirement {
    None,
    Required,
    Unknown,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretBinding {
    /// Human-readable service or application name.
    pub service: String,
    /// Explicit environment label, such as staging or production.
    pub environment: String,
    /// Tenant or organization label; separate bindings may use different credentials.
    pub tenant: String,
    /// Account role or intended automation purpose.
    pub role: String,
    /// Exact origin, application ID, or CLI target; never interpreted as an access grant.
    pub target: String,
    /// Whether human multifactor authentication is required; unknown must prompt the human.
    pub mfa: MfaRequirement,
    /// Nonsecret MFA method label; never a code, seed or recovery credential.
    pub mfa_method: String,
    /// Nonsecret instructions; OTP seeds/codes and recovery material are not supported.
    pub mfa_instructions: String,
}

#[derive(schemars::JsonSchema, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretRecordInput {
    /// Human-readable record title, encrypted at rest.
    pub title: String,
    /// Record or field presentation type; never changes authorization.
    pub kind: SecretKind,
    /// Named fields; values are encrypted at rest and omitted from metadata listings.
    pub fields: BTreeMap<String, SecretField>,
    /// Explicit service/environment/tenant associations; empty means no associations recorded.
    pub bindings: Vec<SecretBinding>,
}

#[derive(schemars::JsonSchema, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretPatch {
    /// Human-readable record title, encrypted at rest.
    pub title: Option<String>,
    /// Record or field presentation type; never changes authorization.
    pub kind: Option<SecretKind>,
    /// Missing keys remain unchanged; null explicitly removes the named field.
    #[serde(default)]
    pub fields: BTreeMap<String, Option<SecretField>>,
    /// Explicit service/environment/tenant associations; empty means no associations recorded.
    pub bindings: Option<Vec<SecretBinding>>,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretScope {
    /// Null means global. A project ID must be owned and Standard at the service boundary.
    pub project_id: Option<String>,
}

#[derive(schemars::JsonSchema, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretWriteRequest {
    /// Requested human scope; agent tools derive scope from authenticated conversation binding.
    pub scope: SecretScope,
    /// Caller-generated UUID retained for idempotent mutation retries.
    pub operation_id: String,
    /// Opaque record identifier; does not grant access without the matching owner and scope.
    pub id: Option<String>,
    /// Current metadata revision; pair with expected_incarnation for compare-and-swap.
    pub expected_revision: Option<u64>,
    /// Current metadata incarnation, required with an ID; changes after restore.
    pub expected_incarnation: Option<String>,
    /// Full record for creation; mutually exclusive with an update patch.
    pub record: Option<SecretRecordInput>,
    /// Partial update for an existing record; requires its metadata revision and incarnation.
    pub patch: Option<SecretPatch>,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretReadRequest {
    /// Requested human scope; agent tools derive scope from authenticated conversation binding.
    pub scope: SecretScope,
    /// Opaque record identifier; does not grant access without the matching owner and scope.
    pub id: String,
    /// Named fields; values are encrypted at rest and omitted from metadata listings.
    pub fields: Option<Vec<String>>,
    /// Optional zero-based binding index; selects exactly one association for the read.
    pub binding: Option<usize>,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretDeleteRequest {
    /// Requested human scope; agent tools derive scope from authenticated conversation binding.
    pub scope: SecretScope,
    /// Opaque record identifier; does not grant access without the matching owner and scope.
    pub id: String,
    /// Current metadata revision; pair with expected_incarnation for compare-and-swap.
    pub expected_revision: u64,
    /// Current metadata incarnation; rejects mutations approved before restore.
    pub expected_incarnation: String,
    /// Caller-generated UUID retained for idempotent mutation retries.
    pub operation_id: String,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretListRequest {
    /// Requested human scope; agent tools derive scope from authenticated conversation binding.
    pub scope: SecretScope,
    #[serde(default)]
    /// Literal case-insensitive metadata search text; never a regular expression.
    pub query: String,
    /// Human-readable service or application name.
    pub service: Option<String>,
    /// Explicit environment label, such as staging or production.
    pub environment: Option<String>,
    /// Tenant or organization label; separate bindings may use different credentials.
    pub tenant: Option<String>,
    /// Record or field presentation type; never changes authorization.
    pub kind: Option<SecretKind>,
    /// Opaque continuation bound to owner, scope and filters; null ends pagination.
    pub cursor: Option<String>,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
pub struct SecretMetadata {
    /// Opaque vault branch UUID; changes after restore, required for updates/deletes.
    pub incarnation: String,
    /// Opaque record identifier; does not grant access without the matching owner and scope.
    pub id: String,
    /// Revision within one vault incarnation; pair with incarnation to reject stale writes.
    pub revision: u64,
    /// Requested human scope; agent tools derive scope from authenticated conversation binding.
    pub scope: SecretScope,
    /// Human-readable record title, encrypted at rest.
    pub title: String,
    /// Record or field presentation type; never changes authorization.
    pub kind: SecretKind,
    /// Explicit service/environment/tenant associations; empty means no associations recorded.
    pub bindings: Vec<SecretBinding>,
}

#[derive(schemars::JsonSchema, Clone, Serialize, Deserialize)]
pub struct SecretReadResponse {
    /// True when any selected binding requires MFA or its MFA requirement is unknown.
    /// Ask the human to complete authentication; no OTP is stored or generated.
    pub human_action_required: bool,
    #[serde(flatten)]
    /// Metadata for the selected record, flattened into the response.
    pub metadata: SecretMetadata,
    /// Named fields; values are encrypted at rest and omitted from metadata listings.
    pub fields: BTreeMap<String, SecretField>,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
pub struct SecretListResponse {
    /// One bounded page of metadata; excludes raw field values.
    pub records: Vec<SecretMetadata>,
    /// Opaque continuation bound to owner, scope and filters; null ends pagination.
    pub cursor: Option<String>,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretMutationReceipt {
    /// Opaque record identifier; does not grant access without the matching owner and scope.
    pub id: String,
    /// Revision within one vault incarnation; pair with incarnation to reject stale writes.
    pub revision: u64,
    /// True for a completed deletion, false for a create or update.
    pub deleted: bool,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
pub struct VaultStatus {
    /// Current vault branch UUID, null before initialization; changes after restore.
    pub incarnation: Option<String>,
    /// Whether the owner already has a vault; false permits first-time initialization.
    pub initialized: bool,
    /// Whether an unexpired decrypted key is available in backend memory.
    pub unlocked: bool,
    /// Stable vault UUID, or null when the vault has not been initialized.
    pub vault_id: Option<String>,
}

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultPassphraseRequest {
    /// Transient user-supplied passphrase; never logged or stored as plaintext.
    pub passphrase: String,
}

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultChangePassphraseRequest {
    /// Current passphrase required to authenticate key-envelope rotation.
    pub current_passphrase: String,
    /// Replacement passphrase used to wrap the existing vault key.
    pub new_passphrase: String,
}

#[derive(schemars::JsonSchema, Serialize, Deserialize)]
pub struct VaultBackup {
    /// Backup envelope version; unsupported versions are rejected.
    pub format: u32,
    /// Base64 encrypted vault snapshot including its encrypted key envelope.
    pub encrypted_data: String,
}

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultRestoreRequest {
    /// Encrypted backup to validate completely before atomic replacement.
    pub backup: VaultBackup,
    /// Transient user-supplied passphrase; never logged or stored as plaintext.
    pub passphrase: String,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretHistoryScope {
    CurrentConversation,
    CurrentProject,
    TemporaryHistory,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretHistorySearchRequest {
    /// Requested history scope; the server checks it against the authenticated conversation binding.
    pub scope: Option<SecretHistoryScope>,
    #[serde(default)]
    /// Literal payload search text; recognized credential labels select their values. Never a regular expression.
    pub query: String,
    #[serde(default)]
    /// Opt in to scanning assistant payload text; false by default.
    pub include_assistant: bool,
    #[serde(default)]
    /// Opt in to scanning supported tool payload strings; false by default.
    pub include_tools: bool,
    /// Opaque continuation bound to owner, scope and filters; null ends pagination.
    pub cursor: Option<String>,
}

#[derive(schemars::JsonSchema, Serialize, Deserialize)]
pub struct SecretHistoryCandidate {
    /// Opaque ten-minute selection handle bound to caller, scope and source revision.
    pub candidate_id: String,
    /// Owned conversation identifier containing this candidate.
    pub conversation_id: String,
    /// Canonical message identifier containing the selected occurrence.
    pub message_id: String,
    /// JSON pointer to the allowlisted payload string; structural identifiers are excluded.
    pub pointer: String,
    /// Revision within one vault incarnation; pair with incarnation to reject stale writes.
    pub revision: i64,
    /// Plaintext field value, delivered only by explicit authorized reads.
    pub value: String,
    /// Short surrounding context with detected value concealed.
    pub masked_context: String,
}

#[derive(schemars::JsonSchema, Serialize, Deserialize)]
pub struct SecretHistorySearchResponse {
    /// Bounded candidate page; raw values require the same explicit approval as reads.
    pub candidates: Vec<SecretHistoryCandidate>,
    /// Opaque continuation bound to owner, scope and filters; null ends pagination.
    pub cursor: Option<String>,
    /// Whether this bounded scan reached the end of the selected history.
    pub complete: bool,
    /// Number of canonical messages inspected in this page.
    pub scanned_messages: usize,
    /// Oversized messages are skipped without reading their content.
    pub skipped_oversized_messages: usize,
    /// Candidate detection is heuristic, never proof that all credentials were found.
    pub exhaustive: bool,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretHistoryMaskRequest {
    /// Caller-generated UUID retained for idempotent mutation retries.
    pub operation_id: String,
    /// Explicit selection of at most fifty candidate handles to mask atomically.
    pub candidate_ids: Vec<String>,
}

#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
pub struct SecretHistoryMaskReceipt {
    /// Owned conversations containing changed canonical messages, for scoped cache invalidation.
    pub conversation_ids: Vec<String>,
    /// Number of selected occurrences successfully replaced.
    pub selected_occurrences: usize,
    /// Changed message IDs mapped to their committed canonical revisions.
    pub revisions: BTreeMap<String, i64>,
    /// Always true: editing canonical history does not erase provider/runtime context.
    pub runtime_context_not_erased: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecretGroup {
    /// Requested human scope; agent tools derive scope from authenticated conversation binding.
    pub scope: SecretScope,
    /// Human-readable scope display name.
    pub name: String,
    /// Project is retained in storage but absent from the active project picker.
    pub retained: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretGroupsRequest {
    /// Owned conversation identifier; omitted for the all-project human settings view.
    pub conversation_id: Option<String>,
}

/// Transient desktop-main IPC wrapping payload. Never returned by an agent tool.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopVaultKey {
    /// Stable vault UUID, or null when the vault has not been initialized.
    pub vault_id: String,
    /// Digest of the current wrapped-key envelope; rotation invalidates old device wrappers.
    pub envelope_id: String,
    /// Transient base64 vault key restricted to Electron main, never the renderer or agent.
    pub key: String,
}

/// Optional human chat binding, flattened only at the authenticated records boundary.
/// Settings omits it; chat requests must preserve the project resolved when the view opened.
#[derive(Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretHumanContext {
    /// Current owned conversation; never used by agent runtime routes.
    pub conversation_id: Option<String>,
    /// Expected owned Standard project, null for temporary chats; rejects stale same-chat UI.
    pub expected_project_id: Option<String>,
}
