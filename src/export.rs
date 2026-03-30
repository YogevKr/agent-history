//! Export a conversation to human-friendly markdown.
//! Only includes user and assistant text messages (no tool calls, outputs, or thinking).

use crate::claude::{extract_text_from_user, ContentBlock, LogEntry};
use crate::codex::{CodexLine, EventMsg, ResponseItem};
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
    md.push_str(&format!("# Session: {} ({})\n", conv.session_id, conv.source));
    let project = conv.project_name.as_deref().unwrap_or("unknown");
    let model = format_model_short(conv.model.as_deref());
    let date = conv.timestamp.format("%Y-%m-%d %H:%M");
    md.push_str(&format!("**Project:** {} | **Model:** {} | **Date:** {}\n", project, model, date));
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
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()?;
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

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let codex_line: CodexLine = match serde_json::from_str(&line) {
            Ok(l) => l,
            Err(_) => continue,
        };

        match codex_line.line_type.as_str() {
            "event_msg" => {
                if let Ok(evt) = serde_json::from_value::<EventMsg>(codex_line.payload) {
                    match evt.event_type.as_str() {
                        "user_message" => {
                            if let Some(msg) = &evt.message {
                                if !msg.is_empty() {
                                    md.push_str("## User\n\n");
                                    md.push_str(msg);
                                    md.push_str("\n\n---\n\n");
                                }
                            }
                        }
                        "agent_message" => {
                            if let Some(msg) = &evt.message {
                                if !msg.is_empty() {
                                    md.push_str("## Codex\n\n");
                                    md.push_str(msg);
                                    md.push_str("\n\n---\n\n");
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(codex_line.payload) {
                    if item.item_type == "message" {
                        let role = item.role.as_deref().unwrap_or("");
                        if role == "developer" {
                            continue;
                        }
                        if let Some(parts) = &item.content {
                            let texts: Vec<&str> = parts
                                .iter()
                                .filter_map(|p| p.text.as_deref())
                                .filter(|t| !t.is_empty())
                                .collect();
                            let combined = texts.join("\n\n");
                            if !combined.is_empty() {
                                let label = match role {
                                    "user" => "User",
                                    "assistant" => "Codex",
                                    _ => continue,
                                };
                                md.push_str(&format!("## {}\n\n", label));
                                md.push_str(&combined);
                                md.push_str("\n\n---\n\n");
                            }
                        }
                    }
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
