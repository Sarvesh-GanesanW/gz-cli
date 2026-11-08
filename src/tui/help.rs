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

pub fn global() -> Vec<HelpSection> {
    vec![
        HelpSection::new(
            "Browse",
            &[
                ("↑ ↓  j k", "move"),
                ("→  ⏎", "expand / open / drill in"),
                ("←  ⌫  esc", "back / collapse"),
                ("space", "toggle a module"),
                ("tab", "sidebar ↔ content"),
                ("ctrl+k  :", "jump anywhere, saved views, save this view"),
                ("/", "search everything"),
                ("ctrl+e", "recent screens and items"),
            ],
        ),
        HelpSection::new(
            "Tables",
            &[
                ("ctrl+f", "filter rows (live; every word must match)"),
                ("r", "reload"),
                ("[  ]", "scroll columns"),
                ("o  O", "sort by the next column / reverse the sort"),
                ("g  G  PgUp PgDn", "first / last / page"),
                ("i", "details of the row"),
                ("a", "actions for the row"),
            ],
        ),
        HelpSection::new(
            "Chat",
            &[
                ("n  ctrl+n", "new chat"),
                ("⏎ / alt+⏎", "send / newline"),
                ("esc", "cancel reply / leave composer"),
                ("PgUp PgDn  wheel", "scroll transcript"),
            ],
        ),
        HelpSection::new(
            "App",
            &[
                ("?", "toggle help"),
                ("ctrl+g", "open the notice in the status bar"),
                ("q  ctrl+c", "quit"),
            ],
        ),
    ]
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
