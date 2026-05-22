//! Export a conversation to human-friendly markdown.
//! Only includes user and assistant text messages (no tool calls, outputs, or thinking).

use crate::claude::{extract_text_from_user, ContentBlock, LogEntry};
use crate::codex_items::{codex_items, read_codex_lines, CodexItem, CodexRole};
use crate::display::format_model_short;
use crate::error::Result;
use crate::history::{Conversation, SessionSource};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// Render a conversation as markdown (user + assistant text only).
pub fn to_markdown(conv: &Conversation) -> Result<String> {
    let mut md = String::new();

    // Header
    md.push_str(&format!(
        "# Session: {} ({})\n",
        conv.session_id, conv.source
    ));
    let project = conv.project_name.as_deref().unwrap_or("unknown");
    let model = format_model_short(conv.model.as_deref());
    let date = conv.timestamp.format("%Y-%m-%d %H:%M");
    md.push_str(&format!(
        "**Project:** {} | **Model:** {} | **Date:** {}\n",
        project, model, date
    ));
    md.push_str("\n---\n\n");

    // Messages
    match conv.source {
        SessionSource::Claude => render_claude_md(&conv.path, &mut md)?,
        SessionSource::Codex => render_codex_md(&conv.path, &mut md)?,
    }

    Ok(md)
}

/// Copy markdown to clipboard via pbcopy.
pub fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

/// Export markdown to a file. Returns the path written.
pub fn export_to_file(conv: &Conversation, md: &str) -> std::io::Result<String> {
    let filename = format!("{}.md", conv.session_id);
    std::fs::write(&filename, md)?;
    Ok(filename)
}

fn render_claude_md(path: &Path, md: &mut String) -> Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry: LogEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        match entry {
            LogEntry::User {
                message,
                parent_tool_use_id,
                ..
            } => {
                if parent_tool_use_id.is_some() {
                    continue;
                }
                let text = extract_text_from_user(&message);
                if !text.is_empty() {
                    md.push_str("## User\n\n");
                    md.push_str(&text);
                    md.push_str("\n\n---\n\n");
                }
            }
            LogEntry::Assistant {
                message,
                parent_tool_use_id,
                ..
            } => {
                if parent_tool_use_id.is_some() {
                    continue;
                }
                let text_parts: Vec<&str> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();

                let combined = text_parts.join("\n\n");
                if !combined.is_empty() {
                    md.push_str("## Claude\n\n");
                    md.push_str(&combined);
                    md.push_str("\n\n---\n\n");
                }
            }
            _ => {}
        }
    }

    // Remove trailing separator
    if md.ends_with("\n\n---\n\n") {
        md.truncate(md.len() - "\n\n---\n\n".len());
        md.push('\n');
    }

    Ok(())
}

fn render_codex_md(path: &Path, md: &mut String) -> Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    for item in codex_items(&read_codex_lines(reader)) {
        if let CodexItem::Message { role, text } = item {
            let label = match role {
                CodexRole::User => "User",
                CodexRole::Assistant => "Codex",
            };
            md.push_str(&format!("## {}\n\n", label));
            md.push_str(&text);
            md.push_str("\n\n---\n\n");
        }
    }

    // Remove trailing separator
    if md.ends_with("\n\n---\n\n") {
        md.truncate(md.len() - "\n\n---\n\n".len());
        md.push('\n');
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn make_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
        f
    }

    fn codex_conversation(path: &Path) -> Conversation {
        Conversation {
            path: path.to_path_buf(),
            source: SessionSource::Codex,
            session_id: "test-session".to_string(),
            timestamp: Local::now(),
            preview: String::new(),
            full_text: String::new(),
            project_name: Some("project".to_string()),
            cwd: None,
            message_count: 0,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: None,
            hierarchy_has_children: false,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: 0,
            hierarchy_order: 0,
            hierarchy_sort_timestamp: Local::now(),
        }
    }

    #[test]
    fn exports_response_item_only_codex_messages() {
        let file = make_jsonl(&[
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"What changed?"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Codex answer"}]}}"#,
        ]);

        let md = to_markdown(&codex_conversation(file.path())).unwrap();

        assert!(md.contains("## User\n\nWhat changed?"));
        assert!(md.contains("## Codex\n\nCodex answer"));
    }

    #[test]
    fn exports_mixed_codex_messages_without_duplicate_response_items() {
        let file = make_jsonl(&[
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Response only"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Duplicated"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:02Z","type":"event_msg","payload":{"type":"user_message","message":"Duplicated"}}"#,
        ]);

        let md = to_markdown(&codex_conversation(file.path())).unwrap();

        assert!(md.contains("Response only"));
        assert!(md.contains("Duplicated"));
        assert_eq!(md.matches("Duplicated").count(), 1);
    }
}
