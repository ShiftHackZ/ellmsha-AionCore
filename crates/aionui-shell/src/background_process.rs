//! App-owned foreground commands retained beyond agent turns, never shell jobs
//! detached from their owner. Process/output handles live until Stop or shutdown.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use aionui_api_types::{
    BackgroundProcessSnapshot, StartBackgroundProcessRequest, TerminalInputRequest, TerminalOutput,
    TerminalResizeRequest, TerminalWaitRequest,
};
use aionui_db::IConversationRepository;
use aionui_process::ManagedPty;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::io::Read;
use std::time::Instant;
use tokio::sync::Mutex;

const OUTPUT_LIMIT: usize = 256 * 1024;
const PROCESS_LIMIT: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum BackgroundProcessError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Background terminal or conversation not found")]
    NotFound,
    #[error("Background process service is shutting down")]
    ShuttingDown,
    #[error("Background process operation failed")]
    Internal,
}

struct Output {
    bytes: Vec<u8>,
    offset: u64,
    screen_revision: u64,
    parser: vt100::Parser,
}
impl Default for Output {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            offset: 0,
            screen_revision: 0,
            parser: vt100::Parser::new(24, 80, 2000),
        }
    }
}
#[derive(Default)]
struct Input {
    revision: u64,
    human: Option<Instant>,
}
impl Input {
    fn human_active(&self) -> bool {
        self.human.is_some_and(|last| last.elapsed() < Duration::from_secs(2))
    }
}
struct Entry {
    user_id: String,
    conversation_id: String,
    id: String,
    title: String,
    command: String,
    cwd: String,
    process: Arc<ManagedPty>,
    output: Arc<std::sync::Mutex<Output>>,
    input: Mutex<Input>,
    closed: AtomicBool,
}
impl Entry {
    async fn summary(&self) -> BackgroundProcessSnapshot {
        let input = self.input.lock().await;
        let output = self.output.lock().unwrap();
        let screen = output.parser.screen();
        let (rows, cols) = screen.size();
        BackgroundProcessSnapshot {
            id: self.id.clone(),
            conversation_id: self.conversation_id.clone(),
            title: self.title.clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            pid: self.process.pid(),
            running: !self.process.has_exited(),
            closed: self.closed.load(Ordering::SeqCst),
            exit_code: self.process.exit_code(),
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: output.offset > output.bytes.len() as u64,
            input_revision: input.revision,
            human_input_active: input.human_active(),
            rows,
            cols,
            cursor: screen.cursor_position(),
        }
    }
    async fn snapshot(&self) -> BackgroundProcessSnapshot {
        let mut snapshot = self.summary().await;
        snapshot.stdout = self.output.lock().unwrap().parser.screen().contents();
        snapshot
    }
}

pub struct BackgroundProcessService {
    conversations: Arc<dyn IConversationRepository>,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    shutting_down: AtomicBool,
}

impl BackgroundProcessService {
    pub fn new(conversations: Arc<dyn IConversationRepository>) -> Self {
        Self {
            conversations,
            entries: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        }
    }

    async fn owned_conversation(
        &self,
        user: &str,
        conversation: &str,
    ) -> Result<aionui_db::models::ConversationRow, BackgroundProcessError> {
        self.conversations
            .get(user, conversation)
            .await
            .map_err(|_| BackgroundProcessError::Internal)?
            .ok_or(BackgroundProcessError::NotFound)
    }

    async fn workspace(&self, user: &str, conversation: &str) -> Result<PathBuf, BackgroundProcessError> {
        let row = self.owned_conversation(user, conversation).await?;
        let extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|_| BackgroundProcessError::Internal)?;
        let workspace = extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .ok_or(BackgroundProcessError::Invalid("Conversation workspace is unavailable"))?;
        tokio::fs::canonicalize(workspace)
            .await
            .map_err(|_| BackgroundProcessError::Invalid("Conversation workspace is unavailable"))
    }

    pub async fn start(
        &self,
        user: &str,
        conversation: &str,
        request: StartBackgroundProcessRequest,
    ) -> Result<BackgroundProcessSnapshot, BackgroundProcessError> {
        if request.command.len() > 16384 || request.command.contains('\0') {
            return Err(BackgroundProcessError::Invalid(
                "Command must contain at most 16384 bytes without NUL",
            ));
        }
        if request
            .title
            .as_ref()
            .is_some_and(|title| title.chars().count() > 120 || title.chars().any(char::is_control))
        {
            return Err(BackgroundProcessError::Invalid(
                "Terminal title must contain at most 120 characters without control characters",
            ));
        }
        let root = self.workspace(user, conversation).await?;
        let cwd = match request.cwd.as_deref() {
            Some(path) => tokio::fs::canonicalize(root.join(path))
                .await
                .map_err(|_| BackgroundProcessError::Invalid("Working directory is unavailable"))?,
            None => root.clone(),
        };
        if !cwd.starts_with(&root) || !cwd.is_dir() {
            return Err(BackgroundProcessError::Invalid(
                "Working directory must be within the conversation workspace",
            ));
        }
        // Serialize admission through spawn so concurrent calls cannot exceed
        // the resource bound or launch after shutdown has started.
        let mut entries = self.entries.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(BackgroundProcessError::ShuttingDown);
        }
        if entries.len() >= PROCESS_LIMIT {
            entries.retain(|_, entry| !entry.process.has_exited());
        }
        if entries.len() >= PROCESS_LIMIT {
            return Err(BackgroundProcessError::Invalid(
                "Too many background terminals; stop an existing process first",
            ));
        }
        let command = request.command.clone();
        let spawn_cwd = cwd.clone();
        // PTY spawn/read/write are blocking OS APIs. Never run them on an async worker.
        let (process, mut reader) = tokio::task::spawn_blocking(move || ManagedPty::spawn(&spawn_cwd, &command))
            .await
            .map_err(|_| BackgroundProcessError::Internal)?
            .map_err(|_| BackgroundProcessError::Internal)?;
        let output = Arc::new(std::sync::Mutex::new(Output::default()));
        let buffer = output.clone();
        tokio::task::spawn_blocking(move || {
            let mut chunk = [0u8; 8192];
            loop {
                let count = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(25));
                        continue;
                    }
                    Err(_) => break,
                };
                let mut output = buffer.lock().unwrap();
                output.parser.process(&chunk[..count]);
                output.offset += count as u64;
                output.bytes.extend_from_slice(&chunk[..count]);
                let cut = output.bytes.len().saturating_sub(OUTPUT_LIMIT);
                if cut > 0 {
                    output.bytes.drain(..cut);
                }
            }
        });
        let id = uuid::Uuid::new_v4().to_string();
        let entry = Arc::new(Entry {
            user_id: user.into(),
            conversation_id: conversation.into(),
            id: id.clone(),
            title: request
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| {
                    if request.command.is_empty() {
                        "Terminal".into()
                    } else {
                        request.command.chars().take(80).collect()
                    }
                }),
            command: request.command,
            cwd: cwd.to_string_lossy().into_owned(),
            process,
            output,
            input: Mutex::new(Input::default()),
            closed: AtomicBool::new(false),
        });
        tracing::info!(process_id = %id, conversation_id = conversation, "Background terminal started");
        let snapshot = entry.snapshot().await;
        entries.insert(id, entry);
        Ok(snapshot)
    }

    pub async fn list(
        &self,
        user: &str,
        conversation: &str,
    ) -> Result<Vec<BackgroundProcessSnapshot>, BackgroundProcessError> {
        self.owned_conversation(user, conversation).await?;
        let entries: Vec<_> = self
            .entries
            .lock()
            .await
            .values()
            .filter(|entry| entry.user_id == user && entry.conversation_id == conversation)
            .cloned()
            .collect();
        let mut snapshots = Vec::new();
        for entry in entries {
            snapshots.push(entry.summary().await);
        }
        snapshots.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(snapshots)
    }

    async fn entry(&self, user: &str, conversation: &str, id: &str) -> Result<Arc<Entry>, BackgroundProcessError> {
        self.owned_conversation(user, conversation).await?;
        self.entries
            .lock()
            .await
            .get(id)
            .filter(|entry| entry.user_id == user && entry.conversation_id == conversation)
            .cloned()
            .ok_or(BackgroundProcessError::NotFound)
    }

    pub async fn read(
        &self,
        user: &str,
        conversation: &str,
        id: &str,
    ) -> Result<BackgroundProcessSnapshot, BackgroundProcessError> {
        Ok(self.entry(user, conversation, id).await?.snapshot().await)
    }

    pub async fn stop(
        &self,
        user: &str,
        conversation: &str,
        id: &str,
    ) -> Result<BackgroundProcessSnapshot, BackgroundProcessError> {
        let entry = self.entry(user, conversation, id).await?;
        entry
            .process
            .terminate()
            .await
            .map_err(|_| BackgroundProcessError::Internal)?;
        tracing::info!(
            process_id = id,
            conversation_id = conversation,
            "Background terminal stopped"
        );
        Ok(entry.snapshot().await)
    }

    /// Close both the shell and all shared views; legacy Stop preserves the tab.
    pub async fn close(
        &self,
        user: &str,
        conversation: &str,
        id: &str,
    ) -> Result<BackgroundProcessSnapshot, BackgroundProcessError> {
        self.stop(user, conversation, id).await?;
        let entry = self.entry(user, conversation, id).await?;
        entry.closed.store(true, Ordering::SeqCst);
        Ok(entry.snapshot().await)
    }

    /// The same PTY receives both user keyboard input and agent writes. Input
    /// revision comparisons prevent stale agents from writing over human edits.
    pub async fn write(
        &self,
        user: &str,
        conversation: &str,
        id: &str,
        request: TerminalInputRequest,
        agent: bool,
    ) -> Result<BackgroundProcessSnapshot, BackgroundProcessError> {
        if request.data.is_empty() || request.data.len() > 65536 {
            return Err(BackgroundProcessError::Invalid("Input must contain 1–65536 bytes"));
        }
        let entry = self.entry(user, conversation, id).await?;
        let mut input = entry.input.lock().await;
        if agent && (input.human_active() || request.expected_revision != Some(input.revision)) {
            return Err(BackgroundProcessError::Invalid(
                "Terminal input changed or user is typing; read again before writing",
            ));
        }
        // Reserve the revision before I/O: even an uncertain/partially written
        // input must invalidate every stale writer, including on cancellation.
        input.revision += 1;
        if !agent {
            input.human = Some(Instant::now());
        }
        let process = entry.process.clone();
        tokio::task::spawn_blocking(move || process.write(request.data.as_bytes()))
            .await
            .map_err(|_| BackgroundProcessError::Internal)?
            .map_err(|_| BackgroundProcessError::Internal)?;
        drop(input);
        Ok(entry.snapshot().await)
    }
    pub async fn resize(
        &self,
        user: &str,
        conversation: &str,
        id: &str,
        request: TerminalResizeRequest,
    ) -> Result<BackgroundProcessSnapshot, BackgroundProcessError> {
        if !(2..=300).contains(&request.rows) || !(2..=500).contains(&request.cols) {
            return Err(BackgroundProcessError::Invalid("Invalid terminal dimensions"));
        }
        let entry = self.entry(user, conversation, id).await?;
        let output = entry.output.clone();
        let process = entry.process.clone();
        tokio::task::spawn_blocking(move || {
            let mut output = output.lock().unwrap();
            process.resize(request.rows, request.cols)?;
            output.parser.screen_mut().set_size(request.rows, request.cols);
            output.screen_revision += 1;
            Ok::<_, aionui_process::ProcessError>(())
        })
        .await
        .map_err(|_| BackgroundProcessError::Internal)?
        .map_err(|_| BackgroundProcessError::Internal)?;
        Ok(entry.snapshot().await)
    }
    pub async fn output(
        &self,
        user: &str,
        conversation: &str,
        id: &str,
        after: Option<u64>,
        screen_revision: Option<u64>,
    ) -> Result<TerminalOutput, BackgroundProcessError> {
        let entry = self.entry(user, conversation, id).await?;
        let input = entry.input.lock().await;
        let output = entry.output.lock().unwrap();
        let start = output.offset - output.bytes.len() as u64;
        let valid = after.filter(|after| {
            *after >= start && *after <= output.offset && screen_revision == Some(output.screen_revision)
        });
        let data = if let Some(after) = valid {
            output.bytes[(after - start) as usize..].to_vec()
        } else {
            output.parser.screen().state_formatted()
        };
        Ok(TerminalOutput {
            data: STANDARD.encode(data),
            next_offset: output.offset,
            reset: valid.is_none(),
            running: !entry.process.has_exited(),
            input_revision: input.revision,
            rows: output.parser.screen().size().0,
            cols: output.parser.screen().size().1,
            screen_revision: output.screen_revision,
        })
    }
    pub async fn wait(
        &self,
        user: &str,
        conversation: &str,
        id: &str,
        request: TerminalWaitRequest,
    ) -> Result<BackgroundProcessSnapshot, BackgroundProcessError> {
        if request.timeout_ms > 10000 || request.text.len() > 4096 {
            return Err(BackgroundProcessError::Invalid("Invalid terminal wait bounds"));
        }
        let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
        loop {
            let snapshot = self.read(user, conversation, id).await?;
            if !snapshot.running || snapshot.stdout.contains(&request.text) || Instant::now() >= deadline {
                return Ok(snapshot);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Called on normal shutdown and desktop-parent death, before HTTP draining.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let entries: Vec<_> = self.entries.lock().await.drain().map(|(_, entry)| entry).collect();
        futures_util::future::join_all(entries.iter().map(|entry| async move {
            if entry.process.terminate().await.is_err() {
                tracing::warn!(process_id = %entry.id, "Background terminal cleanup could not be confirmed");
            }
        }))
        .await;
    }
}
