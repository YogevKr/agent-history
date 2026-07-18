use crate::claude::{
    extract_search_text_from_user, extract_text_from_assistant, extract_text_from_user,
    ContentBlock, LogEntry,
};
use crate::claude_parser::process_claude_file;
use crate::codex_items::{codex_items, read_codex_lines, CodexItem, CodexRole};
use crate::codex_parser::process_codex_file;
use crate::history::{Conversation, SessionSource};
use chrono::{DateTime, Duration, Local};
use rayon::prelude::*;
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SEARCH_INDEX_FILE: &str = "search-index-v4.sqlite";
const MAX_INDEXED_MESSAGE_TEXT: usize = 64 * 1024;

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
    message_rowid_to_ref: HashMap<i64, IndexedMessageRef>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SearchScope {
    #[default]
    Visible,
    Transcript,
    Tools,
    Internal,
    All,
}

impl SearchScope {
    fn matches(self, role: SearchRole) -> bool {
        match self {
            SearchScope::Visible | SearchScope::Transcript => {
                matches!(role, SearchRole::User | SearchRole::Assistant)
            }
            SearchScope::Tools | SearchScope::Internal => {
                matches!(role, SearchRole::Tool | SearchRole::ToolOutput)
            }
            SearchScope::All => true,
        }
    }

    fn sql_filter(self) -> Option<&'static str> {
        match self {
            SearchScope::Visible | SearchScope::Transcript => {
                Some("message_meta.role in ('user', 'assistant')")
            }
            SearchScope::Tools | SearchScope::Internal => {
                Some("message_meta.role in ('tool', 'tool_output')")
            }
            SearchScope::All => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchRole {
    User,
    Assistant,
    Tool,
    ToolOutput,
}

impl SearchRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchRole::User => "user",
            SearchRole::Assistant => "assistant",
            SearchRole::Tool => "tool",
            SearchRole::ToolOutput => "tool_output",
        }
    }

    fn from_str(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" => Some(Self::Tool),
            "tool_output" => Some(Self::ToolOutput),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMessage {
    pub message_index: usize,
    pub role: SearchRole,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct MessageSearchHit {
    pub conversation_index: usize,
    pub message_index: usize,
    pub role: SearchRole,
    pub snippet: String,
    pub score: f64,
}

#[derive(Clone)]
struct IndexedMessageRef {
    conversation_index: usize,
    message_index: usize,
    role: SearchRole,
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

fn normalize_for_index_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for ch in text.chars() {
        if is_zero_width(ch) || ch == '\r' {
            continue;
        }
        if ch.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
                last_space = true;
            }
            continue;
        }
        out.extend(ch.to_lowercase());
        last_space = false;
    }
    out.trim().to_string()
}

fn is_zero_width(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
    )
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

fn full_search_index_body_with_modified(conv: &Conversation, modified: SystemTime) -> String {
    if !conv.full_text.is_empty() {
        return conversation_index_body(conv);
    }

    hydrate_full_conversation(conv, Some(modified))
        .map(|parsed| conversation_index_body(&parsed))
        .unwrap_or_else(|| conversation_index_body(conv))
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

fn conversation_index_body(conv: &Conversation) -> String {
    let mut text = conv.full_text.clone();
    let metadata = conversation_metadata_text(conv);
    if !metadata.is_empty() {
        text.push(' ');
        text.push_str(&metadata);
    }
    normalize_for_index_body(&text)
}

fn conversation_metadata_text(conv: &Conversation) -> String {
    let mut parts = Vec::new();
    if !conv.preview.is_empty() {
        parts.push(conv.preview.as_str());
    }
    if let Some(name) = conv.directory_name.as_deref() {
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

pub fn search_messages_for_conversation(conv: &Conversation) -> Vec<SearchMessage> {
    match conv.source {
        SessionSource::Claude => claude_search_messages(&conv.path).unwrap_or_default(),
        SessionSource::Codex => codex_search_messages(&conv.path).unwrap_or_default(),
    }
}

fn claude_search_messages(path: &Path) -> std::io::Result<Vec<SearchMessage>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines().map_while(std::result::Result::ok) {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(entry) = serde_json::from_str::<LogEntry>(&line) else {
            continue;
        };

        match entry {
            LogEntry::User {
                message,
                parent_tool_use_id,
                ..
            } => {
                let role = if parent_tool_use_id.is_some() {
                    SearchRole::ToolOutput
                } else {
                    SearchRole::User
                };
                let text = if parent_tool_use_id.is_some() {
                    extract_search_text_from_user(&message)
                } else {
                    extract_text_from_user(&message)
                };
                push_search_message(&mut messages, role, truncate_indexed_message_text(&text));
            }
            LogEntry::Assistant {
                message,
                parent_tool_use_id,
                ..
            } => {
                let role = if parent_tool_use_id.is_some() {
                    SearchRole::ToolOutput
                } else {
                    SearchRole::Assistant
                };
                push_search_message(
                    &mut messages,
                    role,
                    truncate_indexed_message_text(&extract_text_from_assistant(&message)),
                );

                for block in message.content {
                    match block {
                        ContentBlock::ToolUse { name, input, .. } => {
                            let text = if input.is_null() {
                                name
                            } else {
                                format!("{name} {input}")
                            };
                            push_search_message(
                                &mut messages,
                                SearchRole::Tool,
                                truncate_indexed_message_text(&text),
                            );
                        }
                        ContentBlock::ToolResult {
                            content: Some(content),
                            ..
                        } => {
                            push_search_message(
                                &mut messages,
                                SearchRole::ToolOutput,
                                truncate_indexed_message_text(&content.to_string()),
                            );
                        }
                        _ => {}
                    }
                }
            }
            LogEntry::Summary { summary } => {
                push_search_message(&mut messages, SearchRole::Assistant, summary);
            }
            LogEntry::CustomTitle { custom_title } => {
                push_search_message(&mut messages, SearchRole::Assistant, custom_title);
            }
            LogEntry::System { extra, subtype, .. } => {
                let text = if extra.is_null() {
                    subtype
                } else {
                    format!("{subtype} {extra}")
                };
                push_search_message(
                    &mut messages,
                    SearchRole::Tool,
                    truncate_indexed_message_text(&text),
                );
            }
            LogEntry::Progress {} | LogEntry::FileHistorySnapshot { .. } => {}
        }
    }

    Ok(messages)
}

fn codex_search_messages(path: &Path) -> std::io::Result<Vec<SearchMessage>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let lines = read_codex_lines(reader);
    let mut messages = Vec::new();

    for item in codex_items(&lines) {
        match item {
            CodexItem::Message { role, text } => {
                let role = match role {
                    CodexRole::User => SearchRole::User,
                    CodexRole::Assistant => SearchRole::Assistant,
                };
                push_search_message(&mut messages, role, truncate_indexed_message_text(&text));
            }
            CodexItem::ToolCall { name } => {
                push_search_message(&mut messages, SearchRole::Tool, name);
            }
            CodexItem::ToolOutput { output } => {
                push_search_message(
                    &mut messages,
                    SearchRole::ToolOutput,
                    truncate_indexed_message_text(&output),
                );
            }
        }
    }

    Ok(messages)
}

fn push_search_message(messages: &mut Vec<SearchMessage>, role: SearchRole, text: String) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }

    messages.push(SearchMessage {
        message_index: messages.len(),
        role,
        text: text.to_string(),
    });
}

fn truncate_indexed_message_text(text: &str) -> String {
    if text.len() <= MAX_INDEXED_MESSAGE_TEXT {
        return text.to_string();
    }

    let mut end = MAX_INDEXED_MESSAGE_TEXT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
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

pub fn search_index_path() -> Option<PathBuf> {
    default_search_index_path()
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

struct SearchMessageMeta {
    rowid: i64,
    path: String,
    message_index: usize,
    role: SearchRole,
}

struct IndexedSession {
    path: String,
    session_id: String,
    fingerprint: SearchFileFingerprint,
    body: String,
    messages: Vec<IndexedMessage>,
}

struct IndexedMessage {
    message_index: usize,
    role: SearchRole,
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
        ensure_private_cache_dir(parent)?;
    }

    let mut conn = Connection::open(db_path)?;
    set_private_file_permissions(db_path)?;
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
            tokenize = 'unicode61'
        );
        CREATE TABLE IF NOT EXISTS message_meta (
            path TEXT NOT NULL,
            message_index INTEGER NOT NULL,
            role TEXT NOT NULL,
            PRIMARY KEY(path, message_index)
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
            body,
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
    let message_rowid_to_ref = read_sqlite_message_meta(&conn)?
        .into_iter()
        .filter_map(|meta| {
            let conversation_index = *path_to_index.get(&meta.path)?;
            Some((
                meta.rowid,
                IndexedMessageRef {
                    conversation_index,
                    message_index: meta.message_index,
                    role: meta.role,
                },
            ))
        })
        .collect();

    Ok(FullSearchIndex::Sqlite(SqliteSearchIndex {
        db_path: db_path.to_path_buf(),
        rowid_to_index,
        message_rowid_to_ref,
    }))
}

fn ensure_private_cache_dir(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path)?;
    set_private_dir_permissions(path)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
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
                body: full_search_index_body_with_modified(conv, modified),
                messages: search_messages_for_conversation(conv)
                    .into_iter()
                    .filter(|message| SearchScope::All.matches(message.role))
                    .map(|message| IndexedMessage {
                        message_index: message.message_index,
                        role: message.role,
                        body: normalize_for_index_body(&message.text),
                    })
                    .collect(),
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
                for message in session.messages {
                    tx.execute(
                        "
                        INSERT INTO message_meta(path, message_index, role)
                        VALUES (?1, ?2, ?3)
                        ",
                        params![
                            &session.path,
                            message.message_index as i64,
                            message.role.as_str()
                        ],
                    )?;
                    let rowid = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO message_fts(rowid, body) VALUES (?1, ?2)",
                        params![rowid, message.body],
                    )?;
                }
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

fn read_sqlite_message_meta(conn: &Connection) -> rusqlite::Result<Vec<SearchMessageMeta>> {
    let mut stmt = conn.prepare(
        "SELECT rowid, path, message_index, role FROM message_meta ORDER BY path, message_index",
    )?;
    let rows = stmt.query_map([], |row| {
        let role: String = row.get(3)?;
        Ok(SearchMessageMeta {
            rowid: row.get(0)?,
            path: row.get(1)?,
            message_index: row.get::<_, i64>(2)?.try_into().unwrap_or(0),
            role: SearchRole::from_str(&role).unwrap_or(SearchRole::Tool),
        })
    })?;

    let mut meta = Vec::new();
    for row in rows {
        meta.push(row?);
    }
    Ok(meta)
}

fn delete_indexed_path(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM session_fts WHERE rowid IN (SELECT rowid FROM session_meta WHERE path = ?1)",
        params![path],
    )?;
    conn.execute("DELETE FROM session_meta WHERE path = ?1", params![path])?;
    conn.execute(
        "DELETE FROM message_fts WHERE rowid IN (SELECT rowid FROM message_meta WHERE path = ?1)",
        params![path],
    )?;
    conn.execute("DELETE FROM message_meta WHERE path = ?1", params![path])?;
    Ok(())
}

/// Filter conversations based on query.
/// Returns indices into the original conversations vec, newest first.
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

    // Keep all result surfaces chronological; relevance only breaks timestamp ties.
    scored.sort_unstable_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
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

pub fn search_message_hits(
    conversations: &[Conversation],
    index: &FullSearchIndex,
    query: &str,
    scope: SearchScope,
    exact: bool,
    now: DateTime<Local>,
) -> Vec<MessageSearchHit> {
    match index {
        FullSearchIndex::Sqlite(index) => {
            search_sqlite_messages(conversations, index, query, scope, exact).unwrap_or_else(|_| {
                search_message_hits_in_memory(conversations, query, scope, exact, now)
            })
        }
        FullSearchIndex::InMemory(_) => {
            search_message_hits_in_memory(conversations, query, scope, exact, now)
        }
    }
}

fn search_message_hits_in_memory(
    conversations: &[Conversation],
    query: &str,
    scope: SearchScope,
    exact: bool,
    now: DateTime<Local>,
) -> Vec<MessageSearchHit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }

    let normalized_query = normalize_for_search(query);
    let query_words: Vec<String> = normalized_query
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if query_words.is_empty() {
        return Vec::new();
    }

    let mut hits: Vec<(MessageSearchHit, DateTime<Local>)> = conversations
        .par_iter()
        .enumerate()
        .flat_map(|(conversation_index, conv)| {
            let query_words = query_words.clone();
            search_messages_for_conversation(conv)
                .into_iter()
                .filter(|message| scope.matches(message.role))
                .filter_map(move |message| {
                    let body = normalize_for_search(&message.text);
                    let matched = if exact {
                        contains_exact_tokens(&body, &query_words)
                    } else {
                        let words: Vec<&str> = query_words.iter().map(String::as_str).collect();
                        score_text(&body, &words, conv.timestamp, now) > 0.0
                    };
                    matched.then(|| {
                        (
                            MessageSearchHit {
                                conversation_index,
                                message_index: message.message_index,
                                role: message.role,
                                snippet: snippet_for_query(&message.text, &query_words),
                                score: 0.0,
                            },
                            conv.timestamp,
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();

    hits.sort_unstable_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.conversation_index.cmp(&b.0.conversation_index))
            .then_with(|| a.0.message_index.cmp(&b.0.message_index))
    });
    hits.into_iter().map(|(hit, _)| hit).collect()
}

fn search_sqlite_messages(
    conversations: &[Conversation],
    index: &SqliteSearchIndex,
    query: &str,
    scope: SearchScope,
    exact: bool,
) -> rusqlite::Result<Vec<MessageSearchHit>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let suffix = if exact { "" } else { "*" };
    let Some(plan) = fts_query_plan(query, suffix) else {
        return Ok(Vec::new());
    };
    let Some((mut where_sql, mut args, has_match)) = fts_where("message_fts", plan) else {
        return Ok(Vec::new());
    };
    if let Some(scope_sql) = scope.sql_filter() {
        where_sql.push_str(" AND ");
        where_sql.push_str(scope_sql);
    }
    let rank_expr = if has_match {
        "bm25(message_fts)"
    } else {
        "0.0"
    };

    let conn = Connection::open_with_flags(&index.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(&format!(
        "
        SELECT message_fts.rowid, {rank_expr} AS rank,
            snippet(message_fts, 0, '[', ']', '...', 16) AS snippet
        FROM message_fts
        JOIN message_meta ON message_meta.rowid = message_fts.rowid
        WHERE {where_sql}
        ORDER BY rank, message_fts.rowid
        "
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.drain(..)), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut hits = Vec::new();
    for row in rows {
        let (rowid, score, snippet) = row?;
        let Some(hit_ref) = index.message_rowid_to_ref.get(&rowid) else {
            continue;
        };
        hits.push(MessageSearchHit {
            conversation_index: hit_ref.conversation_index,
            message_index: hit_ref.message_index,
            role: hit_ref.role,
            snippet: clean_snippet(&snippet),
            score,
        });
    }

    hits.sort_unstable_by(|a, b| {
        conversations[b.conversation_index]
            .timestamp
            .cmp(&conversations[a.conversation_index].timestamp)
            .then_with(|| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.conversation_index.cmp(&b.conversation_index))
            .then_with(|| a.message_index.cmp(&b.message_index))
    });

    Ok(hits)
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

    let Some(plan) = fts_query_plan(query, "*") else {
        return Ok((0..conversations.len()).collect());
    };
    let Some((where_sql, args, has_match)) = fts_where("session_fts", plan) else {
        return Ok((0..conversations.len()).collect());
    };
    let rank_expr = if has_match {
        "bm25(session_fts)"
    } else {
        "0.0"
    };

    let conn = Connection::open_with_flags(&index.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(&format!(
        "
        SELECT rowid, {rank_expr} AS rank
        FROM session_fts
        WHERE {where_sql}
        ORDER BY rank, rowid
        "
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |row| {
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
        b.2.cmp(&a.2)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
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

    let Some(plan) = fts_query_plan(query, "") else {
        return Ok((0..conversations.len()).collect());
    };
    let Some((where_sql, args, has_match)) = fts_where("session_fts", plan) else {
        return Ok((0..conversations.len()).collect());
    };
    let rank_expr = if has_match {
        "bm25(session_fts)"
    } else {
        "0.0"
    };

    let conn = Connection::open_with_flags(&index.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(&format!(
        "
        SELECT rowid, {rank_expr} AS rank
        FROM session_fts
        WHERE {where_sql}
        ORDER BY rank, rowid
        "
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |row| {
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
        b.2.cmp(&a.2)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    Ok(scored.into_iter().map(|(idx, _, _)| idx).collect())
}

struct FtsQueryPlan {
    match_query: Option<String>,
    literal_terms: Vec<String>,
}

fn fts_query_plan(query: &str, suffix: &str) -> Option<FtsQueryPlan> {
    let (word_terms, literal_terms) = query_terms(query);
    if word_terms.is_empty() && literal_terms.is_empty() {
        return None;
    }

    let match_query = if word_terms.is_empty() {
        None
    } else {
        Some(
            word_terms
                .iter()
                .map(|term| quoted_fts_term(term, suffix))
                .collect::<Vec<_>>()
                .join(" AND "),
        )
    };

    Some(FtsQueryPlan {
        match_query,
        literal_terms,
    })
}

fn query_terms(query: &str) -> (Vec<String>, Vec<String>) {
    let mut word_terms = Vec::new();
    let mut literal_terms = Vec::new();
    let normalized_query = normalize_for_index_body(query);

    for term in normalized_query.split_whitespace() {
        let normalized = normalize_for_search(term);
        let words: Vec<String> = normalized
            .split_whitespace()
            .filter(|word| word.chars().any(char::is_alphanumeric))
            .map(str::to_string)
            .collect();
        if words.is_empty() {
            literal_terms.push(term.to_string());
        } else {
            word_terms.extend(words);
        }
    }

    (word_terms, literal_terms)
}

fn quoted_fts_term(term: &str, suffix: &str) -> String {
    format!("\"{}\"{}", term.replace('"', "\"\""), suffix)
}

fn fts_where(table: &str, plan: FtsQueryPlan) -> Option<(String, Vec<String>, bool)> {
    let mut clauses = Vec::new();
    let mut args = Vec::new();
    let mut has_match = false;

    if let Some(match_query) = plan.match_query {
        clauses.push(format!("{table} MATCH ?"));
        args.push(match_query);
        has_match = true;
    }

    for term in plan.literal_terms {
        clauses.push(format!("{table}.body LIKE ? ESCAPE '\\'"));
        args.push(format!("%{}%", escape_like(&term)));
    }

    if clauses.is_empty() {
        None
    } else {
        Some((clauses.join(" AND "), args, has_match))
    }
}

fn escape_like(term: &str) -> String {
    let mut escaped = String::with_capacity(term.len());
    for ch in term.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn clean_snippet(snippet: &str) -> String {
    snippet
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn snippet_for_query(text: &str, query_words: &[String]) -> String {
    let compact = clean_snippet(text);
    if compact.is_empty() {
        return String::new();
    }

    let lower = compact.to_lowercase();
    let first_match = query_words
        .iter()
        .filter(|word| !word.is_empty())
        .filter_map(|word| lower.find(word))
        .min()
        .unwrap_or(0);

    let start = compact[..first_match]
        .char_indices()
        .rev()
        .nth(40)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let end = compact[first_match..]
        .char_indices()
        .nth(120)
        .map(|(idx, _)| first_match + idx)
        .unwrap_or(compact.len());

    let mut snippet = String::new();
    if start > 0 {
        snippet.push_str("...");
    }
    snippet.push_str(&compact[start..end]);
    if end < compact.len() {
        snippet.push_str("...");
    }
    snippet
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
            directory_name: Some("directory".to_string()),
            cwd: None,
            message_count: 1,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: None,
            hierarchy_root_id: None,
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
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/directory"}}"#,
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
    fn full_search_results_are_newest_first_even_when_older_match_ranks_higher() {
        let now = Local::now();
        let older_file = NamedTempFile::new().unwrap();
        let newer_file = NamedTempFile::new().unwrap();
        let mut older = conversation(
            SessionSource::Codex,
            older_file.path().to_path_buf(),
            "needle needle needle needle needle",
            "needle needle needle needle needle",
        );
        older.session_id = "older".to_string();
        older.timestamp = now - Duration::days(2);
        older.hierarchy_sort_timestamp = older.timestamp;
        let mut newer = conversation(
            SessionSource::Codex,
            newer_file.path().to_path_buf(),
            "needle",
            "needle",
        );
        newer.session_id = "newer".to_string();
        newer.timestamp = now - Duration::minutes(1);
        newer.hierarchy_sort_timestamp = newer.timestamp;
        let conversations = vec![older, newer];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);

        assert_eq!(
            search_full(&conversations, &index, "needle", now),
            vec![1, 0]
        );
    }

    #[test]
    fn full_search_quotes_fts_reserved_terms() {
        let file = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/directory"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"AND OR NOT NEAR are literal words"}}"#,
        ]);
        let conversations = vec![conversation(
            SessionSource::Codex,
            file.path().to_path_buf(),
            "reserved words",
            "",
        )];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);

        assert_eq!(
            search_full(&conversations, &index, "AND OR NOT NEAR", Local::now()),
            vec![0]
        );
    }

    #[test]
    fn full_search_uses_literal_fallback_for_symbol_terms() {
        let file = NamedTempFile::new().unwrap();
        let conversations = vec![conversation(
            SessionSource::Codex,
            file.path().to_path_buf(),
            "symbols",
            "Wildcard marker * appears here",
        )];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);

        assert_eq!(
            search_full(&conversations, &index, "*", Local::now()),
            vec![0]
        );
    }

    #[test]
    fn message_search_scope_separates_transcript_from_tools() {
        let file = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/directory"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible transcript"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:02Z","type":"response_item","payload":{"type":"function_call","name":"shell"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:03Z","type":"response_item","payload":{"type":"function_call_output","output":"hidden-tool-needle"}}"#,
        ]);
        let conversations = vec![conversation(
            SessionSource::Codex,
            file.path().to_path_buf(),
            "Visible transcript",
            "",
        )];
        let cache_dir = tempdir().unwrap();
        let index_path = cache_dir.path().join("search-index.sqlite");

        let index = precompute_full_search_index_with_db_path(&conversations, &index_path);
        let visible = search_message_hits(
            &conversations,
            &index,
            "hidden-tool-needle",
            SearchScope::Visible,
            false,
            Local::now(),
        );
        let tools = search_message_hits(
            &conversations,
            &index,
            "hidden-tool-needle",
            SearchScope::Tools,
            false,
            Local::now(),
        );

        assert!(visible.is_empty());
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].role, SearchRole::ToolOutput);
        assert!(tools[0].snippet.contains("hidden"));
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
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/directory"}}"#,
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
                r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"session-id","cwd":"/tmp/directory"}}"#,
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
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"first-session","cwd":"/tmp/directory-a"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Visible first"}}"#,
            r#"{"timestamp":"2026-05-21T20:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"Alpha needle context"}}"#,
        ]);
        let second = codex_jsonl(&[
            r#"{"timestamp":"2026-05-21T20:00:00Z","type":"session_meta","payload":{"id":"second-session","cwd":"/tmp/directory-b"}}"#,
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
