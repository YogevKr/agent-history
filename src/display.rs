use crate::history::{Conversation, SessionSource};
use chrono::{DateTime, Local};
use colored::Colorize;

pub const HIERARCHY_GUTTER_WIDTH: usize = 10;

/// Short session ID.
pub fn short_id(id: &str) -> &str {
    if is_uuid_like(id) {
        let start = id.char_indices().rev().nth(7).map(|(i, _)| i).unwrap_or(0);
        return &id[start..];
    }

    let end = id.char_indices().nth(8).map(|(i, _)| i).unwrap_or(id.len());
    &id[..end]
}

fn is_uuid_like(id: &str) -> bool {
    let mut parts = id.split('-');
    matches!(
        (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next()
        ),
        (Some(a), Some(b), Some(c), Some(d), Some(e), None)
            if a.len() == 8
                && b.len() == 4
                && c.len() == 4
                && d.len() == 4
                && e.len() == 12
                && [a, b, c, d, e]
                    .iter()
                    .all(|part| part.chars().all(|ch| ch.is_ascii_hexdigit()))
    )
}

pub fn format_result(conv: &Conversation) -> String {
    let source_tag = match conv.source {
        SessionSource::Claude => "[claude]".blue().to_string(),
        SessionSource::Codex => "[codex]".green().to_string(),
    };
    let age = format_relative_time(conv.timestamp);
    let hierarchy = format_hierarchy_marker(conv);
    let project = format_project_label(conv);
    let model = format_model_short(conv.model.as_deref());
    let title = get_display_title(conv);
    let preview = truncate(&title, 60);
    let sid = short_id(&conv.session_id).dimmed();
    format!(
        " {} {:>6}  {:<gutter_width$}{:<20}  ({})  {}  \"{}\"",
        source_tag,
        age,
        hierarchy,
        project,
        model,
        sid,
        preview,
        gutter_width = HIERARCHY_GUTTER_WIDTH
    )
}

pub fn format_project_label(conv: &Conversation) -> String {
    if let Some(subagent_name) = conv.subagent_name.as_deref() {
        return subagent_name.to_string();
    }

    conv.project_name
        .as_deref()
        .unwrap_or("unknown")
        .to_string()
}

pub fn format_hierarchy_marker(conv: &Conversation) -> String {
    if let Some(marker) = conv.hierarchy_marker.as_ref() {
        return marker.clone();
    }

    if conv.hierarchy_depth > 0 {
        let connector = if conv.hierarchy_has_next_sibling {
            "├─"
        } else {
            "└─"
        };
        let mut marker = String::new();
        for _ in 0..conv.hierarchy_depth {
            marker.push_str("│ ");
        }
        marker.push_str(connector);
        return marker;
    }

    if conv.hierarchy_has_children {
        return "┬─".to_string();
    }

    String::new()
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::path::PathBuf;

    fn conversation() -> Conversation {
        let timestamp = Local::now();
        Conversation {
            path: PathBuf::from("session.jsonl"),
            source: SessionSource::Codex,
            session_id: "session-id".to_string(),
            timestamp,
            preview: "Review this".to_string(),
            full_text: String::new(),
            project_name: Some("project".to_string()),
            cwd: None,
            message_count: 1,
            model: Some("gpt-5.5".to_string()),
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: Some("review".to_string()),
            hierarchy_has_children: false,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: 1,
            hierarchy_order: 1,
            hierarchy_sort_timestamp: timestamp,
        }
    }

    fn parent_conversation() -> Conversation {
        let timestamp = Local::now();
        Conversation {
            path: PathBuf::from("session.jsonl"),
            source: SessionSource::Codex,
            session_id: "session-id".to_string(),
            timestamp,
            preview: "Review this".to_string(),
            full_text: String::new(),
            project_name: Some("project".to_string()),
            cwd: None,
            message_count: 1,
            model: Some("gpt-5.5".to_string()),
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: None,
            hierarchy_has_children: true,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: 0,
            hierarchy_order: 0,
            hierarchy_sort_timestamp: timestamp,
        }
    }

    #[test]
    fn format_result_labels_parent_rows_with_children() {
        let rendered = format_result(&parent_conversation());

        assert!(rendered.contains("┬─        project"));
    }

    #[test]
    fn format_result_labels_subagent_rows() {
        let rendered = format_result(&conversation());

        assert!(rendered.contains("│ └─      review"));
    }

    #[test]
    fn short_id_uses_uuid_suffix_to_avoid_ulid_prefix_collisions() {
        assert_eq!(short_id("019e4aec-7d77-7783-9e57-0bb26a8d848a"), "6a8d848a");
        assert_eq!(short_id("019e4aec-930e-7a52-8a49-b659df8fb813"), "df8fb813");
    }
}
