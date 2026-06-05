use clap::Parser;

use crate::search::SearchScope;

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum SourceFilter {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SearchGroup {
    Sessions,
    Messages,
}

/// Unified search across Claude Code and Codex CLI session history
#[derive(Parser, Debug)]
#[command(name = "agent-history", version, about)]
pub struct Cli {
    /// Search query (fuzzy matches across session content)
    pub query: Option<String>,

    /// Match exact normalized tokens instead of fuzzy prefixes
    #[arg(long, short = 'x')]
    pub exact: bool,

    /// Filter by session source
    #[arg(long, value_enum)]
    pub source: Option<SourceFilter>,

    /// Filter by directory/cwd name
    #[arg(long, alias = "project")]
    pub directory: Option<String>,

    /// Filter by time (e.g. "7d", "2w", "1m")
    #[arg(long)]
    pub since: Option<String>,

    /// Max results (default: 20)
    #[arg(long, default_value = "20")]
    pub limit: usize,

    /// Group search output by sessions or individual messages
    #[arg(long, value_enum, default_value = "sessions")]
    pub group: SearchGroup,

    /// Search scope for message grouped output
    #[arg(long, value_enum, default_value = "visible")]
    pub scope: SearchScope,

    /// Neighboring messages to include around each message hit
    #[arg(long, default_value = "0")]
    pub context: usize,

    /// List all sessions (no search)
    #[arg(long)]
    pub list: bool,

    /// Show full session content by full ID or unique prefix
    #[arg(long)]
    pub show: Option<String>,

    /// Resume session in its CLI by full ID or unique prefix
    #[arg(long)]
    pub resume: Option<String>,

    /// Export a session as Markdown by full ID or unique prefix
    #[arg(long)]
    pub export: Option<String>,

    /// Output directory for --export
    #[arg(long)]
    pub out: Option<String>,

    /// Only sessions from current directory
    #[arg(long)]
    pub local: bool,

    /// Print source/cache status and exit
    #[arg(long)]
    pub status: bool,

    /// Run health checks and exit
    #[arg(long)]
    pub doctor: bool,

    /// Emit machine-readable JSON where supported
    #[arg(long)]
    pub json: bool,
}

/// Parse a duration string like "7d", "2w", "1m" into seconds
pub fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: i64 = num_str.parse().ok()?;

    match unit {
        "s" => Some(num),
        "m" => Some(num * 60),
        "h" => Some(num * 3600),
        "d" => Some(num * 86400),
        "w" => Some(num * 604800),
        "M" => Some(num * 2592000), // ~30 days
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_secs_days() {
        assert_eq!(parse_duration_secs("7d"), Some(604800));
    }

    #[test]
    fn parse_duration_secs_weeks() {
        assert_eq!(parse_duration_secs("2w"), Some(1209600));
    }

    #[test]
    fn parse_duration_secs_invalid() {
        assert_eq!(parse_duration_secs("abc"), None);
    }
}
