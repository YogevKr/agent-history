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

use crate::cli::{parse_duration_secs, Cli, SourceFilter};
use crate::display::{format_result, short_id};
use crate::filters::SessionFilters;
use crate::history::{Conversation, SessionSource};
use crate::search::{precompute_full_search_index, search_full, search_full_exact};
use crate::session_store::SessionStore;
use chrono::Local;
use clap::Parser;
use rayon::prelude::*;
use std::any::Any;

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

    // Interactive mode: no query and no --list → fzf-style picker
    let is_interactive = args.query.is_none() && !args.list;
    if is_interactive && atty::is(atty::Stream::Stdout) {
        return interactive::run(store, filters);
    }

    // Non-interactive: search or list to stdout
    let filtered = filtered_conversations_for_output(store.conversations(), &filters);
    if let Some(ref query) = args.query {
        let index = precompute_full_search_index(&filtered);
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
            list: false,
            show: None,
            resume: None,
            local: false,
            exact: false,
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
