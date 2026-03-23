use crate::history::Conversation;
use chrono::{DateTime, Duration, Local};
use rayon::prelude::*;

/// Precomputed search data for a conversation
pub struct SearchableConversation {
    /// Lowercased full text for searching
    pub text_lower: String,
    /// Original conversation index
    pub index: usize,
}

/// Normalize text for search: lowercase, replace separators with spaces
fn normalize_for_search(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '_' || ch == '-' || ch == '/' {
            out.push(' ');
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// Precompute lowercased search text for all conversations
pub fn precompute_search_text(conversations: &[Conversation]) -> Vec<SearchableConversation> {
    conversations
        .par_iter()
        .enumerate()
        .map(|(idx, conv)| {
            let mut text = conv.full_text.clone();
            if let Some(ref name) = conv.project_name {
                text.push(' ');
                text.push_str(name);
            }
            SearchableConversation {
                text_lower: normalize_for_search(&text),
                index: idx,
            }
        })
        .collect()
}

/// Filter and score conversations based on query.
/// Returns indices into the original conversations vec, sorted by score descending.
pub fn search(
    conversations: &[Conversation],
    searchable: &[SearchableConversation],
    query: &str,
    now: DateTime<Local>,
) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..conversations.len()).collect();
    }

    let query_lower = normalize_for_search(query);
    let query_words: Vec<&str> = query_lower.split_whitespace().collect();
    if query_words.is_empty() {
        return (0..conversations.len()).collect();
    }

    let mut scored: Vec<(usize, f64, DateTime<Local>)> = searchable
        .par_iter()
        .filter_map(|s| {
            let score = score_text(
                &s.text_lower,
                &query_words,
                conversations[s.index].timestamp,
                now,
            );
            if score > 0.0 {
                Some((s.index, score, conversations[s.index].timestamp))
            } else {
                None
            }
        })
        .collect();

    // Sort by score descending, then by timestamp descending for stability
    scored.sort_unstable_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
    });

    scored.into_iter().map(|(idx, _, _)| idx).collect()
}

/// Score a conversation based on word prefix matching and recency.
/// Each query word must be a prefix of at least one word in the text (AND logic).
fn score_text(
    text_lower: &str,
    query_words: &[&str],
    timestamp: DateTime<Local>,
    now: DateTime<Local>,
) -> f64 {
    if query_words.is_empty() {
        return 0.0;
    }

    // Fast rejection: if a query word isn't present as substring, skip
    for &qw in query_words {
        if !text_lower.contains(qw) {
            return 0.0;
        }
    }

    // Single-pass word matching with prefix match
    let mut matched = vec![false; query_words.len()];
    let mut remaining = query_words.len();

    for text_word in text_lower.split_whitespace() {
        for (i, &qw) in query_words.iter().enumerate() {
            if !matched[i] && text_word.starts_with(qw) {
                matched[i] = true;
                remaining -= 1;
                if remaining == 0 {
                    return (query_words.len() as f64) * recency_multiplier(timestamp, now);
                }
            }
        }
    }

    0.0
}

/// Calculate recency multiplier based on age
fn recency_multiplier(timestamp: DateTime<Local>, now: DateTime<Local>) -> f64 {
    let age = now.signed_duration_since(timestamp);

    if age < Duration::zero() {
        return 3.0;
    }

    if age < Duration::days(1) {
        3.0
    } else if age < Duration::days(7) {
        2.0
    } else if age < Duration::days(30) {
        1.5
    } else {
        1.0
    }
}
