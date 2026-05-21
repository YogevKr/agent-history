//! fzf-like interactive session picker with in-TUI session viewer.

use crate::display::{
    format_hierarchy_marker, format_model_short, format_project_label, format_relative_time,
    get_display_title, short_id, truncate, HIERARCHY_GUTTER_WIDTH,
};
use crate::history::{Conversation, SessionSource};
use crate::search::{
    precompute_full_search_index, precompute_search_text, search, search_full, FullSearchIndex,
    SearchableConversation,
};
use crate::viewer::{self, Span, StyledLine};
use chrono::Local;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, ClearType},
};
use std::collections::HashSet;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use unicode_width::UnicodeWidthChar;

const PICKER_HINT: &str = "  Enter: view  Tab: expand/collapse  \u{2190}: copy ID";

/// Run interactive session picker. Returns Ok(()) on clean exit.
pub fn run(conversations: Vec<Conversation>) -> crate::error::Result<()> {
    if conversations.is_empty() {
        eprintln!("No sessions found");
        return Ok(());
    }

    let expanded_tree_roots = HashSet::new();
    let filtered_indices = collapse_visible_indices(
        &conversations,
        (0..conversations.len()).collect(),
        &expanded_tree_roots,
        true,
    );

    let mut state = PickerState {
        query: String::new(),
        selected: 0,
        filtered_indices,
        searchable: precompute_search_text(&conversations),
        full_search_index: None,
        expanded_tree_roots,
        flash: None,
    };

    terminal::enable_raw_mode().map_err(crate::error::AppError::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)
        .map_err(crate::error::AppError::Io)?;

    let result = main_loop(&mut stdout, &conversations, &mut state);

    // Always restore terminal
    let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();

    if let Err(e) = result {
        return Err(e);
    }
    Ok(())
}

struct PickerState {
    query: String,
    selected: usize,
    filtered_indices: Vec<usize>,
    searchable: Vec<SearchableConversation>,
    full_search_index: Option<FullSearchIndex>,
    expanded_tree_roots: HashSet<String>,
    flash: Option<String>,
}

fn main_loop(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    state: &mut PickerState,
) -> crate::error::Result<()> {
    loop {
        let idx = match picker_loop(stdout, conversations, state) {
            PickerAction::ViewSession(idx) => {
                let viewer_query = state.query.clone();
                match pager_loop(stdout, &conversations[idx], &viewer_query)? {
                    PagerAction::Back => continue,
                    PagerAction::CopyId => idx,
                    PagerAction::Resume => {
                        let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
                        let _ = terminal::disable_raw_mode();
                        return crate::resume::resume_session(&conversations[idx]);
                    }
                    PagerAction::CopyConversation => {
                        match crate::export::to_markdown(&conversations[idx]) {
                            Ok(md) => {
                                if crate::export::copy_to_clipboard(&md).is_ok() {
                                    state.flash =
                                        Some("Copied conversation to clipboard".to_string());
                                } else {
                                    state.flash = Some("Failed to copy to clipboard".to_string());
                                }
                            }
                            Err(_) => {
                                state.flash = Some("Failed to export conversation".to_string());
                            }
                        }
                        continue;
                    }
                    PagerAction::ExportFile => {
                        match crate::export::to_markdown(&conversations[idx]) {
                            Ok(md) => match crate::export::export_to_file(&conversations[idx], &md)
                            {
                                Ok(filename) => {
                                    state.flash = Some(format!("Exported to ./{}", filename));
                                }
                                Err(_) => {
                                    state.flash = Some("Failed to write file".to_string());
                                }
                            },
                            Err(_) => {
                                state.flash = Some("Failed to export conversation".to_string());
                            }
                        }
                        continue;
                    }
                }
            }
            PickerAction::CopyId(idx) => idx,
            PickerAction::Quit => return Ok(()),
        };
        let id = &conversations[idx].session_id;
        let _ = copy_to_clipboard(id);
        state.flash = Some(format!("Copied: {}", id));
    }
}

enum PickerAction {
    ViewSession(usize),
    CopyId(usize),
    Quit,
}

#[derive(Debug, Eq, PartialEq)]
enum PickerKeyAction {
    Quit,
    ViewSession,
    MoveUp,
    MoveDown,
    Backspace,
    ToggleTreeExpansion,
    CopyId,
    Type(char),
    Ignore,
}

fn picker_key_action(evt: &Event) -> PickerKeyAction {
    let Event::Key(key) = evt else {
        return PickerKeyAction::Ignore;
    };

    if key.kind != KeyEventKind::Press {
        return PickerKeyAction::Ignore;
    }

    match *key {
        KeyEvent {
            code: KeyCode::Esc, ..
        }
        | KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => PickerKeyAction::Quit,
        KeyEvent {
            code: KeyCode::Enter,
            ..
        } => PickerKeyAction::ViewSession,
        KeyEvent {
            code: KeyCode::Up, ..
        }
        | KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => PickerKeyAction::MoveUp,
        KeyEvent {
            code: KeyCode::Down,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => PickerKeyAction::MoveDown,
        KeyEvent {
            code: KeyCode::Backspace,
            ..
        } => PickerKeyAction::Backspace,
        KeyEvent {
            code: KeyCode::Tab, ..
        } => PickerKeyAction::ToggleTreeExpansion,
        KeyEvent {
            code: KeyCode::Left,
            ..
        } => PickerKeyAction::CopyId,
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers,
            ..
        } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => PickerKeyAction::Type(c),
        _ => PickerKeyAction::Ignore,
    }
}

// ── Picker (list view) ────────────────────────────────

fn picker_loop(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    state: &mut PickerState,
) -> PickerAction {
    loop {
        if let Err(_) = draw_picker(
            stdout,
            conversations,
            &state.filtered_indices,
            &state.query,
            &state.expanded_tree_roots,
            state.selected,
            state.flash.as_deref(),
        ) {
            return PickerAction::Quit;
        }
        state.flash = None;

        let evt = match event::read() {
            Ok(e) => e,
            Err(_) => return PickerAction::Quit,
        };

        match picker_key_action(&evt) {
            PickerKeyAction::Quit => return PickerAction::Quit,
            PickerKeyAction::ViewSession => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::ViewSession(idx);
                }
            }
            PickerKeyAction::MoveUp => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            PickerKeyAction::MoveDown => {
                if state.selected + 1 < state.filtered_indices.len() {
                    state.selected += 1;
                }
            }
            PickerKeyAction::Backspace => {
                state.query.pop();
                refilter(conversations, state);
            }
            PickerKeyAction::ToggleTreeExpansion => toggle_tree_expansion(conversations, state),
            PickerKeyAction::CopyId => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::CopyId(idx);
                }
            }
            PickerKeyAction::Type(c) => {
                state.query.push(c);
                refilter(conversations, state);
            }
            PickerKeyAction::Ignore => {}
        }
    }
}

fn refilter(conversations: &[Conversation], state: &mut PickerState) {
    if !state.query.is_empty() && state.full_search_index.is_none() {
        state.full_search_index = Some(precompute_full_search_index(conversations));
        state.flash = Some("Indexed full context".to_string());
    }

    let base_indices = if state.query.is_empty() {
        (0..conversations.len()).collect()
    } else if let Some(index) = state.full_search_index.as_ref() {
        search_full(conversations, index, &state.query, Local::now())
    } else {
        search(conversations, &state.searchable, &state.query, Local::now())
    };
    state.filtered_indices = collapse_visible_indices(
        conversations,
        base_indices,
        &state.expanded_tree_roots,
        state.query.is_empty(),
    );
    if state.selected >= state.filtered_indices.len() {
        state.selected = state.filtered_indices.len().saturating_sub(1);
    }
}

fn toggle_tree_expansion(conversations: &[Conversation], state: &mut PickerState) {
    if state.filtered_indices.is_empty() {
        return;
    }

    let selected_index = state.filtered_indices[state.selected];
    let Some(root_id) = tree_root_id(conversations, selected_index) else {
        state.flash = Some("No tree on this row".to_string());
        return;
    };

    if !state.expanded_tree_roots.remove(&root_id) {
        state.expanded_tree_roots.insert(root_id.clone());
    }

    let selected_session_id = conversations[selected_index].session_id.clone();
    refilter(conversations, state);

    if let Some(position) = state
        .filtered_indices
        .iter()
        .position(|idx| conversations[*idx].session_id == selected_session_id)
    {
        state.selected = position;
    } else if let Some(position) = state
        .filtered_indices
        .iter()
        .position(|idx| conversations[*idx].session_id == root_id)
    {
        state.selected = position;
    }
}

fn tree_root_id(conversations: &[Conversation], index: usize) -> Option<String> {
    let conversation = conversations.get(index)?;
    if conversation.hierarchy_depth == 0 {
        return conversation
            .hierarchy_has_children
            .then(|| conversation.session_id.clone());
    }

    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        let candidate = &conversations[cursor];
        if candidate.hierarchy_depth == 0 {
            return candidate
                .hierarchy_has_children
                .then(|| candidate.session_id.clone());
        }
    }

    None
}

fn collapse_visible_indices(
    conversations: &[Conversation],
    base_indices: Vec<usize>,
    expanded_tree_roots: &HashSet<String>,
    collapse_enabled: bool,
) -> Vec<usize> {
    if !collapse_enabled {
        return base_indices;
    }

    let mut visible = Vec::with_capacity(base_indices.len());
    let mut current_tree_expanded = false;
    let mut current_tree_root = false;

    for index in base_indices {
        let conversation = &conversations[index];
        if conversation.hierarchy_depth == 0 {
            current_tree_root = conversation.hierarchy_has_children;
            current_tree_expanded =
                current_tree_root && expanded_tree_roots.contains(&conversation.session_id);
            visible.push(index);
        } else if current_tree_expanded || !current_tree_root {
            visible.push(index);
        }
    }

    visible
}

fn draw_picker(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    filtered_indices: &[usize],
    query: &str,
    expanded_tree_roots: &HashSet<String>,
    selected: usize,
    flash: Option<&str>,
) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let cols = cols as usize;
    let rows = rows as usize;

    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All)
    )?;

    // Line 0: search prompt
    execute!(
        stdout,
        SetForegroundColor(Color::Yellow),
        SetAttribute(Attribute::Bold),
        Print("> "),
        ResetColor,
        Print(query),
    )?;

    // Line 1: match count + hint + flash
    let count = format!("  {}/{}", filtered_indices.len(), conversations.len());
    let hint = PICKER_HINT;
    let flash_text = flash.unwrap_or("");
    let gap = cols.saturating_sub(count.len() + hint.len() + flash_text.len() + 2);
    execute!(
        stdout,
        cursor::MoveTo(0, 1),
        SetForegroundColor(Color::DarkGrey),
        Print(&count),
        Print(hint),
        ResetColor,
    )?;
    if !flash_text.is_empty() {
        execute!(
            stdout,
            Print(" ".repeat(gap)),
            SetForegroundColor(Color::Green),
            Print(flash_text),
            ResetColor,
        )?;
    }

    // Lines 2..rows: session list
    let list_start = 2usize;
    let visible = rows.saturating_sub(list_start);

    let scroll = if selected >= visible {
        selected - visible + 1
    } else {
        0
    };

    for i in 0..visible {
        let list_idx = scroll + i;
        if list_idx >= filtered_indices.len() {
            break;
        }
        let conv = &conversations[filtered_indices[list_idx]];
        let is_selected = list_idx == selected;

        execute!(stdout, cursor::MoveTo(0, (list_start + i) as u16))?;

        if is_selected {
            execute!(stdout, SetAttribute(Attribute::Reverse))?;
        }

        draw_session_line(
            stdout,
            conv,
            cols,
            is_selected,
            expanded_tree_roots,
            query.is_empty(),
        )?;

        if is_selected {
            execute!(stdout, SetAttribute(Attribute::NoReverse))?;
        }
    }

    execute!(stdout, cursor::MoveTo((2 + query.len()) as u16, 0))?;
    stdout.flush()?;
    Ok(())
}

fn draw_session_line(
    stdout: &mut io::Stdout,
    conv: &Conversation,
    max_width: usize,
    is_selected: bool,
    expanded_tree_roots: &HashSet<String>,
    collapse_enabled: bool,
) -> io::Result<()> {
    let source_tag = match conv.source {
        SessionSource::Claude => "claude",
        SessionSource::Codex => "codex",
    };
    let source_color = match conv.source {
        SessionSource::Claude => Color::Blue,
        SessionSource::Codex => Color::Green,
    };

    let age = format_relative_time(conv.timestamp);
    let hierarchy = picker_hierarchy_marker(conv, expanded_tree_roots, collapse_enabled);
    let project = format_project_label(conv);
    let model = format_model_short(conv.model.as_deref());
    let title = get_display_title(conv);
    let sid = short_id(&conv.session_id);

    let model_display = format!("({:<12})", model);
    // fixed columns: " " + 8 (source) + " " + 5 (age) + "  " + hierarchy gutter + 20 (project) + "  " + 14 (model) + "  " + 8 (sid) + " " + 2 (quotes)
    let fixed_len =
        1 + 8 + 1 + 5 + 2 + HIERARCHY_GUTTER_WIDTH + 20 + 2 + model_display.len() + 2 + 8 + 1 + 2;
    let preview_max = max_width.saturating_sub(fixed_len);
    let preview = truncate(&title, preview_max.max(10));

    execute!(
        stdout,
        Print(" "),
        SetForegroundColor(source_color),
        Print(format!("{:<8}", format!("[{}]", source_tag))),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    execute!(stdout, Print(format!(" {:>5}  ", age)))?;

    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "{:<width$}",
            hierarchy,
            width = HIERARCHY_GUTTER_WIDTH
        )),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    let proj_display: String = project.chars().take(20).collect();
    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print(format!("{:<20}", proj_display)),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(format!("  {}", model_display)),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(format!("  {} ", sid)),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    let clean_preview: String = preview
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    execute!(stdout, Print(format!("\"{}\"", clean_preview)))?;

    if is_selected {
        let line_so_far = 1
            + 8
            + 1
            + 5
            + 2
            + HIERARCHY_GUTTER_WIDTH
            + 20
            + 2
            + model_display.len()
            + 2
            + 8
            + 1
            + clean_preview.len()
            + 2;
        let padding = max_width.saturating_sub(line_so_far);
        if padding > 0 {
            execute!(stdout, Print(" ".repeat(padding)))?;
        }
    }

    Ok(())
}

fn picker_hierarchy_marker(
    conv: &Conversation,
    expanded_tree_roots: &HashSet<String>,
    collapse_enabled: bool,
) -> String {
    if collapse_enabled && conv.hierarchy_depth == 0 && conv.hierarchy_has_children {
        if expanded_tree_roots.contains(&conv.session_id) {
            "▾─".to_string()
        } else {
            "▸─".to_string()
        }
    } else {
        format_hierarchy_marker(conv)
    }
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn conversation(session_id: &str, depth: usize, has_children: bool) -> Conversation {
        Conversation {
            path: PathBuf::from(format!("{session_id}.jsonl")),
            source: SessionSource::Codex,
            session_id: session_id.to_string(),
            timestamp: Local::now(),
            preview: session_id.to_string(),
            full_text: String::new(),
            project_name: Some("project".to_string()),
            cwd: None,
            message_count: 1,
            model: Some("gpt-5.5".to_string()),
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: (depth > 0).then(|| session_id.to_string()),
            hierarchy_has_children: has_children,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: depth,
            hierarchy_order: 0,
            hierarchy_sort_timestamp: Local::now(),
        }
    }

    #[test]
    fn collapses_tree_rows_by_default() {
        let conversations = vec![
            conversation("root", 0, true),
            conversation("child", 1, false),
            conversation("grandchild", 2, false),
            conversation("plain", 0, false),
        ];
        let base_indices = vec![0, 1, 2, 3];
        let expanded = HashSet::new();

        let visible = collapse_visible_indices(&conversations, base_indices, &expanded, true);

        assert_eq!(visible, vec![0, 3]);
    }

    #[test]
    fn expanded_tree_rows_show_descendants() {
        let conversations = vec![
            conversation("root", 0, true),
            conversation("child", 1, false),
            conversation("grandchild", 2, false),
            conversation("plain", 0, false),
        ];
        let base_indices = vec![0, 1, 2, 3];
        let expanded = HashSet::from(["root".to_string()]);

        let visible = collapse_visible_indices(&conversations, base_indices, &expanded, true);

        assert_eq!(visible, vec![0, 1, 2, 3]);
    }

    #[test]
    fn search_results_ignore_collapsed_tree_state() {
        let conversations = vec![
            conversation("root", 0, true),
            conversation("child", 1, false),
        ];
        let base_indices = vec![1];
        let expanded = HashSet::new();

        let visible = collapse_visible_indices(&conversations, base_indices, &expanded, false);

        assert_eq!(visible, vec![1]);
    }

    #[test]
    fn picker_hierarchy_marker_marks_collapsed_and_expanded_roots() {
        let root = conversation("root", 0, true);
        let child = conversation("child", 1, false);
        let collapsed = HashSet::new();
        let expanded = HashSet::from(["root".to_string()]);

        assert_eq!(
            picker_hierarchy_marker(&root, &collapsed, true),
            "▸─".to_string()
        );
        assert_eq!(
            picker_hierarchy_marker(&root, &expanded, true),
            "▾─".to_string()
        );
        assert_eq!(
            picker_hierarchy_marker(&child, &collapsed, true),
            format_hierarchy_marker(&child)
        );
    }

    #[test]
    fn right_arrow_is_ignored_in_picker() {
        let event = Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

        assert_eq!(picker_key_action(&event), PickerKeyAction::Ignore);
    }

    #[test]
    fn picker_hint_does_not_advertise_arrow_resume() {
        assert!(!PICKER_HINT.contains("resume"));
        assert!(!PICKER_HINT.contains("\u{2192}"));
    }

    #[test]
    fn viewer_search_terms_split_non_alphanumeric_query() {
        assert_eq!(
            viewer_search_terms("memory_limiter gpt-5.5"),
            vec!["memory", "limiter", "gpt", "5"]
        );
    }

    #[test]
    fn viewer_search_finds_case_insensitive_matches() {
        let lines = vec![
            vec![test_span("User: Hidden Needle")],
            vec![test_span("Assistant: another needle here")],
        ];

        let search = ViewerSearch::new("needle", &lines).unwrap();

        assert_eq!(
            search.matches,
            vec![
                ViewerMatch {
                    line: 0,
                    start: 13,
                    end: 19,
                },
                ViewerMatch {
                    line: 1,
                    start: 19,
                    end: 25,
                },
            ]
        );
        assert_eq!(search.status_label(), "match 1/2");
    }

    #[test]
    fn viewer_search_navigation_wraps_between_matches() {
        let lines = vec![
            vec![test_span("first needle")],
            vec![test_span("second needle")],
        ];
        let mut search = ViewerSearch::new("needle", &lines).unwrap();

        assert_eq!(search.current_line(), Some(0));
        search.next();
        assert_eq!(search.current_line(), Some(1));
        search.next();
        assert_eq!(search.current_line(), Some(0));
        search.previous();
        assert_eq!(search.current_line(), Some(1));
    }

    #[test]
    fn pager_keymap_uses_n_for_match_navigation_and_slash_for_search() {
        assert_eq!(
            pager_key_action(
                &Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
                false,
                false,
            ),
            PagerKeyAction::NextMatch
        );
        assert_eq!(
            pager_key_action(
                &Event::Key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)),
                false,
                false,
            ),
            PagerKeyAction::PreviousMatch
        );
        assert_eq!(
            pager_key_action(
                &Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
                false,
                false,
            ),
            PagerKeyAction::StartSearch
        );
    }

    #[test]
    fn pager_arrows_move_between_matches_when_search_is_active() {
        assert_eq!(
            pager_key_action(
                &Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                false,
                true,
            ),
            PagerKeyAction::NextMatch
        );
        assert_eq!(
            pager_key_action(
                &Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
                false,
                true,
            ),
            PagerKeyAction::PreviousMatch
        );
        assert_eq!(
            pager_key_action(
                &Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
                false,
                true,
            ),
            PagerKeyAction::MoveDown
        );
    }

    #[test]
    fn render_styled_line_clips_to_width_without_terminal_wrap() {
        let mut output = Vec::new();
        let spans = vec![test_span("abcdef")];

        render_styled_line(&mut output, &spans, 4, None, 0).unwrap();

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("abcd"));
        assert!(!rendered.contains("e"));
        assert!(!rendered.contains("f"));
    }

    fn test_span(text: &str) -> Span {
        Span {
            text: text.to_string(),
            fg: None,
            bold: false,
            dim: false,
        }
    }
}

// ── Pager (session viewer) ────────────────────────────

enum PagerAction {
    Back,
    CopyId,
    Resume,
    CopyConversation,
    ExportFile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ViewerMatch {
    line: usize,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
struct ViewerSearch {
    query: String,
    matches: Vec<ViewerMatch>,
    current: usize,
}

impl ViewerSearch {
    fn new(query: &str, lines: &[StyledLine]) -> Option<Self> {
        let terms = viewer_search_terms(query);
        if terms.is_empty() {
            return None;
        }

        let mut matches = Vec::new();
        for (line_idx, line) in lines.iter().enumerate() {
            let text = styled_line_text(line);
            matches.extend(find_viewer_matches(line_idx, &text, &terms));
        }
        matches.sort_by_key(|m| (m.line, m.start, m.end));

        Some(Self {
            query: query.trim().to_string(),
            matches,
            current: 0,
        })
    }

    fn current_match(&self) -> Option<&ViewerMatch> {
        self.matches.get(self.current)
    }

    fn current_line(&self) -> Option<usize> {
        self.current_match().map(|m| m.line)
    }

    fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    fn previous(&mut self) {
        if !self.matches.is_empty() {
            self.current = if self.current == 0 {
                self.matches.len() - 1
            } else {
                self.current - 1
            };
        }
    }

    fn status_label(&self) -> String {
        if self.matches.is_empty() {
            format!("no matches for /{}", self.query)
        } else {
            format!("match {}/{}", self.current + 1, self.matches.len())
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum PagerKeyAction {
    Back,
    CopyConversation,
    CopyId,
    ExportFile,
    Resume,
    Refresh,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    Home,
    End,
    NextMatch,
    PreviousMatch,
    StartSearch,
    CommitSearch,
    CancelSearch,
    BackspaceSearch,
    TypeSearch(char),
    Ignore,
}

fn pager_key_action(
    evt: &Event,
    search_input_mode: bool,
    search_has_matches: bool,
) -> PagerKeyAction {
    let Event::Key(key) = evt else {
        return PagerKeyAction::Ignore;
    };

    if key.kind != KeyEventKind::Press {
        return PagerKeyAction::Ignore;
    }

    if search_input_mode {
        return match *key {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => PagerKeyAction::CancelSearch,
            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => PagerKeyAction::CommitSearch,
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => PagerKeyAction::BackspaceSearch,
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
                PagerKeyAction::TypeSearch(c)
            }
            _ => PagerKeyAction::Ignore,
        };
    }

    match *key {
        KeyEvent {
            code: KeyCode::Esc, ..
        }
        | KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            ..
        }
        | KeyEvent {
            code: KeyCode::Backspace,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => PagerKeyAction::Back,
        KeyEvent {
            code: KeyCode::Char('Y'),
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::SHIFT,
            ..
        } => PagerKeyAction::CopyConversation,
        KeyEvent {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::CopyId,
        KeyEvent {
            code: KeyCode::Char('e'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::ExportFile,
        KeyEvent {
            code: KeyCode::Char('o'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::Resume,
        KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::Refresh,
        KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::NextMatch,
        KeyEvent {
            code: KeyCode::Char('N'),
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::SHIFT,
            ..
        } => PagerKeyAction::PreviousMatch,
        KeyEvent {
            code: KeyCode::Char('/'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::StartSearch,
        KeyEvent {
            code: KeyCode::Up, ..
        } if search_has_matches => PagerKeyAction::PreviousMatch,
        KeyEvent {
            code: KeyCode::Down,
            ..
        } if search_has_matches => PagerKeyAction::NextMatch,
        KeyEvent {
            code: KeyCode::Up, ..
        }
        | KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::MoveUp,
        KeyEvent {
            code: KeyCode::Down,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::MoveDown,
        KeyEvent {
            code: KeyCode::PageUp,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => PagerKeyAction::PageUp,
        KeyEvent {
            code: KeyCode::PageDown,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char(' '),
            ..
        } => PagerKeyAction::PageDown,
        KeyEvent {
            code: KeyCode::Home,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('g'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PagerKeyAction::Home,
        KeyEvent {
            code: KeyCode::End, ..
        }
        | KeyEvent {
            code: KeyCode::Char('G'),
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('g'),
            modifiers: KeyModifiers::SHIFT,
            ..
        } => PagerKeyAction::End,
        _ => PagerKeyAction::Ignore,
    }
}

fn viewer_search_terms(query: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(query.len());
    for ch in query.chars() {
        if ch.is_alphanumeric() {
            normalized.extend(ch.to_lowercase());
        } else {
            normalized.push(' ');
        }
    }
    normalized
        .split_whitespace()
        .fold(Vec::new(), |mut terms, term| {
            if !terms.iter().any(|existing| existing == term) {
                terms.push(term.to_string());
            }
            terms
        })
}

fn styled_line_text(line: &StyledLine) -> String {
    line.iter().map(|span| span.text.as_str()).collect()
}

fn find_viewer_matches(line: usize, text: &str, terms: &[String]) -> Vec<ViewerMatch> {
    let lower = text.to_ascii_lowercase();
    let mut matches = Vec::new();
    for term in terms {
        let mut offset = 0;
        while let Some(found) = lower[offset..].find(term) {
            let start = offset + found;
            let end = start + term.len();
            matches.push(ViewerMatch { line, start, end });
            offset = end;
            if offset >= lower.len() {
                break;
            }
        }
    }
    matches.sort_by_key(|m| (m.line, m.start, m.end));

    let mut merged: Vec<ViewerMatch> = Vec::new();
    for matched in matches {
        if let Some(last) = merged.last_mut() {
            if matched.start < last.end {
                last.end = last.end.max(matched.end);
                continue;
            }
        }
        merged.push(matched);
    }
    merged
}

fn scroll_to_match(line: usize, visible: usize, max_scroll: usize) -> usize {
    line.saturating_sub(visible / 3).min(max_scroll)
}

fn pager_loop(
    stdout: &mut io::Stdout,
    conv: &Conversation,
    initial_query: &str,
) -> crate::error::Result<PagerAction> {
    let mut lines = viewer::build_session_lines(conv)?;
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    let visible = (rows as usize).saturating_sub(1);
    let mut search_query = initial_query.trim().to_string();
    let mut search_input_mode = false;
    let mut search = ViewerSearch::new(&search_query, &lines);
    let mut scroll = if let Some(line) = search.as_ref().and_then(ViewerSearch::current_line) {
        scroll_to_match(line, visible, lines.len().saturating_sub(visible))
    } else {
        lines.len().saturating_sub(visible)
    };

    loop {
        if let Err(e) = draw_pager(
            stdout,
            &lines,
            scroll,
            conv,
            search.as_ref(),
            search_input_mode,
            &search_query,
        ) {
            return Err(crate::error::AppError::Io(e));
        }

        let evt = match event::read() {
            Ok(e) => e,
            Err(e) => return Err(crate::error::AppError::Io(e)),
        };

        let (_, rows) = terminal::size().unwrap_or((80, 24));
        let visible = (rows as usize).saturating_sub(1); // reserve 1 for status bar
        let max_scroll = lines.len().saturating_sub(visible);

        let search_has_matches = search
            .as_ref()
            .is_some_and(|search| !search.matches.is_empty());

        match pager_key_action(&evt, search_input_mode, search_has_matches) {
            PagerKeyAction::Back => return Ok(PagerAction::Back),
            PagerKeyAction::CopyConversation => return Ok(PagerAction::CopyConversation),
            PagerKeyAction::CopyId => return Ok(PagerAction::CopyId),
            PagerKeyAction::ExportFile => return Ok(PagerAction::ExportFile),
            PagerKeyAction::Resume => return Ok(PagerAction::Resume),
            PagerKeyAction::Refresh => {
                let old_len = lines.len();
                lines = viewer::build_session_lines(conv)?;
                search = ViewerSearch::new(&search_query, &lines);
                // If new content appeared and we were at the bottom, follow the tail
                let was_at_bottom = scroll >= old_len.saturating_sub(visible);
                let new_max = lines.len().saturating_sub(visible);
                if let Some(line) = search.as_ref().and_then(ViewerSearch::current_line) {
                    scroll = scroll_to_match(line, visible, new_max);
                } else if was_at_bottom && lines.len() > old_len {
                    scroll = new_max;
                } else if scroll > new_max {
                    scroll = new_max;
                }
            }
            PagerKeyAction::NextMatch => {
                if let Some(search) = search.as_mut() {
                    search.next();
                    if let Some(line) = search.current_line() {
                        scroll = scroll_to_match(line, visible, max_scroll);
                    }
                }
            }
            PagerKeyAction::PreviousMatch => {
                if let Some(search) = search.as_mut() {
                    search.previous();
                    if let Some(line) = search.current_line() {
                        scroll = scroll_to_match(line, visible, max_scroll);
                    }
                }
            }
            PagerKeyAction::StartSearch => {
                search_input_mode = true;
                search_query.clear();
                search = None;
            }
            PagerKeyAction::CommitSearch => {
                search_input_mode = false;
                search = ViewerSearch::new(&search_query, &lines);
                if let Some(line) = search.as_ref().and_then(ViewerSearch::current_line) {
                    scroll = scroll_to_match(line, visible, max_scroll);
                }
            }
            PagerKeyAction::CancelSearch => {
                search_input_mode = false;
            }
            PagerKeyAction::BackspaceSearch => {
                search_query.pop();
                search = ViewerSearch::new(&search_query, &lines);
                if let Some(line) = search.as_ref().and_then(ViewerSearch::current_line) {
                    scroll = scroll_to_match(line, visible, max_scroll);
                }
            }
            PagerKeyAction::TypeSearch(c) => {
                search_query.push(c);
                search = ViewerSearch::new(&search_query, &lines);
                if let Some(line) = search.as_ref().and_then(ViewerSearch::current_line) {
                    scroll = scroll_to_match(line, visible, max_scroll);
                }
            }
            PagerKeyAction::MoveUp => {
                scroll = scroll.saturating_sub(1);
            }
            PagerKeyAction::MoveDown => {
                if scroll < max_scroll {
                    scroll += 1;
                }
            }
            PagerKeyAction::PageUp => {
                scroll = scroll.saturating_sub(visible / 2);
            }
            PagerKeyAction::PageDown => {
                scroll = (scroll + visible / 2).min(max_scroll);
            }
            PagerKeyAction::Home => {
                scroll = 0;
            }
            PagerKeyAction::End => {
                scroll = max_scroll;
            }
            PagerKeyAction::Ignore => {}
        }
    }
}

fn draw_pager(
    stdout: &mut io::Stdout,
    lines: &[StyledLine],
    scroll: usize,
    conv: &Conversation,
    search: Option<&ViewerSearch>,
    search_input_mode: bool,
    search_query: &str,
) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let cols = cols as usize;
    let rows = rows as usize;
    let content_rows = rows.saturating_sub(1); // reserve last row for status

    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All)
    )?;

    for i in 0..content_rows {
        let line_idx = scroll + i;
        if line_idx >= lines.len() {
            break;
        }

        execute!(stdout, cursor::MoveTo(0, i as u16))?;
        render_styled_line(
            stdout,
            &lines[line_idx],
            cols.saturating_sub(1),
            search,
            line_idx,
        )?;
    }

    // Status bar: session details on left, keys + progress on right
    let progress = if lines.is_empty() {
        100
    } else {
        ((scroll + content_rows).min(lines.len()) * 100) / lines.len()
    };
    let project = format_project_label(conv);
    let model = format_model_short(conv.model.as_deref());
    let age = format_relative_time(conv.timestamp);
    let sid = short_id(&conv.session_id);
    let left = format!(" {} ({}) {} {}", project, model, age, sid);
    let search_status = if search_input_mode {
        format!(" /{} ", search_query)
    } else {
        search
            .map(|search| format!(" \u{2191}\u{2193}/nN:{} /:search ", search.status_label()))
            .unwrap_or_else(|| " /:search ".to_string())
    };
    let right = format!(
        "jk:scroll  g/G  y:id Y:copy e:export  o:resume  r:refresh{}q:back  {}% ",
        search_status, progress
    );
    let gap = cols.saturating_sub(left.len() + right.len());
    let status = format!("{}{}{}", left, " ".repeat(gap), right);
    execute!(
        stdout,
        cursor::MoveTo(0, (rows - 1) as u16),
        SetAttribute(Attribute::Reverse),
        Print(format!("{:<width$}", status, width = cols)),
        SetAttribute(Attribute::NoReverse),
    )?;

    stdout.flush()?;
    Ok(())
}

fn render_styled_line<W: Write>(
    stdout: &mut W,
    spans: &[Span],
    max_width: usize,
    search: Option<&ViewerSearch>,
    line_idx: usize,
) -> io::Result<()> {
    let line_matches = search
        .map(|search| search.line_matches(line_idx))
        .unwrap_or_default();
    let mut line_offset = 0;
    let mut remaining_width = max_width;
    for span in spans {
        if remaining_width == 0 {
            break;
        }
        render_span_with_highlights(
            stdout,
            span,
            line_offset,
            &line_matches,
            &mut remaining_width,
        )?;
        line_offset += span.text.len();
    }
    Ok(())
}

impl ViewerSearch {
    fn line_matches(&self, line_idx: usize) -> Vec<(&ViewerMatch, bool)> {
        self.matches
            .iter()
            .enumerate()
            .filter(|(_, m)| m.line == line_idx)
            .map(|(idx, m)| (m, idx == self.current))
            .collect()
    }
}

fn render_span_with_highlights<W: Write>(
    stdout: &mut W,
    span: &Span,
    span_start: usize,
    line_matches: &[(&ViewerMatch, bool)],
    remaining_width: &mut usize,
) -> io::Result<()> {
    let span_end = span_start + span.text.len();
    let mut cursor = 0;

    for (matched, is_active) in line_matches {
        if *remaining_width == 0 {
            return Ok(());
        }
        if matched.end <= span_start || matched.start >= span_end {
            continue;
        }
        let local_start = matched
            .start
            .saturating_sub(span_start)
            .min(span.text.len());
        let local_end = (matched.end.min(span_end) - span_start).min(span.text.len());
        if local_start > cursor {
            print_span_segment(
                stdout,
                span,
                &span.text[cursor..local_start],
                remaining_width,
            )?;
        }
        if local_end > local_start {
            print_highlight_segment(
                stdout,
                &span.text[local_start..local_end],
                *is_active,
                remaining_width,
            )?;
        }
        cursor = local_end;
    }

    if cursor < span.text.len() {
        print_span_segment(stdout, span, &span.text[cursor..], remaining_width)?;
    }

    Ok(())
}

fn print_span_segment<W: Write>(
    stdout: &mut W,
    span: &Span,
    text: &str,
    remaining_width: &mut usize,
) -> io::Result<()> {
    let (text, width) = display_width_prefix(text, *remaining_width);
    if text.is_empty() {
        return Ok(());
    }
    if span.bold {
        execute!(stdout, SetAttribute(Attribute::Bold))?;
    }
    if span.dim {
        execute!(stdout, SetAttribute(Attribute::Dim))?;
    }
    if let Some((r, g, b)) = span.fg {
        execute!(stdout, SetForegroundColor(Color::Rgb { r, g, b }))?;
    }
    execute!(stdout, Print(text))?;
    execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    *remaining_width = remaining_width.saturating_sub(width);
    Ok(())
}

fn print_highlight_segment<W: Write>(
    stdout: &mut W,
    text: &str,
    active: bool,
    remaining_width: &mut usize,
) -> io::Result<()> {
    let (text, width) = display_width_prefix(text, *remaining_width);
    if text.is_empty() {
        return Ok(());
    }
    if active {
        execute!(
            stdout,
            SetForegroundColor(Color::Black),
            SetBackgroundColor(Color::Yellow),
            SetAttribute(Attribute::Bold),
            Print(text),
            ResetColor,
            SetAttribute(Attribute::Reset)
        )?;
    } else {
        execute!(
            stdout,
            SetForegroundColor(Color::Black),
            SetBackgroundColor(Color::DarkYellow),
            Print(text),
            ResetColor,
            SetAttribute(Attribute::Reset)
        )?;
    }
    *remaining_width = remaining_width.saturating_sub(width);
    Ok(())
}

fn display_width_prefix(text: &str, max_width: usize) -> (&str, usize) {
    let mut used = 0;
    let mut end = 0;

    for (idx, ch) in text.char_indices() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > max_width {
            break;
        }
        used += width;
        end = idx + ch.len_utf8();
    }

    (&text[..end], used)
}
