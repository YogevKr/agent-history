use crate::history::{Conversation, SessionSource};
use chrono::{DateTime, Local};
use std::collections::HashSet;
use std::path::PathBuf;

pub const ALL_SOURCES: [SessionSource; 2] = [SessionSource::Claude, SessionSource::Codex];

pub enum DirectorySelection {
    All,
    Only(HashSet<String>),
    Contains(String),
}

pub struct SessionFilters {
    enabled_sources: HashSet<SessionSource>,
    directory_selection: DirectorySelection,
    since_secs: Option<i64>,
    local_cwd: Option<PathBuf>,
    time_anchor: DateTime<Local>,
}

impl SessionFilters {
    pub fn all() -> Self {
        Self {
            enabled_sources: ALL_SOURCES.into_iter().collect(),
            directory_selection: DirectorySelection::All,
            since_secs: None,
            local_cwd: None,
            time_anchor: Local::now(),
        }
    }

    pub fn source_only(source: SessionSource) -> Self {
        Self {
            enabled_sources: HashSet::from([source]),
            directory_selection: DirectorySelection::All,
            since_secs: None,
            local_cwd: None,
            time_anchor: Local::now(),
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
            return self.enabled_sources.insert(source);
        }

        self.enabled_sources.remove(&source)
    }

    pub fn toggle_source(&mut self, source: SessionSource) -> bool {
        self.set_source_enabled(source, !self.source_enabled(source))
    }

    pub fn set_sources_enabled(
        &mut self,
        sources: impl IntoIterator<Item = SessionSource>,
        enabled: bool,
    ) -> bool {
        let mut changed = false;
        for source in sources {
            changed |= self.set_source_enabled(source, enabled);
        }
        changed
    }

    #[cfg(test)]
    pub fn set_no_directories(&mut self) {
        self.directory_selection = DirectorySelection::Only(HashSet::new());
    }

    pub fn set_directory_contains(&mut self, directory: &str) {
        self.directory_selection = DirectorySelection::Contains(directory.to_lowercase());
    }

    pub fn set_since_secs(&mut self, since_secs: i64) {
        self.since_secs = Some(since_secs);
    }

    pub fn set_local_cwd(&mut self, cwd: PathBuf) {
        self.local_cwd = Some(cwd);
    }

    #[cfg(test)]
    pub fn only_directory(&mut self, directory: &str) {
        self.directory_selection = DirectorySelection::Only(HashSet::from([directory.to_string()]));
    }

    pub fn toggle_directory(
        &mut self,
        directory: &str,
        available_directories: impl IntoIterator<Item = String>,
    ) {
        match &mut self.directory_selection {
            DirectorySelection::All => {
                self.directory_selection = DirectorySelection::Only(
                    available_directories
                        .into_iter()
                        .filter(|available| available != directory)
                        .collect(),
                );
            }
            DirectorySelection::Only(directories) => {
                if !directories.remove(directory) {
                    directories.insert(directory.to_string());
                }
            }
            DirectorySelection::Contains(_) => {
                self.directory_selection = DirectorySelection::Only(
                    available_directories
                        .into_iter()
                        .filter(|available| available != directory)
                        .collect(),
                );
            }
        }
    }

    pub fn set_directories_enabled(
        &mut self,
        directories: impl IntoIterator<Item = String>,
        available_directories: impl IntoIterator<Item = String>,
        enabled: bool,
    ) {
        let directories = directories.into_iter().collect::<HashSet<_>>();
        match &mut self.directory_selection {
            DirectorySelection::All => {
                if !enabled {
                    self.directory_selection = DirectorySelection::Only(
                        available_directories
                            .into_iter()
                            .filter(|available| !directories.contains(available))
                            .collect(),
                    );
                }
            }
            DirectorySelection::Only(selected) => {
                if enabled {
                    selected.extend(directories);
                } else {
                    selected.retain(|directory| !directories.contains(directory));
                }
            }
            DirectorySelection::Contains(needle) => {
                let needle = needle.clone();
                let mut selected = available_directories
                    .into_iter()
                    .filter(|available| available.to_lowercase().contains(&needle))
                    .collect::<HashSet<_>>();
                if enabled {
                    selected.extend(directories);
                } else {
                    selected.retain(|directory| !directories.contains(directory));
                }
                self.directory_selection = DirectorySelection::Only(selected);
            }
        }
    }

    pub fn directory_enabled(&self, directory: &str) -> bool {
        match &self.directory_selection {
            DirectorySelection::All => true,
            DirectorySelection::Only(directories) => directories.contains(directory),
            DirectorySelection::Contains(needle) => directory.to_lowercase().contains(needle),
        }
    }

    pub fn matches(&self, conv: &Conversation) -> bool {
        if !self.source_enabled(conv.source) {
            return false;
        }

        if let Some(secs) = self.since_secs {
            let age = self
                .time_anchor
                .signed_duration_since(conv.timestamp)
                .num_seconds();
            if age > secs {
                return false;
            }
        }

        if let Some(cwd) = self.local_cwd.as_ref() {
            if conv.cwd.as_ref() != Some(cwd) {
                return false;
            }
        }

        match &self.directory_selection {
            DirectorySelection::All => true,
            DirectorySelection::Only(directories) => conv
                .directory_name
                .as_deref()
                .is_some_and(|directory| directories.contains(directory)),
            DirectorySelection::Contains(directory) => conv
                .directory_name
                .as_deref()
                .is_some_and(|name| name.to_lowercase().contains(directory)),
        }
    }

    pub fn filter_indices(
        &self,
        conversations: &[Conversation],
        indices: Vec<usize>,
    ) -> Vec<usize> {
        indices
            .into_iter()
            .filter(|&idx| {
                conversations
                    .get(idx)
                    .is_some_and(|conv| self.matches(conv))
            })
            .collect()
    }

    pub fn summary(&self) -> String {
        let agent = if ALL_SOURCES
            .into_iter()
            .all(|source| self.enabled_sources.contains(&source))
        {
            "all".to_string()
        } else {
            let names = self
                .enabled_sources()
                .map(|source| source.to_string())
                .collect::<Vec<_>>();
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(",")
            }
        };

        let directory = match &self.directory_selection {
            DirectorySelection::All => "all".to_string(),
            DirectorySelection::Only(directories) if directories.is_empty() => "none".to_string(),
            DirectorySelection::Only(directories) => {
                let mut names = directories.iter().cloned().collect::<Vec<_>>();
                names.sort();
                names.join(",")
            }
            DirectorySelection::Contains(directory) => directory.clone(),
        };

        let mut parts = vec![
            format!("agent=[{}]", agent),
            format!("directory=[{}]", directory),
        ];
        if let Some(secs) = self.since_secs {
            parts.push(format!("since=[{}s]", secs));
        }
        if let Some(cwd) = self.local_cwd.as_ref() {
            parts.push(format!("local=[{}]", cwd.display()));
        }

        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::path::PathBuf;

    fn conversation(
        source: SessionSource,
        directory_name: Option<&str>,
        subagent_name: Option<&str>,
    ) -> Conversation {
        let timestamp = Local::now();
        Conversation {
            path: PathBuf::from("session.jsonl"),
            source,
            session_id: "session-id".to_string(),
            timestamp,
            preview: String::new(),
            full_text: String::new(),
            directory_name: directory_name.map(str::to_string),
            cwd: None,
            message_count: 1,
            model: None,
            total_tokens: 0,
            duration_minutes: None,
            summary: None,
            custom_title: None,
            git_branch: None,
            subagent_name: subagent_name.map(str::to_string),
            hierarchy_has_children: false,
            hierarchy_has_next_sibling: false,
            hierarchy_marker: None,
            hierarchy_depth: 0,
            hierarchy_order: 0,
            hierarchy_sort_timestamp: timestamp,
        }
    }

    #[test]
    fn all_filters_match_all_sources_and_directories() {
        let filters = SessionFilters::all();

        assert!(filters.matches(&conversation(SessionSource::Claude, Some("frontend"), None)));
        assert!(filters.matches(&conversation(SessionSource::Codex, Some("backend"), None)));
        assert!(filters.matches(&conversation(SessionSource::Codex, None, None)));
        assert_eq!(filters.summary(), "agent=[all] directory=[all]");
    }

    #[test]
    fn source_only_matches_one_source() {
        let filters = SessionFilters::source_only(SessionSource::Claude);

        assert!(filters.matches(&conversation(
            SessionSource::Claude,
            Some("directory"),
            None
        )));
        assert!(!filters.matches(&conversation(SessionSource::Codex, Some("directory"), None)));
    }

    #[test]
    fn directory_only_empty_matches_no_directories() {
        let mut filters = SessionFilters::all();
        filters.set_no_directories();

        assert!(!filters.matches(&conversation(
            SessionSource::Claude,
            Some("directory"),
            None
        )));
        assert!(!filters.matches(&conversation(SessionSource::Codex, None, None)));
        assert_eq!(filters.summary(), "agent=[all] directory=[none]");
    }

    #[test]
    fn directory_only_uses_directory_name_not_subagent_name() {
        let mut filters = SessionFilters::all();
        filters.only_directory("root-directory");

        assert!(filters.matches(&conversation(
            SessionSource::Codex,
            Some("root-directory"),
            Some("reviewer"),
        )));
        assert!(!filters.matches(&conversation(
            SessionSource::Codex,
            Some("other-directory"),
            Some("root-directory"),
        )));
    }

    #[test]
    fn directory_only_unknown_does_not_match_missing_directory_name() {
        let mut filters = SessionFilters::all();
        filters.only_directory("unknown");

        assert!(filters.matches(&conversation(SessionSource::Codex, Some("unknown"), None,)));
        assert!(!filters.matches(&conversation(SessionSource::Codex, None, None)));
    }

    #[test]
    fn directory_contains_matches_later_loaded_directory_names() {
        let mut filters = SessionFilters::all();
        filters.set_directory_contains("agent");

        assert!(filters.matches(&conversation(
            SessionSource::Claude,
            Some("agent-history"),
            None,
        )));
        assert!(filters.matches(&conversation(
            SessionSource::Codex,
            Some("Agent Tools"),
            None,
        )));
        assert!(!filters.matches(&conversation(SessionSource::Codex, Some("other"), None,)));
        assert!(!filters.matches(&conversation(SessionSource::Codex, None, None)));
    }

    #[test]
    fn toggling_directory_from_all_disables_only_that_directory() {
        let mut filters = SessionFilters::all();

        filters.toggle_directory(
            "backend",
            ["frontend", "backend", "cli"].into_iter().map(String::from),
        );

        assert!(filters.directory_enabled("frontend"));
        assert!(!filters.directory_enabled("backend"));
        assert!(filters.directory_enabled("cli"));
        assert!(!filters.directory_enabled("new-directory"));
        assert_eq!(filters.summary(), "agent=[all] directory=[cli,frontend]");
    }

    #[test]
    fn toggling_directory_in_only_mode_adds_and_removes() {
        let mut filters = SessionFilters::all();
        filters.only_directory("frontend");

        filters.toggle_directory(
            "backend",
            ["frontend", "backend"].into_iter().map(String::from),
        );
        assert!(filters.directory_enabled("frontend"));
        assert!(filters.directory_enabled("backend"));

        filters.toggle_directory(
            "frontend",
            ["frontend", "backend"].into_iter().map(String::from),
        );
        assert!(!filters.directory_enabled("frontend"));
        assert!(filters.directory_enabled("backend"));
        assert_eq!(filters.summary(), "agent=[all] directory=[backend]");
    }

    #[test]
    fn disabling_last_source_is_allowed() {
        let mut filters = SessionFilters::source_only(SessionSource::Codex);

        assert!(filters.set_source_enabled(SessionSource::Codex, false));
        assert!(!filters.source_enabled(SessionSource::Codex));
        assert_eq!(filters.summary(), "agent=[none] directory=[all]");
    }

    #[test]
    fn source_summary_reports_active_source() {
        let filters = SessionFilters::source_only(SessionSource::Claude);

        assert_eq!(filters.summary(), "agent=[claude] directory=[all]");
    }
}
