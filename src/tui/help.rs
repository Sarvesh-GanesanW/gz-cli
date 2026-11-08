//! What the `?` overlay lists: the global keys, then the keys, actions and
//! links of the screen or view in front. Screens declare theirs in their
//! `ScreenSpec` (`keys`, `actions`, `links`); views in `View::help`.

use serde_json::Value;

use crate::tui::screen::ScreenSpec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpSection {
    pub title: String,
    pub entries: Vec<(String, String)>,
}

impl HelpSection {
    pub fn new(title: impl Into<String>, entries: &[(&str, &str)]) -> Self {
        Self {
            title: title.into(),
            entries: entries
                .iter()
                .map(|(keys, what)| (keys.to_string(), what.to_string()))
                .collect(),
        }
    }
}

/// Keys a table uses itself; screen actions and links must not reuse them
/// (a test checks every spec).
#[cfg(test)]
pub const GRID_KEYS: &[char] = &[
    'j', 'k', 'h', 'l', 'g', 'G', '/', '[', ']', 'r', 'q', '?', 'a', 'i', ':', 'o', 'O', 'H',
];

/// Keys every detail view uses itself.
#[cfg(test)]
pub const DETAIL_KEYS: &[char] = &['j', 'k', 'g', 'G', 'J', 'y', 'Y', 'a', 'r', 'q', '?'];

/// Keys every table understands.
const TABLE_KEYS: &[(&str, &str)] = &[
    ("ctrl+f", "filter rows (live; every word must match)"),
    ("r", "reload"),
    ("[  ]", "scroll columns"),
    ("o  O", "sort by the next column / reverse the sort"),
    ("g  G  PgUp PgDn", "first / last / page"),
    ("i", "details of the row"),
    ("a", "actions for the row"),
];

const CHAT_KEYS: &[(&str, &str)] = &[
    ("n  ctrl+n", "new chat"),
    ("⏎ / alt+⏎", "send / newline"),
    ("esc", "cancel reply / leave composer"),
    ("PgUp PgDn  wheel", "scroll transcript"),
];

/// "This screen" for a table: its own keys, then the table keys.
pub fn table(spec: Option<&ScreenSpec>) -> HelpSection {
    let mut section = HelpSection::new("This screen", TABLE_KEYS);
    if let Some(spec) = spec {
        let own = spec
            .keys
            .iter()
            .map(|k| (k.keys.to_string(), k.what.to_string()));
        section.entries.splice(0..0, own);
    }
    section
}

/// "This screen" for the chat page.
pub fn chat() -> HelpSection {
    HelpSection::new("This screen", CHAT_KEYS)
}

/// The row's actions and links, or None when the screen offers none.
pub fn actions(spec: &ScreenSpec, row: Option<&Value>) -> Option<HelpSection> {
    let mut section = for_screen("Actions", spec, row);
    section.entries.drain(..spec.keys.len());
    (!section.entries.is_empty()).then_some(section)
}

/// Where you can go from anywhere, then the keys that always work.
pub fn global() -> Vec<HelpSection> {
    vec![
        HelpSection::new(
            "Views",
            &[
                ("H", "home dashboard"),
                ("/", "search everything"),
                ("ctrl+k  :", "jump anywhere, saved views, save this view"),
                ("ctrl+e", "recent screens and items"),
                ("ctrl+g", "open the notice in the status bar"),
                ("i  ⏎", "a row's details"),
            ],
        ),
        HelpSection::new(
            "Global",
            &[
                ("↑ ↓  j k", "move"),
                ("→  ⏎", "expand / open / drill in"),
                ("←  ⌫  esc", "back / collapse"),
                ("space", "toggle a module"),
                ("tab", "sidebar ↔ content"),
                ("?", "toggle help"),
                ("q  ctrl+c", "quit"),
            ],
        ),
    ]
}

/// The `?` overlay's search and scroll position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelpState {
    pub query: String,
    /// The search box has the keys.
    pub searching: bool,
    pub scroll: usize,
}

impl HelpState {
    /// The sections whose title or entries match the query; a matching
    /// title keeps its whole section. Case is ignored.
    pub fn filter(&self, sections: &[HelpSection]) -> Vec<HelpSection> {
        let query = self.query.trim().to_lowercase();
        if query.is_empty() {
            return sections.to_vec();
        }
        let hit = |text: &str| text.to_lowercase().contains(&query);
        sections
            .iter()
            .filter_map(|section| {
                if hit(&section.title) {
                    return Some(section.clone());
                }
                let entries: Vec<(String, String)> = section
                    .entries
                    .iter()
                    .filter(|(keys, what)| hit(keys) || hit(what))
                    .cloned()
                    .collect();
                (!entries.is_empty()).then(|| HelpSection {
                    title: section.title.clone(),
                    entries,
                })
            })
            .collect()
    }
}

/// The screen's own keys, then its actions and links for `row` (all of
/// them when no row is selected).
pub fn for_screen(title: &str, spec: &ScreenSpec, row: Option<&Value>) -> HelpSection {
    let mut entries: Vec<(String, String)> = spec
        .keys
        .iter()
        .map(|k| (k.keys.to_string(), k.what.to_string()))
        .collect();
    let offered = |when| row.is_none_or(|row| crate::tui::screen::allowed(when, row));
    entries.extend(
        spec.actions
            .iter()
            .filter(|a| offered(a.when))
            .map(|a| (a.key.to_string(), a.label.to_string())),
    );
    entries.extend(
        spec.links
            .iter()
            .filter(|l| offered(l.when))
            .map(|l| (l.key.to_string(), format!("go to {}", l.label))),
    );
    HelpSection {
        title: title.to_string(),
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::nav::all_resources;

    #[test]
    fn search_keeps_matching_entries_and_whole_matching_sections() {
        let sections = global();
        let state = HelpState {
            query: "QUIT".into(),
            ..HelpState::default()
        };
        let found = state.filter(&sections);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].entries, vec![("q  ctrl+c".into(), "quit".into())]);
        let state = HelpState {
            query: "views".into(),
            ..HelpState::default()
        };
        assert_eq!(state.filter(&sections)[0], sections[0]);
        let state = HelpState {
            query: "zzz".into(),
            ..HelpState::default()
        };
        assert!(state.filter(&sections).is_empty());
    }

    #[test]
    fn table_help_lists_the_screens_own_keys_first() {
        static SPEC: ScreenSpec = ScreenSpec {
            keys: &[crate::tui::screen::KeyHelp {
                keys: "s",
                what: "browse tables",
            }],
            ..ScreenSpec::BLANK
        };
        let section = table(Some(&SPEC));
        assert_eq!(section.title, "This screen");
        let first_table_key = section
            .entries
            .iter()
            .position(|(keys, _)| keys == "ctrl+f")
            .unwrap();
        assert_eq!(first_table_key, 1);
        assert_eq!(section.entries[0].0, "s");
        assert!(actions(&SPEC, None).is_none(), "no actions, no section");
        let connections = &crate::tui::specs::connections::CONNECTIONS;
        let actions = actions(connections, None).expect("connections have actions");
        assert!(actions
            .entries
            .iter()
            .all(|(keys, _)| keys.chars().count() == 1));
    }

    /// Every key a spec binds must be free on the table and in the detail
    /// view, and unique within the screen.
    #[test]
    fn spec_keys_never_shadow_built_in_keys() {
        for id in all_resources() {
            let Some(grid) = id.grid_spec() else { continue };
            for depth in 0..=grid.drills.len() {
                let Some(spec) = grid.screen_at(depth) else {
                    continue;
                };
                let keys: Vec<char> = spec
                    .actions
                    .iter()
                    .map(|a| a.key)
                    .chain(spec.links.iter().map(|l| l.key))
                    .collect();
                for key in &keys {
                    assert!(
                        !GRID_KEYS.contains(key) && !DETAIL_KEYS.contains(key),
                        "{} binds built-in key {key}",
                        id.title()
                    );
                    let uses = keys.iter().filter(|k| *k == key).count();
                    assert_eq!(uses, 1, "{} binds {key} twice", id.title());
                }
            }
        }
    }
}
