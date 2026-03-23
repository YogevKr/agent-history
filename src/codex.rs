use serde::Deserialize;

/// Top-level line in a Codex JSONL file
#[derive(Debug, Deserialize)]
pub struct CodexLine {
    pub timestamp: String,
    #[serde(rename = "type")]
    pub line_type: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub cli_version: Option<String>,
    #[serde(default)]
    pub git: Option<GitInfo>,
    #[serde(default)]
    pub model_provider: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GitInfo {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TurnContext {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EventMsg {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub info: Option<TokenInfo>,
}

#[derive(Debug, Deserialize)]
pub struct TokenInfo {
    #[serde(default)]
    pub total_token_usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct ResponseItem {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<Vec<ContentPart>>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    #[serde(default)]
    pub text: Option<String>,
}
