use serde::{Deserialize, Serialize};

/// Read-only preview of a BMAD artifact in the conversation's current project.
#[derive(Debug, Deserialize)]
pub struct MemoryArtifactQuery {
    /// Logical memory path, never a local file path or client-selected project.
    pub path: String,
}

/// Complete bounded Markdown artifact. Errors never masquerade as empty content.
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryArtifactResponse {
    /// Authorized logical path that was read.
    pub path: String,
    /// Display title derived from the artifact, without HTML rendering.
    pub title: String,
    /// Markdown source, rendered as untrusted content in a read-only preview.
    pub content: String,
}
