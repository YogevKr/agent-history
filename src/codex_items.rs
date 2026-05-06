use crate::codex::{CodexLine, EventMsg, ResponseItem};
use std::collections::HashMap;
use std::io::BufRead;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CodexRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexItem {
    Message { role: CodexRole, text: String },
    ToolCall { name: String },
    ToolOutput { output: String },
}

pub fn read_codex_lines<R: BufRead>(reader: R) -> Vec<String> {
    let mut lines = Vec::new();
    for line in reader.lines() {
        if let Ok(line) = line {
            lines.push(line);
        }
    }
    lines
}

pub fn codex_items(lines: &[String]) -> Vec<CodexItem> {
    let mut remaining_event_messages = event_message_counts(lines);
    let mut items = Vec::new();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }

        let codex_line: CodexLine = match serde_json::from_str(line) {
            Ok(line) => line,
            Err(_) => continue,
        };

        match codex_line.line_type.as_str() {
            "event_msg" => {
                if let Ok(evt) = serde_json::from_value::<EventMsg>(codex_line.payload) {
                    if let Some((role, text)) = event_message(&evt) {
                        items.push(CodexItem::Message { role, text });
                    }
                }
            }
            "response_item" => {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(codex_line.payload) {
                    match item.item_type.as_str() {
                        "message" => {
                            if let Some((role, text)) = response_message(&item) {
                                let key = (role, text.clone());
                                if let Some(count) = remaining_event_messages.get_mut(&key) {
                                    if *count > 0 {
                                        *count -= 1;
                                        continue;
                                    }
                                }
                                items.push(CodexItem::Message { role, text });
                            }
                        }
                        "function_call" => {
                            if let Some(name) = item.name {
                                items.push(CodexItem::ToolCall { name });
                            }
                        }
                        "function_call_output" => {
                            if let Some(output) = item.output {
                                if !output.is_empty() {
                                    items.push(CodexItem::ToolOutput { output });
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

    items
}

fn event_message_counts(lines: &[String]) -> HashMap<(CodexRole, String), usize> {
    let mut counts = HashMap::new();

    for line in lines {
        let codex_line: CodexLine = match serde_json::from_str(line) {
            Ok(line) => line,
            Err(_) => continue,
        };

        if codex_line.line_type != "event_msg" {
            continue;
        }

        let evt: EventMsg = match serde_json::from_value(codex_line.payload) {
            Ok(evt) => evt,
            Err(_) => continue,
        };

        if let Some((role, text)) = event_message(&evt) {
            *counts.entry((role, text)).or_insert(0) += 1;
        }
    }

    counts
}

fn event_message(evt: &EventMsg) -> Option<(CodexRole, String)> {
    let role = match evt.event_type.as_str() {
        "user_message" => CodexRole::User,
        "agent_message" => CodexRole::Assistant,
        _ => return None,
    };
    let text = evt.message.as_ref()?.to_string();
    if text.is_empty() {
        return None;
    }

    let text = match role {
        CodexRole::User => clean_user_message(&text)?,
        CodexRole::Assistant => text,
    };

    Some((role, text))
}

fn response_message(item: &ResponseItem) -> Option<(CodexRole, String)> {
    let role = match item.role.as_deref()? {
        "user" => CodexRole::User,
        "assistant" => CodexRole::Assistant,
        _ => return None,
    };

    let parts = item
        .content
        .as_ref()?
        .iter()
        .filter_map(|part| part.text.as_deref())
        .filter(|text| !text.is_empty());

    let text = match role {
        CodexRole::User => parts
            .filter_map(clean_user_message)
            .collect::<Vec<_>>()
            .join("\n\n"),
        CodexRole::Assistant => parts.collect::<Vec<_>>().join("\n\n"),
    };

    if text.is_empty() {
        return None;
    }

    Some((role, text))
}

fn clean_user_message(text: &str) -> Option<String> {
    let mut rest = text.trim();

    loop {
        let before = rest;

        if rest.starts_with("# AGENTS.md instructions for ") {
            if let Some(stripped) = strip_through_closing_tag(rest, "INSTRUCTIONS") {
                rest = stripped.trim_start();
                continue;
            }
            return None;
        }

        if let Some(stripped) = strip_leading_tag(rest, "environment_context") {
            rest = stripped.trim_start();
            continue;
        }

        if let Some(stripped) = strip_leading_image(rest) {
            rest = stripped.trim_start();
            continue;
        }

        if is_standalone_image_marker(rest) {
            return None;
        }

        if rest == before {
            break;
        }
    }

    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn strip_through_closing_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let close = format!("</{}>", tag);
    let end = text.find(&close)? + close.len();
    Some(&text[end..])
}

fn strip_leading_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    if !text.starts_with(&open) {
        return None;
    }
    strip_through_closing_tag(text, tag)
}

fn strip_leading_image(text: &str) -> Option<&str> {
    if !(text.starts_with("<image ") || text.starts_with("<image>")) {
        return None;
    }

    strip_through_closing_tag(text, "image")
}

fn is_standalone_image_marker(text: &str) -> bool {
    text == "</image>"
        || ((text.starts_with("<image ") || text == "<image>") && text.ends_with('>'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn deduplicates_response_messages_also_present_as_events() {
        let lines = strings(&[
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:02Z","type":"event_msg","payload":{"type":"user_message","message":"Hello"}}"#,
            r#"{"timestamp":"2026-03-19T14:29:03Z","type":"event_msg","payload":{"type":"agent_message","message":"Hi"}}"#,
        ]);

        let items = codex_items(&lines);

        assert_eq!(
            items,
            vec![
                CodexItem::Message {
                    role: CodexRole::User,
                    text: "Hello".to_string(),
                },
                CodexItem::Message {
                    role: CodexRole::Assistant,
                    text: "Hi".to_string(),
                },
            ]
        );
    }

    #[test]
    fn keeps_response_messages_missing_from_events() {
        let lines = strings(&[
            r#"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Response only"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]}}"#,
            r#"{"timestamp":"2026-03-19T14:29:02Z","type":"event_msg","payload":{"type":"user_message","message":"Hello"}}"#,
        ]);

        let items = codex_items(&lines);

        assert_eq!(
            items,
            vec![
                CodexItem::Message {
                    role: CodexRole::User,
                    text: "Response only".to_string(),
                },
                CodexItem::Message {
                    role: CodexRole::User,
                    text: "Hello".to_string(),
                },
            ]
        );
    }

    #[test]
    fn skips_codex_synthetic_user_context() {
        let lines = strings(&[
            r##"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\n# AGENTS.md\nrules\n</INSTRUCTIONS>"},{"type":"input_text","text":"<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"}]}}"##,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"real prompt"}]}}"#,
        ]);

        let items = codex_items(&lines);

        assert_eq!(
            items,
            vec![CodexItem::Message {
                role: CodexRole::User,
                text: "real prompt".to_string(),
            }]
        );
    }

    #[test]
    fn strips_synthetic_prefixes_before_real_text() {
        let lines = strings(&[
            r##"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nrules\n</INSTRUCTIONS>\n<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>\nreal prompt"}]}}"##,
        ]);

        let items = codex_items(&lines);

        assert_eq!(
            items,
            vec![CodexItem::Message {
                role: CodexRole::User,
                text: "real prompt".to_string(),
            }]
        );
    }

    #[test]
    fn deduplicates_response_messages_with_image_context() {
        let lines = strings(&[
            r##"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<image name=[Image #1]>\n</image>"},{"type":"input_text","text":"check this [Image #1]"}]}}"##,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"event_msg","payload":{"type":"user_message","message":"check this [Image #1]"}}"#,
        ]);

        let items = codex_items(&lines);

        assert_eq!(
            items,
            vec![CodexItem::Message {
                role: CodexRole::User,
                text: "check this [Image #1]".to_string(),
            }]
        );
    }

    #[test]
    fn deduplicates_response_messages_with_split_image_context() {
        let lines = strings(&[
            r##"{"timestamp":"2026-03-19T14:29:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<image name=[Image #1]>"},{"type":"input_image","image_url":"data:image/png;base64,abc"},{"type":"input_text","text":"</image>"},{"type":"input_text","text":"check this [Image #1]"}]}}"##,
            r#"{"timestamp":"2026-03-19T14:29:01Z","type":"event_msg","payload":{"type":"user_message","message":"check this [Image #1]"}}"#,
        ]);

        let items = codex_items(&lines);

        assert_eq!(
            items,
            vec![CodexItem::Message {
                role: CodexRole::User,
                text: "check this [Image #1]".to_string(),
            }]
        );
    }
}
