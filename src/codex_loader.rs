use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use rayon::prelude::*;

use crate::codex::{CodexLine, EventMsg, TurnContext};
use crate::codex_items::read_codex_lines;
use crate::codex_parser::process_codex_file;
use crate::error::Result;
use crate::history::Conversation;

#[derive(Default)]
struct CodexFileMetadata {
    task_turn_id: Option<String>,
    turn_models: Vec<(String, String)>,
}

/// Recursively collect all `rollout-*.jsonl` files under a directory.
fn collect_jsonl_files(dir: &PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.clone()];

    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                    files.push(path);
                }
            }
        }
    }
    files
}

fn collect_file_metadata(path: &PathBuf) -> Option<(PathBuf, CodexFileMetadata)> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let lines = read_codex_lines(reader);

    let mut task_turn_id: Option<String> = None;
    let mut model_candidates: Vec<(Option<String>, String)> = Vec::new();

    for line in &lines {
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
                    if evt.event_type == "task_started" && task_turn_id.is_none() {
                        task_turn_id = evt.turn_id;
                    }
                }
            }
            "turn_context" => {
                if let Ok(tc) = serde_json::from_value::<TurnContext>(codex_line.payload) {
                    if let Some(model) = tc.model {
                        model_candidates.push((tc.turn_id, model));
                    }
                }
            }
            _ => {}
        }
    }

    let turn_models = model_candidates
        .into_iter()
        .filter_map(|(turn_id, model)| {
            turn_id
                .or_else(|| task_turn_id.clone())
                .map(|id| (id, model))
        })
        .collect();

    Some((
        path.clone(),
        CodexFileMetadata {
            task_turn_id,
            turn_models,
        },
    ))
}

fn backfill_missing_models(
    conversations: &mut [Conversation],
    metadata_by_path: &HashMap<PathBuf, CodexFileMetadata>,
) {
    let mut models_by_turn_id: HashMap<String, Option<String>> = HashMap::new();

    for metadata in metadata_by_path.values() {
        for (turn_id, model) in &metadata.turn_models {
            models_by_turn_id
                .entry(turn_id.clone())
                .and_modify(|existing| {
                    if existing.as_deref() != Some(model.as_str()) {
                        *existing = None;
                    }
                })
                .or_insert_with(|| Some(model.clone()));
        }
    }

    for conversation in conversations {
        if conversation.model.is_some() {
            continue;
        }

        let Some(metadata) = metadata_by_path.get(&conversation.path) else {
            continue;
        };
        let Some(turn_id) = metadata.task_turn_id.as_ref() else {
            continue;
        };
        let Some(Some(model)) = models_by_turn_id.get(turn_id) else {
            continue;
        };

        conversation.model = Some(model.clone());
    }
}

/// Load all Codex sessions from the default sessions directory.
///
/// Checks CODEX_HOME env var, defaults to ~/.codex.
/// Sessions live under {root}/sessions/ in YYYY/MM/DD/ subdirectories.
pub fn load_codex_sessions() -> Result<Vec<Conversation>> {
    let root = match std::env::var("CODEX_HOME") {
        Ok(val) => PathBuf::from(val),
        Err(_) => {
            let home = home::home_dir().unwrap_or_else(|| PathBuf::from("~"));
            home.join(".codex")
        }
    };

    let sessions_dir = root.join("sessions");
    if !sessions_dir.is_dir() {
        return Ok(Vec::new());
    }

    let files = collect_jsonl_files(&sessions_dir);
    let metadata_by_path: HashMap<PathBuf, CodexFileMetadata> =
        files.par_iter().filter_map(collect_file_metadata).collect();

    let mut conversations: Vec<Conversation> = files
        .into_par_iter()
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            process_codex_file(path, modified).ok().flatten()
        })
        .collect();

    backfill_missing_models(&mut conversations, &metadata_by_path);

    conversations.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    Ok(conversations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_session(root: &std::path::Path, name: &str, lines: &[&str]) {
        let dir = root.join("sessions/2026/05/20");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    #[test]
    fn backfills_exec_wrapper_model_from_matching_turn_context() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-27-parent.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/project","originator":"codex_exec","source":"exec"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:30Z","type":"event_msg","payload":{"type":"agent_message","message":"Looks good"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-28-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"session_meta","payload":{"id":"child-session","cwd":"/tmp/project","originator":"codex_exec","source":{"subagent":"review"}}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-5.5"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:30Z","type":"event_msg","payload":{"type":"agent_message","message":"Looks good"}}"#,
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let parent = sessions
            .iter()
            .find(|session| session.session_id == "parent-session")
            .unwrap();
        assert_eq!(parent.model.as_deref(), Some("gpt-5.5"));
    }
}
