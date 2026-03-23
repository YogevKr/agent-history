//! fzf-like interactive session picker.

use crate::display::{format_model_short, format_relative_time, get_display_title, truncate};
use crate::history::{Conversation, SessionSource};
use crate::search::{precompute_search_text, search};
use crate::viewer;
use chrono::Local;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{self, Attribute, Color, Print, SetAttribute, SetForegroundColor, ResetColor},
    terminal::{self, ClearType},
};
use std::io::{self, Write};

/// Run interactive session picker. Returns Ok(()) on clean exit.
pub fn run(conversations: Vec<Conversation>) -> crate::error::Result<()> {
    if conversations.is_empty() {
        eprintln!("No sessions found");
        return Ok(());
    }

    let searchable = precompute_search_text(&conversations);

    let mut query = String::new();
    let mut selected: usize = 0;
    let mut filtered_indices: Vec<usize> = (0..conversations.len()).collect();

    terminal::enable_raw_mode().map_err(|e| crate::error::AppError::Io(e))?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)
        .map_err(|e| crate::error::AppError::Io(e))?;

    let result = run_loop(
        &mut stdout,
        &conversations,
        &searchable,
        &mut query,
        &mut selected,
        &mut filtered_indices,
    );

    // Always restore terminal
    let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();

    match result {
        LoopResult::Selected(idx) => {
            viewer::review_session(&conversations[idx])?;
        }
        LoopResult::Cancelled => {}
        LoopResult::Error(e) => return Err(e),
    }

    Ok(())
}

enum LoopResult {
    Selected(usize),
    Cancelled,
    Error(crate::error::AppError),
}

fn run_loop(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    searchable: &[crate::search::SearchableConversation],
    query: &mut String,
    selected: &mut usize,
    filtered_indices: &mut Vec<usize>,
) -> LoopResult {
    loop {
        // Draw
        if let Err(e) = draw(stdout, conversations, filtered_indices, query, *selected) {
            return LoopResult::Error(crate::error::AppError::Io(e));
        }

        // Wait for event
        let evt = match event::read() {
            Ok(e) => e,
            Err(e) => return LoopResult::Error(crate::error::AppError::Io(e)),
        };

        match evt {
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                return LoopResult::Cancelled;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                ..
            }) => {
                if !filtered_indices.is_empty() {
                    let idx = filtered_indices[*selected];
                    return LoopResult::Selected(idx);
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
                if *selected > 0 {
                    *selected -= 1;
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
                if *selected + 1 < filtered_indices.len() {
                    *selected += 1;
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Backspace,
                ..
            }) => {
                query.pop();
                refilter(conversations, searchable, query, filtered_indices, selected);
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                ..
            }) => {
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
                    query.push(c);
                    refilter(conversations, searchable, query, filtered_indices, selected);
                }
            }
            Event::Resize(_, _) => {
                // Redraw on resize
            }
            _ => {}
        }
    }
}

fn refilter(
    conversations: &[Conversation],
    searchable: &[crate::search::SearchableConversation],
    query: &str,
    filtered_indices: &mut Vec<usize>,
    selected: &mut usize,
) {
    if query.is_empty() {
        *filtered_indices = (0..conversations.len()).collect();
    } else {
        *filtered_indices = search(conversations, searchable, query, Local::now());
    }
    if *selected >= filtered_indices.len() {
        *selected = filtered_indices.len().saturating_sub(1);
    }
}

fn draw(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    filtered_indices: &[usize],
    query: &str,
    selected: usize,
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

    // Line 1: match count
    execute!(
        stdout,
        cursor::MoveTo(0, 1),
        SetForegroundColor(Color::DarkGrey),
        Print(format!("  {}/{}", filtered_indices.len(), conversations.len())),
        ResetColor,
    )?;

    // Lines 2..rows: session list
    let list_start = 2usize;
    let visible = rows.saturating_sub(list_start);

    // Scroll offset: keep selected visible
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

    // Put cursor after query text
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

    // Calculate available space for preview
    // " [claude]  2h ago  project-name          (model)  "title""
    let fixed_len = 1 + 8 + 1 + 6 + 2 + 20 + 2 + model.len() + 2 + 3; // approx
    let preview_max = max_width.saturating_sub(fixed_len);
    let preview = truncate(&title, preview_max.max(10));

    // Source tag colored
    execute!(
        stdout,
        Print(" "),
        SetForegroundColor(source_color),
        Print(format!("[{}]", source_tag)),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    // Age
    execute!(
        stdout,
        Print(format!(" {:>5}  ", age)),
    )?;

    // Project (truncated to 20)
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

    // Model
    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(format!("  ({})  ", model)),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    // Preview — replace newlines with spaces for single-line display
    let clean_preview: String = preview.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    execute!(stdout, Print(format!("\"{}\"", clean_preview)))?;

    // Pad rest of line for reverse video
    if is_selected {
        let line_so_far = 1 + 8 + 1 + 5 + 2 + 20 + 2 + model.len() + 4 + clean_preview.len() + 2;
        let padding = max_width.saturating_sub(line_so_far);
        if padding > 0 {
            execute!(stdout, Print(" ".repeat(padding)))?;
        }
    }

    Ok(())
}
