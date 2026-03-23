//! Claude session discovery and loading.

use crate::claude_parser::process_claude_file;
use crate::error::{AppError, Result};
use crate::path::{
    decode_project_dir_name, decode_project_dir_name_to_path, format_short_name_from_path,
};
use rayon::prelude::*;
use std::fs::read_dir;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::history::Conversation;

/// Project directory metadata
struct Project {
    name: String,
    #[allow(dead_code)]
    display_name: String,
    #[allow(dead_code)]
    modified: SystemTime,
}

/// Get the root Claude projects directory (~/.claude/projects).
/// Respects CLAUDE_CONFIG_DIR env variable if set.
fn get_claude_projects_root() -> Result<PathBuf> {
    let claude_dir = if let Ok(config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        PathBuf::from(config_dir)
    } else {
        let home_dir = home::home_dir().ok_or_else(|| {
            AppError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Could not determine home directory",
            ))
        })?;
        home_dir.join(".claude")
    };

    Ok(claude_dir.join("projects"))
}

/// List all projects that contain conversation files
fn list_projects(root: &Path) -> Result<Vec<Project>> {
    let entries = read_dir(root)?;

    let mut projects: Vec<Project> = entries
        .par_bridge()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();

            if !path.is_dir() {
                return None;
            }

            // Check if project has any non-agent .jsonl files
            let has_conversations = read_dir(&path).ok()?.any(|e| {
                e.ok()
                    .map(|e| {
                        let path = e.path();
                        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                        path.extension().map(|s| s == "jsonl").unwrap_or(false)
                            && !name.starts_with("agent-")
                    })
                    .unwrap_or(false)
            });

            if !has_conversations {
                return None;
            }

            let name = path.file_name()?.to_string_lossy().to_string();
            let display_name = decode_project_dir_name(&name);
            let modified = entry
                .metadata()
                .ok()?
                .modified()
                .ok()
                .unwrap_or(SystemTime::UNIX_EPOCH);

            Some(Project {
                name,
                display_name,
                modified,
            })
        })
        .collect();

    projects.sort_by(|a, b| b.modified.cmp(&a.modified));

    Ok(projects)
}

/// Load conversations from a single project directory
fn load_conversations(projects_dir: &Path) -> Result<Vec<Conversation>> {
    let mut files_with_meta = Vec::new();

    for entry in read_dir(projects_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                if filename.starts_with("agent-") {
                    continue;
                }
            }

            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok());

            files_with_meta.push((path, modified));
        }
    }

    files_with_meta.sort_by_key(|(_, modified)| modified.unwrap_or(SystemTime::UNIX_EPOCH));
    files_with_meta.reverse();

    let conversations: Vec<Conversation> = files_with_meta
        .into_par_iter()
        .filter_map(
            |(path, modified)| match process_claude_file(path, modified) {
                Ok(Some(conversation)) => Some(conversation),
                _ => None,
            },
        )
        .collect();

    Ok(conversations)
}

/// Load all Claude sessions from all projects, sorted by timestamp descending.
pub fn load_claude_sessions() -> Result<Vec<Conversation>> {
    let root = get_claude_projects_root()?;

    if !root.exists() {
        return Ok(Vec::new());
    }

    let projects = list_projects(&root)?;

    let mut all_conversations: Vec<Conversation> = projects
        .par_iter()
        .flat_map(|project| {
            let project_dir = root.join(&project.name);
            match load_conversations(&project_dir) {
                Ok(mut convs) => {
                    let fallback_path = decode_project_dir_name_to_path(&project.name);

                    for conv in &mut convs {
                        let project_path =
                            conv.cwd.clone().unwrap_or_else(|| fallback_path.clone());
                        conv.project_name = Some(format_short_name_from_path(&project_path));
                    }
                    convs
                }
                Err(_) => Vec::new(),
            }
        })
        .collect();

    all_conversations.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    Ok(all_conversations)
}
