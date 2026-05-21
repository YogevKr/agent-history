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
mod history;
mod interactive;
mod path;
mod resume;
mod search;
mod syntax;
mod theme;
mod viewer;

use crate::cli::{parse_duration_secs, Cli, SourceFilter};
use crate::codex_loader::CodexLoadOptions;
use crate::display::format_result;
use crate::history::{compare_conversations, Conversation, SessionSource};
use crate::search::{precompute_full_search_text, search};
use chrono::Local;
use clap::Parser;

pub fn run() {
    if let Err(e) = run_inner() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn run_inner() -> error::Result<()> {
    let args = Cli::parse();

    let load_claude = !matches!(args.source, Some(SourceFilter::Codex));
    let load_codex = !matches!(args.source, Some(SourceFilter::Claude));
    let codex_options = CodexLoadOptions {
        include_full_text: false,
    };

    // Load requested sources in parallel. Codex rows stay lightweight here;
    // query mode hydrates full bodies through the persistent search cache.
    let (claude_result, codex_result) = rayon::join(
        || {
            if load_claude {
                claude_loader::load_claude_sessions()
            } else {
                Ok(Vec::new())
            }
        },
        || {
            if load_codex {
                if codex_options.include_full_text {
                    codex_loader::load_codex_sessions_with_options(codex_options)
                } else {
                    codex_loader::load_codex_sessions()
                }
            } else {
                Ok(Vec::new())
            }
        },
    );

    let mut conversations = claude_result.unwrap_or_default();
    conversations.extend(codex_result.unwrap_or_default());

    // Sort all by timestamp descending
    conversations.sort_by(compare_conversations);

    // Deduplicate by session_id (same session can appear in multiple project dirs)
    {
        let mut seen = std::collections::HashSet::new();
        conversations.retain(|c| seen.insert(c.session_id.clone()));
    }

    // Handle --show
    if let Some(ref id) = args.show {
        let conv = resolve_session(&conversations, id)?;
        return viewer::review_session(conv);
    }

    // Handle --resume
    if let Some(ref id) = args.resume {
        let conv = resolve_session(&conversations, id)?;
        return resume::resume_session(conv);
    }

    // Apply filters
    let filtered = apply_filters(conversations, &args);

    // Interactive mode: no query and no --list → fzf-style picker
    let is_interactive = args.query.is_none() && !args.list;
    if is_interactive && atty::is(atty::Stream::Stdout) {
        return interactive::run(filtered);
    }

    // Non-interactive: search or list to stdout
    if let Some(ref query) = args.query {
        let searchable = precompute_full_search_text(&filtered);
        let results = search(&filtered, &searchable, query, Local::now());
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

fn apply_filters(conversations: Vec<Conversation>, args: &Cli) -> Vec<Conversation> {
    let now = Local::now();
    let since_secs = args.since.as_ref().and_then(|s| parse_duration_secs(s));
    let current_dir = if args.local {
        std::env::current_dir().ok()
    } else {
        None
    };

    conversations
        .into_iter()
        .filter(|conv| {
            // Source filter
            if let Some(ref source) = args.source {
                match (source, conv.source) {
                    (SourceFilter::Claude, SessionSource::Claude) => {}
                    (SourceFilter::Codex, SessionSource::Codex) => {}
                    _ => return false,
                }
            }

            // Project filter
            if let Some(ref project) = args.project {
                let proj_lower = project.to_lowercase();
                let matches = conv
                    .project_name
                    .as_ref()
                    .map(|n| n.to_lowercase().contains(&proj_lower))
                    .unwrap_or(false);
                if !matches {
                    return false;
                }
            }

            // Since filter
            if let Some(secs) = since_secs {
                let age = now.signed_duration_since(conv.timestamp).num_seconds();
                if age > secs {
                    return false;
                }
            }

            // Local filter
            if let Some(ref cdir) = current_dir {
                let matches = conv.cwd.as_ref().map(|c| c == cdir).unwrap_or(false);
                if !matches {
                    return false;
                }
            }

            true
        })
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

    let mut matches = conversations
        .iter()
        .filter(|conv| conv.session_id.starts_with(id_or_prefix));

    match (matches.next(), matches.next()) {
        (Some(conv), None) => Ok(conv),
        (None, _) => Err(error::AppError::SessionNotFound(id_or_prefix.to_string())),
        (Some(_), Some(_)) => Err(error::AppError::SessionIdAmbiguous(
            id_or_prefix.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::path::PathBuf;

    fn conversation(session_id: &str) -> Conversation {
        Conversation {
            path: PathBuf::from(format!("{session_id}.jsonl")),
            source: SessionSource::Codex,
            session_id: session_id.to_string(),
            timestamp: Local::now(),
            preview: String::new(),
            full_text: String::new(),
            project_name: None,
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
