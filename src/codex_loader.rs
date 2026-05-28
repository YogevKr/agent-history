use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local, TimeZone};
use rayon::prelude::*;

use crate::codex::{CodexLine, EventMsg, ResponseItem, SessionMeta, TurnContext};
use crate::codex_items::clean_user_message;
use crate::codex_parser::{
    process_codex_file, session_id_from_filename, subagent_dispatch_content,
};
use crate::error::Result;
use crate::history::{compare_conversations, Conversation, SessionSource};

#[derive(Clone, Copy, Debug, Default)]
pub struct CodexLoadOptions {
    pub include_full_text: bool,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TaskModelKey {
    cwd: String,
    turn_id: String,
}

#[derive(Clone, Default)]
struct CodexFileMetadata {
    session_id: Option<String>,
    cwd: Option<String>,
    task_turn_id: Option<String>,
    task_turn_ids: Vec<String>,
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

    fn task_keys(&self) -> Vec<TaskModelKey> {
        let Some(cwd) = self.cwd.clone() else {
            return Vec::new();
        };
        let turn_ids = if self.task_turn_ids.is_empty() {
            self.task_turn_id.iter().cloned().collect()
        } else {
            self.task_turn_ids.clone()
        };
        turn_ids
            .into_iter()
            .map(|turn_id| TaskModelKey {
                cwd: cwd.clone(),
                turn_id,
            })
            .collect()
    }
}

/// Recursively collect all `rollout-*.jsonl` files under a directory.
fn collect_jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

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

fn collect_file_index(
    path: &PathBuf,
) -> Option<(PathBuf, CodexFileMetadata, Option<Conversation>)> {
    let modified = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut task_turn_id: Option<String> = None;
    let mut task_turn_ids: Vec<String> = Vec::new();
    let mut cwd: Option<String> = None;
    let mut model_candidates: Vec<(Option<String>, Option<String>, String)> = Vec::new();
    let mut first_model: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut subagent_name: Option<String> = None;
    let mut parent_session_id: Option<String> = None;
    let mut subagent_depth: Option<usize> = None;
    let mut is_exec_wrapper = false;
    let mut preview = String::new();
    let mut message_count: usize = 0;
    let mut total_tokens: u64 = 0;
    let mut first_timestamp: Option<String> = None;
    let mut session_timestamp: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }

        let codex_line: CodexLine = match serde_json::from_str(&line) {
            Ok(line) => line,
            Err(_) => continue,
        };

        if first_timestamp.is_none() {
            first_timestamp = Some(codex_line.timestamp.clone());
        }

        match codex_line.line_type.as_str() {
            "session_meta" => {
                if let Ok(meta) = serde_json::from_value::<SessionMeta>(codex_line.payload) {
                    if session_id.is_none() {
                        session_id = Some(meta.id);
                    }
                    session_timestamp = Some(codex_line.timestamp);
                    if cwd.is_none() {
                        cwd = meta.cwd.clone();
                    }
                    if git_branch.is_none() {
                        if let Some(git) = meta.git {
                            git_branch = git.branch;
                        }
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
                    if evt.event_type == "task_started" {
                        if let Some(turn_id) = evt.turn_id.as_ref() {
                            if task_turn_id.is_none() {
                                task_turn_id = Some(turn_id.clone());
                            }
                            if !task_turn_ids.contains(turn_id) {
                                task_turn_ids.push(turn_id.clone());
                            }
                        }
                    }
                    if evt.event_type == "token_count" {
                        if let Some(info) = evt.info.as_ref() {
                            if let Some(usage) = info.total_token_usage.as_ref() {
                                total_tokens = usage.total_tokens;
                            }
                        }
                    }
                    if let Some((is_user, text)) = event_message_text(&evt) {
                        message_count += 1;
                        if preview.is_empty()
                            && (is_user || subagent_dispatch_content(&text).is_some())
                        {
                            preview = preview_text(&text, is_user);
                        }
                    }
                }
            }
            "turn_context" => {
                if let Ok(tc) = serde_json::from_value::<TurnContext>(codex_line.payload) {
                    if cwd.is_none() {
                        cwd = tc.cwd.clone();
                    }
                    if let Some(model) = tc.model {
                        if first_model.is_none() {
                            first_model = Some(model.clone());
                        }
                        model_candidates.push((tc.turn_id, tc.cwd, model));
                    }
                }
            }
            "response_item" => {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(codex_line.payload) {
                    if let Some((is_user, text)) = response_item_message_text(&item) {
                        message_count += 1;
                        if preview.is_empty()
                            && (is_user || subagent_dispatch_content(&text).is_some())
                        {
                            preview = preview_text(&text, is_user);
                        }
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

    let metadata = CodexFileMetadata {
        session_id: session_id.clone(),
        cwd: cwd.clone(),
        task_turn_id,
        task_turn_ids,
        turn_models,
        subagent_name,
        parent_session_id,
        subagent_depth,
        is_exec_wrapper,
    };

    let conversation = if message_count == 0 {
        None
    } else {
        let session_id = session_id
            .or_else(|| session_id_from_filename(path))
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });
        let timestamp = parse_conversation_timestamp(
            session_timestamp.as_deref(),
            first_timestamp.as_deref(),
            modified,
        );
        let directory_name = cwd.as_ref().and_then(|path| {
            PathBuf::from(path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(String::from)
        });
        let model =
            first_model.or_else(|| metadata.turn_models.first().map(|(_, model)| model.clone()));

        Some(Conversation {
            path: path.clone(),
            source: SessionSource::Codex,
            session_id,
            timestamp,
            preview,
            full_text: String::new(),
            directory_name,
            cwd: cwd.map(PathBuf::from),
            message_count,
            model,
            total_tokens,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch,
            subagent_name: None,
            hierarchy_has_children: false,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: 0,
            hierarchy_order: 0,
            hierarchy_sort_timestamp: timestamp,
        })
    };

    Some((path.clone(), metadata, conversation))
}

fn event_message_text(evt: &EventMsg) -> Option<(bool, String)> {
    match evt.event_type.as_str() {
        "user_message" => Some((true, clean_user_message(evt.message.as_ref()?)?)),
        "agent_message" => Some((false, evt.message.as_ref()?.to_string())),
        _ => None,
    }
}

fn response_item_message_text(item: &ResponseItem) -> Option<(bool, String)> {
    let is_user = match item.role.as_deref()? {
        "user" => true,
        "assistant" => false,
        _ => return None,
    };
    let parts = item
        .content
        .as_ref()?
        .iter()
        .filter_map(|part| part.text.as_deref())
        .filter(|text| !text.is_empty());

    let text = if is_user {
        parts
            .filter_map(clean_user_message)
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        parts.collect::<Vec<_>>().join("\n\n")
    };

    if text.is_empty() {
        None
    } else {
        Some((is_user, text))
    }
}

fn preview_text(text: &str, is_user: bool) -> String {
    let preview = if is_user {
        text.to_string()
    } else {
        subagent_dispatch_content(text).unwrap_or_else(|| text.to_string())
    };
    preview.chars().take(200).collect()
}

fn parse_conversation_timestamp(
    session_timestamp: Option<&str>,
    first_timestamp: Option<&str>,
    modified: Option<SystemTime>,
) -> DateTime<Local> {
    session_timestamp
        .or(first_timestamp)
        .and_then(|timestamp| {
            DateTime::parse_from_rfc3339(timestamp)
                .ok()
                .map(|timestamp| timestamp.with_timezone(&Local))
        })
        .or_else(|| {
            modified.and_then(|modified| {
                let duration = modified.duration_since(SystemTime::UNIX_EPOCH).ok()?;
                Local.timestamp_opt(duration.as_secs() as i64, 0).single()
            })
        })
        .unwrap_or_else(Local::now)
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
    let task_keys_by_path = task_keys_by_path(metadata_by_path);
    let ThreadEdges {
        mut parent_by_session_id,
        mut children_by_session_id,
        mut child_counts_by_session_id,
    } = explicit_thread_edges(metadata_by_path);
    let legacy_inputs =
        legacy_hierarchy_inputs(conversations, metadata_by_path, &task_keys_by_path);
    let mut thread_group_timestamps: HashMap<String, DateTime<Local>> = HashMap::new();
    let mut timestamp_by_session_id: HashMap<String, DateTime<Local>> = HashMap::new();
    let mut thread_order_by_session_id: HashMap<String, usize> = HashMap::new();
    let mut has_next_sibling_by_session_id: HashMap<String, bool> = HashMap::new();
    let mut hierarchy_marker_by_session_id: HashMap<String, String> = HashMap::new();

    attach_legacy_children_to_roots(
        legacy_inputs.legacy_children_by_key,
        &legacy_inputs.root_session_by_key,
        &mut parent_by_session_id,
        &mut children_by_session_id,
        &mut child_counts_by_session_id,
    );

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
        let mut context = ThreadOrderContext {
            children_by_session_id: &children_by_session_id,
            timestamp_by_session_id: &timestamp_by_session_id,
            subtree_latest_cache: &mut subtree_latest_cache,
            thread_order_by_session_id: &mut thread_order_by_session_id,
            has_next_sibling_by_session_id: &mut has_next_sibling_by_session_id,
            hierarchy_marker_by_session_id: &mut hierarchy_marker_by_session_id,
            next_order: 0,
        };
        assign_thread_order(&root_session_id, &mut context, &[], false, true);
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

        let keys = task_keys_by_path
            .get(&conversation.path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if keys.is_empty() {
            continue;
        }
        let legacy_group_timestamp = keys
            .iter()
            .filter_map(|key| legacy_inputs.group_timestamps.get(key).copied())
            .max();
        let has_legacy_subagents = keys
            .iter()
            .any(|key| legacy_inputs.subagent_counts.get(key).copied().unwrap_or(0) > 0);
        let primary_key = metadata.task_key();
        let has_legacy_parent = primary_key
            .as_ref()
            .is_some_and(|key| legacy_inputs.root_counts.get(key).copied().unwrap_or(0) > 0);

        if let Some(timestamp) = legacy_group_timestamp {
            conversation.hierarchy_sort_timestamp = timestamp;
        }
        conversation.hierarchy_has_children =
            metadata.subagent_name.is_none() && has_legacy_subagents;
        conversation.hierarchy_has_next_sibling = false;
        conversation.hierarchy_marker = conversation
            .hierarchy_has_children
            .then(|| "┬─".to_string());
        if let Some(subagent_name) = metadata.subagent_name.as_ref() {
            conversation.subagent_name = Some(subagent_name.clone());
            conversation.hierarchy_has_children = false;
            if has_legacy_parent {
                conversation.hierarchy_marker = Some("└─".to_string());
                conversation.hierarchy_has_next_sibling = false;
                conversation.hierarchy_depth = 1;
                conversation.hierarchy_order = 1;
            } else {
                conversation.hierarchy_marker = None;
                conversation.hierarchy_has_next_sibling = false;
                conversation.hierarchy_depth = 0;
                conversation.hierarchy_order = 0;
            }
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

struct LegacyHierarchyInputs {
    group_timestamps: HashMap<TaskModelKey, DateTime<Local>>,
    subagent_counts: HashMap<TaskModelKey, usize>,
    root_counts: HashMap<TaskModelKey, usize>,
    legacy_children_by_key: HashMap<TaskModelKey, Vec<(String, DateTime<Local>)>>,
    root_session_by_key: HashMap<TaskModelKey, (String, DateTime<Local>)>,
}

struct ThreadEdges {
    parent_by_session_id: HashMap<String, String>,
    children_by_session_id: HashMap<String, Vec<String>>,
    child_counts_by_session_id: HashMap<String, usize>,
}

fn task_keys_by_path(
    metadata_by_path: &HashMap<PathBuf, CodexFileMetadata>,
) -> HashMap<PathBuf, Vec<TaskModelKey>> {
    metadata_by_path
        .iter()
        .map(|(path, metadata)| (path.clone(), metadata.task_keys()))
        .collect()
}

fn explicit_thread_edges(metadata_by_path: &HashMap<PathBuf, CodexFileMetadata>) -> ThreadEdges {
    let mut parent_by_session_id: HashMap<String, String> = HashMap::new();
    let mut children_by_session_id: HashMap<String, Vec<String>> = HashMap::new();
    let mut child_counts_by_session_id: HashMap<String, usize> = HashMap::new();

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

    ThreadEdges {
        parent_by_session_id,
        children_by_session_id,
        child_counts_by_session_id,
    }
}

fn legacy_hierarchy_inputs(
    conversations: &[Conversation],
    metadata_by_path: &HashMap<PathBuf, CodexFileMetadata>,
    task_keys_by_path: &HashMap<PathBuf, Vec<TaskModelKey>>,
) -> LegacyHierarchyInputs {
    let mut inputs = LegacyHierarchyInputs {
        group_timestamps: HashMap::new(),
        subagent_counts: HashMap::new(),
        root_counts: HashMap::new(),
        legacy_children_by_key: HashMap::new(),
        root_session_by_key: HashMap::new(),
    };

    for conversation in conversations {
        let Some(metadata) = metadata_by_path.get(&conversation.path) else {
            continue;
        };
        let keys = task_keys_by_path
            .get(&conversation.path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        for key in keys {
            inputs
                .group_timestamps
                .entry(key.clone())
                .and_modify(|timestamp| {
                    if conversation.timestamp > *timestamp {
                        *timestamp = conversation.timestamp;
                    }
                })
                .or_insert(conversation.timestamp);

            if metadata.subagent_name.is_some() {
                *inputs.subagent_counts.entry(key.clone()).or_insert(0) += 1;
            } else {
                *inputs.root_counts.entry(key.clone()).or_insert(0) += 1;
                let session_id = metadata
                    .session_id
                    .as_ref()
                    .unwrap_or(&conversation.session_id);
                inputs
                    .root_session_by_key
                    .entry(key.clone())
                    .and_modify(|(existing_session_id, existing_timestamp)| {
                        if conversation.timestamp > *existing_timestamp
                            || (conversation.timestamp == *existing_timestamp
                                && session_id < existing_session_id)
                        {
                            *existing_session_id = session_id.clone();
                            *existing_timestamp = conversation.timestamp;
                        }
                    })
                    .or_insert_with(|| (session_id.clone(), conversation.timestamp));
            }
        }

        if metadata.parent_session_id.is_none() && metadata.subagent_name.is_some() {
            if let Some(key) = metadata.task_key() {
                let session_id = metadata
                    .session_id
                    .as_ref()
                    .unwrap_or(&conversation.session_id);
                inputs
                    .legacy_children_by_key
                    .entry(key)
                    .or_default()
                    .push((session_id.clone(), conversation.timestamp));
            }
        }
    }

    inputs
}

fn attach_legacy_children_to_roots(
    legacy_children_by_key: HashMap<TaskModelKey, Vec<(String, DateTime<Local>)>>,
    root_session_by_key: &HashMap<TaskModelKey, (String, DateTime<Local>)>,
    parent_by_session_id: &mut HashMap<String, String>,
    children_by_session_id: &mut HashMap<String, Vec<String>>,
    child_counts_by_session_id: &mut HashMap<String, usize>,
) {
    for (key, children) in legacy_children_by_key {
        let Some((parent_session_id, _)) = root_session_by_key.get(&key) else {
            continue;
        };
        for (session_id, _) in children {
            if session_id == *parent_session_id || parent_by_session_id.contains_key(&session_id) {
                continue;
            }
            parent_by_session_id.insert(session_id.clone(), parent_session_id.clone());
            children_by_session_id
                .entry(parent_session_id.clone())
                .or_default()
                .push(session_id);
            *child_counts_by_session_id
                .entry(parent_session_id.clone())
                .or_insert(0) += 1;
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

struct ThreadOrderContext<'a> {
    children_by_session_id: &'a HashMap<String, Vec<String>>,
    timestamp_by_session_id: &'a HashMap<String, DateTime<Local>>,
    subtree_latest_cache: &'a mut HashMap<String, Option<DateTime<Local>>>,
    thread_order_by_session_id: &'a mut HashMap<String, usize>,
    has_next_sibling_by_session_id: &'a mut HashMap<String, bool>,
    hierarchy_marker_by_session_id: &'a mut HashMap<String, String>,
    next_order: usize,
}

fn assign_thread_order(
    session_id: &str,
    context: &mut ThreadOrderContext<'_>,
    ancestor_continuations: &[bool],
    has_next_sibling: bool,
    is_root: bool,
) {
    let mut children = context
        .children_by_session_id
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    children.sort_by(|a, b| {
        subtree_latest(
            b,
            context.children_by_session_id,
            context.timestamp_by_session_id,
            context.subtree_latest_cache,
        )
        .cmp(&subtree_latest(
            a,
            context.children_by_session_id,
            context.timestamp_by_session_id,
            context.subtree_latest_cache,
        ))
        .then_with(|| a.cmp(b))
    });

    if context.timestamp_by_session_id.contains_key(session_id) {
        context
            .thread_order_by_session_id
            .insert(session_id.to_string(), context.next_order);
        context.hierarchy_marker_by_session_id.insert(
            session_id.to_string(),
            format_thread_marker(
                ancestor_continuations,
                has_next_sibling,
                !children.is_empty(),
                is_root,
            ),
        );
        context.next_order += 1;
    }

    let child_count = children.len();
    for (index, child) in children.into_iter().enumerate() {
        let child_has_next_sibling = index + 1 < child_count;
        let mut child_ancestor_continuations = ancestor_continuations.to_vec();
        if !is_root {
            child_ancestor_continuations.push(has_next_sibling);
        }
        context
            .has_next_sibling_by_session_id
            .insert(child.clone(), child_has_next_sibling);
        assign_thread_order(
            &child,
            context,
            &child_ancestor_continuations,
            child_has_next_sibling,
            false,
        );
    }
}

fn format_thread_marker(
    ancestor_continuations: &[bool],
    has_next_sibling: bool,
    has_children: bool,
    is_root: bool,
) -> String {
    if ancestor_continuations.is_empty() {
        if !is_root {
            return if has_next_sibling {
                "├─".to_string()
            } else {
                "└─".to_string()
            };
        }
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
    load_codex_sessions_with_options(CodexLoadOptions::default())
}

pub fn load_codex_sessions_with_options(options: CodexLoadOptions) -> Result<Vec<Conversation>> {
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
    let indexed: Vec<(PathBuf, CodexFileMetadata, Option<Conversation>)> =
        files.par_iter().filter_map(collect_file_index).collect();
    let metadata_by_path: HashMap<PathBuf, CodexFileMetadata> = indexed
        .iter()
        .map(|(path, metadata, _)| (path.clone(), metadata.clone()))
        .collect();
    let mut conversations: Vec<Conversation> = if options.include_full_text {
        files
            .into_par_iter()
            .filter_map(|path| {
                let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                process_codex_file(path, modified).ok().flatten()
            })
            .collect()
    } else {
        indexed
            .into_iter()
            .filter_map(|(_, _, conversation)| conversation)
            .collect()
    };

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

    fn write_session_strings(root: &std::path::Path, name: &str, lines: &[String]) {
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
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory","originator":"codex_exec","source":"exec"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:30Z","type":"event_msg","payload":{"type":"agent_message","message":"Looks good"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-28-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"session_meta","payload":{"id":"child-session","cwd":"/tmp/directory","originator":"codex_exec","source":{"subagent":"review"}}}"#,
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
    fn keeps_legacy_subagent_without_parent_as_plain_row() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-28-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"session_meta","payload":{"id":"child-session","cwd":"/tmp/directory","originator":"codex_exec","source":{"subagent":"review"}}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
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

        let child = sessions
            .iter()
            .find(|session| session.session_id == "child-session")
            .unwrap();
        assert_eq!(child.subagent_name.as_deref(), Some("review"));
        assert_eq!(child.hierarchy_depth, 0);
        assert!(!child.hierarchy_has_children);
        assert_eq!(format_hierarchy_marker(child), "");
    }

    #[test]
    fn groups_legacy_subagent_with_later_parent_turn() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-27-parent.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory","originator":"codex-tui","source":"cli"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-root"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Initial task"}}"#,
                r#"{"timestamp":"2026-05-20T22:46:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-child"}}"#,
                r#"{"timestamp":"2026-05-20T22:46:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T15-46-30-child-a.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:46:30Z","type":"session_meta","payload":{"id":"child-a","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":"review"}}}"#,
                r#"{"timestamp":"2026-05-20T22:46:31Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-child"}}"#,
                r#"{"timestamp":"2026-05-20T22:46:32Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T15-46-20-child-b.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:46:20Z","type":"session_meta","payload":{"id":"child-b","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":"audit"}}}"#,
                r#"{"timestamp":"2026-05-20T22:46:21Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-child"}}"#,
                r#"{"timestamp":"2026-05-20T22:46:22Z","type":"event_msg","payload":{"type":"user_message","message":"Audit this"}}"#,
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
        let child_a_position = sessions
            .iter()
            .position(|session| session.session_id == "child-a")
            .unwrap();
        let child_b_position = sessions
            .iter()
            .position(|session| session.session_id == "child-b")
            .unwrap();
        assert!(parent_position < child_a_position);
        assert!(child_a_position < child_b_position);
        assert!(sessions[parent_position].hierarchy_has_children);
        assert_eq!(format_hierarchy_marker(&sessions[child_a_position]), "├─");
        assert_eq!(format_hierarchy_marker(&sessions[child_b_position]), "└─");
    }

    #[test]
    fn load_codex_sessions_uses_lightweight_index_by_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-27-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/directory","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"turn_context","payload":{"turn_id":"turn-1","model":"gpt-5.5"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Index preview"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:30Z","type":"event_msg","payload":{"type":"agent_message","message":"Body that should stay lazy"}}"#,
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let session = sessions
            .iter()
            .find(|session| session.session_id == "session-id")
            .unwrap();
        assert_eq!(session.preview, "Index preview");
        assert_eq!(session.model.as_deref(), Some("gpt-5.5"));
        assert!(session.full_text.is_empty());
    }

    #[test]
    fn load_codex_sessions_counts_messages_and_tokens_after_initial_scan_window() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();
        let mut lines = vec![
            r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"long-session","cwd":"/tmp/directory","originator":"codex-tui"}}"#.to_string(),
            r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"user_message","message":"Index preview"}}"#.to_string(),
        ];
        for i in 0..300 {
            lines.push(format!(
                r#"{{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{{"type":"agent_message","message":"Later answer {i}"}}}}"#
            ));
        }
        lines.push(
            r#"{"timestamp":"2026-05-20T22:45:30Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":400,"output_tokens":599,"total_tokens":999}}}}"#.to_string(),
        );

        write_session_strings(
            root.path(),
            "rollout-2026-05-20T15-45-27-long.jsonl",
            &lines,
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions().unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let session = sessions
            .iter()
            .find(|session| session.session_id == "long-session")
            .unwrap();
        assert_eq!(session.message_count, 301);
        assert_eq!(session.total_tokens, 999);
        assert!(session.full_text.is_empty());
    }

    #[test]
    fn load_codex_sessions_can_include_full_text_for_search() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_codex_home = std::env::var_os("CODEX_HOME");
        let root = tempdir().unwrap();

        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-27-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/directory","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"user_message","message":"Searchable body"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"agent_message","message":"Searchable answer"}}"#,
            ],
        );

        std::env::set_var("CODEX_HOME", root.path());
        let sessions = load_codex_sessions_with_options(CodexLoadOptions {
            include_full_text: true,
        })
        .unwrap();
        if let Some(value) = previous_codex_home {
            std::env::set_var("CODEX_HOME", value);
        } else {
            std::env::remove_var("CODEX_HOME");
        }

        let session = sessions
            .iter()
            .find(|session| session.session_id == "session-id")
            .unwrap();
        assert!(session.full_text.contains("User: Searchable body"));
        assert!(session.full_text.contains("Assistant: Searchable answer"));
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
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory-a","originator":"codex_exec","source":"exec"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-28-unrelated.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"session_meta","payload":{"id":"unrelated-session","cwd":"/tmp/directory-b","originator":"codex_exec","source":{"subagent":"review"}}}"#,
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
                r#"{"timestamp":"2026-05-20T22:45:27Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory","originator":"codex_exec","source":"exec"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"timestamp":"2026-05-20T22:45:29Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T15-45-28-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T22:45:28Z","type":"session_meta","payload":{"id":"child-session","cwd":"/tmp/directory","originator":"codex_exec","source":{"subagent":"review"}}}"#,
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
                r#"{"timestamp":"2026-05-21T14:24:33Z","type":"session_meta","payload":{"id":"019e4aec-7d77-7783-9e57-0bb26a8d848a","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_path":"/root/review_compaction_fix","agent_nickname":"Hypatia","agent_role":"explorer"}}},"thread_source":"subagent","agent_nickname":"Hypatia","agent_role":"explorer"}}"#,
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
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:01Z","type":"turn_context","payload":{"turn_id":"parent-turn","model":"gpt-5.5"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-21T07-24-33-child-session.jsonl",
            &[
                r#"{"timestamp":"2026-05-21T14:24:33Z","type":"session_meta","payload":{"id":"child-session","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Hypatia","agent_role":"explorer"}}},"thread_source":"subagent","agent_nickname":"Hypatia","agent_role":"explorer"}}"#,
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
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-21T07-24-33-active-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-21T14:24:33Z","type":"session_meta","payload":{"id":"active-child","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Hypatia"}}},"thread_source":"subagent","agent_nickname":"Hypatia"}}"#,
                r#"{"timestamp":"2026-05-21T14:24:34Z","type":"event_msg","payload":{"type":"user_message","message":"Review this"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-21T07-25-33-grandchild.jsonl",
            &[
                r#"{"timestamp":"2026-05-21T14:25:33Z","type":"session_meta","payload":{"id":"grandchild","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"active-child","depth":2,"agent_nickname":"Hume"}}},"thread_source":"subagent","agent_nickname":"Hume"}}"#,
                r#"{"timestamp":"2026-05-21T14:25:34Z","type":"event_msg","payload":{"type":"user_message","message":"Nested review"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T08-00-00-stale-child.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T15:00:00Z","type":"session_meta","payload":{"id":"stale-child","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Stale"}}},"thread_source":"subagent","agent_nickname":"Stale"}}"#,
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
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-03-00-child-a.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:03:00Z","type":"session_meta","payload":{"id":"child-a","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Alpha"}}},"thread_source":"subagent","agent_nickname":"Alpha"}}"#,
                r#"{"timestamp":"2026-05-20T14:03:01Z","type":"event_msg","payload":{"type":"user_message","message":"Alpha task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-02-00-child-b.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:02:00Z","type":"session_meta","payload":{"id":"child-b","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Beta"}}},"thread_source":"subagent","agent_nickname":"Beta"}}"#,
                r#"{"timestamp":"2026-05-20T14:02:01Z","type":"event_msg","payload":{"type":"user_message","message":"Beta task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-01-00-child-c.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:01:00Z","type":"session_meta","payload":{"id":"child-c","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Gamma"}}},"thread_source":"subagent","agent_nickname":"Gamma"}}"#,
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
                ("child-a", "├─".to_string()),
                ("child-b", "├─".to_string()),
                ("child-c", "└─".to_string()),
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
                r#"{"timestamp":"2026-05-20T14:00:00Z","type":"session_meta","payload":{"id":"parent-session","cwd":"/tmp/directory","originator":"codex-tui"}}"#,
                r#"{"timestamp":"2026-05-20T14:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Parent task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-03-00-child-a.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:03:00Z","type":"session_meta","payload":{"id":"child-a","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Alpha"}}},"thread_source":"subagent","agent_nickname":"Alpha"}}"#,
                r#"{"timestamp":"2026-05-20T14:03:01Z","type":"event_msg","payload":{"type":"user_message","message":"Alpha task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-04-00-grandchild.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:04:00Z","type":"session_meta","payload":{"id":"grandchild","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"child-a","depth":2,"agent_nickname":"Nested"}}},"thread_source":"subagent","agent_nickname":"Nested"}}"#,
                r#"{"timestamp":"2026-05-20T14:04:01Z","type":"event_msg","payload":{"type":"user_message","message":"Nested task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-05-00-great-grandchild.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:05:00Z","type":"session_meta","payload":{"id":"great-grandchild","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"grandchild","depth":3,"agent_nickname":"Deep"}}},"thread_source":"subagent","agent_nickname":"Deep"}}"#,
                r#"{"timestamp":"2026-05-20T14:05:01Z","type":"event_msg","payload":{"type":"user_message","message":"Deep task"}}"#,
            ],
        );
        write_session(
            root.path(),
            "rollout-2026-05-20T07-02-00-child-b.jsonl",
            &[
                r#"{"timestamp":"2026-05-20T14:02:00Z","type":"session_meta","payload":{"id":"child-b","cwd":"/tmp/directory","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1,"agent_nickname":"Beta"}}},"thread_source":"subagent","agent_nickname":"Beta"}}"#,
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
                ("child-a", "├─".to_string()),
                ("grandchild", "│ └─".to_string()),
                ("great-grandchild", "│   └─".to_string()),
                ("child-b", "└─".to_string()),
            ]
        );
    }
}
