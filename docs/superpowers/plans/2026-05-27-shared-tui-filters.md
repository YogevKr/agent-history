# Shared TUI Filters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add one shared session filtering layer that can be initialized from CLI flags and edited live in the TUI, with lazy loading when a newly enabled source was not loaded at startup.

**Architecture:** Create a reusable `SessionFilters` model for source and project selection, plus a `SessionStore` that owns loaded conversations and tracks which sources are loaded. CLI flags initialize `SessionFilters`; the TUI mutates the same filters; visibility always comes from `SessionFilters::matches`. If the TUI enables a source that is not loaded, `SessionStore` loads it, merges/sorts/dedupes sessions, and the existing project filter is applied to the newly loaded rows.

**Tech Stack:** Rust, clap, crossterm TUI, rayon loaders, existing `Conversation` and `SessionSource` model.

---

## File Structure

- Create `src/filters.rs`
  - Owns source/project filter state and pure filtering behavior.
  - Does not load files and does not draw UI.
  - Project filter uses `All | Only(HashSet<String>) | Contains(String)`; `Only(empty)` represents no projects selected, while `Contains` preserves CLI substring matching across lazy-loaded sources.

- Create `src/session_store.rs`
  - Owns loaded `Conversation` rows and `loaded_sources`.
  - Loads Claude/Codex sources on demand.
  - Sorts and deduplicates after every load.

- Modify `src/history/mod.rs`
  - Add `Hash` derive to `SessionSource` so it can be used in filter sets.

- Modify `src/lib.rs`
  - Register the new modules.
  - Replace ad hoc `apply_filters` with shared `SessionFilters`.
  - Preserve current startup optimization: initially load only CLI-requested source(s).
  - Pass `SessionStore` and `SessionFilters` into the TUI.

- Modify `src/interactive.rs`
  - Store and mutate `SessionFilters`.
  - Route `F3` into a filter overlay.
  - Refilter by applying filters before search/collapse.
  - Lazy-load missing sources when a filter enables them.
  - Add a filter header above search.

---

## Task 1: Add Pure Session Filter Model

**Files:**
- Create: `src/filters.rs`
- Modify: `src/history/mod.rs`
- Later module registration happens in Task 3.

- [ ] **Step 1: Add `Hash` derive to `SessionSource`**

In `src/history/mod.rs`, change:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    Claude,
    Codex,
}
```

to:

```rust
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum SessionSource {
    Claude,
    Codex,
}
```

- [ ] **Step 2: Create failing filter tests**

Create `src/filters.rs` with these tests first:

```rust
use crate::history::{Conversation, SessionSource};
use std::collections::HashSet;

pub const ALL_SOURCES: [SessionSource; 2] = [SessionSource::Claude, SessionSource::Codex];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectSelection {
    All,
    Only(HashSet<String>),
    Contains(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionFilters {
    enabled_sources: HashSet<SessionSource>,
    project_selection: ProjectSelection,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::path::PathBuf;

    fn conversation(id: &str, source: SessionSource, project: Option<&str>) -> Conversation {
        Conversation {
            path: PathBuf::from(format!("{id}.jsonl")),
            source,
            session_id: id.to_string(),
            timestamp: Local::now(),
            preview: String::new(),
            full_text: String::new(),
            project_name: project.map(str::to_string),
            cwd: None,
            message_count: 1,
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
    fn all_filters_match_all_sources_and_projects() {
        let filters = SessionFilters::all();
        let codex = conversation("codex", SessionSource::Codex, Some("agent-history"));
        let claude = conversation("claude", SessionSource::Claude, Some("agent-history"));

        assert!(filters.matches(&codex));
        assert!(filters.matches(&claude));
    }

    #[test]
    fn source_only_matches_one_source() {
        let filters = SessionFilters::source_only(SessionSource::Codex);
        let codex = conversation("codex", SessionSource::Codex, Some("agent-history"));
        let claude = conversation("claude", SessionSource::Claude, Some("agent-history"));

        assert!(filters.matches(&codex));
        assert!(!filters.matches(&claude));
    }

    #[test]
    fn project_only_empty_matches_no_projects() {
        let mut filters = SessionFilters::all();
        filters.set_no_projects();
        let conv = conversation("codex", SessionSource::Codex, Some("agent-history"));

        assert!(!filters.matches(&conv));
    }

    #[test]
    fn project_only_uses_project_name_not_subagent_name() {
        let mut conv = conversation("child", SessionSource::Codex, Some("agent-history"));
        conv.subagent_name = Some("reviewer".to_string());

        let mut filters = SessionFilters::all();
        filters.only_project("reviewer");
        assert!(!filters.matches(&conv));

        filters.only_project("agent-history");
        assert!(filters.matches(&conv));
    }

    #[test]
    fn disabling_last_source_is_rejected() {
        let mut filters = SessionFilters::source_only(SessionSource::Codex);

        assert!(!filters.set_source_enabled(SessionSource::Codex, false));
        assert!(filters.source_enabled(SessionSource::Codex));
    }

    #[test]
    fn source_summary_reports_active_source() {
        let filters = SessionFilters::source_only(SessionSource::Claude);

        assert_eq!(filters.summary(), "agent=[claude] project=[all]");
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run:

```bash
cargo test --lib filters
```

Expected: FAIL because methods such as `SessionFilters::all`, `matches`, and `summary` are not implemented.

- [ ] **Step 4: Implement the filter model**

Replace the non-test portion of `src/filters.rs` with:

```rust
use crate::history::{Conversation, SessionSource};
use std::collections::HashSet;

pub const ALL_SOURCES: [SessionSource; 2] = [SessionSource::Claude, SessionSource::Codex];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectSelection {
    All,
    Only(HashSet<String>),
    Contains(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionFilters {
    enabled_sources: HashSet<SessionSource>,
    project_selection: ProjectSelection,
}

impl SessionFilters {
    pub fn all() -> Self {
        Self {
            enabled_sources: ALL_SOURCES.into_iter().collect(),
            project_selection: ProjectSelection::All,
        }
    }

    pub fn source_only(source: SessionSource) -> Self {
        Self {
            enabled_sources: HashSet::from([source]),
            project_selection: ProjectSelection::All,
        }
    }

    pub fn enabled_sources(&self) -> impl Iterator<Item = SessionSource> + '_ {
        ALL_SOURCES
            .into_iter()
            .filter(|source| self.enabled_sources.contains(source))
    }

    pub fn source_enabled(&self, source: SessionSource) -> bool {
        self.enabled_sources.contains(&source)
    }

    pub fn set_source_enabled(&mut self, source: SessionSource, enabled: bool) -> bool {
        if enabled {
            self.enabled_sources.insert(source);
            return true;
        }

        if self.enabled_sources.len() <= 1 && self.enabled_sources.contains(&source) {
            return false;
        }

        self.enabled_sources.remove(&source);
        true
    }

    pub fn toggle_source(&mut self, source: SessionSource) -> bool {
        self.set_source_enabled(source, !self.source_enabled(source))
    }

    pub fn set_all_projects(&mut self) {
        self.project_selection = ProjectSelection::All;
    }

    pub fn set_no_projects(&mut self) {
        self.project_selection = ProjectSelection::Only(HashSet::new());
    }

    pub fn only_project(&mut self, project: &str) {
        self.project_selection = ProjectSelection::Only(HashSet::from([project.to_string()]));
    }

    pub fn toggle_project(
        &mut self,
        project: &str,
        available_projects: impl IntoIterator<Item = String>,
    ) {
        match &mut self.project_selection {
            ProjectSelection::All => {
                self.project_selection = ProjectSelection::Only(
                    available_projects
                        .into_iter()
                        .filter(|available| available != project)
                        .collect(),
                );
            }
            ProjectSelection::Only(projects) => {
                if !projects.remove(project) {
                    projects.insert(project.to_string());
                }
            }
            ProjectSelection::Contains(_) => {
                self.project_selection = ProjectSelection::Only(
                    available_projects
                        .into_iter()
                        .filter(|available| available != project)
                        .collect(),
                );
            }
        }
    }

    pub fn project_enabled(&self, project: &str) -> bool {
        match &self.project_selection {
            ProjectSelection::All => true,
            ProjectSelection::Only(projects) => projects.contains(project),
            ProjectSelection::Contains(needle) => project.to_lowercase().contains(needle),
        }
    }

    pub fn matches(&self, conv: &Conversation) -> bool {
        if !self.enabled_sources.contains(&conv.source) {
            return false;
        }

        match &self.project_selection {
            ProjectSelection::All => true,
            ProjectSelection::Only(projects) => conv
                .project_name
                .as_deref()
                .is_some_and(|project| projects.contains(project)),
            ProjectSelection::Contains(project) => conv
                .project_name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(project)),
        }
    }

    pub fn filter_indices(&self, conversations: &[Conversation], indices: Vec<usize>) -> Vec<usize> {
        indices
            .into_iter()
            .filter(|idx| conversations.get(*idx).is_some_and(|conv| self.matches(conv)))
            .collect()
    }

    pub fn summary(&self) -> String {
        let agent = if self.enabled_sources.len() == ALL_SOURCES.len() {
            "all".to_string()
        } else {
            ALL_SOURCES
                .into_iter()
                .filter(|source| self.enabled_sources.contains(source))
                .map(|source| source.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };

        let project = match &self.project_selection {
            ProjectSelection::All => "all".to_string(),
            ProjectSelection::Only(projects) if projects.is_empty() => "none".to_string(),
            ProjectSelection::Only(projects) => {
                let mut names = projects.iter().cloned().collect::<Vec<_>>();
                names.sort();
                names.join(",")
            }
        };

        format!("agent=[{}] project=[{}]", agent, project)
    }
}
```

Keep the tests from Step 2 below this implementation.

- [ ] **Step 5: Register module just for test compilation**

In `src/lib.rs`, add:

```rust
mod filters;
```

near the other module declarations.

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test --lib filters
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/history/mod.rs src/filters.rs src/lib.rs
git commit -m "feat: add shared session filters"
```

---

## Task 2: Add Lazy-Loading Session Store

**Files:**
- Create: `src/session_store.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing `SessionStore` tests**

Create `src/session_store.rs` with:

```rust
use crate::filters::SessionFilters;
use crate::history::{compare_conversations, Conversation, SessionSource};
use std::collections::HashSet;

pub struct SessionStore {
    conversations: Vec<Conversation>,
    loaded_sources: HashSet<SessionSource>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Local};
    use std::path::PathBuf;

    fn conversation(id: &str, source: SessionSource, minutes_ago: i64) -> Conversation {
        let timestamp = Local::now() - Duration::minutes(minutes_ago);
        Conversation {
            path: PathBuf::from(format!("{id}.jsonl")),
            source,
            session_id: id.to_string(),
            timestamp,
            preview: String::new(),
            full_text: String::new(),
            project_name: Some("agent-history".to_string()),
            cwd: None,
            message_count: 1,
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
            hierarchy_sort_timestamp: timestamp,
        }
    }

    #[test]
    fn from_loaded_sorts_and_dedupes() {
        let duplicate_old = conversation("same", SessionSource::Codex, 20);
        let newest = conversation("new", SessionSource::Claude, 1);
        let duplicate_new = conversation("same", SessionSource::Codex, 5);

        let store = SessionStore::from_loaded(
            vec![duplicate_old, newest.clone(), duplicate_new.clone()],
            [SessionSource::Codex, SessionSource::Claude],
        );

        assert_eq!(store.conversations()[0].session_id, "new");
        assert_eq!(store.conversations()[1].session_id, "same");
        assert_eq!(store.conversations()[1].source, SessionSource::Codex);
        assert_eq!(store.conversations()[1].timestamp, duplicate_new.timestamp);
        assert_eq!(store.conversations().len(), 2);
    }

    #[test]
    fn from_loaded_keeps_same_session_id_from_different_sources() {
        let codex = conversation("same", SessionSource::Codex, 1);
        let claude = conversation("same", SessionSource::Claude, 2);

        let store = SessionStore::from_loaded(
            vec![claude.clone(), codex.clone()],
            [SessionSource::Codex, SessionSource::Claude],
        );

        assert_eq!(
            store
                .conversations()
                .iter()
                .map(|conversation| (conversation.source, conversation.session_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (SessionSource::Codex, "same"),
                (SessionSource::Claude, "same"),
            ]
        );
    }

    #[test]
    fn missing_enabled_sources_returns_sources_not_loaded() {
        let store = SessionStore::from_loaded(
            vec![conversation("codex", SessionSource::Codex, 1)],
            [SessionSource::Codex],
        );
        let filters = SessionFilters::all();

        assert_eq!(store.missing_enabled_sources(&filters), vec![SessionSource::Claude]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test --lib session_store
```

Expected: FAIL because `SessionStore::from_loaded`, `conversations`, and `missing_enabled_sources` are not implemented and the module is not registered.

- [ ] **Step 3: Implement pure store behavior**

Replace the non-test portion of `src/session_store.rs` with:

```rust
use crate::filters::SessionFilters;
use crate::history::{compare_conversations, Conversation, SessionSource};
use std::collections::HashSet;

pub struct SessionStore {
    conversations: Vec<Conversation>,
    loaded_sources: HashSet<SessionSource>,
}

impl SessionStore {
    pub fn from_loaded(
        conversations: Vec<Conversation>,
        loaded_sources: impl IntoIterator<Item = SessionSource>,
    ) -> Self {
        let mut store = Self {
            conversations,
            loaded_sources: loaded_sources.into_iter().collect(),
        };
        store.normalize();
        store
    }

    pub fn conversations(&self) -> &[Conversation] {
        &self.conversations
    }

    pub fn loaded_source(&self, source: SessionSource) -> bool {
        self.loaded_sources.contains(&source)
    }

    pub fn missing_enabled_sources(&self, filters: &SessionFilters) -> Vec<SessionSource> {
        filters
            .enabled_sources()
            .filter(|source| !self.loaded_sources.contains(source))
            .collect()
    }

    pub fn merge_loaded(
        &mut self,
        source: SessionSource,
        mut conversations: Vec<Conversation>,
    ) {
        self.conversations.append(&mut conversations);
        self.loaded_sources.insert(source);
        self.normalize();
    }

    fn normalize(&mut self) {
        self.conversations.sort_by(compare_conversations);
        let mut seen = HashSet::new();
        self.conversations.retain(|conversation| {
            seen.insert((conversation.source, conversation.session_id.clone()))
        });
    }
}
```

- [ ] **Step 4: Register module**

In `src/lib.rs`, add:

```rust
mod session_store;
```

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test --lib session_store
cargo test --lib filters
```

Expected: both commands PASS.

- [ ] **Step 6: Add real source loading methods**

Append this implementation to `src/session_store.rs`:

```rust
impl SessionStore {
    pub fn load_sources(sources: impl IntoIterator<Item = SessionSource>) -> crate::error::Result<Self> {
        let sources = sources.into_iter().collect::<Vec<_>>();
        let mut conversations = Vec::new();
        let mut loaded_sources = HashSet::new();

        for source in sources {
            conversations.extend(load_source(source)?);
            loaded_sources.insert(source);
        }

        Ok(Self::from_loaded(conversations, loaded_sources))
    }

    pub fn load_missing_enabled_sources(
        &mut self,
        filters: &SessionFilters,
    ) -> crate::error::Result<Vec<SessionSource>> {
        let missing = self.missing_enabled_sources(filters);
        let mut loaded = Vec::new();
        let mut first_error = None;

        for source in missing {
            match load_source(source) {
                Ok(conversations) => {
                    self.merge_loaded(source, conversations);
                    loaded.push(source);
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(loaded)
        }
    }
}

fn load_source(source: SessionSource) -> crate::error::Result<Vec<Conversation>> {
    match source {
        SessionSource::Claude => crate::claude_loader::load_claude_sessions(),
        SessionSource::Codex => crate::codex_loader::load_codex_sessions(),
    }
}
```

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test --lib session_store
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/session_store.rs src/lib.rs
git commit -m "feat: add lazy session store"
```

---

## Task 3: Wire CLI Startup to Shared Filters and Store

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Add tests for CLI filter initialization**

In `src/lib.rs` test module, add:

```rust
#[test]
fn cli_source_initializes_filter_to_codex_only() {
    let args = Cli {
        query: None,
        source: Some(SourceFilter::Codex),
        project: None,
        since: None,
        limit: 20,
        list: false,
        show: None,
        resume: None,
        local: false,
    };

    let filters = filters_from_args(&args, &[]);

    assert!(filters.source_enabled(SessionSource::Codex));
    assert!(!filters.source_enabled(SessionSource::Claude));
}

#[test]
fn cli_project_initializes_exact_known_matching_projects() {
    let mut first = conversation("one");
    first.project_name = Some("agent-history".to_string());
    let mut second = conversation("two");
    second.project_name = Some("other".to_string());

    let args = Cli {
        query: None,
        source: None,
        project: Some("agent".to_string()),
        since: None,
        limit: 20,
        list: false,
        show: None,
        resume: None,
        local: false,
    };

    let filters = filters_from_args(&args, &[first.clone(), second.clone()]);

    assert!(filters.matches(&first));
    assert!(!filters.matches(&second));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test --lib cli_source_initializes_filter_to_codex_only
cargo test --lib cli_project_initializes_exact_known_matching_projects
```

Expected: FAIL because `filters_from_args` is missing.

- [ ] **Step 3: Add imports**

In `src/lib.rs`, replace the imports:

```rust
use crate::cli::{parse_duration_secs, Cli, SourceFilter};
use crate::codex_loader::CodexLoadOptions;
use crate::history::{compare_conversations, Conversation, SessionSource};
```

with:

```rust
use crate::cli::{parse_duration_secs, Cli, SourceFilter};
use crate::filters::SessionFilters;
use crate::history::{Conversation, SessionSource};
use crate::session_store::SessionStore;
```

- [ ] **Step 4: Add CLI-to-filter helpers**

In `src/lib.rs`, add these functions below `run_inner`:

```rust
fn initial_sources_from_args(args: &Cli) -> Vec<SessionSource> {
    match args.source {
        Some(SourceFilter::Claude) => vec![SessionSource::Claude],
        Some(SourceFilter::Codex) => vec![SessionSource::Codex],
        None => vec![SessionSource::Claude, SessionSource::Codex],
    }
}

fn filters_from_args(args: &Cli, conversations: &[Conversation]) -> SessionFilters {
    let mut filters = match args.source {
        Some(SourceFilter::Claude) => SessionFilters::source_only(SessionSource::Claude),
        Some(SourceFilter::Codex) => SessionFilters::source_only(SessionSource::Codex),
        None => SessionFilters::all(),
    };

    if let Some(project_query) = args.project.as_deref() {
        filters.set_project_contains(project_query);
    }

    filters
}
```

Then add this method to `SessionFilters` in `src/filters.rs`:

```rust
pub fn set_project_contains(&mut self, project: &str) {
    self.project_selection = ProjectSelection::Contains(project.to_lowercase());
}
```

- [ ] **Step 5: Replace load/filter flow in `run_inner`**

In `src/lib.rs`, replace the manual `load_claude/load_codex/rayon::join` block and the old `apply_filters` call with:

```rust
    let mut store = SessionStore::load_sources(initial_sources_from_args(&args))?;
    let filters = filters_from_args(&args, store.conversations());
```

Then change show/resume/filter sections to:

```rust
    if let Some(ref id) = args.show {
        let conv = resolve_session(store.conversations(), id)?;
        return viewer::review_session(conv);
    }

    if let Some(ref id) = args.resume {
        let conv = resolve_session(store.conversations(), id)?;
        return resume::resume_session(conv);
    }

    let filtered = apply_non_source_filters(
        store
            .conversations()
            .iter()
            .filter(|conv| filters.matches(conv))
            .cloned()
            .collect(),
        &args,
    );

    let is_interactive = args.query.is_none() && !args.list;
    if is_interactive && atty::is(atty::Stream::Stdout) {
        return interactive::run(store, filters);
    }
```

Rename `apply_filters` to `apply_non_source_filters` and remove the source/project sections inside it, leaving only `--since` and `--local`:

```rust
fn apply_non_source_filters(conversations: Vec<Conversation>, args: &Cli) -> Vec<Conversation> {
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
            if let Some(secs) = since_secs {
                let age = now.signed_duration_since(conv.timestamp).num_seconds();
                if age > secs {
                    return false;
                }
            }

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
```

- [ ] **Step 6: Update `interactive::run` signature temporarily**

In `src/interactive.rs`, change:

```rust
pub fn run(conversations: Vec<Conversation>) -> crate::error::Result<()> {
```

to:

```rust
pub fn run(
    mut store: crate::session_store::SessionStore,
    filters: crate::filters::SessionFilters,
) -> crate::error::Result<()> {
    let conversations = store.conversations().to_vec();
```

Keep the rest of the function temporarily using the local `conversations`. This compiles now; Task 4 will remove the clone and use the store directly.

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test --lib cli_source_initializes_filter_to_codex_only
cargo test --lib cli_project_initializes_exact_known_matching_projects
cargo test --lib
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/lib.rs src/filters.rs src/interactive.rs
git commit -m "feat: initialize shared filters from cli"
```

---

## Task 4: Apply Filters Inside Picker Refiltering

**Files:**
- Modify: `src/interactive.rs`

- [ ] **Step 1: Add tests for filter-aware refiltering**

In `src/interactive.rs` test module, add:

```rust
#[test]
fn refilter_applies_source_filter_before_search() {
    let conversations = vec![
        conversation("codex-match", 0, false),
        {
            let mut conv = conversation("claude-match", 0, false);
            conv.source = SessionSource::Claude;
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
```

Add this helper inside the existing `impl` area for tests by implementing it outside tests:

```rust
impl PickerState {
    #[cfg(test)]
    fn for_test(conversations: &[Conversation], filters: crate::filters::SessionFilters) -> Self {
        Self {
            query: String::new(),
            selected: 0,
            scroll: 0,
            filtered_indices: Vec::new(),
            searchable: precompute_search_text(conversations),
            full_search_index: None,
            expanded_tree_roots: HashSet::new(),
            flash: None,
            filters,
            filter_overlay: None,
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test --lib refilter_applies_source_filter_before_search
```

Expected: FAIL because `PickerState` has no `filters` or `filter_overlay` fields and `refilter` does not apply filters.

- [ ] **Step 3: Add filter state fields**

In `src/interactive.rs`, extend `PickerState`:

```rust
struct PickerState {
    query: String,
    selected: usize,
    scroll: usize,
    filtered_indices: Vec<usize>,
    searchable: Vec<SearchableConversation>,
    full_search_index: Option<FullSearchIndex>,
    expanded_tree_roots: HashSet<String>,
    flash: Option<String>,
    filters: crate::filters::SessionFilters,
    filter_overlay: Option<FilterOverlayState>,
}
```

Add a temporary overlay state:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
struct FilterOverlayState {
    section: FilterSection,
    agent_selected: usize,
    project_selected: usize,
    project_query: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FilterSection {
    Agent,
    Project,
}
```

When constructing `PickerState` in `run`, set:

```rust
        filters,
        filter_overlay: None,
```

- [ ] **Step 4: Update visible row calculation for the filter header**

Change:

```rust
fn picker_visible_rows() -> usize {
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    (rows as usize).saturating_sub(2).max(1)
}
```

to:

```rust
const PICKER_HEADER_ROWS: usize = 3;

fn picker_visible_rows() -> usize {
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    (rows as usize).saturating_sub(PICKER_HEADER_ROWS).max(1)
}
```

- [ ] **Step 5: Apply filters in `refilter`**

Change the beginning of `refilter` to:

```rust
fn refilter(conversations: &[Conversation], state: &mut PickerState) {
    if !state.query.is_empty() && state.full_search_index.is_none() {
        state.full_search_index = Some(precompute_full_search_index(conversations));
        state.flash = Some("Indexed full context".to_string());
    }

    let searched_indices = if state.query.is_empty() {
        (0..conversations.len()).collect()
    } else if let Some(index) = state.full_search_index.as_ref() {
        search_full(conversations, index, &state.query, Local::now())
    } else {
        search(conversations, &state.searchable, &state.query, Local::now())
    };

    let base_indices = state.filters.filter_indices(conversations, searched_indices);
```

Keep the existing `collapse_visible_indices(...)` and selection/scroll logic below that.

- [ ] **Step 6: Recompute indexes after store changes**

Add:

```rust
fn refresh_search_indexes(conversations: &[Conversation], state: &mut PickerState) {
    state.searchable = precompute_search_text(conversations);
    state.full_search_index = None;
}
```

Task 5 will call this after lazy loads.

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test --lib refilter_applies_source_filter_before_search
cargo test --lib interactive::tests
```

Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add src/interactive.rs
git commit -m "feat: apply shared filters in tui"
```

---

## Task 5: Add TUI Filter Overlay and Lazy Source Loading

**Files:**
- Modify: `src/interactive.rs`

- [ ] **Step 1: Add key action tests**

In `src/interactive.rs` test module, add:

```rust
#[test]
fn f3_opens_filter_overlay() {
    assert_eq!(
        picker_key_action(&Event::Key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE))),
        PickerKeyAction::OpenFilters
    );
}

#[test]
fn filter_overlay_escape_closes_without_resetting_filters() {
    let conversations = vec![conversation("codex", 0, false)];
    let mut state = PickerState::for_test(
        &conversations,
        crate::filters::SessionFilters::source_only(SessionSource::Codex),
    );
    state.filter_overlay = Some(FilterOverlayState::default());

    handle_filter_overlay_key(
        &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &conversations,
        &mut state,
    );

    assert!(state.filter_overlay.is_none());
    assert!(state.filters.source_enabled(SessionSource::Codex));
    assert!(!state.filters.source_enabled(SessionSource::Claude));
}

#[test]
fn project_overlay_supports_all_and_none_shortcuts() {
    let conversations = vec![conversation("codex", 0, false)];
    let mut state = PickerState::for_test(&conversations, crate::filters::SessionFilters::all());
    state.filter_overlay = Some(FilterOverlayState {
        section: FilterSection::Project,
        ..FilterOverlayState::default()
    });

    handle_filter_overlay_key(
        &Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        &conversations,
        &mut state,
    );
    assert!(!state.filters.project_enabled("project"));

    handle_filter_overlay_key(
        &Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        &conversations,
        &mut state,
    );
    assert!(state.filters.project_enabled("project"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test --lib f3_opens_filter_overlay
cargo test --lib filter_overlay_escape_closes_without_resetting_filters
cargo test --lib project_overlay_supports_all_and_none_shortcuts
```

Expected: FAIL because key action and overlay handling do not exist.

- [ ] **Step 3: Add overlay defaults and key enum variants**

In `src/interactive.rs`, add:

```rust
impl Default for FilterOverlayState {
    fn default() -> Self {
        Self {
            section: FilterSection::Agent,
            agent_selected: 0,
            project_selected: 0,
            project_query: String::new(),
        }
    }
}
```

Add to `PickerKeyAction`:

```rust
    OpenFilters,
```

Add to `picker_key_action` match:

```rust
        KeyEvent {
            code: KeyCode::F(3),
            ..
        } => PickerKeyAction::OpenFilters,
```

- [ ] **Step 4: Add project option helpers**

Add these functions near other picker helpers:

```rust
fn available_projects(conversations: &[Conversation]) -> Vec<String> {
    let mut projects = conversations
        .iter()
        .filter_map(|conversation| conversation.project_name.clone())
        .collect::<Vec<_>>();
    projects.sort();
    projects.dedup();
    projects
}

fn visible_filter_projects(conversations: &[Conversation], query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    available_projects(conversations)
        .into_iter()
        .filter(|project| query.is_empty() || project.to_lowercase().contains(&query))
        .collect()
}
```

- [ ] **Step 5: Add overlay key handler**

Add:

```rust
fn handle_filter_overlay_key(
    evt: &Event,
    conversations: &[Conversation],
    state: &mut PickerState,
) {
    let Some(mut overlay) = state.filter_overlay.clone() else {
        return;
    };

    let Event::Key(key) = evt else {
        return;
    };
    if key.kind != KeyEventKind::Press {
        return;
    }

    match *key {
        KeyEvent { code: KeyCode::Esc, .. } => {
            state.filter_overlay = None;
            return;
        }
        KeyEvent { code: KeyCode::Left, .. } => overlay.section = FilterSection::Agent,
        KeyEvent { code: KeyCode::Right, .. } => overlay.section = FilterSection::Project,
        KeyEvent { code: KeyCode::Up, .. } => match overlay.section {
            FilterSection::Agent => overlay.agent_selected = overlay.agent_selected.saturating_sub(1),
            FilterSection::Project => overlay.project_selected = overlay.project_selected.saturating_sub(1),
        },
        KeyEvent { code: KeyCode::Down, .. } => match overlay.section {
            FilterSection::Agent => {
                overlay.agent_selected = (overlay.agent_selected + 1).min(crate::filters::ALL_SOURCES.len() - 1);
            }
            FilterSection::Project => {
                let len = visible_filter_projects(conversations, &overlay.project_query).len();
                overlay.project_selected = (overlay.project_selected + 1).min(len.saturating_sub(1));
            }
        },
        KeyEvent { code: KeyCode::Char(' '), modifiers: KeyModifiers::CONTROL, .. } => {
            match overlay.section {
                FilterSection::Agent => {
                    let source = crate::filters::ALL_SOURCES[overlay.agent_selected];
                    state.filters = crate::filters::SessionFilters::source_only(source);
                }
                FilterSection::Project => {
                    if let Some(project) = visible_filter_projects(conversations, &overlay.project_query)
                        .get(overlay.project_selected)
                    {
                        state.filters.only_project(project);
                    }
                }
            }
            refilter(conversations, state);
        }
        KeyEvent { code: KeyCode::Char(' '), .. } => {
            match overlay.section {
                FilterSection::Agent => {
                    let source = crate::filters::ALL_SOURCES[overlay.agent_selected];
                    state.filters.toggle_source(source);
                }
                FilterSection::Project => {
                    if let Some(project) = visible_filter_projects(conversations, &overlay.project_query)
                        .get(overlay.project_selected)
                    {
                        state.filters.toggle_project(project, available_projects(conversations));
                    }
                }
            }
            refilter(conversations, state);
        }
        KeyEvent { code: KeyCode::Backspace, .. } if overlay.section == FilterSection::Project => {
            overlay.project_query.pop();
            overlay.project_selected = 0;
        }
        KeyEvent { code: KeyCode::Char('a'), modifiers: KeyModifiers::CONTROL, .. }
            if overlay.section == FilterSection::Project
        {
            state.filters.set_all_projects();
            refilter(conversations, state);
        }
        KeyEvent { code: KeyCode::Char('d'), modifiers: KeyModifiers::CONTROL, .. }
            if overlay.section == FilterSection::Project
        {
            state.filters.set_no_projects();
            refilter(conversations, state);
        }
        KeyEvent { code: KeyCode::Char(c), modifiers, .. }
            if overlay.section == FilterSection::Project
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT) =>
        {
            overlay.project_query.push(c);
            overlay.project_selected = 0;
        }
        _ => {}
    }

    state.filter_overlay = Some(overlay);
}
```

- [ ] **Step 6: Route overlay events in `picker_loop`**

At the top of the event handling section in `picker_loop`, before `match picker_key_action(&evt)`, add:

```rust
        if state.filter_overlay.is_some() {
            handle_filter_overlay_key(&evt, conversations, state);
            continue;
        }
```

In `match picker_key_action(&evt)`, add:

```rust
            PickerKeyAction::OpenFilters => {
                state.filter_overlay = Some(FilterOverlayState::default());
            }
```

- [ ] **Step 7: Change `main_loop` and `picker_loop` to use `SessionStore`**

Change signatures:

```rust
fn main_loop(
    stdout: &mut io::Stdout,
    store: &mut crate::session_store::SessionStore,
    state: &mut PickerState,
) -> crate::error::Result<()>
```

and:

```rust
fn picker_loop(
    stdout: &mut io::Stdout,
    store: &mut crate::session_store::SessionStore,
    state: &mut PickerState,
) -> PickerAction
```

Inside both functions, replace `conversations` uses with:

```rust
let conversations = store.conversations();
```

Where filter toggles may enable missing sources, after `handle_filter_overlay_key`, add:

```rust
            let load_result = store.load_missing_enabled_sources(&state.filters);
            refresh_search_indexes(store.conversations(), state);
            match load_result {
                Ok(loaded) if !loaded.is_empty() => {
                    state.flash = Some(format!(
                        "Loaded {}",
                        loaded.iter().map(|source| source.to_string()).collect::<Vec<_>>().join(",")
                    ));
                }
                Ok(_) => {}
                Err(err) => {
                    state.flash = Some(format!("Failed to load source: {err}"));
                }
            }
            refilter(store.conversations(), state);
```

This ensures enabling Claude from a Codex-only startup loads Claude and applies the existing project filter immediately. Refreshing indexes after every load attempt also covers partial-success loads where `SessionStore` merges some sources before returning an error for another source.

- [ ] **Step 8: Draw the filter header and overlay**

In `draw_picker`, add a `filters: &crate::filters::SessionFilters` and `filter_overlay: Option<&FilterOverlayState>` parameter.

Change the header drawing to:

```rust
    execute!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print("filter: "),
        ResetColor,
        Print(filters.summary()),
    )?;

    execute!(
        stdout,
        cursor::MoveTo(0, 1),
        SetForegroundColor(Color::Yellow),
        SetAttribute(Attribute::Bold),
        Print("> "),
        ResetColor,
        Print(query),
    )?;

    let count = format!("  {}/{}", filtered_indices.len(), conversations.len());
    execute!(
        stdout,
        cursor::MoveTo(0, 2),
        SetForegroundColor(Color::DarkGrey),
        Print(&count),
        Print(PICKER_HINT),
        ResetColor,
    )?;
```

Change list start:

```rust
    let list_start = PICKER_HEADER_ROWS;
```

Before final cursor placement, draw overlay if present:

```rust
    if let Some(overlay) = filter_overlay {
        draw_filter_overlay(stdout, conversations, filters, overlay, cols, rows)?;
    }
```

Set cursor to search row:

```rust
    execute!(stdout, cursor::MoveTo((2 + query.len()) as u16, 1))?;
```

Add a simple centered overlay:

```rust
fn draw_filter_overlay(
    stdout: &mut io::Stdout,
    conversations: &[Conversation],
    filters: &crate::filters::SessionFilters,
    overlay: &FilterOverlayState,
    cols: usize,
    rows: usize,
) -> io::Result<()> {
    let width = cols.min(72).max(40);
    let height = rows.min(16).max(8);
    let x = (cols.saturating_sub(width) / 2) as u16;
    let y = (rows.saturating_sub(height) / 2) as u16;

    for row in 0..height {
        execute!(
            stdout,
            cursor::MoveTo(x, y + row as u16),
            SetBackgroundColor(Color::Black),
            Print(" ".repeat(width)),
            ResetColor
        )?;
    }

    execute!(
        stdout,
        cursor::MoveTo(x + 2, y + 1),
        SetAttribute(Attribute::Bold),
        Print("Filters"),
        SetAttribute(Attribute::NoBold)
    )?;

    let agent_tab = if overlay.section == FilterSection::Agent { "[agent]" } else { " agent " };
    let project_tab = if overlay.section == FilterSection::Project { "[project]" } else { " project " };
    execute!(
        stdout,
        cursor::MoveTo(x + 2, y + 2),
        Print(agent_tab),
        Print("  "),
        Print(project_tab)
    )?;

    match overlay.section {
        FilterSection::Agent => {
            for (idx, source) in crate::filters::ALL_SOURCES.into_iter().enumerate() {
                let checked = if filters.source_enabled(source) { "[x]" } else { "[ ]" };
                execute!(
                    stdout,
                    cursor::MoveTo(x + 4, y + 4 + idx as u16),
                    if idx == overlay.agent_selected { SetAttribute(Attribute::Reverse) } else { SetAttribute(Attribute::NoReverse) },
                    Print(format!("{} {}", checked, source)),
                    SetAttribute(Attribute::NoReverse)
                )?;
            }
        }
        FilterSection::Project => {
            execute!(
                stdout,
                cursor::MoveTo(x + 4, y + 4),
                Print(format!("search: {}", overlay.project_query))
            )?;
            for (idx, project) in visible_filter_projects(conversations, &overlay.project_query)
                .into_iter()
                .take(height.saturating_sub(7))
                .enumerate()
            {
                let checked = if filters.project_enabled(&project) { "[x]" } else { "[ ]" };
                execute!(
                    stdout,
                    cursor::MoveTo(x + 4, y + 6 + idx as u16),
                    if idx == overlay.project_selected { SetAttribute(Attribute::Reverse) } else { SetAttribute(Attribute::NoReverse) },
                    Print(format!("{} {}", checked, truncate(&project, width.saturating_sub(10)))),
                    SetAttribute(Attribute::NoReverse)
                )?;
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 9: Run tests**

Run:

```bash
cargo test --lib interactive::tests
cargo test --lib
```

Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add src/interactive.rs
git commit -m "feat: add tui filter overlay"
```

---

## Task 6: Polish, Manual TUI Verification, and Docs

**Files:**
- Modify: `README.md`
- Modify: `src/interactive.rs` if manual testing exposes rendering issues.

- [ ] **Step 1: Update README keybindings**

In `README.md`, add a short keybinding line to the TUI section:

```markdown
- `F3`: open filters. Use left/right to switch filter sections, type to narrow, space to toggle the selected row or visible scope, and Esc to close while keeping the current filter state.
```

- [ ] **Step 2: Run full automated checks**

Run:

```bash
cargo fmt --check
cargo test
git diff --check
```

Expected:
- `cargo fmt --check`: exits 0
- `cargo test`: all tests pass
- `git diff --check`: exits 0

- [ ] **Step 3: Manual interactive verification**

Run:

```bash
cargo run --bin ah -- --source codex
```

Verify:
- TUI opens with only Codex visible.
- Header shows `filter: agent=[codex] project=[all]`.
- `F3` opens the filter overlay.
- In `agent`, enabling Claude loads Claude sessions.
- After load, the same project filter is still applied.
- In `project`, Ctrl-D disables all projects and Ctrl-A enables all projects.
- `Esc` closes overlay and keeps changed filter state.
- Search text filters inside the active source/project filter.

Run:

```bash
cargo run --bin ah -- --source codex --project agent-history
```

Verify:
- Header shows `agent=[codex]` and project narrowed to the matching project label.
- Enabling Claude loads Claude sessions.
- Newly loaded Claude sessions appear only if their `project_name` is one of the selected project labels.

- [ ] **Step 4: Commit**

```bash
git add README.md src/interactive.rs
git commit -m "docs: document tui filters"
```

---

## Self-Review

- Spec coverage:
  - Shared CLI/TUI filter model: Task 1 and Task 3.
  - `--source codex` initializes the source filter: Task 3.
  - TUI can enable a source that was not loaded: Task 5.
  - Missing source is lazy-loaded and merged: Task 2 and Task 5.
  - Same project filter applies to newly loaded sessions: Task 1 `ProjectSelection`, Task 5 lazy-load/refilter flow.
  - Project selection uses `All | Only(HashSet) | Contains(String)`, with `Only(empty)` meaning no project and `Contains` preserving CLI substring filters for lazy-loaded sources: Task 1 and Task 3.
  - TUI overlay with `F3`, left/right, space, Ctrl-space, project fuzzy search, and Esc keep-state behavior: Task 5.

- Placeholder scan:
  - No placeholder tokens or unspecified test steps.
  - Every task has exact files, commands, and expected results.

- Type consistency:
  - `SessionFilters`, `ProjectSelection`, `SessionStore`, `FilterOverlayState`, and `FilterSection` names are consistent across tasks.
  - `SessionSource` derives `Hash` before use in `HashSet`.
  - Project filters consistently use `project_name`, not display labels that may show subagent names.
