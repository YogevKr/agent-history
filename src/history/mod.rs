use chrono::{DateTime, Local};
use std::path::PathBuf;

/// Source of a session
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    Claude,
    Codex,
}

impl std::fmt::Display for SessionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionSource::Claude => write!(f, "claude"),
            SessionSource::Codex => write!(f, "codex"),
        }
    }
}

/// Unified conversation representation across all session sources
#[derive(Clone)]
pub struct Conversation {
    pub path: PathBuf,
    pub source: SessionSource,
    pub session_id: String,
    pub timestamp: DateTime<Local>,
    pub preview: String,
    pub full_text: String,
    pub project_name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub message_count: usize,
    pub model: Option<String>,
    pub total_tokens: u64,
    pub duration_minutes: Option<u64>,
    pub summary: Option<String>,
    pub custom_title: Option<String>,
    pub git_branch: Option<String>,
}
