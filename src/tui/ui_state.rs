//! What the TUI remembers between launches: where you were, whether it
//! opens on the home dashboard, and each screen's sort and column scroll.
//! Kept in `tui_state.json` next to the gz config, owner-only. Places are
//! stored by their sidebar titles, not indexes, so adding a module never
//! sends you somewhere else; a missing or unreadable file is a fresh start.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "tui_state.json";

/// A sidebar resource by its titles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub module: String,
    pub resource: String,
}

/// What the TUI shows first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Start {
    /// The home dashboard over the last place.
    #[default]
    Home,
    /// The last place itself.
    Last,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortState {
    /// Column header text, so a reordered spec still finds it.
    pub column: String,
    pub descending: bool,
}

/// One screen's table layout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenState {
    pub sort: Option<SortState>,
    /// Columns scrolled past on the left (`[` / `]`).
    pub col_offset: usize,
}

impl ScreenState {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    pub last: Option<Place>,
    pub start: Start,
    /// Keyed by "Module › Resource".
    pub screens: BTreeMap<String, ScreenState>,
}

impl UiState {
    /// `<gz config folder>/tui_state.json` (honors `XDG_CONFIG_HOME`).
    pub fn path() -> Result<PathBuf> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(dir.join(FILE_NAME))
    }

    /// The state saved at `path`; a missing, unreadable or outdated file
    /// gives the defaults rather than an error, since losing a layout is
    /// harmless and refusing to start is not.
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes the state owner-only, through a temp file so a crash never
    /// leaves half a file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("state path has no folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let text = serde_json::to_string_pretty(self)?;
        let temp = path.with_extension("json.tmp");
        crate::assistant::history::write_private(&temp, &text)?;
        std::fs::rename(&temp, path).with_context(|| format!("cannot write {}", path.display()))
    }

    /// Records `screen` for `key`, dropping it when it is all defaults.
    pub fn set_screen(&mut self, key: String, screen: ScreenState) {
        if screen.is_default() {
            self.screens.remove(&key);
        } else {
            self.screens.insert(key, screen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gz-ui-state-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir.join(FILE_NAME)
    }

    fn sample() -> UiState {
        let mut state = UiState {
            last: Some(Place {
                module: "Lakehouse".into(),
                resource: "Warehouses".into(),
            }),
            start: Start::Last,
            ..UiState::default()
        };
        state.set_screen(
            "Lakehouse › Warehouses".into(),
            ScreenState {
                sort: Some(SortState {
                    column: "name".into(),
                    descending: true,
                }),
                col_offset: 2,
            },
        );
        state
    }

    #[test]
    fn state_survives_a_round_trip() {
        let path = temp_path("round-trip");
        sample().save_to(&path).unwrap();
        assert_eq!(UiState::load_from(&path), sample());
    }

    #[test]
    fn missing_or_corrupt_files_start_fresh() {
        let path = temp_path("corrupt");
        assert_eq!(UiState::load_from(&path), UiState::default());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(UiState::load_from(&path), UiState::default());
        std::fs::write(&path, r#"{"start": "last", "future": 1}"#).unwrap();
        assert_eq!(
            UiState::load_from(&path).start,
            Start::Last,
            "unknown and missing keys are fine"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("mode");
        sample().save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn default_layouts_are_not_stored() {
        let mut state = sample();
        state.set_screen("Lakehouse › Warehouses".into(), ScreenState::default());
        assert!(state.screens.is_empty());
    }
}
