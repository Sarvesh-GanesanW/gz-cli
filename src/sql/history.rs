//! Statement history for the SQL shell, kept next to the config file
//! (`~/.config/gz/sql_history`), one JSON string per line so multi-line
//! statements survive, readable only by the user.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Entries kept on disk and in memory.
const MAX_ENTRIES: usize = 1000;

pub struct History {
    path: Option<PathBuf>,
    entries: Vec<String>,
    position: Option<usize>,
}

pub fn default_path() -> Option<PathBuf> {
    let config = crate::config::config_path().ok()?;
    Some(config.parent()?.join("sql_history"))
}

impl History {
    /// Loads `path`; a missing or unreadable file starts an empty history.
    pub fn load(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_deref()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<String>(line).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let skip = entries.len().saturating_sub(MAX_ENTRIES);
        Self {
            path,
            entries: entries.into_iter().skip(skip).collect(),
            position: None,
        }
    }

    /// Records `entry` (skipping an immediate repeat) and saves.
    pub fn push(&mut self, entry: &str) -> Result<()> {
        self.position = None;
        let entry = entry.trim();
        if entry.is_empty() || self.entries.last().map(String::as_str) == Some(entry) {
            return Ok(());
        }
        self.entries.push(entry.to_string());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        match &self.path {
            Some(path) => save(path, &self.entries),
            None => Ok(()),
        }
    }

    /// The entry one step back (`older`) or forward; `None` past the newest.
    pub fn step(&mut self, older: bool) -> Option<String> {
        let last = self.entries.len().checked_sub(1)?;
        self.position = match (self.position, older) {
            (None, true) => Some(last),
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i >= last => None,
            (Some(i), false) => Some(i + 1),
        };
        self.position.map(|i| self.entries[i].clone())
    }
}

fn save(path: &Path, entries: &[String]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    for entry in entries {
        writeln!(file, "{}", serde_json::to_string(entry)?)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gz-sqlhist-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("sql_history")
    }

    #[test]
    fn persists_multiline_entries_privately() {
        let path = temp("persist");
        let mut history = History::load(Some(path.clone()));
        history.push("select 1\nfrom t;").unwrap();
        history.push("select 1\nfrom t;").unwrap();
        history.push("select 2;").unwrap();
        let again = History::load(Some(path.clone()));
        assert_eq!(again.entries, vec!["select 1\nfrom t;", "select 2;"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn steps_back_and_forward() {
        let mut history = History::load(None);
        history.push("a;").unwrap();
        history.push("b;").unwrap();
        assert_eq!(history.step(true).as_deref(), Some("b;"));
        assert_eq!(history.step(true).as_deref(), Some("a;"));
        assert_eq!(history.step(true).as_deref(), Some("a;"));
        assert_eq!(history.step(false).as_deref(), Some("b;"));
        assert_eq!(history.step(false), None);
    }
}
