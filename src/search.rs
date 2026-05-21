use crate::codex_parser::process_codex_file;
use crate::history::{Conversation, SessionSource};
use chrono::{DateTime, Duration, Local};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SEARCH_CACHE_FILE: &str = "search-index-v1.jsonl";

/// Precomputed search data for a conversation
pub struct SearchableConversation {
    /// Lowercased full text for searching
    pub text_lower: String,
    /// Original conversation index
    pub index: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SearchCacheFingerprint {
    size: u64,
    modified_secs: u64,
    modified_nanos: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SearchCacheEntry {
    path: String,
    fingerprint: SearchCacheFingerprint,
    text_lower: String,
}

/// Normalize text for search: lowercase, replace non-alphanumeric chars with spaces
fn normalize_for_search(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '.' {
            out.extend(ch.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out
}

/// Precompute lowercased search text for all conversations
pub fn precompute_search_text(conversations: &[Conversation]) -> Vec<SearchableConversation> {
    conversations
        .par_iter()
        .enumerate()
        .map(|(idx, conv)| SearchableConversation {
            text_lower: conversation_search_text_lower(conv),
            index: idx,
        })
        .collect()
}

/// Precompute full-context search text, using a persistent cache for lightweight
/// Codex rows so the expensive JSONL body parse is paid only when files change.
pub fn precompute_full_search_text(conversations: &[Conversation]) -> Vec<SearchableConversation> {
    if let Some(cache_path) = default_search_cache_path() {
        return precompute_full_search_text_with_cache_path(conversations, &cache_path);
    }

    precompute_uncached_full_search_text(conversations)
}

fn precompute_full_search_text_with_cache_path(
    conversations: &[Conversation],
    cache_path: &Path,
) -> Vec<SearchableConversation> {
    let cache = read_search_cache(cache_path);
    let built: Vec<(SearchableConversation, Option<SearchCacheEntry>)> = conversations
        .par_iter()
        .enumerate()
        .map(|(idx, conv)| {
            let (text_lower, cache_entry) = full_search_text_lower(conv, &cache);
            (
                SearchableConversation {
                    text_lower,
                    index: idx,
                },
                cache_entry,
            )
        })
        .collect();

    let next_cache: HashMap<String, SearchCacheEntry> = built
        .iter()
        .filter_map(|(_, entry)| entry.clone().map(|entry| (entry.path.clone(), entry)))
        .collect();
    if next_cache != cache {
        write_search_cache(cache_path, &next_cache);
    }

    built
        .into_iter()
        .map(|(searchable, _)| searchable)
        .collect()
}

fn precompute_uncached_full_search_text(
    conversations: &[Conversation],
) -> Vec<SearchableConversation> {
    conversations
        .par_iter()
        .enumerate()
        .map(|(idx, conv)| SearchableConversation {
            text_lower: uncached_full_search_text_lower(conv),
            index: idx,
        })
        .collect()
}

fn full_search_text_lower(
    conv: &Conversation,
    cache: &HashMap<String, SearchCacheEntry>,
) -> (String, Option<SearchCacheEntry>) {
    if conv.source != SessionSource::Codex || !conv.full_text.is_empty() {
        return (conversation_search_text_lower(conv), None);
    }

    let path = conv.path.to_string_lossy().to_string();
    let Some((fingerprint, modified)) = file_fingerprint(&conv.path) else {
        return (conversation_search_text_lower(conv), None);
    };

    if let Some(entry) = cache.get(&path) {
        if entry.fingerprint == fingerprint {
            return (
                cached_text_with_current_metadata(&entry.text_lower, conv),
                Some(entry.clone()),
            );
        }
    }

    if let Some(entry) = build_codex_cache_entry(conv, path, fingerprint, modified) {
        return (
            cached_text_with_current_metadata(&entry.text_lower, conv),
            Some(entry),
        );
    }

    (conversation_search_text_lower(conv), None)
}

fn uncached_full_search_text_lower(conv: &Conversation) -> String {
    if conv.source != SessionSource::Codex || !conv.full_text.is_empty() {
        return conversation_search_text_lower(conv);
    }

    let modified = std::fs::metadata(&conv.path)
        .and_then(|metadata| metadata.modified())
        .ok();
    process_codex_file(conv.path.clone(), modified)
        .ok()
        .flatten()
        .map(|parsed| conversation_search_text_lower(&parsed))
        .unwrap_or_else(|| conversation_search_text_lower(conv))
}

fn build_codex_cache_entry(
    conv: &Conversation,
    path: String,
    fingerprint: SearchCacheFingerprint,
    modified: SystemTime,
) -> Option<SearchCacheEntry> {
    let parsed = process_codex_file(conv.path.clone(), Some(modified))
        .ok()
        .flatten()?;
    Some(SearchCacheEntry {
        path,
        fingerprint,
        text_lower: conversation_search_text_lower(&parsed),
    })
}

fn cached_text_with_current_metadata(cached_text_lower: &str, conv: &Conversation) -> String {
    let metadata = conversation_metadata_text(conv);
    if metadata.is_empty() {
        cached_text_lower.to_string()
    } else {
        format!("{} {}", cached_text_lower, normalize_for_search(&metadata))
    }
}

fn conversation_search_text_lower(conv: &Conversation) -> String {
    let mut text = conv.full_text.clone();
    let metadata = conversation_metadata_text(conv);
    if !metadata.is_empty() {
        text.push(' ');
        text.push_str(&metadata);
    }
    normalize_for_search(&text)
}

fn conversation_metadata_text(conv: &Conversation) -> String {
    let mut parts = Vec::new();
    if !conv.preview.is_empty() {
        parts.push(conv.preview.as_str());
    }
    if let Some(name) = conv.project_name.as_deref() {
        parts.push(name);
    }
    if let Some(name) = conv.subagent_name.as_deref() {
        parts.push(name);
    }
    if let Some(summary) = conv.summary.as_deref() {
        parts.push(summary);
    }
    if let Some(title) = conv.custom_title.as_deref() {
        parts.push(title);
    }
    parts.join(" ")
}

fn default_search_cache_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AGENT_HISTORY_CACHE_DIR") {
        return Some(PathBuf::from(dir).join(SEARCH_CACHE_FILE));
    }
    Some(
        home::home_dir()?
            .join(".cache")
            .join("agent-history")
            .join(SEARCH_CACHE_FILE),
    )
}

fn file_fingerprint(path: &Path) -> Option<(SearchCacheFingerprint, SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((
        SearchCacheFingerprint {
            size: metadata.len(),
            modified_secs: duration.as_secs(),
            modified_nanos: duration.subsec_nanos(),
        },
        modified,
    ))
}

fn read_search_cache(path: &Path) -> HashMap<String, SearchCacheEntry> {
    let Ok(file) = File::open(path) else {
        return HashMap::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(|line| line.ok())
        .filter_map(|line| serde_json::from_str::<SearchCacheEntry>(&line).ok())
        .map(|entry| (entry.path.clone(), entry))
        .collect()
}

fn write_search_cache(path: &Path, entries: &HashMap<String, SearchCacheEntry>) {
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }

    let tmp_path = path.with_extension("jsonl.tmp");
    let Ok(mut file) = File::create(&tmp_path) else {
        return;
    };

    let mut paths: Vec<&String> = entries.keys().collect();
    paths.sort();
    for path in paths {
        let Some(entry) = entries.get(path) else {
            continue;
        };
        if serde_json::to_writer(&mut file, entry).is_err() {
            let _ = fs::remove_file(&tmp_path);
            return;
        }
        if writeln!(file).is_err() {
            let _ = fs::remove_file(&tmp_path);
            return;
        }
    }

    let _ = fs::rename(tmp_path, path);
}

/// Filter and score conversations based on query.
/// Returns indices into the original conversations vec, sorted by score descending.
pub fn search(
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..conversations.len()).collect();
    }

    let query_lower = normalize_for_search(query);
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();
    if query_words.is_empty() {
        return (0..conversations.len()).collect();
    }

    let mut scored: Vec<(usize, f64, DateTime<Local>)> = searchable
        .par_iter()
        .filter_map(|s| {
            let score = score_text(
                &s.text_lower,
                &query_words,
                conversations[s.index].timestamp,
                now,
            );
            if score > 0.0 {
                Some((s.index, score, conversations[s.index].timestamp))
            } else {
                None
            }
        })
        .collect();

    // Sort by score descending, then by timestamp descending for stability
    scored.sort_unstable_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
    });

    scored.into_iter().map(|(idx, _, _)| idx).collect()
}

/// Score a conversation based on word prefix matching and recency.
/// Each query word must be a prefix of at least one word in the text (AND logic).
fn score_text(
    text_lower: &str,
    query_words: &[&str],
    timestamp: DateTime<Local>,
    now: DateTime<Local>,
) -> f64 {
    if query_words.is_empty() {
        return 0.0;
    }

    // Fast rejection: if a query word isn't present as substring, skip
    for &qw in query_words {
        if !text_lower.contains(qw) {
            return 0.0;
        }
    }

    // Single-pass word matching with prefix match
    let mut matched = vec![false; query_words.len()];
    let mut remaining = query_words.len();

    for text_word in text_lower.split_whitespace() {
        for (i, &qw) in query_words.iter().enumerate() {
            if !matched[i] && text_word.starts_with(qw) {
                matched[i] = true;
                remaining -= 1;
                if remaining == 0 {
                    return (query_words.len() as f64) * recency_multiplier(timestamp, now);
                }
            }
        }
    }

    0.0
}

/// Calculate recency multiplier based on age
fn recency_multiplier(timestamp: DateTime<Local>, now: DateTime<Local>) -> f64 {
    let age = now.signed_duration_since(timestamp);

    if age < Duration::zero() {
        return 3.0;
    }

    if age < Duration::days(1) {
        3.0
    } else if age < Duration::days(7) {
        2.0
    } else if age < Duration::days(30) {
        1.5
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{Conversation, SessionSource};
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::{tempdir, NamedTempFile};

    fn conversation(
        source: SessionSource,
        path: PathBuf,
        preview: &str,
        full_text: &str,
    ) -> Conversation {
        let timestamp = Local::now();
        Conversation {
            path,
            source,
            session_id: "session-id".to_string(),
            timestamp,
            preview: preview.to_string(),
            full_text: full_text.to_string(),
            project_name: Some("project".to_string()),
            cwd: None,
            message_count: 1,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: None,
            hierarchy_has_children: false,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: 0,
            hierarchy_order: 0,
            hierarchy_sort_timestamp: timestamp,
        }
    }

    fn codex_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file
    }

    #[test]
    fn precompute_search_text_includes_preview_for_lightweight_rows() {
        let conversations = vec![conversation(
            SessionSource::Codex,
            PathBuf::from("session.jsonl"),
            "Visible preview text",
            "",
        )];
        let searchable = precompute_search_text(&conversations);

        let results = search(&conversations, &searchable, "visible", Local::now());

        assert_eq!(results, vec![0]);
    }

    #[test]
    fn full_search_text_indexes_codex_body_and_writes_cache() {
        let file = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/project"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible preview"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"Hidden needle context"}}"#,
        ]);
        let conversations = vec![conversation(
            SessionSource::Codex,
            file.path().to_path_buf(),
            "Visible preview",
            "",
        )];
        let cache_dir = tempdir().unwrap();
        let cache_path = cache_dir.path().join("search-index.jsonl");

        let searchable = precompute_full_search_text_with_cache_path(&conversations, &cache_path);
        let results = search(&conversations, &searchable, "needle", Local::now());

        assert_eq!(results, vec![0]);
        assert!(cache_path.exists());
        assert!(std::fs::read_to_string(cache_path)
            .unwrap()
            .contains("needle"));
    }

    #[test]
    fn full_search_text_reuses_matching_cache_entry() {
        let file = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/project"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible preview"}}"#,
        ]);
        let conversations = vec![conversation(
            SessionSource::Codex,
            file.path().to_path_buf(),
            "Visible preview",
            "",
        )];
        let cache_dir = tempdir().unwrap();
        let cache_path = cache_dir.path().join("search-index.jsonl");
        let (fingerprint, _) = file_fingerprint(file.path()).unwrap();
        let entry = SearchCacheEntry {
            path: file.path().to_string_lossy().to_string(),
            fingerprint,
            text_lower: "cachedonly".to_string(),
        };
        write_search_cache(&cache_path, &HashMap::from([(entry.path.clone(), entry)]));

        let searchable = precompute_full_search_text_with_cache_path(&conversations, &cache_path);
        let results = search(&conversations, &searchable, "cachedonly", Local::now());

        assert_eq!(results, vec![0]);
    }
}
