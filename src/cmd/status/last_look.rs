//! When you last ran `gz status`, per profile, so the next one shows only
//! the job runs that failed since. Kept in `status_state.json` next to the
//! gz config, owner-only. A missing file means you never looked.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const FILE: &str = "status_state.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    /// Epoch milliseconds of the last look, by profile name.
    #[serde(default)]
    last_look_ms: BTreeMap<String, i64>,
}

pub struct LastLook {
    path: PathBuf,
    profile: String,
}

impl LastLook {
    /// The store in the gz config folder (honors `XDG_CONFIG_HOME`).
    pub fn open(profile: &str) -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(Self::at(dir.join(FILE), profile))
    }

    fn at(path: PathBuf, profile: &str) -> Self {
        Self {
            path,
            profile: profile.to_string(),
        }
    }

    pub fn get(&self) -> Result<Option<i64>> {
        Ok(self.load()?.last_look_ms.get(&self.profile).copied())
    }

    pub fn set(&self, at_ms: i64) -> Result<()> {
        let mut book = self.load()?;
        book.last_look_ms.insert(self.profile.clone(), at_ms);
        let text = serde_json::to_string_pretty(&book).context("cannot encode the last look")?;
        crate::assistant::private_file::write_private(&self.path, &text)
    }

    /// A damaged file is an error rather than a silent fresh start, which
    /// would hide every failure since.
    fn load(&self) -> Result<Book> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Book::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.path.display()))
            }
        };
        serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is damaged; delete it to start over, or pass --since",
                self.path.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_each_profile_apart() {
        let dir = std::env::temp_dir().join(format!("gz-last-look-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        let _ = std::fs::remove_file(&path);
        let stage = LastLook::at(path.clone(), "stage");
        let dev = LastLook::at(path.clone(), "dev");
        assert_eq!(stage.get().unwrap(), None);
        stage.set(10).unwrap();
        dev.set(20).unwrap();
        assert_eq!(stage.get().unwrap(), Some(10));
        assert_eq!(dev.get().unwrap(), Some(20));
        std::fs::write(&path, "not json").unwrap();
        assert!(
            stage.get().is_err(),
            "a damaged file is never read as empty"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
