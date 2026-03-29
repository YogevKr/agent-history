//! fzf-like interactive session picker with in-TUI session viewer.

use crate::display::{format_model_short, format_relative_time, get_display_title, short_id, truncate};
use crate::history::{Conversation, SessionSource};
use crate::search::{precompute_search_text, search, SearchableConversation};
use crate::viewer::{self, Span, StyledLine};
use chrono::Local;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::io::{self, Write};
use std::process::{Command, Stdio};

/// Run interactive session picker. Returns Ok(()) on clean exit.
pub fn run(conversations: Vec<Conversation>) -> crate::error::Result<()> {
    if conversations.is_empty() {
        eprintln!("No sessions found");
        return Ok(());
    }

    let searchable = precompute_search_text(&conversations);

    let mut state = PickerState {
        query: String::new(),
        selected: 0,
        filtered_indices: (0..conversations.len()).collect(),
        flash: None,
    };

    terminal::enable_raw_mode().map_err(crate::error::AppError::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)
        .map_err(crate::error::AppError::Io)?;

    let result = main_loop(&mut stdout, &conversations, &searchable, &mut state);

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
    flash: Option<String>,
}

fn main_loop(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    state: &mut PickerState,
) -> crate::error::Result<()> {
    loop {
        match picker_loop(stdout, conversations, searchable, state) {
            PickerAction::ViewSession(idx) => {
                pager_loop(stdout, &conversations[idx])?;
                // Returns here → back to picker with same state
            }
            PickerAction::CopyId(idx) => {
                let id = &conversations[idx].session_id;
                let _ = copy_to_clipboard(id);
                state.flash = Some(format!("Copied: {}", id));
            }
            PickerAction::Quit => return Ok(()),
        }
    }
}

enum PickerAction {
    ViewSession(usize),
    CopyId(usize),
    Quit,
}

// ── Picker (list view) ────────────────────────────────

fn picker_loop(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    state: &mut PickerState,
) -> PickerAction {
    loop {
        if let Err(_) = draw_picker(stdout, conversations, &state.filtered_indices, &state.query, state.selected, state.flash.as_deref()) {
            return PickerAction::Quit;
        }
        state.flash = None;

        let evt = match event::read() {
            Ok(e) => e,
            Err(_) => return PickerAction::Quit,
        };

        match evt {
            Event::Key(KeyEvent { code: KeyCode::Esc, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. }) => {
                return PickerAction::Quit;
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::ViewSession(idx);
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Up, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('k'), modifiers: KeyModifiers::CONTROL, .. }) => {
                if state.selected > 0 {
                    state.selected -= 1;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('j'), modifiers: KeyModifiers::CONTROL, .. }) => {
                if state.selected + 1 < state.filtered_indices.len() {
                    state.selected += 1;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                state.query.pop();
                refilter(conversations, searchable, state);
            }
            Event::Key(KeyEvent { code: KeyCode::Char('y'), modifiers: KeyModifiers::CONTROL, .. }) => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::CopyId(idx);
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Char(c), modifiers, .. }) => {
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                    state.query.push(c);
                    refilter(conversations, searchable, state);
                }
            }
            _ => {}
        }
    }
}

fn refilter(
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    state: &mut PickerState,
) {
    if state.query.is_empty() {
        state.filtered_indices = (0..conversations.len()).collect();
    } else {
        state.filtered_indices = search(conversations, searchable, &state.query, Local::now());
    }
    if state.selected >= state.filtered_indices.len() {
        state.selected = state.filtered_indices.len().saturating_sub(1);
    }
}

fn draw_picker(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    filtered_indices: &[usize],
    query: &str,
    selected: usize,
    flash: Option<&str>,
) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let cols = cols as usize;
    let rows = rows as usize;

    execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;

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
    let hint = "  Ctrl-y: copy ID";
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

        draw_session_line(stdout, conv, cols, is_selected)?;

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
    let project = conv.project_name.as_deref().unwrap_or("unknown");
    let model = format_model_short(conv.model.as_deref());
    let title = get_display_title(conv);
    let sid = short_id(&conv.session_id);

    let model_display = format!("({:<12})", model);
    // fixed columns: " " + 8 (source) + " " + 5 (age) + "  " + 20 (project) + "  " + 14 (model) + "  " + 8 (sid) + " " + 2 (quotes)
    let fixed_len = 1 + 8 + 1 + 5 + 2 + 20 + 2 + model_display.len() + 2 + 8 + 1 + 2;
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

    let clean_preview: String = preview.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    execute!(stdout, Print(format!("\"{}\"", clean_preview)))?;

    if is_selected {
        let line_so_far = 1 + 8 + 1 + 5 + 2 + 20 + 2 + model_display.len() + 2 + 8 + 1 + clean_preview.len() + 2;
        let padding = max_width.saturating_sub(line_so_far);
        if padding > 0 {
            execute!(stdout, Print(" ".repeat(padding)))?;
        }
    }

    Ok(())
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

// ── Pager (session viewer) ────────────────────────────

fn pager_loop(
    stdout: &mut io::Stdout,
    conv: &Conversation,
) -> crate::error::Result<()> {
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

        let (_, rows) = terminal::size().unwrap_or((80, 24));
        let visible = (rows as usize).saturating_sub(1); // reserve 1 for status bar
        let max_scroll = lines.len().saturating_sub(visible);

        match evt {
            // Back to list
            Event::Key(KeyEvent { code: KeyCode::Esc, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('q'), modifiers: KeyModifiers::NONE, .. })
            | Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                return Ok(());
            }
            Event::Key(KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. }) => {
                return Ok(());
            }
            // Refresh
            Event::Key(KeyEvent { code: KeyCode::Char('r'), modifiers: KeyModifiers::NONE, .. }) => {
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
            Event::Key(KeyEvent { code: KeyCode::Up, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('k'), modifiers: KeyModifiers::NONE, .. }) => {
                scroll = scroll.saturating_sub(1);
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('j'), modifiers: KeyModifiers::NONE, .. }) => {
                if scroll < max_scroll { scroll += 1; }
            }
            Event::Key(KeyEvent { code: KeyCode::PageUp, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('u'), modifiers: KeyModifiers::CONTROL, .. }) => {
                scroll = scroll.saturating_sub(visible / 2);
            }
            Event::Key(KeyEvent { code: KeyCode::PageDown, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('d'), modifiers: KeyModifiers::CONTROL, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char(' '), .. }) => {
                scroll = (scroll + visible / 2).min(max_scroll);
            }
            Event::Key(KeyEvent { code: KeyCode::Home, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('g'), modifiers: KeyModifiers::NONE, .. }) => {
                scroll = 0;
            }
            Event::Key(KeyEvent { code: KeyCode::End, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('G'), modifiers: KeyModifiers::SHIFT, .. }) => {
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

    execute!(stdout, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;

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
    let project = conv.project_name.as_deref().unwrap_or("unknown");
    let model = format_model_short(conv.model.as_deref());
    let age = format_relative_time(conv.timestamp);
    let sid = short_id(&conv.session_id);
    let left = format!(" {} ({}) {} {}", project, model, age, sid);
    let right = format!("jk/↑↓  PgUp/Dn  g/G  r:refresh  q:back  {}% ", progress);
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

fn render_styled_line(stdout: &mut io::Stdout, spans: &[Span], _max_width: usize) -> io::Result<()> {
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
