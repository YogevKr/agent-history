//! Claude session discovery and loading.

use crate::claude_parser::{process_claude_file_with_options, ClaudeParseOptions};
use crate::error::{AppError, Result};
use crate::path::{decode_project_dir_name_to_path, format_short_name_from_path};
use crate::startup_cache::{file_fingerprint, FileFingerprint, StartupCache};
use rayon::prelude::*;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::read_dir;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::history::Conversation;

const CLAUDE_CACHE_SOURCE: &str = "claude";

/// Shared cache state for one full load pass. `fresh` collects newly parsed
/// results for write-back; `live` collects every candidate file so stale cache
/// rows can be pruned.
struct ClaudeCacheContext {
    entries: HashMap<String, (FileFingerprint, String)>,
    fresh: Mutex<Vec<(String, FileFingerprint, String)>>,
    live: Mutex<HashSet<String>>,
}

impl ClaudeCacheContext {
    fn new(cache: &StartupCache) -> Self {
        Self {
            entries: cache.load_source_entries(CLAUDE_CACHE_SOURCE),
            fresh: Mutex::new(Vec::new()),
            live: Mutex::new(HashSet::new()),
        }
    }
}

/// Claude encoded directory metadata.
struct Directory {
    name: String,
    modified: SystemTime,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeLoadOptions {
    pub include_full_text: bool,
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

/// List all encoded directories that contain conversation files.
fn list_directories(root: &Path) -> Result<Vec<Directory>> {
    let entries = read_dir(root)?;

    let mut directories: Vec<Directory> = entries
        .par_bridge()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();

            if !path.is_dir() {
                return None;
            }

            // Check if the encoded directory has any non-agent .jsonl files.
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
            let modified = entry
                .metadata()
                .ok()?
                .modified()
                .ok()
                .unwrap_or(SystemTime::UNIX_EPOCH);

            Some(Directory { name, modified })
        })
        .collect();

    directories.sort_by_key(|directory| Reverse(directory.modified));

    Ok(directories)
}

/// Load conversations from a single encoded directory.
fn load_conversations(
    directory_dir: &Path,
    options: ClaudeLoadOptions,
    cache_ctx: Option<&ClaudeCacheContext>,
) -> Result<Vec<Conversation>> {
    let mut files_with_meta = Vec::new();

    for entry in read_dir(directory_dir)? {
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
        .filter_map(|(path, modified)| load_conversation_file(path, modified, options, cache_ctx))
        .collect();

    Ok(conversations)
}

/// Parse a single file, going through the startup cache when one is active.
/// Filtered-out sessions are cached as `None` so they are not re-parsed on
/// every launch.
fn load_conversation_file(
    path: PathBuf,
    modified: Option<SystemTime>,
    options: ClaudeLoadOptions,
    cache_ctx: Option<&ClaudeCacheContext>,
) -> Option<Conversation> {
    let parse_options = ClaudeParseOptions {
        include_full_text: options.include_full_text,
    };

    let Some(ctx) = cache_ctx else {
        return process_claude_file_with_options(path, modified, parse_options)
            .ok()
            .flatten();
    };

    let path_str = path.to_string_lossy().to_string();
    ctx.live.lock().unwrap().insert(path_str.clone());

    let fingerprint = file_fingerprint(&path);
    if let (Some(fingerprint), Some((cached_fingerprint, data))) =
        (fingerprint, ctx.entries.get(&path_str))
    {
        if *cached_fingerprint == fingerprint {
            if let Ok(conversation) = serde_json::from_str::<Option<Conversation>>(data) {
                return conversation;
            }
        }
    }

    let conversation = process_claude_file_with_options(path, modified, parse_options).ok()?;
    if let (Some(fingerprint), Ok(data)) = (fingerprint, serde_json::to_string(&conversation)) {
        ctx.fresh
            .lock()
            .unwrap()
            .push((path_str, fingerprint, data));
    }
    conversation
}

#[cfg(test)]
fn load_claude_sessions_with_cache(
    options: ClaudeLoadOptions,
    cache_db_path: &Path,
) -> Result<Vec<Conversation>> {
    load_claude_sessions_impl(options, StartupCache::open(cache_db_path))
}

/// Load all Claude sessions from all directories, sorted by timestamp descending.
fn load_claude_sessions_impl(
    options: ClaudeLoadOptions,
    mut cache: Option<StartupCache>,
) -> Result<Vec<Conversation>> {
    let root = get_claude_projects_root()?;

    if !root.exists() {
        return Ok(Vec::new());
    }

    // Full-text loads bypass the cache: it only stores the lightweight form.
    if options.include_full_text {
        cache = None;
    }
    let cache_ctx = cache.as_ref().map(ClaudeCacheContext::new);

    let directories = list_directories(&root)?;

    let mut all_conversations: Vec<Conversation> = directories
        .par_iter()
        .flat_map(|directory| {
            let directory_dir = root.join(&directory.name);
            match load_conversations(&directory_dir, options, cache_ctx.as_ref()) {
                Ok(mut convs) => {
                    let fallback_path = decode_project_dir_name_to_path(&directory.name);

                    for conv in &mut convs {
                        let directory_path =
                            conv.cwd.clone().unwrap_or_else(|| fallback_path.clone());
                        conv.directory_name = Some(format_short_name_from_path(&directory_path));
                    }
                    convs
                }
                Err(_) => Vec::new(),
            }
        })
        .collect();

    if let (Some(cache), Some(ctx)) = (cache.as_mut(), cache_ctx) {
        let fresh = ctx.fresh.into_inner().unwrap_or_default();
        cache.store_entries(CLAUDE_CACHE_SOURCE, &fresh);
        let live = ctx.live.into_inner().unwrap_or_default();
        cache.prune_missing(CLAUDE_CACHE_SOURCE, &live);
    }

    all_conversations.sort_by_key(|conversation| Reverse(conversation.timestamp));

    Ok(all_conversations)
}

pub fn load_claude_sessions() -> Result<Vec<Conversation>> {
    load_claude_sessions_impl(ClaudeLoadOptions::default(), StartupCache::open_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_project_session(config_root: &Path, project: &str, name: &str, lines: &[&str]) {
        let dir = config_root.join("projects").join(project);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    #[test]
    fn startup_cache_round_trips_sessions_and_filtered_files() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR");
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let cache_db = cache_dir.path().join("startup-index.sqlite");

        write_project_session(
            root.path(),
            "-tmp-project",
            "real-session.jsonl",
            &[
                r#"{"type":"user","timestamp":"2026-05-27T10:00:00Z","cwd":"/tmp/project","message":{"role":"user","content":"Real question"}}"#,
                r#"{"type":"assistant","timestamp":"2026-05-27T10:01:00Z","message":{"role":"assistant","model":"claude-sonnet-4-20250514","content":[{"type":"text","text":"Answer"}]}}"#,
            ],
        );
        // Filtered out by the non-interactive temp-session detector; must be
        // cached as `null` so it is not re-parsed every launch.
        write_project_session(
            root.path(),
            "-tmp-project",
            "filtered-session.jsonl",
            &[
                r#"{"type":"attachment","timestamp":"2026-05-27T09:10:59.191Z","entrypoint":"sdk-cli","cwd":"/private/tmp","sessionId":"generated","attachment":{"type":"hook_success"}}"#,
                r#"{"type":"user","timestamp":"2026-05-27T10:00:00Z","cwd":"/private/tmp","message":{"role":"user","content":"generated"}}"#,
                r#"{"type":"assistant","timestamp":"2026-05-27T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"generated"}]}}"#,
            ],
        );

        std::env::set_var("CLAUDE_CONFIG_DIR", root.path());

        let fresh =
            load_claude_sessions_with_cache(ClaudeLoadOptions::default(), &cache_db).unwrap();
        let cached =
            load_claude_sessions_with_cache(ClaudeLoadOptions::default(), &cache_db).unwrap();

        // Prove the second load was served from the cache: rewrite the cached
        // entry and observe the sentinel in the next load.
        let filtered_cached_as_null = {
            let conn = rusqlite::Connection::open(&cache_db).unwrap();
            let changed = conn
                .execute(
                    "UPDATE file_index SET data = replace(data, 'Real question', 'CACHE SENTINEL')
                     WHERE data LIKE '%Real question%'",
                    [],
                )
                .unwrap();
            assert_eq!(changed, 1);
            conn.query_row(
                "SELECT data FROM file_index WHERE path LIKE '%filtered-session.jsonl'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
        };
        let poisoned =
            load_claude_sessions_with_cache(ClaudeLoadOptions::default(), &cache_db).unwrap();

        if let Some(value) = previous_config_dir {
            std::env::set_var("CLAUDE_CONFIG_DIR", value);
        } else {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }

        assert_eq!(fresh.len(), 1);
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].session_id, fresh[0].session_id);
        assert_eq!(cached[0].preview, fresh[0].preview);
        assert_eq!(cached[0].timestamp, fresh[0].timestamp);
        assert_eq!(cached[0].directory_name, fresh[0].directory_name);

        assert_eq!(filtered_cached_as_null, "null");
        assert_eq!(poisoned.len(), 1);
        assert!(poisoned[0].preview.contains("CACHE SENTINEL"));
    }
}
