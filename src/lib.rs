mod claude;
mod claude_loader;
mod claude_parser;
mod cli;
mod codex;
mod codex_items;
mod codex_loader;
mod codex_parser;
mod display;
mod error;
mod export;
mod filters;
mod history;
mod interactive;
mod path;
mod resume;
mod search;
mod session_store;
mod syntax;
mod theme;
mod viewer;

use crate::cli::{parse_duration_secs, Cli, SearchGroup, SourceFilter};
use crate::display::{format_model_short, format_result, short_id, truncate};
use crate::filters::SessionFilters;
use crate::history::{Conversation, SessionSource};
use crate::search::{
    precompute_full_search_index, search_full, search_full_exact, search_index_path,
    search_message_hits, search_messages_for_conversation, MessageSearchHit, SearchMessage,
};
use crate::session_store::SessionStore;
use chrono::Local;
use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;
use std::any::Any;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub fn run() {
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        if !is_broken_pipe_panic(panic_info.payload()) {
            default_panic_hook(panic_info);
        }
    }));

    match std::panic::catch_unwind(run_inner) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
        Err(payload) if is_broken_pipe_panic(payload.as_ref()) => {}
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

fn is_broken_pipe_panic(payload: &(dyn Any + Send)) -> bool {
    panic_payload_message(payload).is_some_and(|message| {
        message.contains("failed printing to stdout") && message.contains("Broken pipe")
    })
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> Option<&str> {
    if let Some(message) = payload.downcast_ref::<String>() {
        Some(message.as_str())
    } else {
        payload.downcast_ref::<&str>().copied()
    }
}

fn run_inner() -> error::Result<()> {
    let args = Cli::parse();

    let initial_sources = initial_sources_from_args(&args);
    let store = load_initial_store(&initial_sources);
    let filters = filters_from_args(&args);

    if args.status {
        return print_status(&store, &args);
    }

    if args.doctor {
        return print_doctor(&store, &args);
    }

    // Handle --show
    if let Some(ref id) = args.show {
        let conv = resolve_session(store.conversations(), id)?;
        return viewer::review_session(conv);
    }

    // Handle --resume
    if let Some(ref id) = args.resume {
        let conv = resolve_session(store.conversations(), id)?;
        return resume::resume_session(conv);
    }

    if let Some(ref id) = args.export {
        let conv = resolve_session(store.conversations(), id)?;
        let md = export::to_markdown(conv)?;
        let out_dir = args.out.as_deref().unwrap_or(".");
        let path = export::export_to_dir(conv, &md, out_dir)?;
        println!("{}", path);
        return Ok(());
    }

    // Interactive mode: no query and no --list → fzf-style picker
    let is_interactive = args.query.is_none() && !args.list;
    if is_interactive && atty::is(atty::Stream::Stdout) {
        return interactive::run(store, filters);
    }

    // Non-interactive: search or list to stdout
    let filtered = filtered_conversations_for_output(store.conversations(), &filters);
    if let Some(ref query) = args.query {
        let index = precompute_full_search_index(&filtered);
        match args.group {
            SearchGroup::Sessions => {
                let results = if args.exact {
                    search_full_exact(&filtered, &index, query, Local::now())
                } else {
                    search_full(&filtered, &index, query, Local::now())
                };
                for &idx in results.iter().take(args.limit) {
                    println!("{}", format_result(&filtered[idx]));
                }
                if results.is_empty() {
                    eprintln!("No results found for '{}'", query);
                }
            }
            SearchGroup::Messages => {
                let hits = search_message_hits(
                    &filtered,
                    &index,
                    query,
                    args.scope,
                    args.exact,
                    Local::now(),
                );
                for hit in hits.iter().take(args.limit) {
                    print_message_hit(&filtered, hit, args.context);
                }
                if hits.is_empty() {
                    eprintln!("No message results found for '{}'", query);
                }
            }
        }
    } else {
        for conv in filtered.iter().take(args.limit) {
            println!("{}", format_result(conv));
        }
        if filtered.is_empty() {
            eprintln!("No sessions found");
        }
    }

    Ok(())
}

#[derive(Serialize)]
struct StatusReport {
    version: &'static str,
    total_sessions: usize,
    sources: BTreeMap<String, SourceReport>,
    cache: CacheReport,
}

#[derive(Serialize)]
struct SourceReport {
    path: String,
    exists: bool,
    sessions: usize,
}

#[derive(Serialize)]
struct CacheReport {
    path: Option<String>,
    exists: bool,
}

#[derive(Serialize)]
struct DoctorReport {
    status: StatusReport,
    ok: bool,
    checks: Vec<DoctorCheck>,
}

#[derive(Serialize)]
struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn print_status(store: &SessionStore, args: &Cli) -> error::Result<()> {
    let report = status_report(store);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("sessions: {}", report.total_sessions);
        for (source, report) in &report.sources {
            println!(
                "{}: {} sessions, path={}, exists={}",
                source, report.sessions, report.path, report.exists
            );
        }
        if let Some(path) = report.cache.path.as_deref() {
            println!("cache: {}, exists={}", path, report.cache.exists);
        }
    }
    Ok(())
}

fn print_doctor(store: &SessionStore, args: &Cli) -> error::Result<()> {
    let report = doctor_report(store);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("ok: {}", report.ok);
        for check in &report.checks {
            let status = if check.ok { "ok" } else { "fail" };
            println!("{}: {} ({})", check.name, status, check.detail);
        }
    }
    Ok(())
}

fn status_report(store: &SessionStore) -> StatusReport {
    let mut sources = BTreeMap::new();
    for source in [SessionSource::Claude, SessionSource::Codex] {
        let path = source_path(source);
        let sessions = store
            .conversations()
            .iter()
            .filter(|conversation| conversation.source == source)
            .count();
        sources.insert(
            source.to_string(),
            SourceReport {
                exists: path.exists(),
                path: path.to_string_lossy().to_string(),
                sessions,
            },
        );
    }

    let cache_path = search_index_path();
    let cache_exists = cache_path.as_ref().is_some_and(|path| path.exists());

    StatusReport {
        version: env!("CARGO_PKG_VERSION"),
        total_sessions: store.conversations().len(),
        sources,
        cache: CacheReport {
            path: cache_path.map(|path| path.to_string_lossy().to_string()),
            exists: cache_exists,
        },
    }
}

fn doctor_report(store: &SessionStore) -> DoctorReport {
    let status = status_report(store);
    let mut checks = Vec::new();

    for (source, source_report) in &status.sources {
        checks.push(DoctorCheck {
            name: "source_path",
            ok: source_report.exists,
            detail: format!("{source}: {}", source_report.path),
        });
    }

    checks.push(DoctorCheck {
        name: "sessions_found",
        ok: status.total_sessions > 0,
        detail: status.total_sessions.to_string(),
    });

    checks.push(DoctorCheck {
        name: "search_cache_path",
        ok: status.cache.path.is_some(),
        detail: status
            .cache
            .path
            .clone()
            .unwrap_or_else(|| "unavailable".to_string()),
    });

    checks.push(sqlite_fts_check());

    let ok = checks.iter().all(|check| check.ok);
    DoctorReport { status, ok, checks }
}

fn sqlite_fts_check() -> DoctorCheck {
    let ok = rusqlite::Connection::open_in_memory()
        .and_then(|conn| {
            conn.execute(
                "CREATE VIRTUAL TABLE agent_history_fts_check USING fts5(body)",
                [],
            )
        })
        .is_ok();
    DoctorCheck {
        name: "sqlite_fts5",
        ok,
        detail: if ok {
            "available".to_string()
        } else {
            "unavailable".to_string()
        },
    }
}

fn source_path(source: SessionSource) -> PathBuf {
    match source {
        SessionSource::Claude => {
            let claude_dir = std::env::var("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|_| {
                    home::home_dir()
                        .map(|home| home.join(".claude"))
                        .ok_or(std::env::VarError::NotPresent)
                })
                .unwrap_or_else(|_| PathBuf::from("~/.claude"));
            claude_dir.join("projects")
        }
        SessionSource::Codex => {
            let codex_dir = std::env::var("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|_| {
                    home::home_dir()
                        .map(|home| home.join(".codex"))
                        .ok_or(std::env::VarError::NotPresent)
                })
                .unwrap_or_else(|_| PathBuf::from("~/.codex"));
            codex_dir.join("sessions")
        }
    }
}

fn print_message_hit(conversations: &[Conversation], hit: &MessageSearchHit, context: usize) {
    let conv = &conversations[hit.conversation_index];
    let directory = crate::display::format_directory_label(conv);
    let model = format_model_short(conv.model.as_deref());
    println!(
        " [{}] {}  {:<20} ({}) {} #{} {}  \"{}\"",
        conv.source,
        conv.timestamp.format("%Y-%m-%d %H:%M"),
        truncate(&directory, 20),
        model,
        short_id(&conv.session_id),
        hit.message_index + 1,
        hit.role.as_str(),
        truncate(&hit.snippet, 160)
    );

    if context == 0 {
        return;
    }

    let messages = search_messages_for_conversation(conv);
    print_message_context(&messages, hit.message_index, context);
}

fn print_message_context(messages: &[SearchMessage], hit_index: usize, context: usize) {
    if messages.is_empty() {
        return;
    }
    let start = hit_index.saturating_sub(context);
    let end = (hit_index + context + 1).min(messages.len());
    for message in &messages[start..end] {
        let marker = if message.message_index == hit_index {
            ">"
        } else {
            " "
        };
        println!(
            "   {} #{:<4} {:<11} {}",
            marker,
            message.message_index + 1,
            message.role.as_str(),
            truncate(&compact_text(&message.text), 180)
        );
    }
}

fn compact_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn initial_sources_from_args(args: &Cli) -> Vec<SessionSource> {
    match args.source.as_ref() {
        Some(SourceFilter::Claude) => vec![SessionSource::Claude],
        Some(SourceFilter::Codex) => vec![SessionSource::Codex],
        None => vec![SessionSource::Claude, SessionSource::Codex],
    }
}

fn load_initial_store(sources: &[SessionSource]) -> SessionStore {
    load_initial_store_with(sources, load_initial_source)
}

fn load_initial_store_with(
    sources: &[SessionSource],
    load_source: impl Fn(SessionSource) -> error::Result<Vec<Conversation>> + Sync,
) -> SessionStore {
    let conversations = sources
        .par_iter()
        .flat_map(|source| load_source(*source).unwrap_or_default())
        .collect::<Vec<_>>();

    SessionStore::from_loaded(conversations, sources.iter().copied())
}

fn load_initial_source(source: SessionSource) -> error::Result<Vec<Conversation>> {
    match source {
        SessionSource::Claude => claude_loader::load_claude_sessions(),
        SessionSource::Codex => codex_loader::load_codex_sessions(),
    }
}

fn filters_from_args(args: &Cli) -> SessionFilters {
    let mut filters = match args.source.as_ref() {
        Some(SourceFilter::Claude) => SessionFilters::source_only(SessionSource::Claude),
        Some(SourceFilter::Codex) => SessionFilters::source_only(SessionSource::Codex),
        None => SessionFilters::all(),
    };

    if let Some(directory) = args.directory.as_ref() {
        filters.set_directory_contains(directory);
    }
    if let Some(since_secs) = args
        .since
        .as_ref()
        .and_then(|since| parse_duration_secs(since))
    {
        filters.set_since_secs(since_secs);
    }
    if args.local {
        if let Ok(cwd) = std::env::current_dir() {
            filters.set_local_cwd(cwd);
        }
    }

    filters
}

fn filtered_conversations_for_output(
    conversations: &[Conversation],
    filters: &SessionFilters,
) -> Vec<Conversation> {
    conversations
        .iter()
        .filter(|conversation| filters.matches(conversation))
        .cloned()
        .collect()
}

fn resolve_session<'a>(
    conversations: &'a [Conversation],
    id_or_prefix: &str,
) -> error::Result<&'a Conversation> {
    let id_or_prefix = id_or_prefix.trim();
    if id_or_prefix.is_empty() {
        return Err(error::AppError::SessionNotFound(id_or_prefix.to_string()));
    }

    if let Some(conv) = conversations
        .iter()
        .find(|conv| conv.session_id == id_or_prefix)
    {
        return Ok(conv);
    }

    if let Some(conv) = resolve_unique_session_by(conversations, id_or_prefix, |conv, query| {
        conv.session_id.starts_with(query)
    })? {
        return Ok(conv);
    }

    if let Some(conv) = resolve_unique_session_by(conversations, id_or_prefix, |conv, query| {
        short_id(&conv.session_id) == query
    })? {
        return Ok(conv);
    }

    Err(error::AppError::SessionNotFound(id_or_prefix.to_string()))
}

fn resolve_unique_session_by<'a>(
    conversations: &'a [Conversation],
    id: &str,
    matches: impl Fn(&Conversation, &str) -> bool,
) -> error::Result<Option<&'a Conversation>> {
    let mut matched = conversations.iter().filter(|conv| matches(conv, id));

    match (matched.next(), matched.next()) {
        (Some(conv), None) => Ok(Some(conv)),
        (None, _) => Ok(None),
        (Some(_), Some(_)) => Err(error::AppError::SessionIdAmbiguous(id.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Local};
    use std::path::PathBuf;

    fn conversation(session_id: &str) -> Conversation {
        conversation_with(session_id, SessionSource::Codex, None)
    }

    fn conversation_with(
        session_id: &str,
        source: SessionSource,
        directory_name: Option<&str>,
    ) -> Conversation {
        Conversation {
            path: PathBuf::from(format!("{session_id}.jsonl")),
            source,
            session_id: session_id.to_string(),
            timestamp: Local::now(),
            preview: String::new(),
            full_text: String::new(),
            directory_name: directory_name.map(str::to_string),
            cwd: None,
            message_count: 0,
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
            hierarchy_sort_timestamp: Local::now(),
        }
    }

    fn cli_with_filters(source: Option<SourceFilter>, directory: Option<&str>) -> Cli {
        Cli {
            query: None,
            source,
            directory: directory.map(str::to_string),
            since: None,
            limit: 20,
            group: SearchGroup::Sessions,
            scope: crate::search::SearchScope::Visible,
            context: 0,
            list: false,
            show: None,
            resume: None,
            export: None,
            out: None,
            local: false,
            exact: false,
            status: false,
            doctor: false,
            json: false,
        }
    }

    #[test]
    fn cli_source_initializes_filter_to_codex_only() {
        let args = cli_with_filters(Some(SourceFilter::Codex), None);
        let conversations = [
            conversation_with("codex", SessionSource::Codex, Some("agent-history")),
            conversation_with("claude", SessionSource::Claude, Some("agent-history")),
        ];

        let filters = filters_from_args(&args);

        assert!(filters.source_enabled(SessionSource::Codex));
        assert!(!filters.source_enabled(SessionSource::Claude));
        assert!(filters.matches(&conversations[0]));
        assert!(!filters.matches(&conversations[1]));
    }

    #[test]
    fn cli_directory_initializes_substring_directory_filter() {
        let args = cli_with_filters(None, Some("agent"));
        let conversations = [
            conversation_with("agent-history", SessionSource::Codex, Some("agent-history")),
            conversation_with("agent-tools", SessionSource::Claude, Some("Agent Tools")),
            conversation_with("other", SessionSource::Codex, Some("other")),
            conversation_with("unknown", SessionSource::Claude, None),
        ];

        let filters = filters_from_args(&args);

        assert!(filters.matches(&conversations[0]));
        assert!(filters.matches(&conversations[1]));
        assert!(!filters.matches(&conversations[2]));
        assert!(!filters.matches(&conversations[3]));
    }

    #[test]
    fn cli_directory_filter_matches_later_enabled_sources() {
        let args = cli_with_filters(Some(SourceFilter::Codex), Some("agent"));
        let later_loaded_claude =
            conversation_with("claude-agent", SessionSource::Claude, Some("agent-history"));

        let mut filters = filters_from_args(&args);
        assert!(!filters.matches(&later_loaded_claude));

        filters.set_source_enabled(SessionSource::Claude, true);
        assert!(filters.matches(&later_loaded_claude));
    }

    #[test]
    fn cli_directory_unknown_does_not_match_missing_directory_name() {
        let args = cli_with_filters(None, Some("unknown"));
        let conversations = [
            conversation_with(
                "unknown-directory",
                SessionSource::Codex,
                Some("unknown-directory"),
            ),
            conversation_with("missing-directory", SessionSource::Claude, None),
        ];

        let filters = filters_from_args(&args);

        assert!(filters.matches(&conversations[0]));
        assert!(!filters.matches(&conversations[1]));
    }

    #[test]
    fn initial_store_marks_failed_sources_loaded_and_keeps_successes() {
        let codex = conversation_with("codex", SessionSource::Codex, Some("agent-history"));
        let store =
            load_initial_store_with(&[SessionSource::Claude, SessionSource::Codex], |source| {
                match source {
                    SessionSource::Claude => Err(error::AppError::CliExecutionError(
                        "claude load failed".to_string(),
                    )),
                    SessionSource::Codex => Ok(vec![codex.clone()]),
                }
            });

        assert!(store.loaded_source(SessionSource::Claude));
        assert!(store.loaded_source(SessionSource::Codex));
        assert_eq!(store.conversations().len(), 1);
        assert_eq!(store.conversations()[0].session_id, "codex");
    }

    #[test]
    fn interactive_store_keeps_directory_filtered_rows_loaded() {
        let args = cli_with_filters(None, Some("agent"));
        let store = SessionStore::from_loaded(
            vec![
                conversation_with("agent-history", SessionSource::Codex, Some("agent-history")),
                conversation_with("other", SessionSource::Codex, Some("other")),
            ],
            [SessionSource::Codex],
        );
        let filters = filters_from_args(&args);

        let filtered = filtered_conversations_for_output(store.conversations(), &filters);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_id, "agent-history");
        assert_eq!(store.conversations().len(), 2);
    }

    #[test]
    fn cli_filters_include_local_and_since_for_interactive_store() {
        let current_dir = std::env::current_dir().unwrap();
        let mut local_recent =
            conversation_with("local-recent", SessionSource::Codex, Some("agent-history"));
        local_recent.cwd = Some(current_dir.clone());
        local_recent.timestamp = Local::now() - Duration::days(1);

        let mut local_old =
            conversation_with("local-old", SessionSource::Codex, Some("agent-history"));
        local_old.cwd = Some(current_dir.clone());
        local_old.timestamp = Local::now() - Duration::days(10);

        let mut other_recent =
            conversation_with("other-recent", SessionSource::Codex, Some("agent-history"));
        other_recent.cwd = Some(current_dir.join("other"));
        other_recent.timestamp = Local::now() - Duration::days(1);

        let mut args = cli_with_filters(None, None);
        args.local = true;
        args.since = Some("7d".to_string());
        let filters = filters_from_args(&args);
        let conversations = vec![local_recent, local_old, other_recent];

        let filtered = filtered_conversations_for_output(&conversations, &filters);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].session_id, "local-recent");
        assert_eq!(
            filters.filter_indices(&conversations, vec![0, 1, 2]),
            vec![0]
        );
    }

    #[test]
    fn resolve_session_accepts_exact_id() {
        let conversations = vec![conversation("abc12345-full")];

        let resolved = resolve_session(&conversations, "abc12345-full").unwrap();

        assert_eq!(resolved.session_id, "abc12345-full");
    }

    #[test]
    fn resolve_session_accepts_unique_short_prefix() {
        let conversations = vec![conversation("abc12345-full"), conversation("def67890-full")];

        let resolved = resolve_session(&conversations, "abc12345").unwrap();

        assert_eq!(resolved.session_id, "abc12345-full");
    }

    #[test]
    fn resolve_session_rejects_missing_prefix() {
        let conversations = vec![conversation("abc12345-full")];

        match resolve_session(&conversations, "missing") {
            Err(error::AppError::SessionNotFound(id)) => assert_eq!(id, "missing"),
            Err(err) => panic!("unexpected error: {err}"),
            Ok(conv) => panic!("unexpected session: {}", conv.session_id),
        }
    }

    #[test]
    fn resolve_session_rejects_ambiguous_prefix() {
        let conversations = vec![
            conversation("abc12345-full"),
            conversation("abc12345-other"),
        ];

        match resolve_session(&conversations, "abc12345") {
            Err(error::AppError::SessionIdAmbiguous(id)) => assert_eq!(id, "abc12345"),
            Err(err) => panic!("unexpected error: {err}"),
            Ok(conv) => panic!("unexpected session: {}", conv.session_id),
        }
    }
}
