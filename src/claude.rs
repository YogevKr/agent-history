use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
pub enum LogEntry {
    Summary {
        summary: String,
    },
    User {
        message: UserMessage,
        /// ISO 8601 timestamp when this message was sent
        #[serde(default)]
        timestamp: Option<String>,
        /// UUID for linking with turn_duration entries
        #[allow(dead_code)]
        uuid: Option<String>,
        /// The working directory when this message was sent
        cwd: Option<String>,
        /// When set, this message is part of a subagent conversation
        #[serde(default, rename = "parent_tool_use_id")]
        parent_tool_use_id: Option<String>,
    },
    Assistant {
        message: AssistantMessage,
        /// ISO 8601 timestamp when this message was sent
        #[serde(default)]
        timestamp: Option<String>,
        /// UUID for linking with turn_duration entries
        #[allow(dead_code)]
        uuid: Option<String>,
        /// When set, this message is part of a subagent conversation
        #[serde(default, rename = "parent_tool_use_id")]
        parent_tool_use_id: Option<String>,
    },
    #[serde(rename = "file-history-snapshot")]
    #[allow(dead_code)]
    FileHistorySnapshot {
        #[serde(rename = "messageId")]
        message_id: String,
        snapshot: serde_json::Value,
        #[serde(rename = "isSnapshotUpdate")]
        is_snapshot_update: bool,
    },
    Progress {
        data: serde_json::Value,
        #[allow(dead_code)]
        #[serde(flatten)]
        extra: serde_json::Value,
    },
    #[allow(dead_code)]
    System {
        subtype: String,
        level: Option<String>,
        /// Duration in milliseconds for turn_duration entries
        #[serde(rename = "durationMs")]
        duration_ms: Option<u64>,
        /// Parent UUID for linking turn_duration to preceding message
        #[serde(rename = "parentUuid")]
        parent_uuid: Option<String>,
        #[serde(flatten)]
        extra: serde_json::Value,
    },
    #[serde(rename = "custom-title")]
    CustomTitle {
        #[serde(rename = "customTitle")]
        custom_title: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct UserMessage {
    #[allow(dead_code)]
    pub role: String,
    pub content: UserContent,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    String(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    #[allow(dead_code)]
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: Option<String>,
    pub usage: Option<TokenUsage>,
    /// Unique message ID to deduplicate streaming entries
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        #[allow(dead_code)]
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        #[allow(dead_code)]
        tool_use_id: String,
        #[serde(default)]
        content: Option<serde_json::Value>,
    },
    Thinking {
        thinking: String,
        #[allow(dead_code)]
        signature: String,
    },
    #[allow(dead_code)]
    Image {
        source: serde_json::Value,
    },
}

/// Maximum characters to index per tool result to bound memory/CPU
const MAX_TOOL_RESULT_CHARS: usize = 16 * 1024;

/// Extract only Text blocks (for previews and user-facing display)
pub fn extract_text_from_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract Text blocks plus ToolResult content (for search indexing)
pub fn extract_search_text_from_blocks(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => parts.push(text.clone()),
            ContentBlock::ToolResult {
                content: Some(content),
                ..
            } => {
                if let Some(text) = extract_tool_result_text(content) {
                    parts.push(truncate_for_search(&text, MAX_TOOL_RESULT_CHARS));
                }
            }
            _ => {}
        }
    }

    parts.join(" ")
}

/// Extract text from a ToolResult content value.
/// Supports both plain string and array-of-blocks formats.
fn extract_tool_result_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|item| match item {
                    serde_json::Value::Object(map) => {
                        let ty = map.get("type").and_then(|v| v.as_str());
                        if ty.is_none() || ty == Some("text") {
                            map.get("text").and_then(|v| v.as_str())
                        } else {
                            None
                        }
                    }
                    serde_json::Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            let joined = parts.join(" ");
            if joined.trim().is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

/// Truncate text for search indexing, keeping head and tail portions
fn truncate_for_search(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let head_target = max * 3 / 4;
    let tail_target = max / 4;
    let head_end = floor_char_boundary(s, head_target);
    let tail_start = ceil_char_boundary(s, s.len().saturating_sub(tail_target));
    format!("{} {}", &s[..head_end], &s[tail_start..])
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

pub fn extract_text_from_user(message: &UserMessage) -> String {
    match &message.content {
        UserContent::String(text) => text.clone(),
        UserContent::Blocks(blocks) => extract_text_from_blocks(blocks),
    }
}

pub fn extract_search_text_from_user(message: &UserMessage) -> String {
    match &message.content {
        UserContent::String(text) => text.clone(),
        UserContent::Blocks(blocks) => extract_search_text_from_blocks(blocks),
    }
}

pub fn extract_text_from_assistant(message: &AssistantMessage) -> String {
    extract_text_from_blocks(&message.content)
}

pub fn extract_search_text_from_assistant(message: &AssistantMessage) -> String {
    extract_search_text_from_blocks(&message.content)
}
