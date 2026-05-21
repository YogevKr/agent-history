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
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::collections::HashSet;
use std::io::{self, Write};
use std::process::{Command, Stdio};

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
            PickerAction::ViewSession(idx) => match pager_loop(stdout, &conversations[idx])? {
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
                                state.flash = Some("Copied conversation to clipboard".to_string());
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
                        Ok(md) => match crate::export::export_to_file(&conversations[idx], &md) {
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
            },
            PickerAction::CopyId(idx) => idx,
            PickerAction::ResumeSession(idx) => {
                let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
                let _ = terminal::disable_raw_mode();
                return crate::resume::resume_session(&conversations[idx]);
            }
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
    ResumeSession(usize),
    Quit,
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

        // Only handle key press events (not release/repeat)
        if matches!(&evt, Event::Key(ke) if ke.kind != KeyEventKind::Press) {
            continue;
        }

        match evt {
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                return PickerAction::Quit;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::ViewSession(idx);
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                if state.selected + 1 < state.filtered_indices.len() {
                    state.selected += 1;
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) => {
                state.query.pop();
                refilter(conversations, state);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                toggle_tree_expansion(conversations, state);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Left,
                ..
            }) => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::CopyId(idx);
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Right,
                ..
            }) => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::ResumeSession(idx);
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) => {
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                    state.query.push(c);
                    refilter(conversations, state);
                }
            }
            _ => {}
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
    let hint = "  Tab: expand/collapse  \u{2190}: copy ID  \u{2192}: resume";
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
}

// ── Pager (session viewer) ────────────────────────────

enum PagerAction {
    Back,
    CopyId,
    Resume,
    CopyConversation,
    ExportFile,
}

fn pager_loop(stdout: &mut io::Stdout, conv: &Conversation) -> crate::error::Result<PagerAction> {
    let mut lines = viewer::build_session_lines(conv)?;
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    let visible = (rows as usize).saturating_sub(1);
    let mut scroll: usize = lines.len().saturating_sub(visible);

    loop {
        if let Err(e) = draw_pager(stdout, &lines, scroll, conv) {
            return Err(crate::error::AppError::Io(e));
        }

        let evt = match event::read() {
            Ok(e) => e,
            Err(e) => return Err(crate::error::AppError::Io(e)),
        };

        // Only handle key press events (not release/repeat)
        if matches!(&evt, Event::Key(ke) if ke.kind != KeyEventKind::Press) {
            continue;
        }

        let (_, rows) = terminal::size().unwrap_or((80, 24));
        let visible = (rows as usize).saturating_sub(1); // reserve 1 for status bar
        let max_scroll = lines.len().saturating_sub(visible);

        match evt {
            // Back to list
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) => {
                return Ok(PagerAction::Back);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                return Ok(PagerAction::Back);
            }
            // Copy full conversation to clipboard (Shift-Y) — must be before lowercase y
            Event::Key(KeyEvent {
                code: KeyCode::Char('Y'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::SHIFT,
                ..
            }) => {
                return Ok(PagerAction::CopyConversation);
            }
            // Copy session ID
            Event::Key(KeyEvent {
                code: KeyCode::Char('y'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                return Ok(PagerAction::CopyId);
            }
            // Export conversation to file
            Event::Key(KeyEvent {
                code: KeyCode::Char('e'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                return Ok(PagerAction::ExportFile);
            }
            // Resume session
            Event::Key(KeyEvent {
                code: KeyCode::Char('o'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                return Ok(PagerAction::Resume);
            }
            // Refresh
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                let old_len = lines.len();
                lines = viewer::build_session_lines(conv)?;
                // If new content appeared and we were at the bottom, follow the tail
                let was_at_bottom = scroll >= old_len.saturating_sub(visible);
                let new_max = lines.len().saturating_sub(visible);
                if was_at_bottom && lines.len() > old_len {
                    scroll = new_max;
                } else if scroll > new_max {
                    scroll = new_max;
                }
            }
            // Scroll
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                scroll = scroll.saturating_sub(1);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                if scroll < max_scroll {
                    scroll += 1;
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::PageUp,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                scroll = scroll.saturating_sub(visible / 2);
            }
            Event::Key(KeyEvent {
                code: KeyCode::PageDown,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                ..
            }) => {
                scroll = (scroll + visible / 2).min(max_scroll);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Home,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::NONE,
                ..
            }) => {
                scroll = 0;
            }
            Event::Key(KeyEvent {
                code: KeyCode::End, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('G'),
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::SHIFT,
                ..
            }) => {
                scroll = max_scroll;
            }
            _ => {}
        }
    }
}

fn draw_pager(
    stdout: &mut io::Stdout,
    lines: &[StyledLine],
    scroll: usize,
    conv: &Conversation,
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
        render_styled_line(stdout, &lines[line_idx], cols)?;
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
    let right = format!(
        "jk/\u{2191}\u{2193}  g/G  y:id Y:copy e:export  o:resume  r:refresh  q:back  {}% ",
        progress
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

fn render_styled_line(
    stdout: &mut io::Stdout,
    spans: &[Span],
    _max_width: usize,
) -> io::Result<()> {
    for span in spans {
        if span.bold {
            execute!(stdout, SetAttribute(Attribute::Bold))?;
        }
        if span.dim {
            execute!(stdout, SetAttribute(Attribute::Dim))?;
        }
        if let Some((r, g, b)) = span.fg {
            execute!(stdout, SetForegroundColor(Color::Rgb { r, g, b }))?;
        }
        execute!(stdout, Print(&span.text))?;
        execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    }
    Ok(())
}
