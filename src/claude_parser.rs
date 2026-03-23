//! JSONL conversation file parsing for Claude sessions.

use crate::claude::{
    extract_search_text_from_assistant, extract_search_text_from_user, extract_text_from_assistant,
    extract_text_from_user, LogEntry, TokenUsage,
};
use crate::error::Result;
use crate::history::{Conversation, SessionSource};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::SystemTime;

/// Maximum characters for full_text search index per conversation.
const MAX_FULL_TEXT_CHARS: usize = 256 * 1024;

/// Process a single Claude conversation JSONL file
pub fn process_claude_file(
    path: PathBuf,
    modified: Option<SystemTime>,
) -> Result<Option<Conversation>> {
    let file = File::open(&path)?;
    let reader = BufReader::new(file);

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_owned();

    let lines: Vec<String> = reader.lines().map_while(|l| l.ok()).collect();

    let mut all_parts = Vec::new();
    let mut preview_parts = Vec::new();
    let mut user_messages = Vec::new();
    let mut seen_real_user_message = false;
    let mut skip_next_assistant = false;
    let mut extracted_cwd: Option<PathBuf> = None;
    let mut message_count: usize = 0;
    let mut extracted_summary: Option<String> = None;
    let mut extracted_custom_title: Option<String> = None;
    let mut extracted_model: Option<String> = None;
    let extracted_git_branch: Option<String> = None;
    let mut token_usage_by_msg: HashMap<String, TokenUsage> = HashMap::new();
    let mut anonymous_token_count: u64 = 0;
    let mut first_timestamp: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    let mut last_timestamp: Option<chrono::DateTime<chrono::FixedOffset>> = None;

    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(entry) = serde_json::from_str::<LogEntry>(line) else {
            continue;
        };

        match entry {
            LogEntry::User {
                message,
                cwd,
                timestamp,
                ..
            } => {
                if let Some(ref ts_str) = timestamp {
                    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                        if first_timestamp.is_none() {
                            first_timestamp = Some(ts);
                        }
                        last_timestamp = Some(ts);
                    }
                }

                if extracted_cwd.is_none() {
                    if let Some(cwd_str) = cwd {
                        extracted_cwd = Some(PathBuf::from(cwd_str));
                    }
                }

                let preview_text = extract_text_from_user(&message);
                let search_text = extract_search_text_from_user(&message);

                if preview_text.is_empty() && search_text.is_empty() {
                    continue;
                }

                if !preview_text.is_empty() {
                    user_messages.push(preview_text.clone());
                }

                let effective_preview =
                    if let Some(skill_preview) = extract_skill_preview(&preview_text) {
                        skill_preview
                    } else if !preview_text.is_empty() && is_clear_metadata_message(&preview_text) {
                        if !search_text.is_empty() {
                            all_parts.push(search_text);
                        }
                        continue;
                    } else {
                        preview_text
                    };

                if !search_text.is_empty() {
                    all_parts.push(search_text);
                }

                let is_warmup = !seen_real_user_message && effective_preview.trim() == "Warmup";
                if is_warmup {
                    skip_next_assistant = true;
                } else if !effective_preview.is_empty() {
                    message_count += 1;
                    preview_parts.push(effective_preview);
                    seen_real_user_message = true;
                }
            }
            LogEntry::Assistant {
                message, timestamp, ..
            } => {
                if let Some(ref ts_str) = timestamp {
                    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                        if first_timestamp.is_none() {
                            first_timestamp = Some(ts);
                        }
                        last_timestamp = Some(ts);
                    }
                }

                if extracted_model.is_none() {
                    if let Some(model) = &message.model {
                        extracted_model = Some(model.clone());
                    }
                }

                if let Some(usage) = &message.usage {
                    if let Some(msg_id) = &message.id {
                        token_usage_by_msg.insert(msg_id.clone(), usage.clone());
                    } else {
                        anonymous_token_count += usage.input_tokens
                            + usage.output_tokens
                            + usage.cache_creation_input_tokens
                            + usage.cache_read_input_tokens;
                    }
                }

                let preview_text = extract_text_from_assistant(&message);
                let search_text = extract_search_text_from_assistant(&message);

                if !search_text.is_empty() {
                    all_parts.push(search_text);
                }

                if skip_next_assistant {
                    skip_next_assistant = false;
                } else if seen_real_user_message && !preview_text.is_empty() {
                    message_count += 1;
                    preview_parts.push(preview_text);
                }
            }
            LogEntry::Summary { summary } => {
                if extracted_summary.is_none() {
                    extracted_summary = Some(summary.clone());
                }
            }
            LogEntry::CustomTitle { custom_title } => {
                let trimmed = custom_title.trim();
                extracted_custom_title = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                };
            }
            LogEntry::System { .. }
            | LogEntry::Progress { .. }
            | LogEntry::FileHistorySnapshot { .. } => {}
        }
    }

    if is_clear_only_conversation(&user_messages) {
        return Ok(None);
    }

    if all_parts.is_empty() || preview_parts.is_empty() {
        return Ok(None);
    }

    let timestamp = modified
        .map(DateTime::<Local>::from)
        .unwrap_or_else(Local::now);

    let preview = preview_parts
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ... ");

    let mut full_text = all_parts.join(" ");
    if let Some(ref summary) = extracted_summary {
        full_text = format!("{} {}", summary, full_text);
    }
    if let Some(ref custom_title) = extracted_custom_title {
        full_text = format!("{} {}", custom_title, full_text);
    }

    let preview = normalize_whitespace(&preview);
    let full_text = normalize_whitespace(&full_text);
    let full_text = truncate_to_char_boundary(&full_text, MAX_FULL_TEXT_CHARS);

    let total_tokens: u64 = token_usage_by_msg
        .values()
        .map(|u| {
            u.input_tokens
                + u.output_tokens
                + u.cache_creation_input_tokens
                + u.cache_read_input_tokens
        })
        .sum::<u64>()
        + anonymous_token_count;

    let duration_minutes = match (first_timestamp, last_timestamp) {
        (Some(first), Some(last)) => {
            let duration = last.signed_duration_since(first);
            let minutes = duration.num_minutes();
            if minutes > 0 {
                Some(minutes as u64)
            } else {
                None
            }
        }
        _ => None,
    };

    Ok(Some(Conversation {
        path,
        source: SessionSource::Claude,
        session_id,
        timestamp,
        preview,
        full_text,
        project_name: None,
        cwd: extracted_cwd,
        message_count,
        model: extracted_model,
        total_tokens,
        duration_minutes,
        summary: extracted_summary,
        custom_title: extracted_custom_title,
        git_branch: extracted_git_branch,
    }))
}

fn is_clear_metadata_message(message: &str) -> bool {
    let trimmed = message.trim();

    trimmed.is_empty()
        || trimmed.starts_with(
            "Caveat: The messages below were generated by the user while running local commands.",
        )
        || trimmed.contains("<local-command-caveat>")
        || trimmed.contains("<command-name>/clear</command-name>")
        || trimmed.contains("<command-message>clear</command-message>")
        || trimmed.contains("<local-command-stdout>")
        || trimmed.starts_with("Base directory for this skill:")
}

fn extract_skill_preview(message: &str) -> Option<String> {
    let trimmed = message.trim();

    let start = trimmed.find("<command-name>")?;
    let end = trimmed.find("</command-name>")?;
    let content_start = start + "<command-name>".len();
    if content_start >= end {
        return None;
    }

    let command_name = &trimmed[content_start..end];
    if command_name == "/clear" {
        return None;
    }

    if let Some(args_start) = trimmed.find("<command-args>") {
        if let Some(args_end) = trimmed.find("</command-args>") {
            let args_content_start = args_start + "<command-args>".len();
            if args_content_start < args_end {
                let args = trimmed[args_content_start..args_end].trim();
                if !args.is_empty() {
                    return Some(format!("{} {}", command_name, args));
                }
            }
        }
    }

    Some(command_name.to_string())
}

fn is_clear_only_conversation(user_messages: &[String]) -> bool {
    if user_messages.is_empty() {
        return false;
    }

    let mut saw_caveat = false;
    let mut saw_command = false;
    let mut saw_stdout = false;

    for msg in user_messages {
        let trimmed = msg.trim();
        if trimmed.is_empty() {
            continue;
        }

        let is_caveat = trimmed.starts_with(
            "Caveat: The messages below were generated by the user while running local commands.",
        );
        let has_command_tag = trimmed.contains("<command-name>/clear</command-name>");
        let has_stdout_tag = trimmed.contains("<local-command-stdout>");

        if is_caveat {
            saw_caveat = true;
        }
        if has_command_tag {
            saw_command = true;
        }
        if has_stdout_tag {
            saw_stdout = true;
        }

        if !(is_caveat || has_command_tag || has_stdout_tag) {
            return false;
        }
    }

    saw_caveat && saw_command && saw_stdout
}

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<&str>>().join(" ")
}

fn truncate_to_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}
