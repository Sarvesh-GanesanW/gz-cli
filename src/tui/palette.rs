//! Ctrl+K / `:` command palette: fuzzy-jump to any resource, recent item,
//! or action.

use crate::tui::nav::{all_resources, ResourceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Open(ResourceId),
    OpenConversation { id: String, title: String },
    NewChat,
    Reload,
    Help,
    SignOutHint,
    Quit,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub label: String,
    pub hint: &'static str,
    pub action: Action,
}

pub struct Palette {
    pub query: String,
    pub selected: usize,
    entries: Vec<Entry>,
    pub matches: Vec<usize>,
}

impl Palette {
    /// Recent items come first so an empty query shows them on top.
    pub fn new(recent: &[Entry]) -> Self {
        let mut entries: Vec<Entry> = recent.to_vec();
        entries.extend(all_resources().map(|id| Entry {
            label: id.title(),
            hint: "go to",
            action: Action::Open(id),
        }));
        entries.extend(
            [
                ("New chat", Action::NewChat),
                ("Reload this view", Action::Reload),
                ("Keyboard shortcuts", Action::Help),
                ("Sign out", Action::SignOutHint),
                ("Quit gz", Action::Quit),
            ]
            .into_iter()
            .map(|(label, action)| Entry {
                label: label.to_string(),
                hint: "action",
                action,
            }),
        );
        let mut palette = Self {
            query: String::new(),
            selected: 0,
            entries,
            matches: Vec::new(),
        };
        palette.refilter();
        palette
    }

    pub fn entry(&self, index: usize) -> &Entry {
        &self.entries[index]
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn chosen(&self) -> Option<Action> {
        let index = *self.matches.get(self.selected)?;
        Some(self.entries[index].action.clone())
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(i32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| fuzzy_score(&self.query, &e.label).map(|s| (s, i)))
            .collect();
        // Stable sort keeps the recent-first order among equal scores.
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }
}

/// Subsequence match, case-insensitive. Higher is better: consecutive
/// characters and word starts score extra, so "dash" ranks "Dashboards"
/// above "Designer › Datasets". `None` when `query` isn't a subsequence.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut pos = 0;
    let mut previous: Option<usize> = None;
    for q in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (pos..text.len()).find(|&i| text[i] == q)?;
        score += 1;
        if previous == Some(found.wrapping_sub(1)) {
            score += 5;
        }
        let at_word_start = found == 0 || !text[found - 1].is_alphanumeric();
        if at_word_start {
            score += 3;
        }
        previous = Some(found);
        pos = found + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_subsequences_and_rejects_others() {
        assert!(fuzzy_score("dsh", "Dashboards").is_some());
        assert!(fuzzy_score("xyz", "Dashboards").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn prefers_consecutive_and_word_start_matches() {
        let dash = fuzzy_score("dash", "Designer › Dashboards").unwrap();
        let scattered = fuzzy_score("dash", "Designer › Datasets hidden").unwrap_or(0);
        assert!(dash > scattered);
    }

    #[test]
    fn filters_and_picks_an_action() {
        let mut palette = Palette::new(&[]);
        for c in "wareh".chars() {
            palette.push(c);
        }
        let first = palette.entry(palette.matches[0]).label.clone();
        assert_eq!(first, "Lakehouse › Warehouses");
        assert!(matches!(palette.chosen(), Some(Action::Open(_))));
        palette.pop();
        palette.query.clear();
        palette.push('q');
        palette.push('u');
        palette.push('i');
        palette.push('t');
        assert_eq!(palette.chosen(), Some(Action::Quit));
    }

    #[test]
    fn recent_items_lead_an_empty_query() {
        let recent = vec![Entry {
            label: "gz CLI test".into(),
            hint: "recent chat",
            action: Action::OpenConversation {
                id: "c1".into(),
                title: "gz CLI test".into(),
            },
        }];
        let palette = Palette::new(&recent);
        assert_eq!(palette.entry(palette.matches[0]).label, "gz CLI test");
    }
}
