//! Readable session viewer — re-parses JSONL and builds styled lines.

use crate::claude::{extract_text_from_user, ContentBlock, LogEntry};
use crate::codex::{CodexLine, EventMsg, ResponseItem};
use crate::error::Result;
use crate::history::{Conversation, SessionSource};
use crossterm::style::Color;
use std::fs::File;
use std::io::{BufRead, BufReader};

const SEPARATOR: &str = "────────────────────────────────────────────────────────";
const TOOL_OUTPUT_MAX: usize = 2048;

/// A styled span within a line
#[derive(Clone)]
pub struct Span {
    pub text: String,
    pub fg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
}

impl Span {
    fn plain(text: &str) -> Self {
        Self { text: text.to_string(), fg: None, bold: false, dim: false }
    }
    fn colored(text: &str, fg: Color) -> Self {
        Self { text: text.to_string(), fg: Some(fg), bold: false, dim: false }
    }
    fn bold_colored(text: &str, fg: Color) -> Self {
        Self { text: text.to_string(), fg: Some(fg), bold: true, dim: false }
    }
    fn bold(text: &str) -> Self {
        Self { text: text.to_string(), fg: None, bold: true, dim: false }
    }
    fn dim(text: &str) -> Self {
        Self { text: text.to_string(), fg: None, bold: false, dim: true }
    }
}

/// A line is a vec of styled spans
pub type StyledLine = Vec<Span>;

/// Build all styled lines for a session
pub fn build_session_lines(conv: &Conversation) -> Result<Vec<StyledLine>> {
    let mut lines = Vec::new();

    // Header
    lines.push(vec![Span::bold("Session: "), Span::plain(&conv.session_id), Span::dim(&format!(" ({})", conv.source))]);
    lines.push(vec![Span::bold("Project: "), Span::plain(conv.project_name.as_deref().unwrap_or("unknown"))]);
    if let Some(ref model) = conv.model {
        lines.push(vec![Span::bold("Model: "), Span::plain(model)]);
    }
    if let Some(ref branch) = conv.git_branch {
        lines.push(vec![Span::bold("Branch: "), Span::plain(branch)]);
    }
    lines.push(vec![Span::bold("Messages: "), Span::plain(&conv.message_count.to_string())]);
    if conv.total_tokens > 0 {
        lines.push(vec![Span::bold("Tokens: "), Span::plain(&conv.total_tokens.to_string())]);
    }
    if let Some(dur) = conv.duration_minutes {
        lines.push(vec![Span::bold("Duration: "), Span::plain(&format!("{}m", dur))]);
    }
    lines.push(vec![Span::dim(SEPARATOR)]);
    lines.push(vec![]);

    // Content
    match conv.source {
        SessionSource::Claude => build_claude_lines(&conv.path, &mut lines)?,
        SessionSource::Codex => build_codex_lines(&conv.path, &mut lines)?,
    }

    Ok(lines)
}

/// Render session to stdout (for non-interactive --show)
pub fn review_session(conv: &Conversation) -> Result<()> {
    use colored::Colorize;

    println!("{} {} ({})", "Session:".bold(), conv.session_id, conv.source.to_string().dimmed());
    println!("{} {}", "Project:".bold(), conv.project_name.as_deref().unwrap_or("unknown"));
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

    match conv.source {
        SessionSource::Claude => print_claude(&conv.path)?,
        SessionSource::Codex => print_codex(&conv.path)?,
    }
    Ok(())
}

// ── Line builders (for TUI) ───────────────────────────

fn push_role(lines: &mut Vec<StyledLine>, role: &str, text: &str, is_user: bool) {
    let color = if is_user { Color::Green } else { Color::Blue };
    lines.push(vec![Span::bold_colored(&format!("{}:", role), color)]);
    for line in text.lines() {
        lines.push(vec![Span::plain(&format!("  {}", line))]);
    }
    lines.push(vec![]);
}

fn push_tool(lines: &mut Vec<StyledLine>, name: &str) {
    lines.push(vec![Span::plain("   "), Span::colored("▸ ", Color::Yellow), Span::colored(name, Color::Yellow)]);
}

fn push_tool_output(lines: &mut Vec<StyledLine>, output: &str) {
    let truncated = truncate_tool_output(output);
    for (i, line) in truncated.lines().enumerate() {
        if i == 0 {
            lines.push(vec![Span::dim(&format!("   ╰─ {}", line))]);
        } else {
            lines.push(vec![Span::dim(&format!("      {}", line))]);
        }
    }
}

fn build_claude_lines(path: &std::path::Path, lines: &mut Vec<StyledLine>) -> Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        if line.trim().is_empty() { continue; }
        let entry: LogEntry = match serde_json::from_str(&line) { Ok(e) => e, Err(_) => continue };

        match entry {
            LogEntry::User { message, parent_tool_use_id, .. } => {
                if parent_tool_use_id.is_some() { continue; }
                let text = extract_text_from_user(&message);
                if !text.is_empty() {
                    push_role(lines, "You", &text, true);
                }
            }
            LogEntry::Assistant { message, parent_tool_use_id, .. } => {
                if parent_tool_use_id.is_some() { continue; }
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            text_parts.push(text.as_str());
                        }
                        ContentBlock::ToolUse { name, .. } => {
                            tool_calls.push(name.as_str());
                        }
                        ContentBlock::ToolResult { content: Some(content), .. } => {
                            let text = tool_result_text(content);
                            if !text.is_empty() {
                                push_tool_output(lines, &text);
                            }
                        }
                        _ => {}
                    }
                }

                let combined = text_parts.join("\n");
                if !combined.is_empty() {
                    push_role(lines, "Assistant", &combined, false);
                }
                for name in &tool_calls {
                    push_tool(lines, name);
                }
                if !combined.is_empty() || !tool_calls.is_empty() {
                    lines.push(vec![]);
                }
            }
            LogEntry::Summary { summary } => {
                lines.push(vec![Span::bold_colored("Summary: ", Color::Cyan), Span::colored(&summary, Color::Cyan)]);
                lines.push(vec![]);
            }
            _ => {}
        }
    }
    Ok(())
}

fn build_codex_lines(path: &std::path::Path, lines: &mut Vec<StyledLine>) -> Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        if line.trim().is_empty() { continue; }
        let codex_line: CodexLine = match serde_json::from_str(&line) { Ok(l) => l, Err(_) => continue };

        match codex_line.line_type.as_str() {
            "event_msg" => {
                if let Ok(evt) = serde_json::from_value::<EventMsg>(codex_line.payload) {
                    match evt.event_type.as_str() {
                        "user_message" => {
                            if let Some(msg) = &evt.message {
                                if !msg.is_empty() { push_role(lines, "You", msg, true); }
                            }
                        }
                        "agent_message" => {
                            if let Some(msg) = &evt.message {
                                if !msg.is_empty() { push_role(lines, "Assistant", msg, false); }
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
                            if let Some(name) = &item.name { push_tool(lines, name); }
                        }
                        "function_call_output" => {
                            if let Some(output) = &item.output {
                                if !output.is_empty() { push_tool_output(lines, output); }
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

// ── Stdout printer (for non-interactive) ──────────────

fn print_claude(path: &std::path::Path) -> Result<()> {
    use colored::Colorize;
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        if line.trim().is_empty() { continue; }
        let entry: LogEntry = match serde_json::from_str(&line) { Ok(e) => e, Err(_) => continue };
        match entry {
            LogEntry::User { message, parent_tool_use_id, .. } => {
                if parent_tool_use_id.is_some() { continue; }
                let text = extract_text_from_user(&message);
                if !text.is_empty() { print_role_stdout("You", &text, true); }
            }
            LogEntry::Assistant { message, parent_tool_use_id, .. } => {
                if parent_tool_use_id.is_some() { continue; }
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => text_parts.push(text.as_str()),
                        ContentBlock::ToolUse { name, .. } => tool_calls.push(name.as_str()),
                        ContentBlock::ToolResult { content: Some(content), .. } => {
                            let text = tool_result_text(content);
                            if !text.is_empty() {
                                println!("   {} {}", "╰─".dimmed(), truncate_tool_output(&text).dimmed());
                            }
                        }
                        _ => {}
                    }
                }
                let combined = text_parts.join("\n");
                if !combined.is_empty() { print_role_stdout("Assistant", &combined, false); }
                for name in &tool_calls { println!("   {} {}", "▸".yellow(), name.yellow()); }
                if !combined.is_empty() || !tool_calls.is_empty() { println!(); }
            }
            LogEntry::Summary { summary } => {
                println!("{} {}\n", "Summary:".bold().cyan(), summary.cyan());
            }
            _ => {}
        }
    }
    Ok(())
}

fn print_codex(path: &std::path::Path) -> Result<()> {
    use colored::Colorize;
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        if line.trim().is_empty() { continue; }
        let codex_line: CodexLine = match serde_json::from_str(&line) { Ok(l) => l, Err(_) => continue };
        match codex_line.line_type.as_str() {
            "event_msg" => {
                if let Ok(evt) = serde_json::from_value::<EventMsg>(codex_line.payload) {
                    match evt.event_type.as_str() {
                        "user_message" => {
                            if let Some(msg) = &evt.message {
                                if !msg.is_empty() { print_role_stdout("You", msg, true); }
                            }
                        }
                        "agent_message" => {
                            if let Some(msg) = &evt.message {
                                if !msg.is_empty() { print_role_stdout("Assistant", msg, false); }
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
                                use colored::Colorize;
                                println!("   {} {}", "▸".yellow(), name.yellow());
                            }
                        }
                        "function_call_output" => {
                            if let Some(output) = &item.output {
                                if !output.is_empty() {
                                    use colored::Colorize;
                                    println!("   {} {}", "╰─".dimmed(), truncate_tool_output(output).dimmed());
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

fn print_role_stdout(role: &str, text: &str, is_user: bool) {
    use colored::Colorize;
    let label = if is_user {
        format!("{}:", role).green().bold().to_string()
    } else {
        format!("{}:", role).blue().bold().to_string()
    };
    println!("{}", label);
    for line in text.lines() { println!("  {}", line); }
    println!();
}

// ── Shared helpers ────────────────────────────────────

fn truncate_tool_output(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or(s);
    if first_line.len() <= TOOL_OUTPUT_MAX && s.lines().count() <= 3 {
        let lines: Vec<&str> = s.lines().take(3).collect();
        let result = lines.join("\n     ");
        if s.lines().count() > 3 {
            format!("{}\n     ...", result)
        } else {
            result
        }
    } else if first_line.len() > 120 {
        let mut end = 120;
        while end > 0 && !first_line.is_char_boundary(end) { end -= 1; }
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
            items.iter().filter_map(|item| {
                if let serde_json::Value::Object(map) = item {
                    map.get("text").and_then(|v| v.as_str()).map(String::from)
                } else if let serde_json::Value::String(s) = item {
                    Some(s.clone())
                } else { None }
            }).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}
