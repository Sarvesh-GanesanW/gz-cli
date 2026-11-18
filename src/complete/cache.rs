//! Completion's per-profile cache: `<gz config dir>/completion/<profile>.json`,
//! names and descriptions only (never a token), each list stamped with when
//! it was read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::target::Item;

/// A list younger than this is served without asking the server again.
pub const FRESH_SECS: i64 = 300;
/// A refresh started less than this long ago is still running.
const REFRESH_LOCK_SECS: u64 = 30;

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    lists: BTreeMap<String, List>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct List {
    at: i64,
    items: Vec<(String, String)>,
}

/// A cached list and whether it is still fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    pub items: Vec<Item>,
    pub fresh: bool,
}

pub struct Cache {
    path: PathBuf,
}

impl Cache {
    pub fn for_profile(profile: &str) -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config
            .parent()
            .context("the gz config file has no folder")?
            .join("completion");
        Ok(Self {
            path: dir.join(format!("{}.json", file_safe(profile))),
        })
    }

    pub fn get(&self, key: &str, now: i64) -> Option<Cached> {
        let list = read(&self.path).lists.get(key)?.clone();
        Some(Cached {
            items: list
                .items
                .into_iter()
                .map(|(value, about)| Item::new(value, about))
                .collect(),
            fresh: now - list.at < FRESH_SECS,
        })
    }

    /// Stores `items` under `key`, replacing the file atomically so a
    /// completion reading it at the same time sees the old or the new one.
    pub fn put(&self, key: &str, items: &[Item], now: i64) -> Result<()> {
        let mut file = read(&self.path);
        file.lists.insert(
            key.to_string(),
            List {
                at: now,
                items: items
                    .iter()
                    .map(|i| (i.value.clone(), i.about.clone()))
                    .collect(),
            },
        );
        let dir = self.path.parent().context("cache path has no folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let temp = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::to_vec(&file)?)
            .with_context(|| format!("cannot write {}", temp.display()))?;
        std::fs::rename(&temp, &self.path)
            .with_context(|| format!("cannot replace {}", self.path.display()))
    }

    /// Claims the refresh of `key`, unless one started moments ago.
    pub fn claim_refresh(&self, key: &str) -> bool {
        let lock = self.lock_path(key);
        let recent = std::fs::metadata(&lock)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age.as_secs() < REFRESH_LOCK_SECS);
        if recent {
            return false;
        }
        if let Some(dir) = lock.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&lock, b"").is_ok()
    }

    pub fn release_refresh(&self, key: &str) {
        std::fs::remove_file(self.lock_path(key)).ok();
    }

    fn lock_path(&self, key: &str) -> PathBuf {
        let stem = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        self.path
            .with_file_name(format!("{stem}.{}.lock", file_safe(key)))
    }
}

/// A broken or missing file is an empty cache: completion never fails on it.
fn read(path: &Path) -> File {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn file_safe(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_in(dir: &Path) -> Cache {
        Cache {
            path: dir.join("completion").join("stage.json"),
        }
    }

    #[test]
    fn stores_lists_and_tells_fresh_from_stale() {
        let dir = std::env::temp_dir().join(format!("gz-complete-cache-{}", std::process::id()));
        let cache = cache_in(&dir);
        assert_eq!(cache.get("kind:project", 1000), None);
        let items = vec![Item::new("Sales", "id 1999")];
        cache.put("kind:project", &items, 1000).unwrap();
        let hit = cache.get("kind:project", 1000 + FRESH_SECS - 1).unwrap();
        assert_eq!(hit.items, items);
        assert!(hit.fresh);
        assert!(!cache.get("kind:project", 1000 + FRESH_SECS).unwrap().fresh);
        let text = std::fs::read_to_string(&cache.path).unwrap();
        assert!(!text.to_lowercase().contains("token"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_refresh_at_a_time() {
        let dir = std::env::temp_dir().join(format!("gz-complete-lock-{}", std::process::id()));
        let cache = cache_in(&dir);
        assert!(cache.claim_refresh("kind:job"));
        assert!(!cache.claim_refresh("kind:job"));
        cache.release_refresh("kind:job");
        assert!(cache.claim_refresh("kind:job"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
