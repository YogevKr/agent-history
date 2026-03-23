//! Syntect-based syntax highlighting for code blocks.

use crate::theme::theme;
use std::sync::OnceLock;
use syntect::highlighting::{ThemeSet, Style};
use syntect::parsing::SyntaxSet;
use syntect::easy::HighlightLines;
use syntect::util::LinesWithEndings;

struct SyntectState {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
}

static STATE: OnceLock<SyntectState> = OnceLock::new();

fn state() -> &'static SyntectState {
    STATE.get_or_init(|| SyntectState {
        syntax_set: SyntaxSet::load_defaults_newlines(),
        theme_set: ThemeSet::load_defaults(),
    })
}

/// A single highlighted token for TUI rendering.
pub struct HighlightedToken {
    pub text: String,
    pub fg: (u8, u8, u8),
    pub bold: bool,
}

/// Normalize common language aliases to syntect-recognized names.
fn normalize_lang(lang: &str) -> Option<&'static str> {
    match lang.trim().to_lowercase().as_str() {
        "js" | "jsx" => Some("JavaScript"),
        "ts" | "tsx" => Some("TypeScript"),
        "sh" | "shell" | "zsh" => Some("Bash"),
        "py" => Some("Python"),
        "rb" => Some("Ruby"),
        "rs" => Some("Rust"),
        "yml" => Some("YAML"),
        "md" | "markdown" => Some("Markdown"),
        "tf" | "hcl" => Some("HCL"),
        "dockerfile" => Some("Dockerfile"),
        "proto" | "protobuf" => Some("Protocol Buffers"),
        _ => None,
    }
}

fn find_syntax<'a>(st: &'a SyntectState, lang: &str) -> Option<&'a syntect::parsing::SyntaxReference> {
    if let Some(normalized) = normalize_lang(lang) {
        if let Some(syn) = st.syntax_set.find_syntax_by_token(normalized) {
            return Some(syn);
        }
    }
    st.syntax_set
        .find_syntax_by_token(lang)
        .or_else(|| st.syntax_set.find_syntax_by_extension(lang))
}

/// Highlight code for TUI pager — returns styled tokens per line.
pub fn highlight_code_tui(code: &str, lang: &str) -> Option<Vec<Vec<HighlightedToken>>> {
    let st = state();
    let syntax = find_syntax(st, lang)?;
    let theme_name = theme().syntect_theme;
    let syntect_theme = st.theme_set.themes.get(theme_name)?;

    let mut highlighter = HighlightLines::new(syntax, syntect_theme);
    let mut result = Vec::new();

    for line in LinesWithEndings::from(code) {
        let ranges: Vec<(Style, &str)> = highlighter.highlight_line(line, &st.syntax_set).ok()?;
        let tokens: Vec<HighlightedToken> = ranges
            .into_iter()
            .map(|(style, text)| {
                let fg = style.foreground;
                HighlightedToken {
                    text: text.trim_end_matches('\n').to_string(),
                    fg: (fg.r, fg.g, fg.b),
                    bold: false,
                }
            })
            .filter(|t| !t.text.is_empty())
            .collect();
        result.push(tokens);
    }

    Some(result)
}

/// Highlight code for stdout — returns ANSI-escaped string.
pub fn highlight_code_ansi(code: &str, lang: &str) -> Option<String> {
    let st = state();
    let syntax = find_syntax(st, lang)?;
    let theme_name = theme().syntect_theme;
    let syntect_theme = st.theme_set.themes.get(theme_name)?;

    let mut highlighter = HighlightLines::new(syntax, syntect_theme);
    let mut output = String::new();

    for line in LinesWithEndings::from(code) {
        let ranges: Vec<(Style, &str)> = highlighter.highlight_line(line, &st.syntax_set).ok()?;
        for (style, text) in ranges {
            let fg = style.foreground;
            output.push_str(&format!("\x1b[38;2;{};{};{}m{}\x1b[0m", fg.r, fg.g, fg.b, text));
        }
    }

    Some(output)
}
