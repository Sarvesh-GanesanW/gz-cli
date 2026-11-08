//! `skills.lock.toml`: where each installed skill came from (source,
//! subdir, ref, the exact commit) and the sha256 of every file and of the
//! whole tree, so gz notices when an installed skill changes on disk.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::assistant::private_file::write_private;

/// One installed skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    /// The https URL or absolute local folder it was installed from.
    pub source: String,
    /// The `#subdir` given at install, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
    /// The skill's folder inside the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The branch, tag or commit asked for (`None`: the default branch).
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The commit that was installed (git sources).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub bundle: String,
    pub installed_at: String,
    pub tree_sha256: String,
    /// File (relative, `/`-separated) → sha256.
    pub files: BTreeMap<String, String>,
    /// Who last changed a team skill on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    /// When a team skill last changed on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LockFile {
    #[serde(default)]
    skills: BTreeMap<String, LockEntry>,
}

/// The lockfile at a fixed path.
#[derive(Debug, Clone)]
pub struct Lock {
    path: PathBuf,
    entries: BTreeMap<String, LockEntry>,
}

impl Lock {
    pub fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            toml::from_str::<LockFile>(&text)
                .with_context(|| format!("cannot parse {}", path.display()))?
                .skills
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, entries })
    }

    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.entries.get(name)
    }

    pub fn entries(&self) -> &BTreeMap<String, LockEntry> {
        &self.entries
    }

    pub fn set(&mut self, name: &str, entry: LockEntry) {
        self.entries.insert(name.to_string(), entry);
    }

    pub fn remove(&mut self, name: &str) -> Option<LockEntry> {
        self.entries.remove(name)
    }

    pub fn save(&self) -> Result<()> {
        let file = LockFile {
            skills: self.entries.clone(),
        };
        let text = toml::to_string_pretty(&file).context("cannot encode the skills lockfile")?;
        write_private(
            &self.path,
            &format!("# Written by gz; edits here make installed skills untrusted.\n{text}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip_with_ref_as_a_toml_key() {
        let path = std::env::temp_dir()
            .join(format!("gz-lock-{}", crate::cmd::new_uuid()))
            .join("skills.lock.toml");
        let mut lock = Lock::load(path.clone()).unwrap();
        let entry = LockEntry {
            source: "https://github.com/o/r.git".into(),
            subdir: None,
            path: Some("skills/pdf".into()),
            reference: Some("v1".into()),
            commit: Some("a".repeat(40)),
            bundle: "r".into(),
            installed_at: "2026-11-08T00:00:00Z".into(),
            tree_sha256: "t".into(),
            files: BTreeMap::from([("SKILL.md".into(), "s".into())]),
            updated_by: None,
            updated_at: None,
        };
        lock.set("pdf", entry.clone());
        lock.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ref = \"v1\""), "{text}");
        assert_eq!(Lock::load(path).unwrap().get("pdf"), Some(&entry));
    }
}
