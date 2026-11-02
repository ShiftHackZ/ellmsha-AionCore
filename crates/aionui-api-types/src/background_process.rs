use serde::{Deserialize, Serialize};

/// Start a foreground command inside an app-owned background terminal.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StartBackgroundProcessRequest {
    /// Optional initial foreground command, at most 16384 bytes. Empty starts a persistent interactive shell.
    #[serde(default)]
    pub command: String,
    /// Optional display title (at most 120 characters); defaults to the command.
    pub title: Option<String>,
    /// Optional relative/absolute directory within the conversation workspace.
    pub cwd: Option<String>,
}

/// Private, bounded terminal snapshot visible only to the owning conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundProcessSnapshot {
    /// Opaque app-owned identifier; never accepts a caller-supplied OS PID.
    pub id: String,
    /// Owning conversation used for authorization and terminal association.
    pub conversation_id: String,
    /// Human-readable terminal label.
    pub title: String,
    /// Original foreground shell command, displayed only in the private terminal.
    pub command: String,
    /// Canonical authorized working directory.
    pub cwd: String,
    /// Observed child PID for display, not an authority for process operations.
    pub pid: u32,
    /// Whether the owned command has not yet exited.
    pub running: bool,
    /// Explicitly closed sessions are removed from UI tabs but retain their final snapshot.
    pub closed: bool,
    /// Observed exit code; null while running or when terminated by a signal.
    pub exit_code: Option<i32>,
    /// Current rendered screen, including full-screen terminal applications.
    pub stdout: String,
    /// Empty for PTYs: stdout and stderr are combined in the terminal stream.
    pub stderr: String,
    /// Whether older stdout bytes were discarded from the bounded buffer.
    pub stdout_truncated: bool,
    /// Monotonic input revision; agent mutations must compare this before writing.
    pub input_revision: u64,
    /// True for two seconds after human input; agent input is refused during this lease.
    pub human_input_active: bool,
    /// Current PTY dimensions (rows, columns).
    pub rows: u16,
    /// Current PTY width in character cells.
    pub cols: u16,
    /// Zero-based cursor row and column in the current screen.
    pub cursor: (u16, u16),
}

/// Keyboard input into an owned shared terminal. Authentication determines the
/// actor: a runtime token always means agent; callers cannot impersonate a user.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalInputRequest {
    /// UTF-8 text/control characters, 1–65536 bytes; does not append Enter.
    pub data: String,
    /// Required for agent writes. Must equal the input revision from the last read.
    pub expected_revision: Option<u64>,
}
/// Resize an owned PTY. The visible user viewport owns its dimensions.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalResizeRequest {
    /// Height in rows, between 2 and 300.
    pub rows: u16,
    /// Width in columns, between 2 and 500.
    pub cols: u16,
}
/// Incremental binary-safe output for xterm.js, with bounded-buffer recovery.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalOutput {
    /// Base64 VT bytes to write into the emulator.
    pub data: String,
    /// Absolute next byte offset; use as `after` for the next read.
    pub next_offset: u64,
    /// Clear/reset the emulator before applying data (reconnect/buffer overflow).
    pub reset: bool,
    /// Whether the shell is still alive.
    pub running: bool,
    /// Current input revision shared with all writers.
    pub input_revision: u64,
    /// Terminal dimensions applied by the server; resize/reconnect must resync.
    pub rows: u16,
    /// Current width in cells.
    pub cols: u16,
    /// Monotonic resize revision, independent of output byte offsets.
    pub screen_revision: u64,
}
/// Bounded wait, also useful without a connected renderer.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TerminalWaitRequest {
    /// Literal text to find on the current rendered screen; no untrusted regex.
    pub text: String,
    /// Maximum wait in milliseconds, 0–10000. Silence is never command completion.
    pub timeout_ms: u64,
}
