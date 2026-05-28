use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, TimeZone};

use crate::codex::{CodexLine, EventMsg, SessionMeta, TurnContext};
use crate::codex_items::{codex_items, read_codex_lines, CodexItem, CodexRole};
use crate::error::Result;
use crate::history::{Conversation, SessionSource};

const MAX_FULL_TEXT: usize = 256 * 1024; // 256KB cap
const MAX_OUTPUT_PER_ITEM: usize = 16 * 1024; // 16KB cap per function_call_output

/// Extract session ID from a Codex JSONL filename.
///
/// Filename pattern: `rollout-{timestamp}-{ulid}.jsonl`
/// The ULID is the last 4 hyphen-separated segments.
/// E.g. `rollout-2026-03-19T14-28-54-019d0611-f81c-7403-9bbb-20856d019138.jsonl`
///       -> `019d0611-f81c-7403-9bbb-20856d019138`
pub(crate) fn session_id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    // Last 5 segments form the UUID (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)
    let ulid = parts[parts.len() - 5..].join("-");
    Some(ulid)
}

fn append_text(full_text: &mut String, text: &str) {
    if full_text.len() >= MAX_FULL_TEXT {
        return;
    }
    let remaining = MAX_FULL_TEXT - full_text.len();
    if text.len() <= remaining {
        full_text.push_str(text);
    } else {
        let mut end = remaining;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        full_text.push_str(&text[..end]);
    }
}

pub(crate) fn subagent_dispatch_content(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let content = value.get("content")?.as_str()?.trim();
    if content.is_empty() {
        return None;
    }

    let recipient = value.get("recipient").and_then(|value| value.as_str());
    let trigger_turn = value.get("trigger_turn").and_then(|value| value.as_bool());
    if recipient.is_none() && trigger_turn != Some(true) {
        return None;
    }

    Some(content.to_string())
}

/// Process a single Codex JSONL session file into a Conversation.
pub fn process_codex_file(
    path: PathBuf,
    modified: Option<SystemTime>,
) -> Result<Option<Conversation>> {
    let file = File::open(&path)?;
    let reader = BufReader::new(file);

    let mut session_id: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut model: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut preview = String::new();
    let mut full_text = String::new();
    let mut message_count: usize = 0;
    let mut total_tokens: u64 = 0;
    let mut first_timestamp: Option<String> = None;
    let mut session_timestamp: Option<String> = None;

    let lines = read_codex_lines(reader);

    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }

        let codex_line: CodexLine = match serde_json::from_str(line) {
            Ok(cl) => cl,
            Err(_) => continue,
        };

        if first_timestamp.is_none() {
            first_timestamp = Some(codex_line.timestamp.clone());
        }

        match codex_line.line_type.as_str() {
            "session_meta" => {
                if let Ok(meta) = serde_json::from_value::<SessionMeta>(codex_line.payload) {
                    session_id = Some(meta.id);
                    session_timestamp = Some(codex_line.timestamp);
                    if let Some(c) = meta.cwd {
                        cwd = Some(PathBuf::from(c));
                    }
                    if let Some(git) = meta.git {
                        git_branch = git.branch;
                    }
                }
            }
            "turn_context" if model.is_none() => {
                if let Ok(tc) = serde_json::from_value::<TurnContext>(codex_line.payload) {
                    if tc.model.is_some() {
                        model = tc.model;
                    }
                }
            }
            "event_msg" => {
                if let Ok(evt) = serde_json::from_value::<EventMsg>(codex_line.payload) {
                    if evt.event_type.as_str() == "token_count" {
                        if let Some(info) = evt.info {
                            if let Some(usage) = info.total_token_usage {
                                total_tokens = usage.total_tokens;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    for item in codex_items(&lines) {
        match item {
            CodexItem::Message { role, text } => {
                message_count += 1;
                let label = match role {
                    CodexRole::User => {
                        if preview.is_empty() {
                            preview = text.chars().take(200).collect();
                        }
                        "User: "
                    }
                    CodexRole::Assistant => {
                        if preview.is_empty() {
                            if let Some(content) = subagent_dispatch_content(&text) {
                                preview = content.chars().take(200).collect();
                            }
                        }
                        "Assistant: "
                    }
                };
                append_text(&mut full_text, label);
                append_text(&mut full_text, &text);
                append_text(&mut full_text, "\n\n");
            }
            CodexItem::ToolCall { name } => {
                append_text(&mut full_text, &format!("[Tool: {}]\n\n", name));
            }
            CodexItem::ToolOutput { output } => {
                let truncated = if output.len() > MAX_OUTPUT_PER_ITEM {
                    let mut end = MAX_OUTPUT_PER_ITEM;
                    while end > 0 && !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    &output[..end]
                } else {
                    output.as_str()
                };
                append_text(&mut full_text, &format!("[Tool Output]\n{}\n\n", truncated));
            }
        }
    }

    if message_count == 0 {
        return Ok(None);
    }

    // Resolve session ID: prefer session_meta.id, fall back to filename
    let session_id = session_id
        .or_else(|| session_id_from_filename(&path))
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

    // Resolve timestamp
    let timestamp = session_timestamp
        .as_deref()
        .or(first_timestamp.as_deref())
        .and_then(|ts| {
            DateTime::parse_from_rfc3339(ts)
                .ok()
                .map(|dt| dt.with_timezone(&Local))
        })
        .or_else(|| {
            modified.and_then(|m| {
                let duration = m.duration_since(SystemTime::UNIX_EPOCH).ok()?;
                Local.timestamp_opt(duration.as_secs() as i64, 0).single()
            })
        })
        .unwrap_or_else(Local::now);

    // directory_name: last component of cwd
    let directory_name = cwd
        .as_ref()
        .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(String::from));

    Ok(Some(Conversation {
        path,
        source: SessionSource::Codex,
        session_id,
        timestamp,
        preview,
        full_text,
        directory_name,
        cwd,
        message_count,
        model,
        total_tokens,
        duration_minutes: None,
        summary: None,
        custom_title: None,
        git_branch,
        subagent_name: None,
        hierarchy_has_children: false,
        hierarchy_has_next_sibling: false,
        hierarchy_marker: None,
        hierarchy_depth: 0,
        hierarchy_order: 0,
        hierarchy_sort_timestamp: timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f
    }

    #[test]
    fn session_id_from_filename_extracts_ulid() {
        let path =
            Path::new("rollout-2026-03-19T14-28-54-019d0611-f81c-7403-9bbb-20856d019138.jsonl");
        assert_eq!(
            session_id_from_filename(path),
            Some("019d0611-f81c-7403-9bbb-20856d019138".to_string())
        );
    }

    #[test]
    fn session_id_from_filename_short_name() {
        let path = Path::new("short.jsonl");
        assert_eq!(session_id_from_filename(path), None);
    }

    #[test]
    fn process_empty_file_returns_none() {
        let f = make_jsonl(&[]);
        let result = process_codex_file(f.path().to_path_buf(), None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn process_basic_session() {
        let lines = &[
            r#"{"timestamp":"2026-03-19T14:28:54Z","type":"session_meta","payload":{"id":"test-id-123","cwd":"/home/user/directory","git":{"branch":"main"}}}"#,
            r#"{"timestamp":"2026-03-19T14:28:55Z","type":"turn_context","payload":{"model":"gpt-4o"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"event_msg","payload":{"type":"user_message","message":"Hello world"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"event_msg","payload":{"type":"agent_message","message":"Hi there!"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}"#,
        ];
        let f = make_jsonl(lines);
        let conv = process_codex_file(f.path().to_path_buf(), None)
            .unwrap()
            .unwrap();
        assert_eq!(conv.session_id, "test-id-123");
        assert_eq!(conv.preview, "Hello world");
        assert_eq!(conv.model, Some("gpt-4o".to_string()));
        assert_eq!(conv.total_tokens, 150);
        assert_eq!(conv.message_count, 2);
        assert_eq!(conv.git_branch, Some("main".to_string()));
        assert_eq!(conv.directory_name, Some("directory".to_string()));
        assert_eq!(conv.source, SessionSource::Codex);
    }

    #[test]
    fn process_response_item_messages() {
        let lines = &[
            r#"{"timestamp":"2026-03-19T14:28:54Z","type":"session_meta","payload":{"id":"resp-test"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"What is Rust?"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Rust is a systems language."}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:02Z","type":"response_item","payload":{"type":"function_call","name":"read_file","arguments":"{}","call_id":"c1"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"file contents here"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:04Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system prompt"}]}}"#,
        ];
        let f = make_jsonl(lines);
        let conv = process_codex_file(f.path().to_path_buf(), None)
            .unwrap()
            .unwrap();
        assert_eq!(conv.session_id, "resp-test");
        assert_eq!(conv.preview, "What is Rust?");
        assert!(conv.full_text.contains("User: What is Rust?"));
        assert!(conv
            .full_text
            .contains("Assistant: Rust is a systems language."));
        assert!(conv.full_text.contains("[Tool: read_file]"));
        assert!(conv.full_text.contains("[Tool Output]"));
        assert!(conv.full_text.contains("file contents here"));
        // developer message should be skipped
        assert!(!conv.full_text.contains("system prompt"));
        // 2 messages (user + assistant content parts)
        assert_eq!(conv.message_count, 2);
    }

    #[test]
    fn subagent_dispatch_envelope_sets_preview_when_user_context_is_stripped() {
        let lines = &[
            r#"{"timestamp":"2026-05-21T14:24:33Z","type":"session_meta","payload":{"id":"subagent-session","cwd":"/tmp/directory","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","agent_nickname":"Hypatia"}}},"thread_source":"subagent","agent_nickname":"Hypatia"}}"#,
            r##"{"timestamp":"2026-05-21T14:24:34Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /tmp/directory\n\n<INSTRUCTIONS>\nignore this context\n</INSTRUCTIONS>"}]}}"##,
            r#"{"timestamp":"2026-05-21T14:24:35Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"{\"author\":\"/root\",\"recipient\":\"/root/review_compaction_fix\",\"other_recipients\":[],\"content\":\"Review the local unstaged diff for compaction correctness.\",\"trigger_turn\":true}"}]}}"#,
            r#"{"timestamp":"2026-05-21T14:24:36Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"No actionable findings."}]}}"#,
        ];
        let f = make_jsonl(lines);
        let conv = process_codex_file(f.path().to_path_buf(), None)
            .unwrap()
            .unwrap();

        assert_eq!(
            conv.preview,
            "Review the local unstaged diff for compaction correctness."
        );
    }

    #[test]
    fn dual_source_dedup_prefers_event_msg() {
        // When both event_msg and response_item contain the same messages,
        // only event_msg should be counted (not double-counted).
        let lines = &[
            r#"{"timestamp":"2026-03-19T14:28:54Z","type":"session_meta","payload":{"id":"dedup-test"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi there"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:02Z","type":"event_msg","payload":{"type":"user_message","message":"Hello"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:03Z","type":"event_msg","payload":{"type":"agent_message","message":"Hi there"}}"#,
        ];
        let f = make_jsonl(lines);
        let conv = process_codex_file(f.path().to_path_buf(), None)
            .unwrap()
            .unwrap();
        assert_eq!(conv.session_id, "dedup-test");
        // Should only count 2 messages (from event_msg), not 4
        assert_eq!(conv.message_count, 2);
    }

    #[test]
    fn mixed_sources_keep_response_messages_missing_from_events() {
        let lines = &[
            r#"{"timestamp":"2026-03-19T14:28:54Z","type":"session_meta","payload":{"id":"mixed-test"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Response-only question"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Response-only answer"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Duplicated question"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:03Z","type":"event_msg","payload":{"type":"user_message","message":"Duplicated question"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:04Z","type":"event_msg","payload":{"type":"agent_message","message":"Event-only answer"}}"#,
        ];
        let f = make_jsonl(lines);
        let conv = process_codex_file(f.path().to_path_buf(), None)
            .unwrap()
            .unwrap();

        assert_eq!(conv.message_count, 4);
        assert!(conv.full_text.contains("User: Response-only question"));
        assert!(conv.full_text.contains("Assistant: Response-only answer"));
        assert!(conv.full_text.contains("User: Duplicated question"));
        assert!(conv.full_text.contains("Assistant: Event-only answer"));
        assert_eq!(conv.full_text.matches("Duplicated question").count(), 1);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let lines = &[
            "not json at all",
            r#"{"timestamp":"2026-03-19T14:28:54Z","type":"session_meta","payload":{"id":"skip-test"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"event_msg","payload":{"type":"user_message","message":"works"}}"#,
        ];
        let f = make_jsonl(lines);
        let conv = process_codex_file(f.path().to_path_buf(), None)
            .unwrap()
            .unwrap();
        assert_eq!(conv.session_id, "skip-test");
        assert_eq!(conv.message_count, 1);
    }
}
