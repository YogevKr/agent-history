use crate::claude_parser::process_claude_file;
use crate::codex_parser::process_codex_file;
use crate::history::{Conversation, SessionSource};
use chrono::{DateTime, Duration, Local};
use rayon::prelude::*;
use rusqlite::{params, Connection, OpenFlags};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SEARCH_INDEX_FILE: &str = "search-index-v3.sqlite";

/// Precomputed search data for a conversation
#[derive(Clone)]
pub struct SearchableConversation {
    /// Lowercased full text for searching
    pub text_lower: String,
    /// Original conversation index
    pub index: usize,
}

/// Persistent full-context search index.
#[derive(Clone)]
pub enum FullSearchIndex {
    Sqlite(SqliteSearchIndex),
    InMemory(Vec<SearchableConversation>),
}

#[derive(Clone)]
pub struct SqliteSearchIndex {
    db_path: PathBuf,
    rowid_to_index: HashMap<i64, usize>,
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

/// Precompute full-context search as a persistent SQLite FTS5 index. Falls back
/// to in-memory search when the index cannot be opened or updated.
pub fn precompute_full_search_index(conversations: &[Conversation]) -> FullSearchIndex {
    if let Some(db_path) = default_search_index_path() {
        return precompute_full_search_index_with_db_path(conversations, &db_path);
    }

    FullSearchIndex::InMemory(precompute_uncached_full_search_text(conversations))
}

fn precompute_full_search_index_with_db_path(
    conversations: &[Conversation],
    db_path: &Path,
) -> FullSearchIndex {
    build_sqlite_search_index(conversations, db_path).unwrap_or_else(|_| {
        FullSearchIndex::InMemory(precompute_uncached_full_search_text(conversations))
    })
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

fn uncached_full_search_text_lower(conv: &Conversation) -> String {
    if !conv.full_text.is_empty() {
        return conversation_search_text_lower(conv);
    }

    let modified = std::fs::metadata(&conv.path)
        .and_then(|metadata| metadata.modified())
        .ok();
    hydrate_full_conversation(conv, modified)
        .map(|parsed| conversation_search_text_lower(&parsed))
        .unwrap_or_else(|| conversation_search_text_lower(conv))
}

fn full_search_text_lower_with_modified(conv: &Conversation, modified: SystemTime) -> String {
    if !conv.full_text.is_empty() {
        return conversation_search_text_lower(conv);
    }

    hydrate_full_conversation(conv, Some(modified))
        .map(|parsed| conversation_search_text_lower(&parsed))
        .unwrap_or_else(|| conversation_search_text_lower(conv))
}

fn hydrate_full_conversation(
    conv: &Conversation,
    modified: Option<SystemTime>,
) -> Option<Conversation> {
    match conv.source {
        SessionSource::Claude => process_claude_file(conv.path.clone(), modified)
            .ok()
            .flatten(),
        SessionSource::Codex => process_codex_file(conv.path.clone(), modified)
            .ok()
            .flatten(),
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

fn default_search_index_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AGENT_HISTORY_CACHE_DIR") {
        return Some(PathBuf::from(dir).join(SEARCH_INDEX_FILE));
    }
    Some(
        home::home_dir()?
            .join(".cache")
            .join("agent-history")
            .join(SEARCH_INDEX_FILE),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SearchFileFingerprint {
    size: i64,
    modified_secs: i64,
    modified_nanos: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchMeta {
    rowid: i64,
    session_id: String,
    fingerprint: SearchFileFingerprint,
}

struct IndexedSession {
    path: String,
    session_id: String,
    fingerprint: SearchFileFingerprint,
    body: String,
}

enum IndexChange {
    Upsert(IndexedSession),
    Delete(String),
}

type SearchIndexResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn file_fingerprint(path: &Path) -> Option<(SearchFileFingerprint, SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((
        SearchFileFingerprint {
            size: metadata.len().try_into().ok()?,
            modified_secs: duration.as_secs().try_into().ok()?,
            modified_nanos: duration.subsec_nanos().into(),
        },
        modified,
    ))
}

fn build_sqlite_search_index(
    conversations: &[Conversation],
    db_path: &Path,
) -> SearchIndexResult<FullSearchIndex> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut conn = Connection::open(db_path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        CREATE TABLE IF NOT EXISTS session_meta (
            path TEXT NOT NULL UNIQUE,
            session_id TEXT NOT NULL,
            size INTEGER NOT NULL,
            modified_secs INTEGER NOT NULL,
            modified_nanos INTEGER NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(
            body,
            content = '',
            contentless_delete = 1,
            tokenize = 'unicode61'
        );
        ",
    )?;

    let meta = sync_sqlite_search_index(&mut conn, conversations)?;

    let path_to_index: HashMap<String, usize> = conversations
        .iter()
        .enumerate()
        .map(|(idx, conv)| (conv.path.to_string_lossy().to_string(), idx))
        .collect();
    let rowid_to_index = meta
        .into_iter()
        .filter_map(|(path, meta)| path_to_index.get(&path).map(|idx| (meta.rowid, *idx)))
        .collect();

    Ok(FullSearchIndex::Sqlite(SqliteSearchIndex {
        db_path: db_path.to_path_buf(),
        rowid_to_index,
    }))
}

fn sync_sqlite_search_index(
    conn: &mut Connection,
    conversations: &[Conversation],
) -> SearchIndexResult<HashMap<String, SearchMeta>> {
    let existing = read_sqlite_meta(conn)?;
    let changes: Vec<IndexChange> = conversations
        .par_iter()
        .filter_map(|conv| {
            let path = conv.path.to_string_lossy().to_string();
            let Some((fingerprint, modified)) = file_fingerprint(&conv.path) else {
                return Some(IndexChange::Delete(path));
            };
            if existing.get(&path).is_some_and(|meta| {
                meta.session_id == conv.session_id && meta.fingerprint == fingerprint
            }) {
                return None;
            }

            Some(IndexChange::Upsert(IndexedSession {
                path,
                session_id: conv.session_id.clone(),
                fingerprint,
                body: full_search_text_lower_with_modified(conv, modified),
            }))
        })
        .collect();

    if changes.is_empty() {
        return Ok(existing);
    }

    let tx = conn.transaction()?;
    for change in changes {
        match change {
            IndexChange::Upsert(session) => {
                delete_indexed_path(&tx, &session.path)?;
                tx.execute(
                    "
                    INSERT INTO session_meta(path, session_id, size, modified_secs, modified_nanos)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ",
                    params![
                        session.path,
                        session.session_id,
                        session.fingerprint.size,
                        session.fingerprint.modified_secs,
                        session.fingerprint.modified_nanos
                    ],
                )?;
                let rowid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO session_fts(rowid, body) VALUES (?1, ?2)",
                    params![rowid, session.body],
                )?;
            }
            IndexChange::Delete(path) => {
                delete_indexed_path(&tx, &path)?;
            }
        }
    }
    tx.commit()?;
    Ok(read_sqlite_meta(conn)?)
}

fn read_sqlite_meta(conn: &Connection) -> rusqlite::Result<HashMap<String, SearchMeta>> {
    let mut stmt = conn.prepare(
        "SELECT rowid, path, session_id, size, modified_secs, modified_nanos FROM session_meta",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(1)?,
            SearchMeta {
                rowid: row.get(0)?,
                session_id: row.get(2)?,
                fingerprint: SearchFileFingerprint {
                    size: row.get(3)?,
                    modified_secs: row.get(4)?,
                    modified_nanos: row.get(5)?,
                },
            },
        ))
    })?;

    let mut meta = HashMap::new();
    for row in rows {
        let (path, entry) = row?;
        meta.insert(path, entry);
    }
    Ok(meta)
}

fn delete_indexed_path(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM session_fts WHERE rowid IN (SELECT rowid FROM session_meta WHERE path = ?1)",
        params![path],
    )?;
    conn.execute("DELETE FROM session_meta WHERE path = ?1", params![path])?;
    Ok(())
}

/// Filter and score conversations based on query.
/// Returns indices into the original conversations vec, sorted by score descending.
pub fn search(
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
) -> Vec<usize> {
    if let Some(exact_query) = exact_syntax_query(query) {
        return search_exact(conversations, searchable, exact_query, now);
    }

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

/// Filter conversations whose normalized search text contains every query token exactly.
pub fn search_exact(
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    query: &str,
    _now: DateTime<Local>,
) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..conversations.len()).collect();
    }

    let query_lower = normalize_for_search(query);
    let query_words: Vec<String> = query_lower.split_whitespace().map(String::from).collect();
    if query_words.is_empty() {
        return (0..conversations.len()).collect();
    }

    let mut matches: Vec<(usize, DateTime<Local>)> = searchable
        .par_iter()
        .filter_map(|s| {
            if contains_exact_tokens(&s.text_lower, &query_words) {
                Some((s.index, conversations[s.index].timestamp))
            } else {
                None
            }
        })
        .collect();

    matches.sort_unstable_by_key(|matched| Reverse(matched.1));
    matches.into_iter().map(|(idx, _)| idx).collect()
}

fn contains_exact_tokens(text_lower: &str, query_words: &[String]) -> bool {
    query_words
        .iter()
        .all(|query_word| text_lower.split_whitespace().any(|word| word == query_word))
}

/// Search conversations through the full-context index.
pub fn search_full(
    conversations: &[Conversation],
    index: &FullSearchIndex,
    query: &str,
    now: DateTime<Local>,
) -> Vec<usize> {
    if let Some(exact_query) = exact_syntax_query(query) {
        return search_full_exact(conversations, index, exact_query, now);
    }

    match index {
        FullSearchIndex::Sqlite(index) => search_sqlite(conversations, index, query)
            .unwrap_or_else(|_| {
                let searchable = precompute_uncached_full_search_text(conversations);
                search(conversations, &searchable, query, now)
            }),
        FullSearchIndex::InMemory(searchable) => search(conversations, searchable, query, now),
    }
}

/// Search conversations through the full-context index using exact token matches.
pub fn search_full_exact(
    conversations: &[Conversation],
    index: &FullSearchIndex,
    query: &str,
    now: DateTime<Local>,
) -> Vec<usize> {
    match index {
        FullSearchIndex::Sqlite(index) => {
            let candidates =
                search_sqlite_exact(conversations, index, query).unwrap_or_else(|_| {
                    let searchable = precompute_uncached_full_search_text(conversations);
                    search_exact(conversations, &searchable, query, now)
                });
            filter_exact_full_transcript_matches(conversations, candidates, query)
        }
        FullSearchIndex::InMemory(searchable) => {
            let candidates = search_exact(conversations, searchable, query, now);
            filter_exact_full_transcript_matches(conversations, candidates, query)
        }
    }
}

fn filter_exact_full_transcript_matches(
    conversations: &[Conversation],
    candidates: Vec<usize>,
    query: &str,
) -> Vec<usize> {
    let Some(query_words) = exact_query_words(query) else {
        return candidates;
    };

    candidates
        .into_iter()
        .filter(|&idx| {
            let text_lower = full_transcript_text_lower(&conversations[idx]);
            contains_exact_tokens(&text_lower, &query_words)
        })
        .collect()
}

fn exact_query_words(query: &str) -> Option<Vec<String>> {
    let normalized = normalize_for_search(query);
    let query_words: Vec<String> = normalized.split_whitespace().map(String::from).collect();
    (!query_words.is_empty()).then_some(query_words)
}

fn exact_syntax_query(query: &str) -> Option<&str> {
    let query = query.trim();
    let exact_query = query.strip_prefix('\'')?.trim();
    (!exact_query.is_empty()).then_some(exact_query)
}

fn full_transcript_text_lower(conv: &Conversation) -> String {
    if !conv.full_text.is_empty() {
        return normalize_for_search(&conv.full_text);
    }

    let modified = std::fs::metadata(&conv.path)
        .and_then(|metadata| metadata.modified())
        .ok();
    hydrate_full_conversation(conv, modified)
        .map(|parsed| normalize_for_search(&parsed.full_text))
        .unwrap_or_default()
}

fn search_sqlite(
    conversations: &[Conversation],
    index: &SqliteSearchIndex,
    query: &str,
) -> rusqlite::Result<Vec<usize>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok((0..conversations.len()).collect());
    }

    let Some(match_query) = fts_match_query(query) else {
        return Ok((0..conversations.len()).collect());
    };

    let conn = Connection::open_with_flags(&index.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "
        SELECT rowid, bm25(session_fts) AS rank
        FROM session_fts
        WHERE session_fts MATCH ?1
        ORDER BY rank
        ",
    )?;
    let rows = stmt.query_map(params![match_query], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;

    let mut scored = Vec::new();
    for row in rows {
        let (rowid, rank) = row?;
        let Some(&idx) = index.rowid_to_index.get(&rowid) else {
            continue;
        };
        scored.push((idx, rank, conversations[idx].timestamp));
    }

    scored.sort_unstable_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
    });

    Ok(scored.into_iter().map(|(idx, _, _)| idx).collect())
}

fn search_sqlite_exact(
    conversations: &[Conversation],
    index: &SqliteSearchIndex,
    query: &str,
) -> rusqlite::Result<Vec<usize>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok((0..conversations.len()).collect());
    }

    let Some(match_query) = fts_exact_match_query(query) else {
        return Ok((0..conversations.len()).collect());
    };

    let conn = Connection::open_with_flags(&index.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "
        SELECT rowid, bm25(session_fts) AS rank
        FROM session_fts
        WHERE session_fts MATCH ?1
        ORDER BY rank
        ",
    )?;
    let rows = stmt.query_map(params![match_query], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;

    let mut scored = Vec::new();
    for row in rows {
        let (rowid, rank) = row?;
        let Some(&idx) = index.rowid_to_index.get(&rowid) else {
            continue;
        };
        scored.push((idx, rank, conversations[idx].timestamp));
    }

    scored.sort_unstable_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
    });

    Ok(scored.into_iter().map(|(idx, _, _)| idx).collect())
}

fn fts_match_query(query: &str) -> Option<String> {
    fts_match_query_with_suffix(query, "*")
}

fn fts_exact_match_query(query: &str) -> Option<String> {
    fts_match_query_with_suffix(query, "")
}

fn fts_match_query_with_suffix(query: &str, suffix: &str) -> Option<String> {
    let mut normalized = String::with_capacity(query.len());
    for ch in query.chars() {
        if ch.is_alphanumeric() {
            normalized.extend(ch.to_lowercase());
        } else {
            normalized.push(' ');
        }
    }

    let terms: Vec<String> = normalized
        .split_whitespace()
        .map(|term| format!("{term}{suffix}"))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
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
    use std::io::Seek;
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

    fn write_codex_jsonl(file: &mut NamedTempFile, lines: &[&str]) {
        file.as_file_mut().set_len(0).unwrap();
        file.as_file_mut().rewind().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.as_file_mut().sync_all().unwrap();
    }

    fn codex_jsonl(lines: &[&str]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        write_codex_jsonl(&mut file, lines);
        file
    }

    fn claude_jsonl(lines: &[&str]) -> NamedTempFile {
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
    fn exact_search_requires_whole_tokens() {
        let conversations = vec![
            conversation(
                SessionSource::Codex,
                PathBuf::from("first.jsonl"),
                "pup command",
                "",
            ),
            conversation(
                SessionSource::Codex,
                PathBuf::from("second.jsonl"),
                "puppet config",
                "",
            ),
        ];
        let searchable = precompute_search_text(&conversations);

        let fuzzy_results = search(&conversations, &searchable, "pup", Local::now());
        let exact_results = search_exact(&conversations, &searchable, "pup", Local::now());

        assert_eq!(fuzzy_results.len(), 2);
        assert!(fuzzy_results.contains(&0));
        assert!(fuzzy_results.contains(&1));
        assert_eq!(exact_results, vec![0]);
    }

    #[test]
    fn exact_search_can_use_query_syntax() {
        let conversations = vec![
            conversation(
                SessionSource::Codex,
                PathBuf::from("first.jsonl"),
                "pup command",
                "",
            ),
            conversation(
                SessionSource::Codex,
                PathBuf::from("second.jsonl"),
                "puppet config",
                "",
            ),
        ];
        let searchable = precompute_search_text(&conversations);

        assert_eq!(
            search(&conversations, &searchable, "'pup", Local::now()),
            vec![0]
        );
    }

    #[test]
    fn full_search_index_finds_codex_body_terms_with_sqlite_fts() {
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
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);
        let results = search_full(&conversations, &index, "needle", Local::now());

        assert_eq!(results, vec![0]);
        assert!(index_path.exists());
    }

    #[test]
    fn full_exact_search_uses_sqlite_without_prefix_matches() {
        let first = NamedTempFile::new().unwrap();
        let second = NamedTempFile::new().unwrap();
        let third = NamedTempFile::new().unwrap();
        let conversations = vec![
            conversation(
                SessionSource::Codex,
                first.path().to_path_buf(),
                "first",
                "pup command trace",
            ),
            conversation(
                SessionSource::Codex,
                second.path().to_path_buf(),
                "second",
                "puppet command trace",
            ),
            conversation(
                SessionSource::Codex,
                third.path().to_path_buf(),
                "pup metadata only",
                "unrelated command trace",
            ),
        ];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);

        let fuzzy_results = search_full(&conversations, &index, "pup", Local::now());
        assert_eq!(fuzzy_results.len(), 3);
        assert!(fuzzy_results.contains(&0));
        assert!(fuzzy_results.contains(&1));
        assert!(fuzzy_results.contains(&2));
        assert_eq!(
            search_full_exact(&conversations, &index, "pup", Local::now()),
            vec![0]
        );
    }

    #[test]
    fn full_exact_search_can_use_query_syntax() {
        let first = NamedTempFile::new().unwrap();
        let second = NamedTempFile::new().unwrap();
        let conversations = vec![
            conversation(
                SessionSource::Codex,
                first.path().to_path_buf(),
                "first",
                "pup command trace",
            ),
            conversation(
                SessionSource::Codex,
                second.path().to_path_buf(),
                "second",
                "puppet command trace",
            ),
        ];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);

        assert_eq!(
            search_full(&conversations, &index, "'pup", Local::now()),
            vec![0]
        );
    }

    #[test]
    fn full_search_index_hydrates_lightweight_claude_body_terms() {
        let file = claude_jsonl(&[
            r#"{"type":"user","timestamp":"2026-05-21T20:00:01Z","cwd":"/tmp/project","message":{"role":"user","content":"Visible preview"}}"#,
            r#"{"type":"assistant","timestamp":"2026-05-21T20:00:02Z","message":{"role":"assistant","model":"claude-sonnet-4-20250514","content":[{"type":"text","text":"Hidden needle context"}]}}"#,
        ]);
        let conversations = vec![conversation(
            SessionSource::Claude,
            file.path().to_path_buf(),
            "Visible preview",
            "",
        )];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);
        let results = search_full(&conversations, &index, "hidden needle", Local::now());

        assert_eq!(results, vec![0]);
    }

    #[test]
    fn full_search_index_reindexes_changed_files() {
        let mut file = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/project"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible preview"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"First needle context"}}"#,
        ]);
        let conversations = vec![conversation(
            SessionSource::Codex,
            file.path().to_path_buf(),
            "Visible preview",
            "",
        )];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);
        assert_eq!(
            search_full(&conversations, &index, "first needle", Local::now()),
            vec![0]
        );

        write_codex_jsonl(
            &mut file,
            &[
                r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/project"}}"#,
                r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible preview"}}"#,
                r#"{"timestamp":"2026-05-21T20:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"Second needle context with more bytes"}}"#,
            ],
        );

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);

        assert_eq!(
            search_full(&conversations, &index, "second needle", Local::now()),
            vec![0]
        );
        assert!(search_full(&conversations, &index, "first needle", Local::now()).is_empty());
    }

    #[test]
    fn full_search_index_keeps_rows_outside_current_filter_scope() {
        let first = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"first-session","cwd":"/tmp/project-a"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible first"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"Alpha needle context"}}"#,
        ]);
        let second = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"second-session","cwd":"/tmp/project-b"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible second"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"Beta needle context"}}"#,
        ]);
        let all_conversations = vec![
            conversation(
                SessionSource::Codex,
                first.path().to_path_buf(),
                "Visible first",
                "",
            ),
            conversation(
                SessionSource::Codex,
                second.path().to_path_buf(),
                "Visible second",
                "",
            ),
        ];
        let filtered_conversations = vec![all_conversations[0].clone()];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        precompute_full_search_index_with_db_path(&all_conversations, &index_path);
        precompute_full_search_index_with_db_path(&filtered_conversations, &index_path);

        let conn = Connection::open(&index_path).unwrap();
        let meta = read_sqlite_meta(&conn).unwrap();
        assert!(meta.contains_key(&first.path().to_string_lossy().to_string()));
        assert!(meta.contains_key(&second.path().to_string_lossy().to_string()));
    }
}
