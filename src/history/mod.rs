use chrono::{DateTime, Local};
use std::cmp::Ordering;
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
    pub subagent_name: Option<String>,
    pub hierarchy_depth: usize,
    pub hierarchy_order: usize,
    pub hierarchy_sort_timestamp: DateTime<Local>,
}

pub fn compare_conversations(a: &Conversation, b: &Conversation) -> Ordering {
    b.hierarchy_sort_timestamp
        .cmp(&a.hierarchy_sort_timestamp)
        .then_with(|| a.hierarchy_order.cmp(&b.hierarchy_order))
        .then_with(|| b.timestamp.cmp(&a.timestamp))
        .then_with(|| a.session_id.cmp(&b.session_id))
}
