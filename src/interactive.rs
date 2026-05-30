//! fzf-like interactive session picker with in-TUI session viewer.

use crate::display::{
    format_directory_label, format_hierarchy_marker, format_model_short, format_relative_time,
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
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const PICKER_HINT: &str =
    "  Enter: view  Left/Right: column  PgUp/PgDn Home/End  Tab: expand/collapse";
const PICKER_HEADER_ROWS: usize = 4;
const PICKER_SOURCE_WIDTH: usize = 8;
const PICKER_AGE_WIDTH: usize = 5;
const PICKER_DIRECTORY_WIDTH: usize = 20;
const PICKER_MODEL_WIDTH: usize = 22;
const PICKER_MIN_HIERARCHY_GUTTER_WIDTH: usize = 3;
const FILTER_OVERLAY_MIN_NAV_WIDTH: usize = 12;
const FILTER_OVERLAY_MAX_NAV_WIDTH: usize = 20;

/// Run interactive session picker. Returns Ok(()) on clean exit.
pub fn run(
    mut store: crate::session_store::SessionStore,
    filters: crate::filters::SessionFilters,
) -> crate::error::Result<()> {
    if store.conversations().is_empty() {
        eprintln!("No sessions found");
        return Ok(());
    }

    let expanded_tree_roots = HashSet::new();
    let filtered_indices =
        initial_filtered_indices(store.conversations(), &filters, &expanded_tree_roots);

    let mut state = PickerState {
        query: String::new(),
        selected: 0,
        scroll: 0,
        filtered_indices,
        searchable: precompute_search_text(store.conversations()),
        full_search_index: None,
        full_search_index_rx: None,
        full_search_query_tx: None,
        full_search_result_rx: None,
        full_search_pending: false,
        expanded_tree_roots,
        flash: None,
        filters,
        filter_overlay: None,
        focused_column: PickerColumn::Preview,
    };

    terminal::enable_raw_mode().map_err(crate::error::AppError::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)
        .map_err(crate::error::AppError::Io)?;

    let result = main_loop(&mut stdout, &mut store, &mut state);

    // Always restore terminal
    let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();

    result?;
    Ok(())
}

fn initial_filtered_indices(
    conversations: &[Conversation],
    filters: &crate::filters::SessionFilters,
    expanded_tree_roots: &HashSet<String>,
) -> Vec<usize> {
    let base_indices = filters.filter_indices(conversations, (0..conversations.len()).collect());
    collapse_visible_indices(conversations, base_indices, expanded_tree_roots, true)
}

struct PickerState {
    query: String,
    selected: usize,
    scroll: usize,
    filtered_indices: Vec<usize>,
    searchable: Vec<SearchableConversation>,
    full_search_index: Option<FullSearchIndex>,
    full_search_index_rx: Option<Receiver<FullSearchIndex>>,
    full_search_query_tx: Option<Sender<String>>,
    full_search_result_rx: Option<Receiver<FullSearchResult>>,
    full_search_pending: bool,
    expanded_tree_roots: HashSet<String>,
    flash: Option<String>,
    filters: crate::filters::SessionFilters,
    filter_overlay: Option<FilterOverlayState>,
    focused_column: PickerColumn,
}

struct FullSearchResult {
    query: String,
    indices: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerColumn {
    Agent,
    Age,
    Directory,
    Model,
    Preview,
}

impl PickerColumn {
    const VISIBLE: [Self; 5] = [
        Self::Agent,
        Self::Age,
        Self::Directory,
        Self::Model,
        Self::Preview,
    ];

    fn previous(self) -> Self {
        let index = Self::VISIBLE
            .iter()
            .position(|column| *column == self)
            .unwrap_or(0);
        Self::VISIBLE[(index + Self::VISIBLE.len() - 1) % Self::VISIBLE.len()]
    }

    fn next(self) -> Self {
        let index = Self::VISIBLE
            .iter()
            .position(|column| *column == self)
            .unwrap_or(0);
        Self::VISIBLE[(index + 1) % Self::VISIBLE.len()]
    }

    fn filter_section(self) -> Option<FilterSection> {
        match self {
            Self::Agent => Some(FilterSection::Agent),
            Self::Age | Self::Directory | Self::Model | Self::Preview => {
                Some(FilterSection::Directory)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FilterOverlayState {
    section: FilterSection,
    agent_selected: usize,
    directory_selected: usize,
    agent_query: String,
    directory_query: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FilterSection {
    Agent,
    Directory,
}

impl PickerState {
    #[cfg(test)]
    fn for_test(conversations: &[Conversation], filters: crate::filters::SessionFilters) -> Self {
        let expanded_tree_roots = HashSet::new();
        let filtered_indices =
            initial_filtered_indices(conversations, &filters, &expanded_tree_roots);

        Self {
            query: String::new(),
            selected: 0,
            scroll: 0,
            filtered_indices,
            searchable: precompute_search_text(conversations),
            full_search_index: None,
            full_search_index_rx: None,
            full_search_query_tx: None,
            full_search_result_rx: None,
            full_search_pending: false,
            expanded_tree_roots,
            flash: None,
            filters,
            filter_overlay: None,
            focused_column: PickerColumn::Preview,
        }
    }
}

impl FilterOverlayState {
    fn new(section: FilterSection) -> Self {
        Self {
            section,
            agent_selected: 0,
            directory_selected: 0,
            agent_query: String::new(),
            directory_query: String::new(),
        }
    }
}

fn main_loop(
    stdout: &mut io::Stdout,
    store: &mut crate::session_store::SessionStore,
    state: &mut PickerState,
) -> crate::error::Result<()> {
    loop {
        let idx = match picker_loop(stdout, store, state) {
            PickerAction::ViewSession(idx) => {
                let viewer_query = state.query.clone();
                match pager_loop(stdout, &store.conversations()[idx], &viewer_query)? {
                    PagerAction::Back => continue,
                    PagerAction::CopyId => idx,
                    PagerAction::Resume => {
                        let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
                        let _ = terminal::disable_raw_mode();
                        return crate::resume::resume_session(&store.conversations()[idx]);
                    }
                    PagerAction::CopyConversation => {
                        match crate::export::to_markdown(&store.conversations()[idx]) {
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
                        match crate::export::to_markdown(&store.conversations()[idx]) {
                            Ok(md) => match crate::export::export_to_file(
                                &store.conversations()[idx],
                                &md,
                            ) {
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
            PickerAction::Quit => return Ok(()),
        };
        let id = &store.conversations()[idx].session_id;
        let _ = copy_to_clipboard(id);
        state.flash = Some(format!("Copied: {}", id));
    }
}

enum PickerAction {
    ViewSession(usize),
    Quit,
}

#[derive(Debug, Eq, PartialEq)]
enum PickerKeyAction {
    Quit,
    ViewSession,
    OpenFilterOverlay,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    Home,
    End,
    Backspace,
    ToggleTreeExpansion,
    PreviousColumn,
    NextColumn,
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
            code: KeyCode::Char('/'),
            modifiers: KeyModifiers::NONE,
            ..
        } => PickerKeyAction::OpenFilterOverlay,
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
            code: KeyCode::PageUp,
            ..
        } => PickerKeyAction::PageUp,
        KeyEvent {
            code: KeyCode::PageDown,
            ..
        } => PickerKeyAction::PageDown,
        KeyEvent {
            code: KeyCode::Home,
            ..
        } => PickerKeyAction::Home,
        KeyEvent {
            code: KeyCode::End, ..
        } => PickerKeyAction::End,
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
        } => PickerKeyAction::PreviousColumn,
        KeyEvent {
            code: KeyCode::Right,
            ..
        } => PickerKeyAction::NextColumn,
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers,
            ..
        } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => PickerKeyAction::Type(c),
        _ => PickerKeyAction::Ignore,
    }
}

#[derive(Debug, Eq, PartialEq)]
enum FilterOverlayKeyAction {
    Close,
    PreviousSection,
    NextSection,
    MoveUp,
    MoveDown,
    Toggle,
    BackspaceQuery,
    TypeQuery(char),
    Ignore,
}

fn filter_overlay_key_action(evt: &Event) -> FilterOverlayKeyAction {
    let Event::Key(key) = evt else {
        return FilterOverlayKeyAction::Ignore;
    };

    if key.kind != KeyEventKind::Press {
        return FilterOverlayKeyAction::Ignore;
    }

    match *key {
        KeyEvent {
            code: KeyCode::Esc, ..
        } => FilterOverlayKeyAction::Close,
        KeyEvent {
            code: KeyCode::Left,
            ..
        } => FilterOverlayKeyAction::PreviousSection,
        KeyEvent {
            code: KeyCode::Right,
            ..
        } => FilterOverlayKeyAction::NextSection,
        KeyEvent {
            code: KeyCode::Up, ..
        } => FilterOverlayKeyAction::MoveUp,
        KeyEvent {
            code: KeyCode::Down,
            ..
        } => FilterOverlayKeyAction::MoveDown,
        KeyEvent {
            code: KeyCode::Char(' '),
            ..
        } if key.modifiers.is_empty() => FilterOverlayKeyAction::Toggle,
        KeyEvent {
            code: KeyCode::Backspace,
            ..
        } => FilterOverlayKeyAction::BackspaceQuery,
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers,
            ..
        } if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
            FilterOverlayKeyAction::TypeQuery(c)
        }
        _ => FilterOverlayKeyAction::Ignore,
    }
}

fn handle_picker_key_action(
    _conversations: &[Conversation],
    state: &mut PickerState,
    action: PickerKeyAction,
) {
    if action == PickerKeyAction::OpenFilterOverlay && state.filter_overlay.is_none() {
        if let Some(section) = state.focused_column.filter_section() {
            state.filter_overlay = Some(FilterOverlayState::new(section));
        }
    }
}

fn handle_filter_overlay_key_action(
    conversations: &[Conversation],
    state: &mut PickerState,
    action: FilterOverlayKeyAction,
) -> bool {
    if state.filter_overlay.is_none() {
        return false;
    }

    match action {
        FilterOverlayKeyAction::Close => {
            state.filter_overlay = None;
            false
        }
        FilterOverlayKeyAction::PreviousSection | FilterOverlayKeyAction::NextSection => {
            let Some(overlay) = state.filter_overlay.as_mut() else {
                return false;
            };
            overlay.section = match overlay.section {
                FilterSection::Agent => FilterSection::Directory,
                FilterSection::Directory => FilterSection::Agent,
            };
            clamp_filter_overlay_selection(conversations, overlay);
            false
        }
        FilterOverlayKeyAction::MoveUp => {
            let Some(overlay) = state.filter_overlay.as_mut() else {
                return false;
            };
            move_filter_overlay_selection(conversations, overlay, -1);
            false
        }
        FilterOverlayKeyAction::MoveDown => {
            let Some(overlay) = state.filter_overlay.as_mut() else {
                return false;
            };
            move_filter_overlay_selection(conversations, overlay, 1);
            false
        }
        FilterOverlayKeyAction::Toggle => {
            let mutated = toggle_filter_overlay_selection(conversations, state);
            if mutated {
                refilter(conversations, state);
            }
            mutated
        }
        FilterOverlayKeyAction::BackspaceQuery => {
            let Some(overlay) = state.filter_overlay.as_mut() else {
                return false;
            };
            overlay_query_mut(overlay).pop();
            clamp_filter_overlay_selection(conversations, overlay);
            false
        }
        FilterOverlayKeyAction::TypeQuery(c) => {
            let Some(overlay) = state.filter_overlay.as_mut() else {
                return false;
            };
            overlay_query_mut(overlay).push(c);
            clamp_filter_overlay_selection(conversations, overlay);
            false
        }
        FilterOverlayKeyAction::Ignore => false,
    }
}

fn toggle_filter_overlay_selection(
    conversations: &[Conversation],
    state: &mut PickerState,
) -> bool {
    let Some(overlay) = state.filter_overlay.as_ref() else {
        return false;
    };

    match overlay.section {
        FilterSection::Agent => {
            let sources = filtered_overlay_sources(overlay);
            if sources.is_empty() {
                return false;
            }

            if overlay.agent_selected == 0 {
                let enable = sources
                    .iter()
                    .any(|source| !state.filters.source_enabled(*source));
                return state.filters.set_sources_enabled(sources, enable);
            }

            let Some(source) = sources.get(overlay.agent_selected - 1).copied() else {
                return false;
            };
            state.filters.toggle_source(source)
        }
        FilterSection::Directory => {
            let directories = filtered_overlay_directories(conversations, overlay);
            if directories.is_empty() {
                return false;
            }

            if overlay.directory_selected == 0 {
                let enable = directories
                    .iter()
                    .any(|directory| !state.filters.directory_enabled(directory));
                state.filters.set_directories_enabled(
                    directories,
                    available_directories(conversations),
                    enable,
                );
                return true;
            }

            let Some(directory) = directories.get(overlay.directory_selected - 1) else {
                return false;
            };
            // Leaving All mode snapshots currently known directory names; later
            // lazy-loaded directories only match if their name is in that set.
            state
                .filters
                .toggle_directory(directory, available_directories(conversations));
            true
        }
    }
}

fn move_filter_overlay_selection(
    conversations: &[Conversation],
    overlay: &mut FilterOverlayState,
    delta: isize,
) {
    let len = filter_overlay_row_count(conversations, overlay);
    if len == 0 {
        overlay.directory_selected = 0;
        return;
    }

    let selected = match overlay.section {
        FilterSection::Agent => &mut overlay.agent_selected,
        FilterSection::Directory => &mut overlay.directory_selected,
    };
    let next = (*selected as isize + delta).clamp(0, len as isize - 1);
    *selected = next as usize;
}

fn clamp_filter_overlay_selection(
    conversations: &[Conversation],
    overlay: &mut FilterOverlayState,
) {
    let agent_len = filtered_overlay_sources(overlay).len();
    overlay.agent_selected = overlay.agent_selected.min(agent_len);

    let directory_len = filtered_overlay_directories(conversations, overlay).len();
    overlay.directory_selected = overlay.directory_selected.min(directory_len);
}

fn filter_overlay_row_count(conversations: &[Conversation], overlay: &FilterOverlayState) -> usize {
    match overlay.section {
        FilterSection::Agent => filtered_overlay_sources(overlay).len() + 1,
        FilterSection::Directory => filtered_overlay_directories(conversations, overlay).len() + 1,
    }
}

fn overlay_query_mut(overlay: &mut FilterOverlayState) -> &mut String {
    match overlay.section {
        FilterSection::Agent => &mut overlay.agent_query,
        FilterSection::Directory => &mut overlay.directory_query,
    }
}

fn available_directories(conversations: &[Conversation]) -> Vec<String> {
    let mut directories = conversations
        .iter()
        .filter_map(|conversation| conversation.directory_name.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

fn filtered_overlay_directories(
    conversations: &[Conversation],
    overlay: &FilterOverlayState,
) -> Vec<String> {
    available_directories(conversations)
        .into_iter()
        .filter(|directory| fuzzy_match(directory, &overlay.directory_query))
        .collect()
}

fn filtered_overlay_sources(overlay: &FilterOverlayState) -> Vec<SessionSource> {
    crate::filters::ALL_SOURCES
        .into_iter()
        .filter(|source| fuzzy_match(&source.to_string(), &overlay.agent_query))
        .collect()
}

fn fuzzy_match(text: &str, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    let mut text_chars = text.chars().flat_map(char::to_lowercase);
    for query_char in query.chars().flat_map(char::to_lowercase) {
        if !text_chars.any(|text_char| text_char == query_char) {
            return false;
        }
    }
    true
}

fn picker_visible_rows() -> usize {
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    (rows as usize).saturating_sub(PICKER_HEADER_ROWS).max(1)
}

fn max_picker_scroll(len: usize, visible: usize) -> usize {
    len.saturating_sub(visible.max(1))
}

fn keep_picker_selection_visible(
    selected: usize,
    scroll: usize,
    len: usize,
    visible: usize,
) -> usize {
    if len == 0 {
        return 0;
    }

    let visible = visible.max(1);
    let selected = selected.min(len - 1);
    let max_scroll = max_picker_scroll(len, visible);
    let scroll = scroll.min(max_scroll);

    if selected < scroll {
        selected
    } else if selected >= scroll + visible {
        selected.saturating_sub(visible - 1).min(max_scroll)
    } else {
        scroll
    }
}

fn move_picker_selection_and_scroll(
    selected: usize,
    scroll: usize,
    len: usize,
    visible: usize,
    action: PickerKeyAction,
) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }

    let visible = visible.max(1);
    let selected = selected.min(len - 1);
    let scroll = keep_picker_selection_visible(selected, scroll, len, visible);
    let row = selected.saturating_sub(scroll).min(visible - 1);
    let max_scroll = max_picker_scroll(len, visible);

    let next_selected = match action {
        PickerKeyAction::MoveUp => selected.saturating_sub(1),
        PickerKeyAction::MoveDown => (selected + 1).min(len - 1),
        PickerKeyAction::PageUp => selected.saturating_sub(visible),
        PickerKeyAction::PageDown => selected.saturating_add(visible).min(len - 1),
        PickerKeyAction::Home => 0,
        PickerKeyAction::End => len - 1,
        _ => selected,
    };

    let next_scroll = match action {
        PickerKeyAction::PageUp | PickerKeyAction::PageDown => {
            next_selected.saturating_sub(row).min(max_scroll)
        }
        PickerKeyAction::Home => 0,
        PickerKeyAction::End => max_scroll,
        _ => keep_picker_selection_visible(next_selected, scroll, len, visible),
    };

    (next_selected, next_scroll)
}

// ── Picker (list view) ────────────────────────────────

fn picker_loop(
    stdout: &mut io::Stdout,
    store: &mut crate::session_store::SessionStore,
    state: &mut PickerState,
) -> PickerAction {
    loop {
        let conversations = store.conversations();
        poll_full_search_index(conversations, state);
        poll_full_search_result(conversations, state);

        let render_state = PickerRenderState {
            query: &state.query,
            filters: &state.filters,
            filter_overlay: state.filter_overlay.as_ref(),
            expanded_tree_roots: &state.expanded_tree_roots,
            selected: state.selected,
            scroll: state.scroll,
            flash: state.flash.as_deref(),
            focused_column: state.focused_column,
        };
        if draw_picker(stdout, conversations, &state.filtered_indices, render_state).is_err() {
            return PickerAction::Quit;
        }
        state.flash = None;

        let waiting_for_background =
            state.full_search_index_rx.is_some() || state.full_search_pending;
        let evt = if waiting_for_background {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(e) => e,
                    Err(_) => return PickerAction::Quit,
                },
                Ok(false) => continue,
                Err(_) => return PickerAction::Quit,
            }
        } else {
            match event::read() {
                Ok(e) => e,
                Err(_) => return PickerAction::Quit,
            }
        };

        if state.filter_overlay.is_some() {
            let action = filter_overlay_key_action(&evt);
            let mutated = handle_filter_overlay_key_action(store.conversations(), state, action);
            if mutated {
                load_missing_enabled_sources_for_filters(store, state);
            }
            continue;
        }

        match picker_key_action(&evt) {
            PickerKeyAction::Quit => return PickerAction::Quit,
            PickerKeyAction::ViewSession => {
                if !state.filtered_indices.is_empty() {
                    let idx = state.filtered_indices[state.selected];
                    return PickerAction::ViewSession(idx);
                }
            }
            PickerKeyAction::OpenFilterOverlay => {
                handle_picker_key_action(
                    store.conversations(),
                    state,
                    PickerKeyAction::OpenFilterOverlay,
                );
            }
            action @ (PickerKeyAction::MoveUp
            | PickerKeyAction::MoveDown
            | PickerKeyAction::PageUp
            | PickerKeyAction::PageDown
            | PickerKeyAction::Home
            | PickerKeyAction::End) => {
                (state.selected, state.scroll) = move_picker_selection_and_scroll(
                    state.selected,
                    state.scroll,
                    state.filtered_indices.len(),
                    picker_visible_rows(),
                    action,
                );
            }
            PickerKeyAction::Backspace => {
                state.query.pop();
                refilter(store.conversations(), state);
                start_full_search_index(store.conversations(), state);
                request_full_search(state);
            }
            PickerKeyAction::ToggleTreeExpansion => {
                toggle_tree_expansion(store.conversations(), state)
            }
            PickerKeyAction::PreviousColumn => {
                state.focused_column = state.focused_column.previous();
            }
            PickerKeyAction::NextColumn => {
                state.focused_column = state.focused_column.next();
            }
            PickerKeyAction::Type(c) => {
                state.query.push(c);
                refilter(store.conversations(), state);
                start_full_search_index(store.conversations(), state);
                request_full_search(state);
            }
            PickerKeyAction::Ignore => {}
        }
    }
}

fn load_missing_enabled_sources_for_filters(
    store: &mut crate::session_store::SessionStore,
    state: &mut PickerState,
) {
    if store.missing_enabled_sources(&state.filters).is_empty() {
        return;
    }

    let result = store.load_missing_enabled_sources(&state.filters);
    refresh_search_indexes(store.conversations(), state);
    refilter(store.conversations(), state);
    start_full_search_index(store.conversations(), state);
    request_full_search(state);

    match result {
        Ok(loaded) if !loaded.is_empty() => {
            let names = loaded
                .into_iter()
                .map(|source| source.to_string())
                .collect::<Vec<_>>()
                .join(",");
            state.flash = Some(format!("Loaded {}", names));
        }
        Ok(_) => {}
        Err(error) => {
            state.flash = Some(format!("Failed to load source: {}", error));
        }
    }
}

fn refilter(conversations: &[Conversation], state: &mut PickerState) {
    let searched_indices = if state.query.is_empty() {
        state.full_search_pending = false;
        (0..conversations.len()).collect()
    } else {
        search(conversations, &state.searchable, &state.query, Local::now())
    };
    apply_filtered_indices(conversations, state, searched_indices);
}

fn apply_filtered_indices(
    conversations: &[Conversation],
    state: &mut PickerState,
    searched_indices: Vec<usize>,
) {
    let base_indices = state
        .filters
        .filter_indices(conversations, searched_indices);
    state.filtered_indices = collapse_visible_indices(
        conversations,
        base_indices,
        &state.expanded_tree_roots,
        state.query.is_empty(),
    );
    if state.selected >= state.filtered_indices.len() {
        state.selected = state.filtered_indices.len().saturating_sub(1);
    }
    state.scroll = keep_picker_selection_visible(
        state.selected,
        state.scroll,
        state.filtered_indices.len(),
        picker_visible_rows(),
    );
}

fn refresh_search_indexes(conversations: &[Conversation], state: &mut PickerState) {
    state.searchable = precompute_search_text(conversations);
    state.full_search_index = None;
    state.full_search_index_rx = None;
    state.full_search_query_tx = None;
    state.full_search_result_rx = None;
    state.full_search_pending = false;
}

fn start_full_search_index(conversations: &[Conversation], state: &mut PickerState) {
    if state.query.is_empty()
        || state.full_search_index.is_some()
        || state.full_search_index_rx.is_some()
    {
        return;
    }

    let (tx, rx) = mpsc::channel();
    let conversations = conversations.to_vec();
    thread::spawn(move || {
        let index = precompute_full_search_index(&conversations);
        let _ = tx.send(index);
    });
    state.full_search_index_rx = Some(rx);
    state.flash = Some("Indexing full context".to_string());
}

fn poll_full_search_index(conversations: &[Conversation], state: &mut PickerState) {
    let result = state.full_search_index_rx.as_ref().map(|rx| rx.try_recv());

    match result {
        Some(Ok(index)) => {
            start_full_search_worker(conversations, &index, state);
            state.full_search_index = Some(index);
            state.full_search_index_rx = None;
            if !state.query.is_empty() {
                state.flash = Some("Indexed full context".to_string());
                request_full_search(state);
            }
        }
        Some(Err(TryRecvError::Disconnected)) => {
            state.full_search_index_rx = None;
        }
        Some(Err(TryRecvError::Empty)) | None => {}
    }
}

fn start_full_search_worker(
    conversations: &[Conversation],
    index: &FullSearchIndex,
    state: &mut PickerState,
) {
    if state.full_search_query_tx.is_some() {
        return;
    }

    let (query_tx, query_rx) = mpsc::channel::<String>();
    let (result_tx, result_rx) = mpsc::channel::<FullSearchResult>();
    let conversations = conversations.to_vec();
    let index = index.clone();

    thread::spawn(move || {
        while let Ok(mut query) = query_rx.recv() {
            while let Ok(next_query) = query_rx.try_recv() {
                query = next_query;
            }

            if query.trim().is_empty() {
                continue;
            }

            let indices = search_full(&conversations, &index, &query, Local::now());
            if result_tx.send(FullSearchResult { query, indices }).is_err() {
                break;
            }
        }
    });

    state.full_search_query_tx = Some(query_tx);
    state.full_search_result_rx = Some(result_rx);
}

fn request_full_search(state: &mut PickerState) {
    if state.query.is_empty() {
        return;
    }

    if let Some(tx) = state.full_search_query_tx.as_ref() {
        if tx.send(state.query.clone()).is_ok() {
            state.full_search_pending = true;
            state.flash = Some("Searching full context".to_string());
        } else {
            state.full_search_pending = false;
        }
    }
}

fn poll_full_search_result(conversations: &[Conversation], state: &mut PickerState) {
    let mut latest = None;
    let mut disconnected = false;

    if let Some(rx) = state.full_search_result_rx.as_ref() {
        loop {
            match rx.try_recv() {
                Ok(result) => latest = Some(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
    }

    if disconnected {
        state.full_search_result_rx = None;
        state.full_search_query_tx = None;
        state.full_search_pending = false;
    }

    let Some(result) = latest else {
        return;
    };

    if result.query == state.query {
        apply_filtered_indices(conversations, state, result.indices);
        state.full_search_pending = false;
        state.flash = Some("Searched full context".to_string());
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
    state.scroll = keep_picker_selection_visible(
        state.selected,
        state.scroll,
        state.filtered_indices.len(),
        picker_visible_rows(),
    );
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

struct PickerRenderState<'a> {
    query: &'a str,
    filters: &'a crate::filters::SessionFilters,
    filter_overlay: Option<&'a FilterOverlayState>,
    expanded_tree_roots: &'a HashSet<String>,
    selected: usize,
    scroll: usize,
    flash: Option<&'a str>,
    focused_column: PickerColumn,
}

fn draw_picker(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    filtered_indices: &[usize],
    render_state: PickerRenderState<'_>,
) -> io::Result<()> {
    let (cols, rows) = terminal::size()?;
    let cols = cols as usize;
    let rows = rows as usize;

    execute!(
        stdout,
        cursor::MoveTo(0, 0),
        terminal::Clear(ClearType::All)
    )?;

    // Line 0: search scope summary
    let filter_line =
        truncate_to_width_text(&format!("search: {}", render_state.filters.summary()), cols);
    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(filter_line),
        ResetColor,
    )?;

    // Line 1: search prompt
    let query_width = cols.saturating_sub(2);
    let query_text = truncate_to_width_text(render_state.query, query_width);
    execute!(
        stdout,
        cursor::MoveTo(0, 1),
        SetForegroundColor(Color::Yellow),
        SetAttribute(Attribute::Bold),
        Print("> "),
        ResetColor,
        Print(&query_text),
    )?;

    // Line 2: match count + hint + flash
    let count = format!("  {}/{}", filtered_indices.len(), conversations.len());
    let status_line = picker_status_line(&count, PICKER_HINT, render_state.flash, cols);
    execute!(
        stdout,
        cursor::MoveTo(0, 2),
        SetForegroundColor(Color::DarkGrey),
        Print(status_line),
        ResetColor,
    )?;

    // Lines 4..rows: session list
    let list_start = PICKER_HEADER_ROWS;
    let visible = rows.saturating_sub(list_start);
    let selected = render_state
        .selected
        .min(filtered_indices.len().saturating_sub(1));
    let scroll = keep_picker_selection_visible(
        selected,
        render_state.scroll,
        filtered_indices.len(),
        visible,
    );
    let hierarchy_width = picker_hierarchy_gutter_width(
        conversations,
        filtered_indices,
        scroll,
        visible,
        render_state.expanded_tree_roots,
        render_state.query.is_empty(),
    );

    // Line 3: session table header
    execute!(stdout, cursor::MoveTo(0, 3))?;
    draw_picker_column_header(stdout, cols, hierarchy_width, render_state.focused_column)?;

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
            hierarchy_width,
            is_selected,
            render_state.expanded_tree_roots,
            render_state.query.is_empty(),
        )?;

        if is_selected {
            execute!(stdout, SetAttribute(Attribute::NoReverse))?;
        }
    }

    if let Some(overlay) = render_state.filter_overlay {
        draw_filter_overlay(
            stdout,
            conversations,
            render_state.filters,
            overlay,
            cols,
            rows,
        )?;
    }

    let cursor_col = 2usize
        .saturating_add(UnicodeWidthStr::width(render_state.query))
        .min(cols.saturating_sub(1));
    execute!(stdout, cursor::MoveTo(cursor_col as u16, 1))?;
    stdout.flush()?;
    Ok(())
}

fn picker_status_line(count: &str, hint: &str, flash: Option<&str>, cols: usize) -> String {
    let mut line = format!("{}{}", count, hint);
    if let Some(flash) = flash.filter(|flash| !flash.is_empty()) {
        let gap = cols
            .saturating_sub(UnicodeWidthStr::width(line.as_str()) + UnicodeWidthStr::width(flash))
            .max(1);
        line.push_str(&" ".repeat(gap));
        line.push_str(flash);
    }

    truncate_to_width_text(&line, cols)
}

fn truncate_to_width_text(text: &str, cols: usize) -> String {
    truncate_to_display_width(text, cols)
}

fn truncate_to_display_width(text: &str, max_width: usize) -> String {
    let mut rendered_width = 0;
    let mut clipped = String::new();

    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if rendered_width + ch_width > max_width {
            break;
        }
        rendered_width += ch_width;
        clipped.push(ch);
    }

    clipped
}

fn draw_filter_overlay(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    filters: &crate::filters::SessionFilters,
    overlay: &FilterOverlayState,
    cols: usize,
    rows: usize,
) -> io::Result<()> {
    let panel = filter_overlay_panel(conversations, filters, overlay);
    let line_count = panel.right_rows.len().max(FILTER_OVERLAY_MIN_BODY_ROWS)
        + FILTER_OVERLAY_VERTICAL_PADDING * 2
        + 2;
    let (x, y, width, height) = filter_overlay_bounds(cols, rows, line_count);
    let nav_width = filter_overlay_nav_width(width);
    let nav_content_width = nav_width.saturating_sub(FILTER_OVERLAY_INNER_PADDING * 2);
    let right_width = width.saturating_sub(nav_width + FILTER_OVERLAY_INNER_PADDING * 3 + 3);

    draw_filter_overlay_backdrop(stdout, x, y, width, height, cols, rows)?;
    draw_filter_overlay_border(stdout, x, y, width, height, overlay.section)?;
    draw_filter_overlay_nav(
        stdout,
        x + 1 + FILTER_OVERLAY_INNER_PADDING,
        y + 1 + FILTER_OVERLAY_VERTICAL_PADDING,
        nav_content_width,
        &panel,
        overlay,
    )?;
    draw_filter_overlay_divider(
        stdout,
        x + 1 + nav_width,
        y + 1 + FILTER_OVERLAY_VERTICAL_PADDING,
        height.saturating_sub(2 + FILTER_OVERLAY_VERTICAL_PADDING * 2),
    )?;

    let right_x = x + nav_width + FILTER_OVERLAY_INNER_PADDING * 2 + 2;
    let body_rows = height.saturating_sub(2 + FILTER_OVERLAY_VERTICAL_PADDING * 2);
    for row in 0..body_rows {
        let content_y = y + 1 + FILTER_OVERLAY_VERTICAL_PADDING + row;
        let (content, selected) = panel
            .right_rows
            .get(row)
            .map(|line| (line.content.as_str(), line.selected))
            .unwrap_or(("", false));
        execute!(
            stdout,
            cursor::MoveTo(right_x as u16, content_y as u16),
            SetBackgroundColor(Color::Black),
            Print(" ".repeat(right_width)),
            cursor::MoveTo(right_x as u16, content_y as u16),
        )?;
        if selected {
            execute!(stdout, SetAttribute(Attribute::Reverse))?;
        }

        let text = truncate(content, right_width);
        execute!(stdout, Print(format!("{:<right_width$}", text)))?;

        if selected {
            execute!(stdout, SetAttribute(Attribute::NoReverse))?;
        }
        execute!(stdout, ResetColor)?;
    }

    Ok(())
}

const FILTER_OVERLAY_MIN_BODY_ROWS: usize = 7;
const FILTER_OVERLAY_INNER_PADDING: usize = 2;
const FILTER_OVERLAY_VERTICAL_PADDING: usize = 1;

#[derive(Debug, Eq, PartialEq)]
struct FilterOverlayPanel {
    agent_summary: String,
    directory_summary: String,
    right_rows: Vec<FilterOverlayLine>,
}

#[derive(Debug, Eq, PartialEq)]
struct FilterOverlayLine {
    content: String,
    selected: bool,
}

fn filter_overlay_panel(
    conversations: &[Conversation],
    filters: &crate::filters::SessionFilters,
    overlay: &FilterOverlayState,
) -> FilterOverlayPanel {
    let right_rows = match overlay.section {
        FilterSection::Agent => filter_overlay_agent_rows(filters, overlay),
        FilterSection::Directory => filter_overlay_directory_rows(conversations, filters, overlay),
    };

    FilterOverlayPanel {
        agent_summary: format!(
            "{}/{}",
            selected_source_count(filters, overlay),
            filtered_overlay_sources(overlay).len()
        ),
        directory_summary: {
            let directories = filtered_overlay_directories(conversations, overlay);
            format!(
                "{}/{}",
                selected_directory_count(filters, &directories),
                directories.len()
            )
        },
        right_rows,
    }
}

fn filter_overlay_agent_rows(
    filters: &crate::filters::SessionFilters,
    overlay: &FilterOverlayState,
) -> Vec<FilterOverlayLine> {
    let sources = filtered_overlay_sources(overlay);
    let mut rows = vec![
        FilterOverlayLine {
            content: filter_overlay_search_line("agent", &overlay.agent_query),
            selected: false,
        },
        FilterOverlayLine {
            content: filter_overlay_scope_line(
                overlay.agent_query.as_str(),
                selected_source_count(filters, overlay),
                sources.len(),
                overlay.agent_selected == 0,
            ),
            selected: overlay.agent_selected == 0,
        },
    ];

    if sources.is_empty() {
        rows.push(FilterOverlayLine {
            content: "No agents".to_string(),
            selected: false,
        });
    } else {
        rows.extend(sources.into_iter().enumerate().map(|(idx, source)| {
            let checked = if filters.source_enabled(source) {
                "[x]"
            } else {
                "[ ]"
            };
            FilterOverlayLine {
                content: format!("{} {}", checked, source),
                selected: idx + 1 == overlay.agent_selected,
            }
        }));
    }

    rows
}

fn filter_overlay_directory_rows(
    conversations: &[Conversation],
    filters: &crate::filters::SessionFilters,
    overlay: &FilterOverlayState,
) -> Vec<FilterOverlayLine> {
    let directories = filtered_overlay_directories(conversations, overlay);
    let mut rows = vec![
        FilterOverlayLine {
            content: filter_overlay_search_line("directory", &overlay.directory_query),
            selected: false,
        },
        FilterOverlayLine {
            content: filter_overlay_scope_line(
                overlay.directory_query.as_str(),
                selected_directory_count(filters, &directories),
                directories.len(),
                overlay.directory_selected == 0,
            ),
            selected: overlay.directory_selected == 0,
        },
    ];

    if directories.is_empty() {
        rows.push(FilterOverlayLine {
            content: "No directories".to_string(),
            selected: false,
        });
    } else {
        rows.extend(directories.into_iter().enumerate().map(|(idx, directory)| {
            let checked = if filters.directory_enabled(&directory) {
                "[x]"
            } else {
                "[ ]"
            };
            FilterOverlayLine {
                content: format!("{} {}", checked, directory),
                selected: idx + 1 == overlay.directory_selected,
            }
        }));
    }

    rows
}

fn filter_overlay_search_line(label: &str, query: &str) -> String {
    if query.is_empty() {
        format!("Search {}: _", label)
    } else {
        format!("Search {}: {}", label, query)
    }
}

fn draw_filter_overlay_backdrop(
    stdout: &mut io::Stdout,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    cols: usize,
    rows: usize,
) -> io::Result<()> {
    let (clear_x, clear_y, clear_width, clear_height) =
        filter_overlay_backdrop_bounds(x, y, width, height, cols, rows);

    for row in 0..clear_height {
        execute!(
            stdout,
            cursor::MoveTo(clear_x as u16, (clear_y + row) as u16),
            Print(" ".repeat(clear_width)),
            ResetColor
        )?;
    }

    Ok(())
}

fn filter_overlay_backdrop_bounds(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    cols: usize,
    rows: usize,
) -> (usize, usize, usize, usize) {
    let clear_x = x.saturating_sub(FILTER_OVERLAY_INNER_PADDING);
    let clear_y = y.saturating_sub(FILTER_OVERLAY_VERTICAL_PADDING);
    let clear_right = (x + width + FILTER_OVERLAY_INNER_PADDING).min(cols);
    let clear_bottom = (y + height + FILTER_OVERLAY_VERTICAL_PADDING).min(rows);

    (
        clear_x,
        clear_y,
        clear_right.saturating_sub(clear_x),
        clear_bottom.saturating_sub(clear_y),
    )
}

fn draw_filter_overlay_border(
    stdout: &mut io::Stdout,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    section: FilterSection,
) -> io::Result<()> {
    if width < 2 || height < 2 {
        return Ok(());
    }

    let title = filter_overlay_title(section);
    let top_line = filter_overlay_top_border(width, title);
    let bottom_line = format!("└{}┘", "─".repeat(width.saturating_sub(2)));

    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        SetBackgroundColor(Color::Black),
        cursor::MoveTo(x as u16, y as u16),
        Print(top_line)
    )?;

    for row in 1..height.saturating_sub(1) {
        execute!(
            stdout,
            cursor::MoveTo(x as u16, (y + row) as u16),
            Print("│"),
            Print(" ".repeat(width.saturating_sub(2))),
            Print("│")
        )?;
    }

    execute!(
        stdout,
        cursor::MoveTo(x as u16, (y + height - 1) as u16),
        Print(bottom_line),
        ResetColor
    )?;

    Ok(())
}

fn filter_overlay_title(section: FilterSection) -> &'static str {
    match section {
        FilterSection::Agent => " Search · agent ",
        FilterSection::Directory => " Search · directory ",
    }
}

fn filter_overlay_top_border(width: usize, title: &str) -> String {
    if width <= 2 {
        return "─".repeat(width);
    }

    let available = width.saturating_sub(2);
    let title = truncate(title, available);
    let remainder = available.saturating_sub(title.chars().count());
    format!("┌{}{}┐", title, "─".repeat(remainder))
}

fn draw_filter_overlay_nav(
    stdout: &mut io::Stdout,
    x: usize,
    y: usize,
    width: usize,
    panel: &FilterOverlayPanel,
    overlay: &FilterOverlayState,
) -> io::Result<()> {
    let rows = [
        (
            format!("agent   {}", panel.agent_summary),
            overlay.section == FilterSection::Agent,
        ),
        (
            format!("directory {}", panel.directory_summary),
            overlay.section == FilterSection::Directory,
        ),
        ("".to_string(), false),
        ("Esc close".to_string(), false),
        ("←/→ section".to_string(), false),
        ("type narrow".to_string(), false),
        ("Space toggle".to_string(), false),
    ];

    for (idx, (content, selected)) in rows.into_iter().enumerate() {
        execute!(
            stdout,
            cursor::MoveTo(x as u16, (y + idx) as u16),
            SetBackgroundColor(Color::Black)
        )?;
        if selected {
            execute!(stdout, SetAttribute(Attribute::Reverse))?;
        } else if idx >= 3 {
            execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        }

        let text = truncate(&content, width);
        execute!(stdout, Print(format!("{:<width$}", text)))?;

        if selected {
            execute!(stdout, SetAttribute(Attribute::NoReverse))?;
        }
        execute!(stdout, ResetColor)?;
    }

    Ok(())
}

fn draw_filter_overlay_divider(
    stdout: &mut io::Stdout,
    x: usize,
    y: usize,
    height: usize,
) -> io::Result<()> {
    execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
    for row in 0..height {
        execute!(
            stdout,
            cursor::MoveTo(x as u16, (y + row) as u16),
            SetBackgroundColor(Color::Black),
            Print("│")
        )?;
    }
    execute!(stdout, ResetColor)?;
    Ok(())
}

fn filter_overlay_nav_width(width: usize) -> usize {
    let available = width.saturating_sub(4);
    available
        .min(FILTER_OVERLAY_MAX_NAV_WIDTH)
        .max(available.min(FILTER_OVERLAY_MIN_NAV_WIDTH))
}

fn filter_overlay_scope_line(
    query: &str,
    selected_count: usize,
    total_count: usize,
    selected: bool,
) -> String {
    let label = if query.trim().is_empty() {
        "all".to_string()
    } else {
        query.to_string()
    };
    let marker = if selected { ">" } else { " " };
    format!(
        "{} {} · {}/{} selected",
        marker, label, selected_count, total_count
    )
}

fn selected_source_count(
    filters: &crate::filters::SessionFilters,
    overlay: &FilterOverlayState,
) -> usize {
    filtered_overlay_sources(overlay)
        .into_iter()
        .filter(|source| filters.source_enabled(*source))
        .count()
}

fn selected_directory_count(
    filters: &crate::filters::SessionFilters,
    directories: &[String],
) -> usize {
    directories
        .iter()
        .filter(|directory| filters.directory_enabled(directory))
        .count()
}

fn filter_overlay_bounds(
    cols: usize,
    rows: usize,
    line_count: usize,
) -> (usize, usize, usize, usize) {
    let width = cols.min(80).max(cols.min(52));
    let max_height = rows.saturating_sub(2).max(1);
    let height = line_count.min(max_height);
    let x = cols.saturating_sub(width) / 2;
    let y = rows.saturating_sub(height) / 2;

    (x, y, width, height)
}

fn draw_picker_column_header(
    stdout: &mut io::Stdout,
    max_width: usize,
    hierarchy_width: usize,
    focused_column: PickerColumn,
) -> io::Result<()> {
    let fixed_cells = [
        HeaderCell::spacer(1),
        HeaderCell::new(
            "agent",
            PICKER_SOURCE_WIDTH,
            focused_column == PickerColumn::Agent,
            HeaderAlign::Left,
        ),
        HeaderCell::spacer(1),
        HeaderCell::new(
            "age",
            PICKER_AGE_WIDTH,
            focused_column == PickerColumn::Age,
            HeaderAlign::Right,
        ),
        HeaderCell::spacer(2),
        HeaderCell::spacer(hierarchy_width),
        HeaderCell::new(
            "directory",
            PICKER_DIRECTORY_WIDTH,
            focused_column == PickerColumn::Directory,
            HeaderAlign::Left,
        ),
        HeaderCell::spacer(2),
        HeaderCell::new(
            "model",
            PICKER_MODEL_WIDTH,
            focused_column == PickerColumn::Model,
            HeaderAlign::Left,
        ),
        HeaderCell::spacer(2),
    ];
    draw_picker_header_cells(stdout, &fixed_cells)?;

    let fixed_width = picker_row_fixed_width(hierarchy_width);
    if max_width > fixed_width {
        let preview_width = max_width - fixed_width;
        let preview = HeaderCell::new(
            "preview",
            preview_width,
            focused_column == PickerColumn::Preview,
            HeaderAlign::Left,
        );
        draw_picker_header_cells(stdout, &[preview])?;
    }

    execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

#[derive(Clone, Copy)]
struct HeaderCell<'a> {
    label: &'a str,
    width: usize,
    focused: bool,
    align: HeaderAlign,
}

impl<'a> HeaderCell<'a> {
    fn new(label: &'a str, width: usize, focused: bool, align: HeaderAlign) -> Self {
        Self {
            label,
            width,
            focused,
            align,
        }
    }

    fn spacer(width: usize) -> Self {
        Self::new("", width, false, HeaderAlign::Left)
    }
}

#[derive(Clone, Copy)]
enum HeaderAlign {
    Left,
    Right,
}

fn draw_picker_header_cells(stdout: &mut io::Stdout, cells: &[HeaderCell<'_>]) -> io::Result<()> {
    for cell in cells {
        draw_picker_header_cell(stdout, cell.label, cell.width, cell.focused, cell.align)?;
    }
    Ok(())
}

fn draw_picker_header_cell(
    stdout: &mut io::Stdout,
    label: &str,
    width: usize,
    focused: bool,
    align: HeaderAlign,
) -> io::Result<()> {
    let label = truncate(label, width);
    let text = match align {
        HeaderAlign::Left => format!("{:<width$}", label, width = width),
        HeaderAlign::Right => format!("{:>width$}", label, width = width),
    };

    execute!(
        stdout,
        SetForegroundColor(if focused {
            Color::Yellow
        } else {
            Color::DarkGrey
        }),
        SetAttribute(if focused {
            Attribute::Bold
        } else {
            Attribute::NormalIntensity
        }),
        Print(text),
    )?;
    Ok(())
}

fn picker_row_fixed_width(hierarchy_width: usize) -> usize {
    1 + PICKER_SOURCE_WIDTH
        + 1
        + PICKER_AGE_WIDTH
        + 2
        + hierarchy_width
        + PICKER_DIRECTORY_WIDTH
        + 2
        + PICKER_MODEL_WIDTH
        + 2
}

fn picker_hierarchy_gutter_width(
    conversations: &[Conversation],
    filtered_indices: &[usize],
    scroll: usize,
    visible: usize,
    expanded_tree_roots: &HashSet<String>,
    collapse_enabled: bool,
) -> usize {
    filtered_indices
        .iter()
        .skip(scroll)
        .take(visible)
        .map(|idx| {
            picker_hierarchy_marker(&conversations[*idx], expanded_tree_roots, collapse_enabled)
                .chars()
                .count()
        })
        .max()
        .unwrap_or(0)
        .clamp(PICKER_MIN_HIERARCHY_GUTTER_WIDTH, HIERARCHY_GUTTER_WIDTH)
}

fn draw_session_line(
    stdout: &mut io::Stdout,
    conv: &Conversation,
    max_width: usize,
    hierarchy_width: usize,
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
    let directory = format_directory_label(conv);
    let model = format_model_short(conv.model.as_deref());
    let title = get_display_title(conv);

    let model_inner_width = PICKER_MODEL_WIDTH.saturating_sub(2);
    let model_display = format!(
        "({:<inner_width$})",
        truncate(&model, model_inner_width),
        inner_width = model_inner_width
    );
    let preview_max = max_width.saturating_sub(picker_row_fixed_width(hierarchy_width));
    let preview = truncate(&title, preview_max.max(10));

    execute!(
        stdout,
        Print(" "),
        SetForegroundColor(source_color),
        Print(format!(
            "{:<width$}",
            format!("[{}]", source_tag),
            width = PICKER_SOURCE_WIDTH
        )),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    execute!(
        stdout,
        Print(format!(" {:>width$}  ", age, width = PICKER_AGE_WIDTH))
    )?;

    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print(format!("{:<width$}", hierarchy, width = hierarchy_width)),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    let dir_display: String = directory.chars().take(PICKER_DIRECTORY_WIDTH).collect();
    execute!(
        stdout,
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "{:<width$}",
            dir_display,
            width = PICKER_DIRECTORY_WIDTH
        )),
        ResetColor,
    )?;
    if is_selected {
        execute!(stdout, SetAttribute(Attribute::Reverse))?;
    }

    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print(format!(
            "  {:<width$}",
            model_display,
            width = PICKER_MODEL_WIDTH
        )),
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
        let line_so_far = picker_row_fixed_width(hierarchy_width) + clean_preview.len();
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
            "▾".to_string()
        } else {
            "▸".to_string()
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
            directory_name: Some("directory".to_string()),
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
    fn initial_filtered_indices_keep_full_conversations_and_narrow_visible_rows() {
        let conversations = vec![conversation("matching", 0, false), {
            let mut conv = conversation("other", 0, false);
            conv.directory_name = Some("other".to_string());
            conv
        }];
        let mut filters = crate::filters::SessionFilters::all();
        filters.only_directory("directory");
        let expanded = HashSet::new();

        let visible = initial_filtered_indices(&conversations, &filters, &expanded);

        assert_eq!(visible, vec![0]);
        assert_eq!(conversations.len(), 2);
    }

    #[test]
    fn refilter_applies_source_filter_before_search() {
        let conversations = vec![
            {
                let mut conv = conversation("codex-match", 0, false);
                conv.source = SessionSource::Codex;
                conv.preview = "match".to_string();
                conv
            },
            {
                let mut conv = conversation("claude-match", 0, false);
                conv.source = SessionSource::Claude;
                conv.preview = "match".to_string();
                conv
            },
        ];
        let mut state = PickerState::for_test(
            &conversations,
            crate::filters::SessionFilters::source_only(SessionSource::Codex),
        );
        state.query = "match".to_string();

        refilter(&conversations, &mut state);

        assert_eq!(state.filtered_indices, vec![0]);
    }

    #[test]
    fn slash_opens_search_overlay_for_focused_agent_column() {
        let conversations = vec![conversation("codex", 0, false)];
        let mut state =
            PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
        state.focused_column = PickerColumn::Agent;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        handle_picker_key_action(&conversations, &mut state, picker_key_action(&event));

        assert_eq!(
            state.filter_overlay,
            Some(FilterOverlayState {
                section: FilterSection::Agent,
                agent_selected: 0,
                agent_query: String::new(),
                directory_selected: 0,
                directory_query: String::new(),
            })
        );
    }

    #[test]
    fn slash_opens_search_overlay_for_focused_directory_column() {
        let conversations = vec![conversation("codex", 0, false)];
        let mut state =
            PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
        state.focused_column = PickerColumn::Directory;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        handle_picker_key_action(&conversations, &mut state, picker_key_action(&event));

        assert_eq!(
            state.filter_overlay,
            Some(FilterOverlayState {
                section: FilterSection::Directory,
                agent_selected: 0,
                agent_query: String::new(),
                directory_selected: 0,
                directory_query: String::new(),
            })
        );
    }

    #[test]
    fn slash_opens_directory_search_for_default_preview_column() {
        let conversations = vec![conversation("codex", 0, false)];
        let mut state =
            PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
        let event = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        handle_picker_key_action(&conversations, &mut state, picker_key_action(&event));

        assert_eq!(
            state.filter_overlay.as_ref().map(|overlay| overlay.section),
            Some(FilterSection::Directory)
        );
    }

    #[test]
    fn slash_opens_directory_search_for_focused_age_and_model_columns() {
        let conversations = vec![conversation("codex", 0, false)];
        let mut state =
            PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
        let event = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        state.focused_column = PickerColumn::Age;
        handle_picker_key_action(&conversations, &mut state, picker_key_action(&event));
        assert_eq!(
            state.filter_overlay.as_ref().map(|overlay| overlay.section),
            Some(FilterSection::Directory)
        );

        state.filter_overlay = None;
        state.focused_column = PickerColumn::Model;
        handle_picker_key_action(&conversations, &mut state, picker_key_action(&event));
        assert_eq!(
            state.filter_overlay.as_ref().map(|overlay| overlay.section),
            Some(FilterSection::Directory)
        );
    }

    #[test]
    fn filter_overlay_escape_closes_without_resetting_filters() {
        let conversations = vec![conversation("codex", 0, false)];
        let mut state = PickerState::for_test(
            &conversations,
            crate::filters::SessionFilters::source_only(SessionSource::Codex),
        );
        state.filter_overlay = Some(FilterOverlayState::new(FilterSection::Agent));

        let mutated = handle_filter_overlay_key_action(
            &conversations,
            &mut state,
            FilterOverlayKeyAction::Close,
        );

        assert!(!mutated);
        assert!(state.filter_overlay.is_none());
        assert!(state.filters.source_enabled(SessionSource::Codex));
        assert!(!state.filters.source_enabled(SessionSource::Claude));
    }

    #[test]
    fn directory_overlay_scope_row_toggles_all_and_none() {
        let conversations = vec![conversation("frontend", 0, false), {
            let mut conv = conversation("backend", 0, false);
            conv.directory_name = Some("backend".to_string());
            conv
        }];
        let mut state =
            PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
        state.filter_overlay = Some(FilterOverlayState {
            section: FilterSection::Directory,
            agent_selected: 0,
            agent_query: String::new(),
            directory_selected: 0,
            directory_query: String::new(),
        });

        let disabled_all = handle_filter_overlay_key_action(
            &conversations,
            &mut state,
            filter_overlay_key_action(&Event::Key(KeyEvent::new(
                KeyCode::Char(' '),
                KeyModifiers::NONE,
            ))),
        );
        assert!(disabled_all);
        assert_eq!(state.filtered_indices, Vec::<usize>::new());

        let enabled_all = handle_filter_overlay_key_action(
            &conversations,
            &mut state,
            filter_overlay_key_action(&Event::Key(KeyEvent::new(
                KeyCode::Char(' '),
                KeyModifiers::NONE,
            ))),
        );
        assert!(enabled_all);
        assert_eq!(state.filtered_indices, vec![0, 1]);
    }

    #[test]
    fn directory_overlay_plain_a_types_query() {
        let conversations = vec![conversation("alpha", 0, false)];
        let mut state =
            PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
        state.filter_overlay = Some(FilterOverlayState {
            section: FilterSection::Directory,
            agent_selected: 0,
            agent_query: String::new(),
            directory_selected: 0,
            directory_query: String::new(),
        });

        let mutated = handle_filter_overlay_key_action(
            &conversations,
            &mut state,
            filter_overlay_key_action(&Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            ))),
        );

        assert!(!mutated);
        assert_eq!(
            state.filter_overlay.as_ref().unwrap().directory_query,
            "a".to_string()
        );
    }

    #[test]
    fn directory_toggle_from_all_uses_specific_known_directory_set() {
        let conversations = vec![
            {
                let mut conv = conversation("a", 0, false);
                conv.directory_name = Some("a".to_string());
                conv
            },
            {
                let mut conv = conversation("b", 0, false);
                conv.directory_name = Some("b".to_string());
                conv
            },
        ];
        let mut state =
            PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
        state.filter_overlay = Some(FilterOverlayState {
            section: FilterSection::Directory,
            agent_selected: 0,
            agent_query: String::new(),
            directory_selected: 1,
            directory_query: String::new(),
        });

        let mutated = handle_filter_overlay_key_action(
            &conversations,
            &mut state,
            FilterOverlayKeyAction::Toggle,
        );

        let mut later_directory = conversation("c", 0, false);
        later_directory.directory_name = Some("c".to_string());

        assert!(mutated);
        assert!(!state.filters.directory_enabled("a"));
        assert!(state.filters.directory_enabled("b"));
        assert!(!state.filters.directory_enabled("c"));
        assert!(state.filters.matches(&conversations[1]));
        assert!(!state.filters.matches(&later_directory));
    }

    #[test]
    fn overlay_source_toggle_allows_empty_sources() {
        let conversations = vec![conversation("codex", 0, false)];
        let mut state = PickerState::for_test(
            &conversations,
            crate::filters::SessionFilters::source_only(SessionSource::Codex),
        );
        state.filter_overlay = Some(FilterOverlayState {
            section: FilterSection::Agent,
            agent_selected: 2,
            agent_query: String::new(),
            directory_selected: 0,
            directory_query: String::new(),
        });

        let mutated = handle_filter_overlay_key_action(
            &conversations,
            &mut state,
            FilterOverlayKeyAction::Toggle,
        );

        assert!(mutated);
        assert!(!state.filters.source_enabled(SessionSource::Codex));
        assert_eq!(state.filtered_indices, Vec::<usize>::new());
    }

    #[test]
    fn filter_overlay_bounds_centers_actual_rendered_height() {
        let (_, y, _, height) = filter_overlay_bounds(80, 24, 13);

        assert_eq!(y, 5);
        assert_eq!(height, 13);
    }

    #[test]
    fn filter_overlay_panel_uses_left_nav_summaries_and_search_row() {
        let conversations = vec![
            {
                let mut conv = conversation("wors-alpha", 0, false);
                conv.directory_name = Some("wors-alpha".to_string());
                conv
            },
            {
                let mut conv = conversation("wors-beta", 0, false);
                conv.directory_name = Some("wors-beta".to_string());
                conv
            },
        ];
        let mut overlay = FilterOverlayState {
            section: FilterSection::Directory,
            agent_selected: 0,
            agent_query: String::new(),
            directory_selected: 0,
            directory_query: "wors".to_string(),
        };
        let filters = crate::filters::SessionFilters::all();

        let panel = filter_overlay_panel(&conversations, &filters, &overlay);

        assert_eq!(panel.agent_summary, "2/2");
        assert_eq!(panel.directory_summary, "2/2");
        assert_eq!(panel.right_rows[0].content, "Search directory: wors");
        assert_eq!(panel.right_rows[1].content, "> wors · 2/2 selected");
        assert!(panel.right_rows[1].selected);

        overlay.directory_query = String::new();
        let panel = filter_overlay_panel(&conversations, &filters, &overlay);
        assert_eq!(panel.right_rows[0].content, "Search directory: _");
        assert_eq!(panel.right_rows[1].content, "> all · 2/2 selected");
    }

    #[test]
    fn filter_overlay_nav_width_fits_directory_count() {
        let content_width =
            filter_overlay_nav_width(80).saturating_sub(FILTER_OVERLAY_INNER_PADDING * 2);

        assert!(content_width >= "directory 99/99".len());
    }

    #[test]
    fn filter_overlay_nav_width_does_not_exceed_available_width() {
        assert!(filter_overlay_nav_width(10) <= 6);
    }

    #[test]
    fn filter_overlay_border_title_is_embedded_in_top_rule() {
        assert_eq!(
            filter_overlay_top_border(24, filter_overlay_title(FilterSection::Directory)),
            "┌ Search · directory ──┐"
        );
    }

    #[test]
    fn filter_overlay_backdrop_adds_clipped_padding() {
        assert_eq!(
            filter_overlay_backdrop_bounds(10, 5, 40, 8, 80, 24),
            (8, 4, 44, 10)
        );
        assert_eq!(
            filter_overlay_backdrop_bounds(0, 0, 40, 8, 42, 9),
            (0, 0, 42, 9)
        );
    }

    #[test]
    fn picker_status_line_never_exceeds_terminal_width() {
        let line = picker_status_line(
            "  123/456",
            "  Enter: view",
            Some("Loaded claude,codex"),
            20,
        );

        assert_eq!(line.len(), 20);
    }

    #[test]
    fn picker_header_rows_include_column_header() {
        assert_eq!(PICKER_HEADER_ROWS, 4);
    }

    #[test]
    fn picker_row_fixed_width_matches_column_widths() {
        let hierarchy_width = 4;
        let expected = 1
            + PICKER_SOURCE_WIDTH
            + 1
            + PICKER_AGE_WIDTH
            + 2
            + hierarchy_width
            + PICKER_DIRECTORY_WIDTH
            + 2
            + PICKER_MODEL_WIDTH
            + 2;

        assert_eq!(picker_row_fixed_width(hierarchy_width), expected);
    }

    #[test]
    fn picker_hierarchy_gutter_width_tracks_visible_rows() {
        let conversations = vec![
            conversation("root", 0, true),
            conversation("child", 1, false),
            conversation("nested", 2, false),
            conversation("plain", 0, false),
        ];
        let expanded = HashSet::from(["root".to_string()]);

        assert_eq!(
            picker_hierarchy_gutter_width(&conversations, &[0], 0, 1, &HashSet::new(), true),
            3
        );
        assert_eq!(
            picker_hierarchy_gutter_width(&conversations, &[0, 1], 0, 2, &expanded, true),
            3
        );
        assert_eq!(
            picker_hierarchy_gutter_width(&conversations, &[2], 0, 1, &expanded, true),
            4
        );
        assert_eq!(
            picker_hierarchy_gutter_width(&conversations, &[3], 0, 1, &expanded, true),
            3
        );
    }

    #[test]
    fn truncates_header_text_by_display_width() {
        assert_eq!(truncate_to_display_width("ab界c", 3), "ab");
        assert_eq!(truncate_to_display_width("ab界c", 4), "ab界");
    }

    #[test]
    fn picker_hierarchy_marker_marks_collapsed_and_expanded_roots() {
        let root = conversation("root", 0, true);
        let child = conversation("child", 1, false);
        let collapsed = HashSet::new();
        let expanded = HashSet::from(["root".to_string()]);

        assert_eq!(
            picker_hierarchy_marker(&root, &collapsed, true),
            "▸".to_string()
        );
        assert_eq!(
            picker_hierarchy_marker(&root, &expanded, true),
            "▾".to_string()
        );
        assert_eq!(
            picker_hierarchy_marker(&child, &collapsed, true),
            format_hierarchy_marker(&child)
        );
    }

    #[test]
    fn picker_column_focus_defaults_to_preview() {
        let conversations = vec![conversation("session", 0, false)];
        let state = PickerState::for_test(&conversations, crate::filters::SessionFilters::all());

        assert_eq!(state.focused_column, PickerColumn::Preview);
    }

    #[test]
    fn picker_column_focus_moves_across_visible_columns() {
        assert_eq!(PickerColumn::Preview.next(), PickerColumn::Agent);
        assert_eq!(PickerColumn::Preview.previous(), PickerColumn::Model);
        assert_eq!(PickerColumn::Directory.next(), PickerColumn::Model);
        assert_eq!(PickerColumn::Directory.previous(), PickerColumn::Age);
    }

    #[test]
    fn picker_keymap_supports_column_navigation() {
        assert_eq!(
            picker_key_action(&Event::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE
            ))),
            PickerKeyAction::PreviousColumn
        );
        assert_eq!(
            picker_key_action(&Event::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::NONE
            ))),
            PickerKeyAction::NextColumn
        );
    }

    #[test]
    fn picker_keymap_supports_page_and_home_end_navigation() {
        assert_eq!(
            picker_key_action(&Event::Key(KeyEvent::new(
                KeyCode::PageDown,
                KeyModifiers::NONE
            ))),
            PickerKeyAction::PageDown
        );
        assert_eq!(
            picker_key_action(&Event::Key(KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::NONE
            ))),
            PickerKeyAction::PageUp
        );
        assert_eq!(
            picker_key_action(&Event::Key(KeyEvent::new(
                KeyCode::Home,
                KeyModifiers::NONE
            ))),
            PickerKeyAction::Home
        );
        assert_eq!(
            picker_key_action(&Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))),
            PickerKeyAction::End
        );
    }

    #[test]
    fn picker_navigation_pages_and_jumps_within_bounds() {
        assert_eq!(
            move_picker_selection_and_scroll(0, 0, 20, 5, PickerKeyAction::PageDown),
            (5, 5)
        );
        assert_eq!(
            move_picker_selection_and_scroll(4, 0, 20, 5, PickerKeyAction::PageDown),
            (9, 5)
        );
        assert_eq!(
            move_picker_selection_and_scroll(9, 5, 20, 5, PickerKeyAction::PageUp),
            (4, 0)
        );
        assert_eq!(
            move_picker_selection_and_scroll(7, 3, 20, 5, PickerKeyAction::Home),
            (0, 0)
        );
        assert_eq!(
            move_picker_selection_and_scroll(7, 3, 20, 5, PickerKeyAction::End),
            (19, 15)
        );
        assert_eq!(
            move_picker_selection_and_scroll(7, 3, 0, 5, PickerKeyAction::End),
            (0, 0)
        );
    }

    #[test]
    fn picker_arrow_navigation_scrolls_only_at_view_edges() {
        assert_eq!(
            move_picker_selection_and_scroll(2, 0, 20, 5, PickerKeyAction::MoveDown),
            (3, 0)
        );
        assert_eq!(
            move_picker_selection_and_scroll(4, 0, 20, 5, PickerKeyAction::MoveDown),
            (5, 1)
        );
        assert_eq!(
            move_picker_selection_and_scroll(5, 1, 20, 5, PickerKeyAction::MoveUp),
            (4, 1)
        );
        assert_eq!(
            move_picker_selection_and_scroll(1, 1, 20, 5, PickerKeyAction::MoveUp),
            (0, 0)
        );
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

    #[test]
    fn viewer_search_can_navigate_matches_after_soft_wrapping_long_rows() {
        let raw_lines = vec![vec![test_span(
            "first memory_limiter second memory_limiter",
        )]];
        let wrapped = wrap_styled_lines(&raw_lines, 20);
        let wrapped_text: Vec<String> = wrapped.iter().map(styled_line_text).collect();

        assert_eq!(wrapped.len(), 4);
        assert_eq!(
            wrapped_text,
            vec!["first", "memory_limiter", "second", "memory_limiter"]
        );

        let mut search = ViewerSearch::new("memory_limiter", &wrapped).unwrap();
        assert_eq!(search.current_line(), Some(1));
        search.next();
        assert_eq!(search.current_line(), Some(3));
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
        if self.matches.is_empty() {
            return;
        }

        let fallback = (self.current + 1) % self.matches.len();
        let current_line = self.matches[self.current].line;
        for step in 1..=self.matches.len() {
            let candidate = (self.current + step) % self.matches.len();
            if self.matches[candidate].line != current_line {
                self.current = candidate;
                return;
            }
        }
        self.current = fallback;
    }

    fn previous(&mut self) {
        if self.matches.is_empty() {
            return;
        }

        let fallback = if self.current == 0 {
            self.matches.len() - 1
        } else {
            self.current - 1
        };
        let current_line = self.matches[self.current].line;
        for step in 1..=self.matches.len() {
            let candidate = if self.current >= step {
                self.current - step
            } else {
                self.matches.len() + self.current - step
            };
            if self.matches[candidate].line != current_line {
                self.current = candidate;
                return;
            }
        }
        self.current = fallback;
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

#[derive(Clone, Copy, Eq, PartialEq)]
struct SpanStyle {
    fg: Option<(u8, u8, u8)>,
    bold: bool,
    dim: bool,
}

#[derive(Clone, Copy)]
struct StyledChar {
    ch: char,
    style: SpanStyle,
}

fn pager_body_width(cols: usize) -> usize {
    cols.saturating_sub(1).max(1)
}

fn wrap_styled_lines(lines: &[StyledLine], max_width: usize) -> Vec<StyledLine> {
    let max_width = max_width.max(1);
    let mut wrapped = Vec::new();

    for line in lines {
        let mut current = Vec::new();
        let mut current_width = 0;
        let mut last_break = None;

        for span in line {
            let style = SpanStyle {
                fg: span.fg,
                bold: span.bold,
                dim: span.dim,
            };

            for ch in span.text.chars() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                while current_width > 0 && current_width + ch_width > max_width {
                    if let Some(break_idx) = last_break.filter(|idx| *idx > 0) {
                        wrapped.push(styled_chars_to_line(&current[..break_idx]));
                        current = current[(break_idx + 1)..].to_vec();
                        trim_leading_whitespace(&mut current);
                        current_width = styled_chars_width(&current);
                    } else {
                        wrapped.push(styled_chars_to_line(&current));
                        current.clear();
                        current_width = 0;
                    }
                    last_break = current.iter().rposition(|unit| unit.ch.is_whitespace());
                }

                if current.is_empty() && ch.is_whitespace() {
                    continue;
                }
                current.push(StyledChar { ch, style });
                current_width += ch_width;
                if ch.is_whitespace() {
                    last_break = Some(current.len() - 1);
                }
            }
        }

        wrapped.push(styled_chars_to_line(&current));
    }

    wrapped
}

fn trim_leading_whitespace(chars: &mut Vec<StyledChar>) {
    let first_non_whitespace = chars
        .iter()
        .position(|unit| !unit.ch.is_whitespace())
        .unwrap_or(chars.len());
    if first_non_whitespace > 0 {
        chars.drain(..first_non_whitespace);
    }
}

fn styled_chars_width(chars: &[StyledChar]) -> usize {
    chars
        .iter()
        .map(|unit| UnicodeWidthChar::width(unit.ch).unwrap_or(0))
        .sum()
}

fn styled_chars_to_line(chars: &[StyledChar]) -> StyledLine {
    let mut line = Vec::new();
    let Some(first) = chars.first() else {
        return line;
    };

    let mut style = first.style;
    let mut text = String::new();

    for unit in chars {
        if unit.style != style {
            line.push(Span {
                text,
                fg: style.fg,
                bold: style.bold,
                dim: style.dim,
            });
            style = unit.style;
            text = String::new();
        }
        text.push(unit.ch);
    }

    if !text.is_empty() {
        line.push(Span {
            text,
            fg: style.fg,
            bold: style.bold,
            dim: style.dim,
        });
    }

    line
}

fn pager_loop(
    stdout: &mut io::Stdout,
    conv: &Conversation,
    initial_query: &str,
) -> crate::error::Result<PagerAction> {
    let mut raw_lines = viewer::build_session_lines(conv)?;
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut wrap_width = pager_body_width(cols as usize);
    let mut lines = wrap_styled_lines(&raw_lines, wrap_width);
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

        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let current_wrap_width = pager_body_width(cols as usize);
        let mut rewrapped = false;
        if current_wrap_width != wrap_width {
            wrap_width = current_wrap_width;
            lines = wrap_styled_lines(&raw_lines, wrap_width);
            search = ViewerSearch::new(&search_query, &lines);
            rewrapped = true;
        }
        let visible = (rows as usize).saturating_sub(1); // reserve 1 for status bar
        let max_scroll = lines.len().saturating_sub(visible);
        if rewrapped {
            if let Some(line) = search.as_ref().and_then(ViewerSearch::current_line) {
                scroll = scroll_to_match(line, visible, max_scroll);
            }
        } else if scroll > max_scroll {
            scroll = max_scroll;
        }

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
                raw_lines = viewer::build_session_lines(conv)?;
                lines = wrap_styled_lines(&raw_lines, wrap_width);
                search = ViewerSearch::new(&search_query, &lines);
                // If new content appeared and we were at the bottom, follow the tail
                let was_at_bottom = scroll >= old_len.saturating_sub(visible);
                let new_max = lines.len().saturating_sub(visible);
                if let Some(line) = search.as_ref().and_then(ViewerSearch::current_line) {
                    scroll = scroll_to_match(line, visible, new_max);
                } else if scroll > new_max || (was_at_bottom && lines.len() > old_len) {
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
            pager_body_width(cols),
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
    let directory = format_directory_label(conv);
    let model = format_model_short(conv.model.as_deref());
    let age = format_relative_time(conv.timestamp);
    let sid = short_id(&conv.session_id);
    let left = format!(" {} ({}) {} {}", directory, model, age, sid);
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
