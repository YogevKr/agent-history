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

    #[cfg(test)]
    pub fn loaded_source(&self, source: SessionSource) -> bool {
        self.loaded_sources.contains(&source)
    }

    pub fn missing_enabled_sources(&self, filters: &SessionFilters) -> Vec<SessionSource> {
        filters
            .enabled_sources()
            .filter(|source| !self.loaded_sources.contains(source))
            .collect()
    }

    pub fn merge_loaded(&mut self, source: SessionSource, mut conversations: Vec<Conversation>) {
        self.conversations.append(&mut conversations);
        self.loaded_sources.insert(source);
        self.normalize();
    }

    #[allow(dead_code)]
    pub fn load_sources(
        sources: impl IntoIterator<Item = SessionSource>,
    ) -> crate::error::Result<Self> {
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

    fn normalize(&mut self) {
        self.conversations.sort_by(compare_conversations);
        let mut seen = HashSet::new();
        self.conversations.retain(|conversation| {
            seen.insert((conversation.source, conversation.session_id.clone()))
        });
    }
}

fn load_source(source: SessionSource) -> crate::error::Result<Vec<Conversation>> {
    match source {
        SessionSource::Claude => crate::claude_loader::load_claude_sessions(),
        SessionSource::Codex => crate::codex_loader::load_codex_sessions(),
    }
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
            directory_name: Some("agent-history".to_string()),
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

        assert_eq!(
            store.missing_enabled_sources(&filters),
            vec![SessionSource::Claude]
        );
    }

    #[test]
    fn merge_loaded_marks_source_and_normalizes() {
        let mut store = SessionStore::from_loaded(
            vec![conversation("same", SessionSource::Codex, 20)],
            [SessionSource::Codex],
        );

        store.merge_loaded(
            SessionSource::Claude,
            vec![
                conversation("new", SessionSource::Claude, 1),
                conversation("same", SessionSource::Claude, 5),
            ],
        );

        assert!(store.loaded_source(SessionSource::Claude));
        assert_eq!(
            store
                .conversations()
                .iter()
                .map(|conversation| (conversation.source, conversation.session_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (SessionSource::Claude, "new"),
                (SessionSource::Claude, "same"),
                (SessionSource::Codex, "same"),
            ]
        );
    }
}
