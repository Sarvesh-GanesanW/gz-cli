//! What the user allowed "for this chat", kept per conversation in a
//! private file next to gz's config, so `gz code --continue`, `-c <id>` and
//! `/resume` of the same conversation keep those answers. A new
//! conversation starts with none.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant::permissions::Rules;
use crate::assistant::private_file::write_private;

/// Files of conversations untouched this long are deleted when gz next
/// saves one: a chat's grants are not meant to outlive the chat.
const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// One conversation's chat-scoped answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatGrants {
    #[serde(default)]
    pub rules: Rules,
    /// Server tools allowed for this chat, by name.
    #[serde(default)]
    pub actions: BTreeSet<String>,
    /// Hashes of scripts that tried to write while running read-only.
    #[serde(default)]
    pub scripts_wrote: BTreeSet<String>,
}

impl ChatGrants {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// `<gz config folder>/chat-grants/<conversation id>.json`, owner-only.
#[derive(Debug, Clone)]
pub struct ChatGrantStore {
    dir: PathBuf,
}

impl ChatGrantStore {
    /// The store next to gz's config (honors `XDG_CONFIG_HOME`). Unit tests
    /// get none, so a session they build never writes the user's config.
    pub fn open() -> Result<Self> {
        if cfg!(test) {
            anyhow::bail!("unit tests keep chat grants in memory");
        }
        let config = crate::config::config_path()?;
        let dir = config.parent().context("config path has no folder")?;
        Ok(Self::at(dir.join("chat-grants")))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The file for `conversation`; `None` for an id that isn't a plain
    /// name (ids are UUIDs, but they come from the command line too).
    fn path(&self, conversation: &str) -> Option<PathBuf> {
        let plain = !conversation.is_empty()
            && conversation
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        plain.then(|| self.dir.join(format!("{conversation}.json")))
    }

    /// What was allowed in `conversation`; empty when nothing was.
    pub fn load(&self, conversation: &str) -> Result<ChatGrants> {
        let Some(path) = self.path(conversation).filter(|p| p.exists()) else {
            return Ok(ChatGrants::default());
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is damaged; delete it to start this chat's permissions over",
                path.display()
            )
        })
    }

    /// Saves `grants` for `conversation` (removes the file when empty) and
    /// deletes other conversations' files gone stale.
    pub fn save(&self, conversation: &str, grants: &ChatGrants) -> Result<()> {
        let Some(path) = self.path(conversation) else {
            return Ok(());
        };
        if grants.is_empty() {
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("cannot remove {}", path.display()))?;
            }
            return Ok(());
        }
        let text = serde_json::to_string_pretty(grants)?;
        write_private(&path, &text)?;
        self.prune();
        Ok(())
    }

    /// Deletes files untouched for [`KEEP_FOR`]; best effort.
    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > KEEP_FOR);
            if stale {
                std::fs::remove_file(entry.path()).ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    #[test]
    fn grants_round_trip_per_conversation() {
        let store = ChatGrantStore::at(temp_dir("chat-grants").join("store"));
        let mut grants = ChatGrants::default();
        grants.rules.edits = true;
        grants.rules.commands.insert("cargo test".into());
        grants.actions.insert("submit_job".into());
        store.save("conv-1", &grants).unwrap();
        assert_eq!(store.load("conv-1").unwrap(), grants);
        assert!(store.load("conv-2").unwrap().is_empty());
        store.save("conv-1", &ChatGrants::default()).unwrap();
        assert!(store.load("conv-1").unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("chat-grants-mode").join("store");
        let store = ChatGrantStore::at(dir.clone());
        let mut grants = ChatGrants::default();
        grants.rules.scripts = true;
        store.save("c", &grants).unwrap();
        let mode = std::fs::metadata(dir.join("c.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn odd_ids_never_become_paths() {
        let dir = temp_dir("chat-grants-ids");
        let store = ChatGrantStore::at(dir.join("store"));
        let mut grants = ChatGrants::default();
        grants.rules.edits = true;
        store.save("../escape", &grants).unwrap();
        assert!(!dir.join("escape.json").exists());
        assert!(store.load("../escape").unwrap().is_empty());
    }

    #[test]
    fn a_damaged_file_says_how_to_recover() {
        let dir = temp_dir("chat-grants-bad").join("store");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("c.json"), "{not json").unwrap();
        let err = ChatGrantStore::at(dir).load("c").unwrap_err();
        assert!(format!("{err:#}").contains("delete it"), "{err:#}");
    }
}
