use crate::service::SecretsError;
use aionui_api_types::secrets::*;
use aionui_db::{HistoryEdit, HistoryScope, ISecretHistoryRepository};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
struct Candidate {
    owner: String,
    caller: String,
    scope: HistoryScope,
    message: String,
    revision: i64,
    pointer: String,
    start: usize,
    end: usize,
    digest: [u8; 32],
    created: Instant,
}
#[derive(Clone)]
struct Cursor {
    owner: String,
    caller: String,
    scope: HistoryScope,
    request: String,
    after: String,
    offset: usize,
    revision: Option<i64>,
    message: Option<String>,
    created: Instant,
}

pub struct SecretHistoryService {
    repository: Arc<dyn ISecretHistoryRepository>,
    candidates: Mutex<BTreeMap<String, Candidate>>,
    cursors: Mutex<BTreeMap<String, Cursor>>,
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}
fn alive(created: Instant) -> bool {
    created.elapsed() < Duration::from_secs(600)
}

impl SecretHistoryService {
    pub fn new(repository: Arc<dyn ISecretHistoryRepository>) -> Self {
        Self {
            repository,
            candidates: Mutex::new(BTreeMap::new()),
            cursors: Mutex::new(BTreeMap::new()),
        }
    }
    /// `project` is a currently owned Standard project, resolved by the runtime boundary.
    pub async fn search(
        &self,
        owner: &str,
        caller: &str,
        project: Option<&str>,
        mut request: SecretHistorySearchRequest,
    ) -> Result<SecretHistorySearchResponse, SecretsError> {
        if request.query.len() > 512 {
            return Err(SecretsError::InvalidInput);
        }
        let scope = match request.scope.clone().unwrap_or(SecretHistoryScope::CurrentConversation) {
            SecretHistoryScope::CurrentConversation => HistoryScope::Conversation(caller.into()),
            SecretHistoryScope::CurrentProject => {
                HistoryScope::Project(project.ok_or(SecretsError::ProjectScopeRequired)?.into())
            }
            SecretHistoryScope::TemporaryHistory if project.is_none() => HistoryScope::Temporary,
            _ => return Err(SecretsError::Forbidden),
        };
        let cursor = request.cursor.take();
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&request).map_err(|_| SecretsError::InvalidInput)?)
        );
        let (after, resume_offset, resume_revision, resume_message) = if let Some(id) = cursor {
            let mut cache = self.cursors.lock().await;
            cache.retain(|_, c| alive(c.created));
            let c = cache
                .get(&id)
                .filter(|c| c.owner == owner && c.caller == caller && c.scope == scope && c.request == fingerprint)
                .ok_or(SecretsError::Conflict)?;
            (c.after.clone(), c.offset, c.revision, c.message.clone())
        } else {
            (String::new(), 0, None, None)
        };
        let rows = self
            .repository
            .page(owner, &scope, &after, request.include_assistant, request.include_tools)
            .await?;
        if let Some(id) = &resume_message
            && rows.first().is_none_or(|row| &row.id != id)
        {
            return Err(SecretsError::Conflict);
        }
        let mut candidates = vec![];
        let mut scanned = 0;
        let mut bytes = 0;
        let mut last = after;
        let mut complete = true;
        let mut offset = 0;
        let mut revision = None;
        let mut message = None;
        let mut skipped_oversized_messages = 0;
        let mut cache = self.candidates.lock().await;
        cache.retain(|_, c| alive(c.created));
        'rows: for (row_index, row) in rows.into_iter().enumerate() {
            if scanned == 200 {
                complete = false;
                break;
            }
            if row.byte_size > 1024 * 1024 {
                scanned += 1;
                skipped_oversized_messages += 1;
                last = row.id;
                continue;
            }
            if row.content.is_empty() && row.byte_size > 0 || bytes + row.content.len() > 1024 * 1024 {
                complete = false;
                break;
            }
            scanned += 1;
            bytes += row.content.len();
            if row_index == 0 && resume_revision.is_some_and(|value| value != row.revision) {
                return Err(SecretsError::Conflict);
            }
            let Ok(value) = serde_json::from_str::<Value>(&row.content) else {
                last = row.id;
                continue;
            };
            let mut leaves = vec![];
            payload_leaves(&value, &row.r#type, &mut leaves);
            let mut span_index = 0;
            for (pointer, text) in leaves {
                for (start, end) in payload_spans(text, &pointer, &request.query) {
                    let current_index = span_index;
                    span_index += 1;
                    if row_index == 0 && current_index < resume_offset {
                        continue;
                    }
                    if candidates.len() == 20 {
                        complete = false;
                        offset = current_index;
                        revision = Some(row.revision);
                        message = Some(row.id.clone());
                        break 'rows;
                    }
                    let id = Uuid::now_v7().to_string();
                    cache.insert(
                        id.clone(),
                        Candidate {
                            owner: owner.into(),
                            caller: caller.into(),
                            scope: scope.clone(),
                            message: row.id.clone(),
                            revision: row.revision,
                            pointer: pointer.clone(),
                            start,
                            end,
                            digest: digest(&text[start..end]),
                            created: Instant::now(),
                        },
                    );
                    candidates.push(SecretHistoryCandidate {
                        candidate_id: id,
                        conversation_id: row.conversation_id.clone(),
                        message_id: row.id.clone(),
                        pointer: pointer.clone(),
                        revision: row.revision,
                        value: text[start..end].into(),
                        masked_context: masked_context(text, &pointer, &request.query, start, end),
                    });
                }
            }
            last = row.id;
        }
        // Bounded process memory. Discard oldest handles rather than retaining arbitrary chat copies.
        while cache.len() > 10000 {
            let Some(id) = cache.keys().next().cloned() else { break };
            cache.remove(&id);
        }
        let cursor = if !complete {
            let id = Uuid::now_v7().to_string();
            let mut cursors = self.cursors.lock().await;
            cursors.retain(|_, c| alive(c.created));
            while cursors.len() >= 1000 {
                let Some(old) = cursors.keys().next().cloned() else {
                    break;
                };
                cursors.remove(&old);
            }
            cursors.insert(
                id.clone(),
                Cursor {
                    owner: owner.into(),
                    caller: caller.into(),
                    scope,
                    request: fingerprint,
                    after: last,
                    offset,
                    revision,
                    message,
                    created: Instant::now(),
                },
            );
            Some(id)
        } else {
            None
        };
        Ok(SecretHistorySearchResponse {
            candidates,
            cursor,
            complete,
            scanned_messages: scanned,
            skipped_oversized_messages,
            exhaustive: false,
        })
    }
    pub async fn mask(
        &self,
        owner: &str,
        caller: &str,
        project: Option<&str>,
        request: SecretHistoryMaskRequest,
    ) -> Result<SecretHistoryMaskReceipt, SecretsError> {
        self.mask_checked(owner, caller, project, request, &aionui_db::UncheckedHistoryCommit)
            .await
    }
    pub async fn mask_checked(
        &self,
        owner: &str,
        caller: &str,
        project: Option<&str>,
        mut request: SecretHistoryMaskRequest,
        guard: &dyn aionui_db::HistoryCommitGuard,
    ) -> Result<SecretHistoryMaskReceipt, SecretsError> {
        request.operation_id = Uuid::parse_str(&request.operation_id)
            .map_err(|_| SecretsError::InvalidInput)?
            .to_string();
        if request.candidate_ids.is_empty() || request.candidate_ids.len() > 50 {
            return Err(SecretsError::InvalidInput);
        }
        request.candidate_ids.sort();
        request.candidate_ids.dedup();
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&request).map_err(|_| SecretsError::InvalidInput)?)
        );
        if self
            .repository
            .receipt(owner, caller, &request.operation_id, &fingerprint)
            .await?
            .is_some()
        {
            let encoded = self
                .repository
                .mask(
                    owner,
                    caller,
                    &HistoryScope::Conversation(caller.into()),
                    &request.operation_id,
                    &fingerprint,
                    &[],
                    "",
                    guard,
                )
                .await?;
            return serde_json::from_str(&encoded).map_err(|_| SecretsError::Storage);
        }
        let selected = {
            let mut cache = self.candidates.lock().await;
            cache.retain(|_, c| alive(c.created));
            request
                .candidate_ids
                .iter()
                .map(|id| {
                    cache
                        .get(id)
                        .filter(|c| c.owner == owner && c.caller == caller)
                        .cloned()
                        .ok_or(SecretsError::Conflict)
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let scope = selected.first().ok_or(SecretsError::InvalidInput)?.scope.clone();
        if selected.iter().any(|c| c.scope != scope) {
            return Err(SecretsError::Forbidden);
        }
        match &scope {
            HistoryScope::Project(id) if Some(id.as_str()) != project => return Err(SecretsError::Forbidden),
            HistoryScope::Temporary if project.is_some() => return Err(SecretsError::Forbidden),
            HistoryScope::Conversation(id) if id != caller => return Err(SecretsError::Forbidden),
            _ => {}
        }
        let ids = selected
            .iter()
            .map(|c| c.message.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let rows = self.repository.selected(owner, &scope, &ids).await?;
        let conversation_ids = rows
            .iter()
            .map(|row| row.conversation_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut edits = vec![];
        let mut revisions = BTreeMap::new();
        for row in rows {
            if matches!(row.status.as_deref(), Some("work" | "pending")) {
                return Err(SecretsError::MessageBusy);
            }
            let mut value: Value = serde_json::from_str(&row.content).map_err(|_| SecretsError::Conflict)?;
            let mut fields: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
            for c in selected.iter().filter(|c| c.message == row.id) {
                if c.revision != row.revision {
                    return Err(SecretsError::Conflict);
                }
                let text = value
                    .pointer(&c.pointer)
                    .and_then(Value::as_str)
                    .ok_or(SecretsError::Conflict)?;
                if text.get(c.start..c.end).is_none_or(|s| digest(s) != c.digest) {
                    return Err(SecretsError::Conflict);
                }
                fields.entry(c.pointer.clone()).or_default().push((c.start, c.end));
            }
            for (pointer, mut ranges) in fields {
                ranges.sort();
                let mut merged: Vec<(usize, usize)> = vec![];
                for (start, end) in ranges {
                    if let Some(last) = merged.last_mut()
                        && start <= last.1
                    {
                        last.1 = last.1.max(end);
                    } else {
                        merged.push((start, end));
                    }
                }
                let leaf = value.pointer_mut(&pointer).ok_or(SecretsError::Conflict)?;
                let mut text = leaf.as_str().ok_or(SecretsError::Conflict)?.to_owned();
                for (start, end) in merged.into_iter().rev() {
                    text.replace_range(start..end, "<masked>");
                }
                *leaf = Value::String(text);
            }
            revisions.insert(row.id.clone(), row.revision + 1);
            edits.push(HistoryEdit {
                message_id: row.id,
                revision: row.revision,
                content: serde_json::to_string(&value).map_err(|_| SecretsError::Conflict)?,
            });
        }
        let receipt = SecretHistoryMaskReceipt {
            conversation_ids,
            selected_occurrences: selected.len(),
            revisions,
            runtime_context_not_erased: true,
        };
        let encoded = serde_json::to_string(&receipt).map_err(|_| SecretsError::Storage)?;
        let result = self
            .repository
            .mask(
                owner,
                caller,
                &scope,
                &request.operation_id,
                &fingerprint,
                &edits,
                &encoded,
                guard,
            )
            .await?;
        serde_json::from_str(&result).map_err(|_| SecretsError::Storage)
    }
}

// Only known message payloads are mutable; structural/identity metadata never participates.
fn payload_leaves<'a>(value: &'a Value, kind: &str, out: &mut Vec<(String, &'a str)>) {
    match kind {
        "text" => {
            if let Some(text) = value.get("content").and_then(Value::as_str) {
                out.push(("/content".into(), text));
            }
        }
        "tool_call" => {
            for field in ["args", "input", "output", "result"] {
                if let Some(payload) = value.get(field) {
                    string_leaves(payload, &format!("/{field}"), out);
                }
            }
        }
        "acp_tool_call" => {
            for pointer in [
                "/update/raw_input",
                "/update/raw_output",
                "/update/rawInput",
                "/update/rawOutput",
            ] {
                if let Some(payload) = value.pointer(pointer) {
                    string_leaves(payload, pointer, out);
                }
            }
            if let Some(items) = value.pointer("/update/content").and_then(Value::as_array) {
                for (i, item) in items.iter().enumerate() {
                    if let Some(text) = item.pointer("/content/text").and_then(Value::as_str) {
                        out.push((format!("/update/content/{i}/content/text"), text));
                    }
                }
            }
        }
        _ => {}
    }
}
fn string_leaves<'a>(value: &'a Value, pointer: &str, out: &mut Vec<(String, &'a str)>) {
    match value {
        Value::String(text) => out.push((pointer.into(), text)),
        Value::Array(values) => {
            for (i, value) in values.iter().enumerate() {
                string_leaves(value, &format!("{pointer}/{i}"), out)
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                string_leaves(
                    value,
                    &format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                    out,
                )
            }
        }
        _ => {}
    }
}
fn credential_label(label: &str) -> bool {
    ["password", "passwd", "token", "api_key", "secret", "credential"]
        .iter()
        .any(|s| label.to_ascii_lowercase().ends_with(s))
}
fn payload_spans(text: &str, pointer: &str, query: &str) -> Vec<(usize, usize)> {
    let label = pointer.rsplit('/').find(|part| credential_label(part));
    if let Some(label) = label
        && (query.is_empty() || label.to_ascii_lowercase().contains(&query.to_ascii_lowercase()))
        && !text.is_empty()
        && text != "<masked>"
    {
        return vec![(0, text.len())];
    }
    spans(text, query)
}
// Context contains at most 48 surrounding Unicode scalars per side, plus masks.
// Mask every detected credential and every literal-query occurrence, not just
// the selected span, so nearby candidates do not leak into the excerpt.
fn masked_context(text: &str, pointer: &str, query: &str, start: usize, end: usize) -> String {
    let left = text[..start].char_indices().rev().nth(47).map_or(0, |(i, _)| i);
    let right = text[end..].char_indices().nth(48).map_or(text.len(), |(i, _)| end + i);
    let mut hidden = payload_spans(text, pointer, "");
    hidden.extend(payload_spans(text, pointer, query));
    hidden.push((start, end));
    hidden.sort_unstable();
    let mut context = if left > 0 { "…".to_owned() } else { String::new() };
    let mut cursor = left;
    for (a, b) in hidden {
        if b <= cursor || a >= right {
            continue;
        }
        let a = a.max(cursor);
        context.push_str(&text[cursor..a]);
        context.push_str("<masked>");
        cursor = b.min(right);
    }
    context.push_str(&text[cursor..right]);
    if right < text.len() {
        context.push('…');
    }
    context
}
fn spans(text: &str, query: &str) -> Vec<(usize, usize)> {
    let mut spans = vec![];
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if let Some(split) = line.find([':', '=']) {
            let label = line[..split].trim().to_ascii_lowercase();
            let known = ["password", "passwd", "token", "api_key", "secret", "credential"]
                .iter()
                .find(|s| label.ends_with(**s));
            if known.is_some_and(|label| query.is_empty() || label.contains(&query.to_ascii_lowercase())) {
                let raw = &line[split + 1..];
                let trimmed = raw.trim().trim_matches(['"', '\'']);
                if !trimmed.is_empty() && trimmed != "<masked>" {
                    let start = offset + split + 1 + raw.find(trimmed).unwrap_or(0);
                    spans.push((start, start + trimmed.len()));
                }
            }
        }
        offset += line.len();
    }
    if !query.is_empty() && spans.is_empty() {
        spans.extend(text.match_indices(query).map(|(start, v)| (start, start + v.len())));
    }
    spans
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
