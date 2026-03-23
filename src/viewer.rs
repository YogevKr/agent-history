//! Readable session viewer — re-parses JSONL and builds styled lines.

use crate::claude::{extract_text_from_user, ContentBlock, LogEntry};
use crate::codex::{CodexLine, EventMsg, ResponseItem};
use crate::error::Result;
use crate::history::{Conversation, SessionSource};
use crate::syntax;
use crate::theme::theme;
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Parser as MdParser, Tag, TagEnd};
use std::fs::File;
use std::io::{BufRead, BufReader};

const SEPARATOR: &str = "────────────────────────────────────────────────────────";
const TOOL_OUTPUT_MAX: usize = 2048;

/// Ledger layout constants
const NAME_WIDTH: usize = 7;
const SEP: &str = " │ ";

/// A styled span within a line
#[derive(Clone)]
pub struct Span {
    pub text: String,
    pub fg: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub dim: bool,
}

impl Span {
    fn plain(text: &str) -> Self {
        Self { text: text.to_string(), fg: None, bold: false, dim: false }
    }
    fn rgb(text: &str, fg: (u8, u8, u8)) -> Self {
        Self { text: text.to_string(), fg: Some(fg), bold: false, dim: false }
    }
    fn bold_rgb(text: &str, fg: (u8, u8, u8)) -> Self {
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

/// Get terminal width (fallback 80)
fn term_width() -> usize {
    crossterm::terminal::size().map(|(c, _)| c as usize).unwrap_or(80)
}

/// Content width available after ledger gutter
fn content_width() -> usize {
    term_width().saturating_sub(NAME_WIDTH + SEP.len())
}

// ── Ledger helpers ────────────────────────────────────

fn ledger_first(role: &str, content: &str) -> String {
    format!("{:>NAME_WIDTH$}{}{}", role, SEP, content)
}

fn ledger_cont(content: &str) -> String {
    format!("{:>NAME_WIDTH$}{}{}", "", SEP, content)
}

fn ledger_blank() -> String {
    format!("{:>NAME_WIDTH$}{}", "", SEP.trim_end())
}

/// Build all styled lines for a session
pub fn build_session_lines(conv: &Conversation) -> Result<Vec<StyledLine>> {
    let mut lines = Vec::new();
    let t = theme();

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
    lines.push(vec![Span::rgb(SEPARATOR, t.border)]);
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

fn push_role(lines: &mut Vec<StyledLine>, _role: &str, text: &str, is_user: bool) {
    let t = theme();
    let color = if is_user { t.user_color } else { t.assistant_color };
    let label = if is_user { "You" } else { "Claude" };
    let cw = content_width();

    if is_user {
        let mut first = true;
        for line in text.lines() {
            if first {
                lines.push(vec![Span::bold_rgb(&ledger_first(label, line), color)]);
                first = false;
            } else {
                lines.push(vec![Span::plain(&ledger_cont(line))]);
            }
        }
    } else {
        let md_lines = markdown_to_lines(text, cw);
        let mut first = true;
        for md_line in md_lines {
            if md_line.is_empty() {
                lines.push(vec![Span::rgb(&ledger_blank(), t.border)]);
            } else if first {
                let mut out: StyledLine = vec![Span::bold_rgb(&format!("{:>NAME_WIDTH$}{}", label, SEP), color)];
                out.extend(md_line);
                lines.push(out);
                first = false;
            } else {
                let mut out: StyledLine = vec![Span::rgb(&format!("{:>NAME_WIDTH$}{}", "", SEP), t.border)];
                out.extend(md_line);
                lines.push(out);
            }
        }
    }
    lines.push(vec![Span::rgb(&ledger_blank(), t.border)]);
}

/// Parse markdown text into styled lines for the TUI pager.
fn markdown_to_lines(text: &str, max_width: usize) -> Vec<StyledLine> {
    let parser = MdParser::new(text);
    let mut result: Vec<StyledLine> = Vec::new();
    let mut current: StyledLine = Vec::new();
    let mut bold = false;
    let mut code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut heading = false;
    let mut blockquote = false;
    let mut list_depth: usize = 0;
    let mut paragraph_buf = String::new();
    let mut in_paragraph = false;
    let t = theme();

    for event in parser {
        match event {
            MdEvent::Start(Tag::Strong) => bold = true,
            MdEvent::End(TagEnd::Strong) => bold = false,
            MdEvent::Start(Tag::Emphasis) => bold = true,
            MdEvent::End(TagEnd::Emphasis) => bold = false,
            MdEvent::Start(Tag::Heading { .. }) => heading = true,
            MdEvent::End(TagEnd::Heading(_)) => {
                heading = false;
                result.push(std::mem::take(&mut current));
            }
            MdEvent::Start(Tag::CodeBlock(kind)) => {
                // Flush any pending paragraph text
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
                code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            MdEvent::End(TagEnd::CodeBlock) => {
                // Attempt syntect highlighting
                if let Some(highlighted) = syntax::highlight_code_tui(&code_buf, &code_lang) {
                    for tokens in highlighted {
                        let mut line: StyledLine = vec![Span::plain("  ")];
                        for tok in tokens {
                            line.push(Span { text: tok.text, fg: Some(tok.fg), bold: tok.bold, dim: false });
                        }
                        result.push(line);
                    }
                } else {
                    // Fallback: theme code color
                    for line in code_buf.lines() {
                        result.push(vec![Span::plain("  "), Span::rgb(line, t.code_block_fg)]);
                    }
                }
                code_block = false;
                code_buf.clear();
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
            }
            MdEvent::Start(Tag::BlockQuote(_)) => blockquote = true,
            MdEvent::End(TagEnd::BlockQuote(_)) => blockquote = false,
            MdEvent::Start(Tag::List(_)) => {
                // Flush paragraph if any
                if in_paragraph && !paragraph_buf.is_empty() {
                    flush_wrapped_paragraph(&mut result, &paragraph_buf, max_width, t.text_primary);
                    paragraph_buf.clear();
                }
                list_depth += 1;
            }
            MdEvent::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            MdEvent::Start(Tag::Item) => {
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                current.push(Span::rgb(&format!("{}• ", indent), t.list_bullet));
            }
            MdEvent::End(TagEnd::Item) => {
                result.push(std::mem::take(&mut current));
            }
            MdEvent::Start(Tag::Paragraph) => {
                in_paragraph = true;
                paragraph_buf.clear();
            }
            MdEvent::End(TagEnd::Paragraph) => {
                // Flush accumulated paragraph text with wrapping
                if !paragraph_buf.is_empty() {
                    flush_wrapped_paragraph(&mut result, &paragraph_buf, max_width, t.text_primary);
                    paragraph_buf.clear();
                }
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
                result.push(vec![]); // blank line
                in_paragraph = false;
            }
            MdEvent::Text(txt) => {
                if code_block {
                    code_buf.push_str(&txt);
                } else if heading {
                    current.push(Span::bold_rgb(&txt, t.heading));
                } else if blockquote {
                    current.push(Span::dim(&format!("│ {}", txt)));
                } else if list_depth > 0 {
                    // Inside a list item — don't wrap, just append
                    if bold {
                        current.push(Span::bold(&txt));
                    } else {
                        current.push(Span::plain(&txt));
                    }
                } else if in_paragraph {
                    paragraph_buf.push_str(&txt);
                } else if bold {
                    current.push(Span::bold(&txt));
                } else {
                    current.push(Span::plain(&txt));
                }
            }
            MdEvent::Code(code) => {
                if in_paragraph {
                    paragraph_buf.push('`');
                    paragraph_buf.push_str(&code);
                    paragraph_buf.push('`');
                } else {
                    current.push(Span::rgb(&format!("`{}`", code), t.code_inline));
                }
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                if in_paragraph {
                    paragraph_buf.push(' ');
                } else {
                    result.push(std::mem::take(&mut current));
                }
            }
            MdEvent::Rule => {
                result.push(std::mem::take(&mut current));
                result.push(vec![Span::rgb(SEPARATOR, t.border)]);
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    // Remove trailing empty lines
    while result.last().map_or(false, |l| l.is_empty()) {
        result.pop();
    }
    result
}

/// Wrap a paragraph string to max_width and push as styled lines.
fn flush_wrapped_paragraph(result: &mut Vec<StyledLine>, text: &str, max_width: usize, _fg: (u8, u8, u8)) {
    let t = theme();
    let width = if max_width > 4 { max_width } else { 80 };
    let wrapped = textwrap::wrap(text, width);
    for line in wrapped {
        // Handle inline code within wrapped text
        let spans = parse_inline_code(&line, t);
        result.push(spans);
    }
}

/// Parse inline `code` spans from a text line.
fn parse_inline_code(text: &str, t: &crate::theme::Theme) -> StyledLine {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        if start > 0 {
            spans.push(Span::plain(&rest[..start]));
        }
        let after = &rest[start + 1..];
        if let Some(end) = after.find('`') {
            spans.push(Span::rgb(&format!("`{}`", &after[..end]), t.code_inline));
            rest = &after[end + 1..];
        } else {
            spans.push(Span::plain(&rest[start..]));
            return spans;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::plain(rest));
    }
    spans
}

/// Extract the key argument from a tool call's input JSON.
fn tool_call_summary(name: &str, input: &serde_json::Value) -> String {
    let detail = match name {
        "Bash" => input.get("command").and_then(|v| v.as_str()).map(|s| truncate_str(s, 80)),
        "Read" => input.get("file_path").and_then(|v| v.as_str()).map(String::from),
        "Edit" => input.get("file_path").and_then(|v| v.as_str()).map(String::from),
        "Write" => input.get("file_path").and_then(|v| v.as_str()).map(String::from),
        "Grep" => input.get("pattern").and_then(|v| v.as_str()).map(|s| format!("\"{}\"", s)),
        "Glob" => input.get("pattern").and_then(|v| v.as_str()).map(String::from),
        "WebFetch" => input.get("url").and_then(|v| v.as_str()).map(|s| truncate_str(s, 80)),
        "Agent" | "Task" => input.get("description").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    };
    match detail {
        Some(d) => format!("{}: {}", name, d),
        None => name.to_string(),
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) { end -= 1; }
        format!("{}...", &s[..end])
    }
}

fn push_tool(lines: &mut Vec<StyledLine>, name: &str, input: &serde_json::Value) {
    let t = theme();
    let summary = tool_call_summary(name, input);
    lines.push(vec![
        Span::rgb(&format!("{:>NAME_WIDTH$}{}", "", SEP), t.border),
        Span::rgb("▸ ", t.tool_color),
        Span::rgb(&summary, t.tool_color),
    ]);
}

fn push_tool_output(lines: &mut Vec<StyledLine>, output: &str) {
    let t = theme();
    let truncated = truncate_tool_output(output);
    for (i, line) in truncated.lines().enumerate() {
        if i == 0 {
            lines.push(vec![
                Span::rgb(&format!("{:>NAME_WIDTH$}{}", "", SEP), t.border),
                Span::dim(&format!("  ╰─ {}", line)),
            ]);
        } else {
            lines.push(vec![
                Span::rgb(&format!("{:>NAME_WIDTH$}{}", "", SEP), t.border),
                Span::dim(&format!("     {}", line)),
            ]);
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
                let mut tool_calls: Vec<(&str, &serde_json::Value)> = Vec::new();

                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            text_parts.push(text.as_str());
                        }
                        ContentBlock::ToolUse { name, input, .. } => {
                            tool_calls.push((name.as_str(), input));
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
                for (name, input) in &tool_calls {
                    push_tool(lines, name, input);
                }
                if !combined.is_empty() || !tool_calls.is_empty() {
                    lines.push(vec![]);
                }
            }
            LogEntry::Summary { summary } => {
                let t = theme();
                lines.push(vec![Span::bold_rgb("Summary: ", t.accent), Span::rgb(&summary, t.accent)]);
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
    let empty_input = serde_json::Value::Null;

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
                            if let Some(name) = &item.name { push_tool(lines, name, &empty_input); }
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
                let mut tool_calls: Vec<(&str, &serde_json::Value)> = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => text_parts.push(text.as_str()),
                        ContentBlock::ToolUse { name, input, .. } => tool_calls.push((name.as_str(), input)),
                        ContentBlock::ToolResult { content: Some(content), .. } => {
                            let text = tool_result_text(content);
                            if !text.is_empty() {
                                print_tool_output_stdout(&text);
                            }
                        }
                        _ => {}
                    }
                }
                let combined = text_parts.join("\n");
                if !combined.is_empty() { print_role_stdout("Assistant", &combined, false); }
                for (name, input) in &tool_calls { print_tool_stdout(name, input); }
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
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let empty_input = serde_json::Value::Null;
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
                                print_tool_stdout(name, &empty_input);
                            }
                        }
                        "function_call_output" => {
                            if let Some(output) = &item.output {
                                if !output.is_empty() {
                                    print_tool_output_stdout(output);
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

fn print_role_stdout(_role: &str, text: &str, is_user: bool) {
    use colored::{Colorize, CustomColor};
    let t = theme();
    let (r, g, b) = if is_user { t.user_color } else { t.assistant_color };
    let label = if is_user { "You" } else { "Claude" };
    let ledger_label = format!("{:>NAME_WIDTH$} │ ", label);
    print!("{}", ledger_label.custom_color(CustomColor::new(r, g, b)).bold());
    if is_user {
        let mut first = true;
        for line in text.lines() {
            if first {
                println!("{}", line);
                first = false;
            } else {
                println!("{}{}", continuation_gutter_stdout(), line);
            }
        }
    } else {
        print_markdown_stdout(text);
    }
    println!("{}", continuation_gutter_stdout().dimmed());
}

fn continuation_gutter_stdout() -> String {
    format!("{:>NAME_WIDTH$} │ ", "")
}

fn print_tool_stdout(name: &str, input: &serde_json::Value) {
    use colored::{Colorize, CustomColor};
    let t = theme();
    let (r, g, b) = t.tool_color;
    let summary = tool_call_summary(name, input);
    let gutter = format!("{:>NAME_WIDTH$} │ ", "");
    println!("{}{}", gutter, format!("▸ {}", summary).custom_color(CustomColor::new(r, g, b)));
}

fn print_tool_output_stdout(output: &str) {
    use colored::Colorize;
    let gutter = format!("{:>NAME_WIDTH$} │ ", "");
    let truncated = truncate_tool_output(output);
    for (i, line) in truncated.lines().enumerate() {
        if i == 0 {
            println!("{}  {}", gutter, format!("╰─ {}", line).dimmed());
        } else {
            println!("{}     {}", gutter, line.dimmed());
        }
    }
}

fn print_markdown_stdout(text: &str) {
    use colored::{Colorize, CustomColor};
    let parser = MdParser::new(text);
    let mut line_buf = String::new();
    let mut bold = false;
    let mut code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut heading = false;
    let mut blockquote = false;
    let mut list_depth: usize = 0;
    let t = theme();
    let gutter_owned = continuation_gutter_stdout();
    let gutter = gutter_owned.as_str();

    let flush = |buf: &mut String, gutter: &str| {
        if !buf.is_empty() {
            println!("{}{}", gutter, buf);
            buf.clear();
        }
    };

    let max_width = term_width().saturating_sub(NAME_WIDTH + 3);

    for event in parser {
        match event {
            MdEvent::Start(Tag::Strong) | MdEvent::Start(Tag::Emphasis) => bold = true,
            MdEvent::End(TagEnd::Strong) | MdEvent::End(TagEnd::Emphasis) => bold = false,
            MdEvent::Start(Tag::Heading { .. }) => heading = true,
            MdEvent::End(TagEnd::Heading(_)) => {
                heading = false;
                if !line_buf.is_empty() {
                    let (r, g, b) = t.heading;
                    println!("{}{}", gutter, line_buf.custom_color(CustomColor::new(r, g, b)).bold());
                    line_buf.clear();
                }
            }
            MdEvent::Start(Tag::CodeBlock(kind)) => {
                flush(&mut line_buf, &gutter);
                code_block = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            MdEvent::End(TagEnd::CodeBlock) => {
                // Try syntect ANSI output
                if let Some(ansi) = syntax::highlight_code_ansi(&code_buf, &code_lang) {
                    for line in ansi.lines() {
                        println!("{}  {}", gutter, line);
                    }
                } else {
                    let (r, g, b) = t.code_block_fg;
                    for line in code_buf.lines() {
                        println!("{}  {}", gutter, line.custom_color(CustomColor::new(r, g, b)));
                    }
                }
                code_block = false;
                code_buf.clear();
            }
            MdEvent::Start(Tag::BlockQuote(_)) => blockquote = true,
            MdEvent::End(TagEnd::BlockQuote(_)) => blockquote = false,
            MdEvent::Start(Tag::List(_)) => {
                flush(&mut line_buf, &gutter);
                list_depth += 1;
            }
            MdEvent::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            MdEvent::Start(Tag::Item) => {
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                line_buf.push_str(&format!("{}• ", indent));
            }
            MdEvent::End(TagEnd::Item) => flush(&mut line_buf, &gutter),
            MdEvent::End(TagEnd::Paragraph) => {
                // Wrap paragraph text
                if !line_buf.is_empty() {
                    let wrapped = textwrap::wrap(&line_buf, max_width);
                    for wline in wrapped {
                        println!("{}{}", gutter, wline);
                    }
                    line_buf.clear();
                }
                println!("{}", gutter.trim_end());
            }
            MdEvent::Text(txt) => {
                if code_block {
                    code_buf.push_str(&txt);
                } else if heading {
                    line_buf.push_str(&txt);
                } else if blockquote {
                    line_buf.push_str(&format!("{} {}", "│".dimmed(), txt.dimmed()));
                } else if bold {
                    line_buf.push_str(&txt.bold().to_string());
                } else {
                    line_buf.push_str(&txt);
                }
            }
            MdEvent::Code(code) => {
                let (r, g, b) = t.code_inline;
                line_buf.push_str(&format!("`{}`", code).custom_color(CustomColor::new(r, g, b)).to_string());
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                line_buf.push(' ');
            }
            MdEvent::Rule => {
                flush(&mut line_buf, &gutter);
                println!("{}{}", gutter, SEPARATOR.dimmed());
            }
            _ => {}
        }
    }
    flush(&mut line_buf, &gutter);
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
