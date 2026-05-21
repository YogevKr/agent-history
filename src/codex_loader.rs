use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

use chrono::{DateTime, Local};
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
    session_id: Option<String>,
    cwd: Option<String>,
    task_turn_id: Option<String>,
    turn_models: Vec<(TaskModelKey, String)>,
    subagent_name: Option<String>,
    parent_session_id: Option<String>,
    subagent_depth: Option<usize>,
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
    let mut session_id: Option<String> = None;
    let mut subagent_name: Option<String> = None;
    let mut parent_session_id: Option<String> = None;
    let mut subagent_depth: Option<usize> = None;
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
                    if session_id.is_none() {
                        session_id = Some(meta.id);
                    }
                    if cwd.is_none() {
                        cwd = meta.cwd.clone();
                    }
                    if let Some(source) = meta.source {
                        if source.as_str() == Some("exec") {
                            is_exec_wrapper = true;
                        }
                        if subagent_name.is_none()
                            || parent_session_id.is_none()
                            || subagent_depth.is_none()
                        {
                            let metadata = extract_subagent_metadata(
                                &source,
                                meta.agent_nickname.as_deref(),
                                meta.agent_role.as_deref(),
                                meta.agent_path.as_deref(),
                            );
                            if subagent_name.is_none() {
                                subagent_name =
                                    metadata.as_ref().map(|metadata| metadata.name.clone());
                            }
                            if parent_session_id.is_none() {
                                parent_session_id = metadata
                                    .as_ref()
                                    .and_then(|metadata| metadata.parent_session_id.clone());
                            }
                            if subagent_depth.is_none() {
                                subagent_depth =
                                    metadata.as_ref().and_then(|metadata| metadata.depth);
                            }
                        }
                    } else if meta.thread_source.as_deref() == Some("subagent")
                        && subagent_name.is_none()
                    {
                        subagent_name = meta
                            .agent_nickname
                            .or(meta.agent_role)
                            .or_else(|| subagent_name_from_path(meta.agent_path.as_deref()));
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
            session_id,
            cwd,
            task_turn_id,
            turn_models,
            subagent_name,
            parent_session_id,
            subagent_depth,
            is_exec_wrapper,
        },
    ))
}

struct SubagentMetadata {
    name: String,
    parent_session_id: Option<String>,
    depth: Option<usize>,
}

fn extract_subagent_metadata(
    source: &serde_json::Value,
    nickname: Option<&str>,
    role: Option<&str>,
    path: Option<&str>,
) -> Option<SubagentMetadata> {
    let subagent = source.get("subagent")?;

    if let Some(name) = subagent.as_str() {
        return Some(SubagentMetadata {
            name: name.to_string(),
            parent_session_id: None,
            depth: Some(1),
        });
    }

    let thread_spawn = subagent.get("thread_spawn");
    let name = nickname
        .or_else(|| {
            thread_spawn?
                .get("agent_nickname")
                .and_then(|value| value.as_str())
        })
        .or(role)
        .map(String::from)
        .or_else(|| subagent_name_from_path(path))?;

    Some(SubagentMetadata {
        name,
        parent_session_id: thread_spawn
            .and_then(|thread_spawn| thread_spawn.get("parent_thread_id"))
            .and_then(|value| value.as_str())
            .map(String::from),
        depth: thread_spawn
            .and_then(|thread_spawn| thread_spawn.get("depth"))
            .and_then(|value| value.as_u64())
            .and_then(|value| usize::try_from(value).ok()),
    })
}

fn subagent_name_from_path(path: Option<&str>) -> Option<String> {
    path?
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(String::from)
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
    let mut parent_by_session_id: HashMap<String, String> = HashMap::new();
    let mut children_by_session_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut child_counts_by_session_id: HashMap<String, usize> = HashMap::new();
    let mut thread_group_timestamps: HashMap<String, DateTime<Local>> = HashMap::new();
    let mut timestamp_by_session_id: HashMap<String, DateTime<Local>> = HashMap::new();
    let mut thread_order_by_session_id: HashMap<String, usize> = HashMap::new();
    let mut has_next_sibling_by_session_id: HashMap<String, bool> = HashMap::new();
    let mut hierarchy_marker_by_session_id: HashMap<String, String> = HashMap::new();

    for metadata in metadata_by_path.values() {
        let Some(session_id) = metadata.session_id.as_ref() else {
            continue;
        };
        let Some(parent_session_id) = metadata.parent_session_id.as_ref() else {
            continue;
        };

        parent_by_session_id.insert(session_id.clone(), parent_session_id.clone());
        children_by_session_id
            .entry(parent_session_id.clone())
            .or_default()
            .push(session_id.clone());
        *child_counts_by_session_id
            .entry(parent_session_id.clone())
            .or_insert(0) += 1;
    }

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

    for conversation in conversations.iter() {
        let Some(metadata) = metadata_by_path.get(&conversation.path) else {
            continue;
        };
        let session_id = metadata
            .session_id
            .as_ref()
            .unwrap_or(&conversation.session_id);
        timestamp_by_session_id.insert(session_id.clone(), conversation.timestamp);

        if !parent_by_session_id.contains_key(session_id)
            && !child_counts_by_session_id.contains_key(session_id)
        {
            continue;
        }

        let root_session_id = root_session_id(session_id, &parent_by_session_id);
        thread_group_timestamps
            .entry(root_session_id)
            .and_modify(|timestamp| {
                if conversation.timestamp > *timestamp {
                    *timestamp = conversation.timestamp;
                }
            })
            .or_insert(conversation.timestamp);
    }

    let mut thread_roots = HashSet::new();
    for session_id in parent_by_session_id
        .keys()
        .chain(child_counts_by_session_id.keys())
    {
        thread_roots.insert(root_session_id(session_id, &parent_by_session_id));
    }
    let mut subtree_latest_cache = HashMap::new();
    for root_session_id in thread_roots {
        let mut next_order = 0;
        assign_thread_order(
            &root_session_id,
            &children_by_session_id,
            &timestamp_by_session_id,
            &mut subtree_latest_cache,
            &mut thread_order_by_session_id,
            &mut has_next_sibling_by_session_id,
            &mut hierarchy_marker_by_session_id,
            &[],
            false,
            &mut next_order,
        );
    }

    for conversation in conversations {
        let Some(metadata) = metadata_by_path.get(&conversation.path) else {
            continue;
        };
        let session_id = metadata
            .session_id
            .as_ref()
            .unwrap_or(&conversation.session_id);
        let participates_in_thread_group = parent_by_session_id.contains_key(session_id)
            || child_counts_by_session_id.contains_key(session_id);

        if participates_in_thread_group {
            let root_session_id = root_session_id(session_id, &parent_by_session_id);
            if let Some(timestamp) = thread_group_timestamps.get(&root_session_id) {
                conversation.hierarchy_sort_timestamp = *timestamp;
            }
            conversation.hierarchy_has_children = child_counts_by_session_id
                .get(session_id)
                .copied()
                .unwrap_or(0)
                > 0;
            conversation.hierarchy_has_next_sibling = has_next_sibling_by_session_id
                .get(session_id)
                .copied()
                .unwrap_or(false);
            conversation.hierarchy_marker = hierarchy_marker_by_session_id.get(session_id).cloned();
            if let Some(subagent_name) = metadata.subagent_name.as_ref() {
                conversation.subagent_name = Some(subagent_name.clone());
                conversation.hierarchy_depth = metadata.subagent_depth.unwrap_or(1);
            } else {
                conversation.hierarchy_depth = 0;
            }
            conversation.hierarchy_order = thread_order_by_session_id
                .get(session_id)
                .copied()
                .unwrap_or(0);
            continue;
        }

        let Some(key) = metadata.task_key() else {
            continue;
        };

        if let Some(timestamp) = group_timestamps.get(&key) {
            conversation.hierarchy_sort_timestamp = *timestamp;
        }
        conversation.hierarchy_has_children = subagent_counts.get(&key).copied().unwrap_or(0) > 0;
        conversation.hierarchy_has_next_sibling = false;
        conversation.hierarchy_marker = conversation
            .hierarchy_has_children
            .then(|| "┬─".to_string());
        if let Some(subagent_name) = metadata.subagent_name.as_ref() {
            conversation.subagent_name = Some(subagent_name.clone());
            conversation.hierarchy_has_children = false;
            conversation.hierarchy_has_next_sibling = false;
            conversation.hierarchy_marker = Some("│ └─".to_string());
            conversation.hierarchy_depth = 1;
            conversation.hierarchy_order = 1;
        } else if metadata.is_exec_wrapper {
            conversation.hierarchy_has_next_sibling = false;
            conversation.hierarchy_marker = conversation
                .hierarchy_has_children
                .then(|| "┬─".to_string());
            conversation.hierarchy_depth = 0;
            conversation.hierarchy_order = 0;
        }
    }
}

fn root_session_id(session_id: &str, parent_by_session_id: &HashMap<String, String>) -> String {
    let mut current = session_id;
    let mut seen = HashSet::new();

    while let Some(parent) = parent_by_session_id.get(current) {
        if !seen.insert(current.to_string()) {
            break;
        }
        current = parent;
    }

    current.to_string()
}

fn assign_thread_order(
    session_id: &str,
    children_by_session_id: &HashMap<String, Vec<String>>,
    timestamp_by_session_id: &HashMap<String, DateTime<Local>>,
    subtree_latest_cache: &mut HashMap<String, Option<DateTime<Local>>>,
    thread_order_by_session_id: &mut HashMap<String, usize>,
    has_next_sibling_by_session_id: &mut HashMap<String, bool>,
    hierarchy_marker_by_session_id: &mut HashMap<String, String>,
    ancestor_continuations: &[bool],
    has_next_sibling: bool,
    next_order: &mut usize,
) {
    let mut children = children_by_session_id
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    children.sort_by(|a, b| {
        subtree_latest(
            b,
            children_by_session_id,
            timestamp_by_session_id,
            subtree_latest_cache,
        )
        .cmp(&subtree_latest(
            a,
            children_by_session_id,
            timestamp_by_session_id,
            subtree_latest_cache,
        ))
        .then_with(|| a.cmp(b))
    });

    if timestamp_by_session_id.contains_key(session_id) {
        thread_order_by_session_id.insert(session_id.to_string(), *next_order);
        hierarchy_marker_by_session_id.insert(
            session_id.to_string(),
            format_thread_marker(
                ancestor_continuations,
                has_next_sibling,
                !children.is_empty(),
            ),
        );
        *next_order += 1;
    }

    let child_count = children.len();
    for (index, child) in children.into_iter().enumerate() {
        let child_has_next_sibling = index + 1 < child_count;
        let mut child_ancestor_continuations = ancestor_continuations.to_vec();
        child_ancestor_continuations.push(if ancestor_continuations.is_empty() {
            true
        } else {
            has_next_sibling
        });
        has_next_sibling_by_session_id.insert(child.clone(), child_has_next_sibling);
        assign_thread_order(
            &child,
            children_by_session_id,
            timestamp_by_session_id,
            subtree_latest_cache,
            thread_order_by_session_id,
            has_next_sibling_by_session_id,
            hierarchy_marker_by_session_id,
            &child_ancestor_continuations,
            child_has_next_sibling,
            next_order,
        );
    }
}

fn format_thread_marker(
    ancestor_continuations: &[bool],
    has_next_sibling: bool,
    has_children: bool,
) -> String {
    if ancestor_continuations.is_empty() {
        return if has_children {
            "┬─".to_string()
        } else {
            String::new()
        };
    }

    let mut marker = String::new();
    for has_continuation in ancestor_continuations {
        if *has_continuation {
            marker.push_str("│ ");
        } else {
            marker.push_str("  ");
        }
    }
    marker.push_str(if has_next_sibling { "├─" } else { "└─" });
    marker
}

fn subtree_latest(
    session_id: &str,
    children_by_session_id: &HashMap<String, Vec<String>>,
    timestamp_by_session_id: &HashMap<String, DateTime<Local>>,
    cache: &mut HashMap<String, Option<DateTime<Local>>>,
) -> Option<DateTime<Local>> {
    if let Some(timestamp) = cache.get(session_id) {
        return *timestamp;
    }

    let mut latest = timestamp_by_session_id.get(session_id).copied();
    if let Some(children) = children_by_session_id.get(session_id) {
        for child in children {
            if let Some(child_latest) = subtree_latest(
                child,
                children_by_session_id,
                timestamp_by_session_id,
                cache,
            ) {
                latest = Some(latest.map_or(child_latest, |timestamp| timestamp.max(child_latest)));
            }
        }
    }

    cache.insert(session_id.to_string(), latest);
    latest
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
    use crate::display::format_hierarchy_marker;
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

    #[test]
    fn detects_thread_spawn_subagent_metadata() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-21T07-24-33-019e4aec-7d77-7783-9e57-0bb26a8d848a.jsonl",
            &[
                r#"{"timestamp":"2026-05-21T14:24:33Z","type":"session_meta","payload":{"id":"019e4aec-7d77-7783-9e57-0bb26a8d848a","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_path":"/root/review_compaction_fix","agent_nickname":"Hypatia","agent_role":"explorer"}}},"thread_source":"subagent","agent_nickname":"Hypatia","agent_role":"explorer"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:34Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:35Z","type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-5.5"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:36Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let child = sessions
            .iter()
            .find(|session| session.session_id == "019e4aec-7d77-7783-9e57-0bb26a8d848a")
            .unwrap();
        assert_eq!(child.subagent_name.as_deref(), Some("Hypatia"));
        assert_eq!(child.hierarchy_depth, 1);
    }

    #[test]
    fn groups_thread_spawn_subagents_after_parent_session() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T07-00-00-parent-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/project","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:01Z","type":"turn_context","payload":{"turn_id":"parent-turn","model":"gpt-5.5"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-21T07-24-33-child-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-21T14:24:33Z","type":"session_meta","payload":{"id":"child-session","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Hypatia","agent_role":"explorer"}}},"thread_source":"subagent","agent_nickname":"Hypatia","agent_role":"explorer"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:34Z","type":"event_msg","payload":{"type":"task_started","turn_id":"child-turn"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:35Z","type":"turn_context","payload":{"turn_id":"child-turn","model":"gpt-5.5"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:36Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
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

        let parent = &sessions[parent_position];
        assert!(parent.hierarchy_has_children);
        assert_eq!(
            parent.hierarchy_sort_timestamp,
            sessions[child_position].hierarchy_sort_timestamp
        );
    }

    #[test]
    fn orders_thread_spawn_subtrees_by_latest_activity() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T07-00-00-parent-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/project","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-21T07-24-33-active-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-21T14:24:33Z","type":"session_meta","payload":{"id":"active-child","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Hypatia"}}},"thread_source":"subagent","agent_nickname":"Hypatia"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:34Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-21T07-25-33-grandchild.jsonl",
            &[
                r#"{"timestamp":"2026-05-21T14:25:33Z","type":"session_meta","payload":{"id":"grandchild","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"active-child","depth":2,"agent_nickname":"Hume"}}},"thread_source":"subagent","agent_nickname":"Hume"}}"#,
                r#"{"timestamp":"2026-05-21T14:25:34Z","type":"event_msg","payload":{"type":"user_message","message":"Nested review"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T08-00-00-stale-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T15:00:00Z","type":"session_meta","payload":{"id":"stale-child","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Stale"}}},"thread_source":"subagent","agent_nickname":"Stale"}}"#,
                r#"{"timestamp":"2026-05-20T15:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Old review"}}"#,
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let ids: Vec<&str> = sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "parent-session",
                "active-child",
                "grandchild",
                "stale-child"
            ]
        );
    }

    #[test]
    fn renders_thread_spawn_sibling_connectors() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T07-00-00-parent-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/project","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-03-00-child-a.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:03:00Z","type":"session_meta","payload":{"id":"child-a","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Alpha"}}},"thread_source":"subagent","agent_nickname":"Alpha"}}"#,
                r#"{"timestamp":"2026-05-20T14:03:01Z","type":"event_msg","payload":{"type":"user_message","message":"Alpha task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-02-00-child-b.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:02:00Z","type":"session_meta","payload":{"id":"child-b","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Beta"}}},"thread_source":"subagent","agent_nickname":"Beta"}}"#,
                r#"{"timestamp":"2026-05-20T14:02:01Z","type":"event_msg","payload":{"type":"user_message","message":"Beta task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-01-00-child-c.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:01:00Z","type":"session_meta","payload":{"id":"child-c","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Gamma"}}},"thread_source":"subagent","agent_nickname":"Gamma"}}"#,
                r#"{"timestamp":"2026-05-20T14:01:01Z","type":"event_msg","payload":{"type":"user_message","message":"Gamma task"}}"#,
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let rendered: Vec<(&str, String)> = sessions
            .iter()
            .map(|session| {
                (
                    session.session_id.as_str(),
                    format_hierarchy_marker(session),
                )
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                ("parent-session", "┬─".to_string()),
                ("child-a", "│ ├─".to_string()),
                ("child-b", "│ ├─".to_string()),
                ("child-c", "│ └─".to_string()),
            ]
        );
    }

    #[test]
    fn renders_thread_spawn_nested_parent_chain_markers() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T07-00-00-parent-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/project","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-03-00-child-a.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:03:00Z","type":"session_meta","payload":{"id":"child-a","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Alpha"}}},"thread_source":"subagent","agent_nickname":"Alpha"}}"#,
                r#"{"timestamp":"2026-05-20T14:03:01Z","type":"event_msg","payload":{"type":"user_message","message":"Alpha task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-04-00-grandchild.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:04:00Z","type":"session_meta","payload":{"id":"grandchild","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"child-a","depth":2,"agent_nickname":"Nested"}}},"thread_source":"subagent","agent_nickname":"Nested"}}"#,
                r#"{"timestamp":"2026-05-20T14:04:01Z","type":"event_msg","payload":{"type":"user_message","message":"Nested task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-05-00-great-grandchild.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:05:00Z","type":"session_meta","payload":{"id":"great-grandchild","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"grandchild","depth":3,"agent_nickname":"Deep"}}},"thread_source":"subagent","agent_nickname":"Deep"}}"#,
                r#"{"timestamp":"2026-05-20T14:05:01Z","type":"event_msg","payload":{"type":"user_message","message":"Deep task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-02-00-child-b.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:02:00Z","type":"session_meta","payload":{"id":"child-b","cwd":"/tmp/project","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Beta"}}},"thread_source":"subagent","agent_nickname":"Beta"}}"#,
                r#"{"timestamp":"2026-05-20T14:02:01Z","type":"event_msg","payload":{"type":"user_message","message":"Beta task"}}"#,
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let rendered: Vec<(&str, String)> = sessions
            .iter()
            .map(|session| {
                (
                    session.session_id.as_str(),
                    format_hierarchy_marker(session),
                )
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                ("parent-session", "┬─".to_string()),
                ("child-a", "│ ├─".to_string()),
                ("grandchild", "│ │ └─".to_string()),
                ("great-grandchild", "│ │   └─".to_string()),
                ("child-b", "│ └─".to_string()),
            ]
        );
    }
}
