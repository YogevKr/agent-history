mod claude;
mod claude_loader;
mod claude_parser;
mod cli;
mod codex;
mod codex_loader;
mod codex_parser;
mod display;
mod error;
mod history;
mod path;
mod resume;
mod search;

use crate::cli::{parse_duration_secs, Cli, SourceFilter};
use crate::display::{format_result, show_session};
use crate::history::{Conversation, SessionSource};
use crate::search::{precompute_search_text, search};
use chrono::Local;
use clap::Parser;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn run() -> error::Result<()> {
    let args = Cli::parse();

    // Load from both sources in parallel
    let (claude_result, codex_result) = rayon::join(
        || claude_loader::load_claude_sessions(),
        || codex_loader::load_codex_sessions(),
    );

    let mut conversations = claude_result.unwrap_or_default();
    conversations.extend(codex_result.unwrap_or_default());

    // Sort all by timestamp descending
    conversations.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    // Handle --show
    if let Some(ref id) = args.show {
        if let Some(conv) = conversations.iter().find(|c| c.session_id == *id) {
            show_session(conv);
            return Ok(());
        }
        return Err(error::AppError::SessionNotFound(id.clone()));
    }

    // Handle --resume
    if let Some(ref id) = args.resume {
        if let Some(conv) = conversations.iter().find(|c| c.session_id == *id) {
            return resume::resume_session(conv);
        }
        return Err(error::AppError::SessionNotFound(id.clone()));
    }

    // Apply filters
    let filtered = apply_filters(conversations, &args);

    // Search or list
    if let Some(ref query) = args.query {
        let searchable = precompute_search_text(&filtered);
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
