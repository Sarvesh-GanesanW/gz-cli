//! Which versions of project hooks files the user trusted or declined,
//! by file path and content hash (`hooks-trust.json` next to the gz
//! config, owner-only). A changed file matches neither and is asked about
//! again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::assistant::private_file::write_private;

const FILE_NAME: &str = "hooks-trust.json";

/// What the user said about one version of a project hooks file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Trusted,
    Declined,
    /// Never asked about this version.
    Unknown,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// File path → sha256 of the trusted content.
    #[serde(default)]
    trusted: BTreeMap<String, String>,
    #[serde(default)]
    declined: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    /// The store in the gz config folder (honours `XDG_CONFIG_HOME`).
    pub fn open() -> Result<Self> {
        let config = crate::config::config_path()?;
        let dir = config.parent().context("the config path has no folder")?;
        Ok(Self::at(dir.join(FILE_NAME)))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn verdict(&self, file: &Path, sha256: &str) -> Result<Verdict> {
        let saved = self.load()?;
        let key = key(file);
        let is = |map: &BTreeMap<String, String>| map.get(&key).is_some_and(|s| s == sha256);
        Ok(if is(&saved.trusted) {
            Verdict::Trusted
        } else if is(&saved.declined) {
            Verdict::Declined
        } else {
            Verdict::Unknown
        })
    }

    /// Records the user's answer for this version, replacing any earlier one.
    pub fn record(&self, file: &Path, sha256: &str, trusted: bool) -> Result<()> {
        let mut saved = self.load()?;
        let key = key(file);
        saved.trusted.remove(&key);
        saved.declined.remove(&key);
        let into = if trusted {
            &mut saved.trusted
        } else {
            &mut saved.declined
        };
        into.insert(key, sha256.to_string());
        let text = serde_json::to_string_pretty(&saved).context("cannot encode hook trust")?;
        write_private(&self.path, &text)
    }

    fn load(&self) -> Result<Saved> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Saved::default()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.path.display()))
            }
        };
        serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON; delete it to be asked about project hooks again",
                self.path.display()
            )
        })
    }
}

fn key(file: &Path) -> String {
    file.to_string_lossy().into_owned()
}
