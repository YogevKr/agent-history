use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use rayon::prelude::*;

use crate::codex::{CodexLine, EventMsg, SessionMeta, TurnContext};
use crate::codex_items::read_codex_lines;
use crate::codex_parser::process_codex_file;
use crate::error::Result;
use crate::history::{compare_conversations, Conversation};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TaskModelKey {
    cwd: String,
    turn_id: String,
}

#[derive(Default)]
struct CodexFileMetadata {
    cwd: Option<String>,
    task_turn_id: Option<String>,
    turn_models: Vec<(TaskModelKey, String)>,
    subagent_name: Option<String>,
    is_exec_wrapper: bool,
}

impl CodexFileMetadata {
    fn task_key(&self) -> Option<TaskModelKey> {
        Some(TaskModelKey {
            cwd: self.cwd.clone()?,
            turn_id: self.task_turn_id.clone()?,
        })
    }
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
    let mut cwd: Option<String> = None;
    let mut model_candidates: Vec<(Option<String>, Option<String>, String)> = Vec::new();
    let mut subagent_name: Option<String> = None;
    let mut is_exec_wrapper = false;

    for line in &lines {
        if line.trim().is_empty() {
            continue;
        }

        let codex_line: CodexLine = match serde_json::from_str(line) {
            Ok(line) => line,
            Err(_) => continue,
        };

        match codex_line.line_type.as_str() {
            "session_meta" => {
                if let Ok(meta) = serde_json::from_value::<SessionMeta>(codex_line.payload) {
                    if cwd.is_none() {
                        cwd = meta.cwd.clone();
                    }
                    if let Some(source) = meta.source {
                        if source.as_str() == Some("exec") {
                            is_exec_wrapper = true;
                        }
                        if subagent_name.is_none() {
                            subagent_name = source
                                .get("subagent")
                                .and_then(|value| value.as_str())
                                .map(String::from);
                        }
                    }
                }
            }
            "event_msg" => {
                if let Ok(evt) = serde_json::from_value::<EventMsg>(codex_line.payload) {
                    if evt.event_type == "task_started" && task_turn_id.is_none() {
                        task_turn_id = evt.turn_id;
                    }
                }
            }
            "turn_context" => {
                if let Ok(tc) = serde_json::from_value::<TurnContext>(codex_line.payload) {
                    if cwd.is_none() {
                        cwd = tc.cwd.clone();
                    }
                    if let Some(model) = tc.model {
                        model_candidates.push((tc.turn_id, tc.cwd, model));
                    }
                }
            }
            _ => {}
        }
    }

    let turn_models = model_candidates
        .into_iter()
        .filter_map(|(turn_id, candidate_cwd, model)| {
            let turn_id = turn_id.or_else(|| task_turn_id.clone())?;
            let cwd = candidate_cwd.or_else(|| cwd.clone())?;
            Some((TaskModelKey { cwd, turn_id }, model))
        })
        .collect();

    Some((
        path.clone(),
        CodexFileMetadata {
            cwd,
            task_turn_id,
            turn_models,
            subagent_name,
            is_exec_wrapper,
        },
    ))
}

fn backfill_missing_models(
    conversations: &mut [Conversation],
    metadata_by_path: &HashMap<PathBuf, CodexFileMetadata>,
) {
    let mut models_by_task: HashMap<TaskModelKey, Option<String>> = HashMap::new();

    for metadata in metadata_by_path.values() {
        for (key, model) in &metadata.turn_models {
            models_by_task
                .entry(key.clone())
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
        let Some(cwd) = metadata.cwd.as_ref() else {
            continue;
        };
        let Some(turn_id) = metadata.task_turn_id.as_ref() else {
            continue;
        };
        let key = TaskModelKey {
            cwd: cwd.clone(),
            turn_id: turn_id.clone(),
        };
        let Some(Some(model)) = models_by_task.get(&key) else {
            continue;
        };

        conversation.model = Some(model.clone());
    }
}

fn annotate_hierarchy(
    conversations: &mut [Conversation],
    metadata_by_path: &HashMap<PathBuf, CodexFileMetadata>,
) {
    let mut group_timestamps = HashMap::new();
    let mut subagent_counts: HashMap<TaskModelKey, usize> = HashMap::new();

    for conversation in conversations.iter() {
        let Some(metadata) = metadata_by_path.get(&conversation.path) else {
            continue;
        };
        let Some(key) = metadata.task_key() else {
            continue;
        };

        group_timestamps
            .entry(key.clone())
            .and_modify(|timestamp| {
                if conversation.timestamp > *timestamp {
                    *timestamp = conversation.timestamp;
                }
            })
            .or_insert(conversation.timestamp);

        if metadata.subagent_name.is_some() {
            *subagent_counts.entry(key).or_insert(0) += 1;
        }
    }

    for conversation in conversations {
        let Some(metadata) = metadata_by_path.get(&conversation.path) else {
            continue;
        };
        let Some(key) = metadata.task_key() else {
            continue;
        };

        if let Some(timestamp) = group_timestamps.get(&key) {
            conversation.hierarchy_sort_timestamp = *timestamp;
        }
        conversation.hierarchy_has_children = subagent_counts.get(&key).copied().unwrap_or(0) > 0;
        if let Some(subagent_name) = metadata.subagent_name.as_ref() {
            conversation.subagent_name = Some(subagent_name.clone());
            conversation.hierarchy_has_children = false;
            conversation.hierarchy_depth = 1;
            conversation.hierarchy_order = 1;
        } else if metadata.is_exec_wrapper {
            conversation.hierarchy_depth = 0;
            conversation.hierarchy_order = 0;
        }
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
    annotate_hierarchy(&mut conversations, &metadata_by_path);

    conversations.sort_by(compare_conversations);

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
        assert!(parent.hierarchy_has_children);

        let child = sessions
            .iter()
            .find(|session| session.session_id == "child-session")
            .unwrap();
        assert_eq!(child.subagent_name.as_deref(), Some("review"));
        assert!(!child.hierarchy_has_children);
    }

    #[test]
    fn does_not_backfill_model_from_matching_turn_id_in_different_cwd() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-27-parent.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/project-a","originator":"codex_exec","source":"exec"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-28-unrelated.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"session_meta","payload":{"id":"unrelated-session","cwd":"/tmp/project-b","originator":"codex_exec","source":{"subagent":"review"}}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-5.5"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review that"}}"#,
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
        assert_eq!(parent.model, None);
    }

    #[test]
    fn orders_exec_wrapper_before_matching_subagent() {
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
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let parent_position = sessions
            .iter()
            .position(|session| session.session_id == "parent-session")
            .unwrap();
        let child_position = sessions
            .iter()
            .position(|session| session.session_id == "child-session")
            .unwrap();
        assert!(parent_position < child_position);
    }
}
