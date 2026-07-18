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
    let directory = conv.directory_name.as_deref().unwrap_or("unknown");
    let model = format_model_short(conv.model.as_deref());
    let date = conv.timestamp.format("%Y-%m-%d %H:%M");
    md.push_str(&format!(
        "**Directory:** {} | **Model:** {} | **Date:** {}\n",
        directory, model, date
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
    let filename = markdown_filename(conv);
    write_private_file(Path::new(&filename), md.as_bytes())?;
    Ok(filename)
}

/// Export markdown to a directory. Returns the path written.
pub fn export_to_dir(conv: &Conversation, md: &str, out_dir: &str) -> std::io::Result<String> {
    let out_dir = Path::new(out_dir);
    ensure_export_dir(out_dir)?;
    let path = out_dir.join(markdown_filename(conv));
    write_private_file(&path, md.as_bytes())?;
    Ok(path.to_string_lossy().to_string())
}

fn ensure_export_dir(out_dir: &Path) -> std::io::Result<()> {
    match std::fs::metadata(out_dir) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "export path exists and is not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(out_dir)?;
            set_private_dir_permissions(out_dir)
        }
        Err(error) => Err(error),
    }
}

fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    set_private_file_permissions(path)?;
    file.write_all(data)
}

fn markdown_filename(conv: &Conversation) -> String {
    let id = safe_filename_part(&conv.session_id, "session");
    format!("{}-{}.md", conv.source, id)
}

fn safe_filename_part(value: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches(|ch| matches!(ch, '-' | '_' | '.'));
    if out.is_empty() {
        fallback.to_string()
    } else {
        out.chars().take(120).collect()
    }
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
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
    use tempfile::{tempdir, NamedTempFile};

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
            directory_name: Some("directory".to_string()),
            cwd: None,
            message_count: 0,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: None,
            hierarchy_root_id: None,
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

    #[test]
    fn export_to_dir_uses_safe_filename() {
        let file = make_jsonl(&[]);
        let mut conv = codex_conversation(file.path());
        conv.session_id = "../unsafe/session:id".to_string();
        let out_dir = tempdir().unwrap();

        let path = export_to_dir(&conv, "private", out_dir.path().to_str().unwrap()).unwrap();

        assert!(path.ends_with("codex-unsafe-session-id.md"));
        assert!(Path::new(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn export_to_dir_writes_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let file = make_jsonl(&[]);
        let conv = codex_conversation(file.path());
        let root = tempdir().unwrap();
        let out_dir = root.path().join("exports");

        let path = export_to_dir(&conv, "private", out_dir.to_str().unwrap()).unwrap();

        let dir_mode = std::fs::metadata(&out_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
