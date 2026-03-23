use std::path::PathBuf;

use rayon::prelude::*;

use crate::codex_parser::process_codex_file;
use crate::error::Result;
use crate::history::Conversation;

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

    let mut conversations: Vec<Conversation> = files
        .into_par_iter()
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            process_codex_file(path, modified).ok().flatten()
        })
        .collect();

    conversations.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    Ok(conversations)
}
