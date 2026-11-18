//! Statement history for the SQL shell, one file per profile next to the
//! config file (`~/.config/gz/sql_history-<profile>`), one JSON string per
//! line so multi-line statements survive, readable only by the user.
//! Statements that look like they carry a credential are never kept.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::Regex;

/// Entries kept on disk and in memory.
const MAX_ENTRIES: usize = 1000;

pub struct History {
    path: Option<PathBuf>,
    entries: Vec<String>,
    position: Option<usize>,
}

/// The history file before it was split per profile; the `default`
/// profile starts from it.
const SHARED_FILE: &str = "sql_history";

/// `profile`'s history file; a new `default` history starts from the
/// shared file older versions wrote.
pub fn profile_path(profile: &str) -> Option<PathBuf> {
    let dir = crate::config::config_path().ok()?.parent()?.to_path_buf();
    let path = dir.join(file_name(profile));
    let shared = dir.join(SHARED_FILE);
    if profile == "default" && !path.exists() && shared.exists() {
        // Best effort: without the copy the history simply starts empty.
        let _ = fs::copy(&shared, &path);
    }
    Some(path)
}

/// `sql_history-<profile>`, with anything but letters, digits, `-` and
/// `_` in the name replaced.
fn file_name(profile: &str) -> String {
    let safe: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{SHARED_FILE}-{safe}")
}

/// True when `statement` looks like it holds a credential: a password,
/// secret, token or key given a value, `IDENTIFIED BY`, `CREATE SECRET`,
/// a URL with a password, or an AWS access key id.
pub fn looks_secret(statement: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(
            r#"(?ix)
            \b[a-z0-9_.-]*?
              (password|passwd|pwd|secret|token|credentials?|api[_.-]?key
               |access[_.-]?key([_.-]?id)?|private[_.-]?key|session[_.-]?key|secret[_.-]?key)
              ['"`]?
              ( \s*(=|:|=>)\s*['"]?[^\s'",)]   # key = value
              | \s+['"][^'"]+['"] )            # OPTIONS (password "p")
            | identified\s+by
            | create\s+(or\s+replace\s+)?(temporary\s+)?secret
            | [a-z][a-z0-9+.-]*://[^/\s:@]+:[^/\s@]+@
            | \b(AKIA|ASIA)[0-9A-Z]{16}\b
            "#,
        )
        .expect("the secret pattern is a valid regex")
    });
    pattern.is_match(statement)
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

    /// Records `entry` (skipping an immediate repeat) and saves. Returns
    /// false, keeping nothing, when it looks like it holds a credential.
    pub fn push(&mut self, entry: &str) -> Result<bool> {
        self.position = None;
        let entry = entry.trim();
        if looks_secret(entry) {
            return Ok(false);
        }
        if entry.is_empty() || self.entries.last().map(String::as_str) == Some(entry) {
            return Ok(true);
        }
        self.entries.push(entry.to_string());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        if let Some(path) = &self.path {
            save(path, &self.entries)?;
        }
        Ok(true)
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
    fn statements_with_credentials_are_never_kept() {
        let path = temp("secret");
        let mut history = History::load(Some(path.clone()));
        let secret = [
            "SET spark.hadoop.fs.s3a.secret.key = 'abc123';",
            "ALTER TABLE t SET TBLPROPERTIES ('client.access-key-id'='AK')",
            "CREATE USER bob IDENTIFIED BY 'pw';",
            "create or replace secret s (TYPE s3, KEY_ID 'x')",
            "select * from read_csv('https://bob:hunter2@host/f.csv')",
            "select 'AKIAABCDEFGHIJKLMNOP'",
            "OPTIONS (password \"p\", user 'u')",
            "set api_key: xyz",
        ];
        for statement in secret {
            assert!(looks_secret(statement), "{statement}");
            assert!(!history.push(statement).unwrap());
        }
        for plain in [
            "select token_count, password_changed_at from users where token_count > 3;",
            "select * from GZWarehousePROD.DemoDB.ProductionMetrics limit 5;",
            "select count(*) from t where secret is null;",
            "select * from t where token_count = 3;",
        ] {
            assert!(!looks_secret(plain), "{plain}");
            assert!(history.push(plain).unwrap());
        }
        assert_eq!(History::load(Some(path)).entries.len(), 4);
    }

    #[test]
    fn each_profile_has_its_own_file() {
        assert_eq!(file_name("default"), "sql_history-default");
        assert_eq!(file_name("dev/../x"), "sql_history-dev____x");
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
