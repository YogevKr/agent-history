//! Readable session viewer — re-parses JSONL and renders formatted output.

use crate::claude::{
    extract_text_from_blocks, extract_text_from_user, ContentBlock, LogEntry,
};
use crate::codex::{CodexLine, EventMsg, ResponseItem, SessionMeta, TurnContext};
use crate::error::Result;
use crate::history::{Conversation, SessionSource};
use colored::Colorize;
use std::fs::File;
use std::io::{BufRead, BufReader};

const SEPARATOR: &str = "────────────────────────────────────────────────────────";
const TOOL_OUTPUT_MAX: usize = 2048;

/// Render a session in readable format
pub fn review_session(conv: &Conversation) -> Result<()> {
    print_header(conv);

    match conv.source {
        SessionSource::Claude => review_claude(&conv.path)?,
        SessionSource::Codex => review_codex(&conv.path)?,
    }

    Ok(())
}

fn print_header(conv: &Conversation) {
    println!(
        "{} {} ({})",
        "Session:".bold(),
        conv.session_id,
        conv.source.to_string().dimmed()
    );
    println!(
        "{} {}",
        "Project:".bold(),
        conv.project_name.as_deref().unwrap_or("unknown")
    );
    if let Some(ref model) = conv.model {
        println!("{} {}", "Model:".bold(), model);
    }
    if let Some(ref branch) = conv.git_branch {
        println!("{} {}", "Branch:".bold(), branch);
    }
    println!("{} {}", "Messages:".bold(), conv.message_count);
    if conv.total_tokens > 0 {
        println!("{} {}", "Tokens:".bold(), conv.total_tokens);
    }
    if let Some(dur) = conv.duration_minutes {
        println!("{} {}m", "Duration:".bold(), dur);
    }
    println!("{}", SEPARATOR.dimmed());
    println!();
}

// ── Claude viewer ──────────────────────────────────────

fn review_claude(path: &std::path::Path) -> Result<()> {
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
                    print_role("You", &text, true);
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
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                text_parts.push(text.as_str());
                            }
                        }
                        ContentBlock::ToolUse { name, .. } => {
                            tool_calls.push(name.as_str());
                        }
                        ContentBlock::ToolResult { content, .. } => {
                            if let Some(content) = content {
                                let text = tool_result_text(content);
                                if !text.is_empty() {
                                    let truncated = truncate_tool_output(&text);
                                    println!(
                                        "   {} {}",
                                        "╰─".dimmed(),
                                        truncated.dimmed()
                                    );
                                }
                            }
                        }
                        ContentBlock::Thinking { .. } | ContentBlock::Image { .. } => {}
                    }
                }

                let combined = text_parts.join("\n");
                if !combined.is_empty() {
                    print_role("Assistant", &combined, false);
                }

                for name in &tool_calls {
                    println!("   {} {}", "▸".yellow(), name.yellow());
                }

                if !combined.is_empty() || !tool_calls.is_empty() {
                    println!();
                }
            }
            LogEntry::Summary { summary } => {
                println!(
                    "{} {}\n",
                    "Summary:".bold().cyan(),
                    summary.cyan()
                );
            }
            _ => {}
        }
    }

    Ok(())
}

// ── Codex viewer ───────────────────────────────────────

fn review_codex(path: &std::path::Path) -> Result<()> {
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
                                    print_role("You", msg, true);
                                }
                            }
                        }
                        "agent_message" => {
                            if let Some(msg) = &evt.message {
                                if !msg.is_empty() {
                                    print_role("Assistant", msg, false);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(codex_line.payload) {
                    match item.item_type.as_str() {
                        "function_call" => {
                            if let Some(name) = &item.name {
                                println!("   {} {}", "▸".yellow(), name.yellow());
                            }
                        }
                        "function_call_output" => {
                            if let Some(output) = &item.output {
                                if !output.is_empty() {
                                    let truncated = truncate_tool_output(output);
                                    println!(
                                        "   {} {}",
                                        "╰─".dimmed(),
                                        truncated.dimmed()
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

// ── Helpers ────────────────────────────────────────────

fn print_role(role: &str, text: &str, is_user: bool) {
    let label = if is_user {
        format!("{}:", role).green().bold().to_string()
    } else {
        format!("{}:", role).blue().bold().to_string()
    };
    println!("{}", label);
    // Indent the text slightly
    for line in text.lines() {
        println!("  {}", line);
    }
    println!();
}

fn truncate_tool_output(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() <= TOOL_OUTPUT_MAX && s.lines().count() <= 3 {
        // Show up to 3 lines
        let lines: Vec<&str> = s.lines().take(3).collect();
        let result = lines.join("\n     ");
        if s.lines().count() > 3 {
            format!("{}\n     ...", result)
        } else {
            result
        }
    } else if first_line.len() > 120 {
        let mut end = 120;
        while end > 0 && !first_line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &first_line[..end])
    } else {
        let line_count = s.lines().count();
        format!("{} ({} more lines)", first_line, line_count - 1)
    }
}

fn tool_result_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            items
                .iter()
                .filter_map(|item| {
                    if let serde_json::Value::Object(map) = item {
                        map.get("text").and_then(|v| v.as_str()).map(String::from)
                    } else if let serde_json::Value::String(s) = item {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => String::new(),
    }
}
