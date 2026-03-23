use crate::history::{Conversation, SessionSource};
use chrono::{DateTime, Local};
use colored::Colorize;

/// Short session ID (first 8 chars)
pub fn short_id(id: &str) -> &str {
    let end = id.char_indices().nth(8).map(|(i, _)| i).unwrap_or(id.len());
    &id[..end]
}

pub fn format_result(conv: &Conversation) -> String {
    let source_tag = match conv.source {
        SessionSource::Claude => "[claude]".blue().to_string(),
        SessionSource::Codex => "[codex]".green().to_string(),
    };
    let age = format_relative_time(conv.timestamp);
    let project = conv.project_name.as_deref().unwrap_or("unknown");
    let model = format_model_short(conv.model.as_deref());
    let title = get_display_title(conv);
    let preview = truncate(&title, 60);
    let sid = short_id(&conv.session_id).dimmed();
    format!(
        " {} {:>6}  {:<20}  ({})  {}  \"{}\"",
        source_tag, age, project, model, sid, preview
    )
}

pub fn format_relative_time(timestamp: DateTime<Local>) -> String {
    let now = Local::now();
    let duration = now.signed_duration_since(timestamp);
    let secs = duration.num_seconds();
    if secs < 60 {
        return "now".to_string();
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86400 {
        return format!("{}h", secs / 3600);
    }
    if secs < 604800 {
        return format!("{}d", secs / 86400);
    }
    format!("{}w", secs / 604800)
}

pub fn format_model_short(model: Option<&str>) -> String {
    match model {
        None => "?".to_string(),
        Some(m) => {
            // "claude-opus-4-6-20251101" -> "opus-4-6"
            // "claude-sonnet-4-6-20251101" -> "sonnet-4-6"
            if let Some(rest) = m.strip_prefix("claude-") {
                // Strip trailing date suffix like "-20251101"
                let base = if let Some(pos) = rest.rfind('-') {
                    let suffix = &rest[pos + 1..];
                    if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
                        &rest[..pos]
                    } else {
                        rest
                    }
                } else {
                    rest
                };
                base.to_string()
            } else {
                m.to_string()
            }
        }
    }
}

pub fn get_display_title(conv: &Conversation) -> String {
    conv.custom_title
        .as_deref()
        .or(conv.summary.as_deref())
        .unwrap_or(&conv.preview)
        .to_string()
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(3);
    let truncated: String = s.chars().take(take).collect();
    format!("{}...", truncated)
}

pub fn show_session(conv: &Conversation) {
    println!("Session: {} ({})", conv.session_id, conv.source);
    println!(
        "Project: {}",
        conv.project_name.as_deref().unwrap_or("unknown")
    );
    if let Some(ref model) = conv.model {
        println!("Model: {}", model);
    }
    if let Some(ref branch) = conv.git_branch {
        println!("Branch: {}", branch);
    }
    println!("Messages: {}", conv.message_count);
    println!("---");
    println!("{}", conv.full_text);
}
