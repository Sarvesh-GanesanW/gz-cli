//! `diagnostics.toml` in the gz config folder: whether gz checks the files
//! its tools edit, how long a checker may take, and the user's own
//! checkers (per project through `folder`).
//!
//! The file lives with the user's config, never in a repository, because
//! its commands run unsandboxed as the user.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The settings file, next to `config.toml`.
pub const FILE_NAME: &str = "diagnostics.toml";
/// How long a checker may run when the file does not say.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// A longer timeout than this is a typo, not a wish.
const MAX_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    /// Run the built-in fast checkers (ruff, shellcheck, go vet) when they
    /// are installed.
    pub auto: bool,
    pub timeout: Duration,
    pub checkers: Vec<Configured>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            auto: true,
            timeout: DEFAULT_TIMEOUT,
            checkers: Vec::new(),
        }
    }
}

/// One `[[checkers]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    pub name: String,
    /// Shell command; `{files}` becomes the edited files, quoted.
    pub command: String,
    /// File extensions it checks (no dot); empty means every file.
    pub extensions: Vec<String>,
    /// The project it applies to; it also runs there. `None`: every folder,
    /// run in gz's current folder.
    pub folder: Option<PathBuf>,
    pub timeout: Duration,
    /// Run once before the edit too, so only new problems are reported.
    pub compare_before: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    enabled: Option<bool>,
    auto: Option<bool>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    checkers: Vec<CheckerEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckerEntry {
    name: Option<String>,
    command: String,
    #[serde(default)]
    extensions: Vec<String>,
    folder: Option<PathBuf>,
    timeout_secs: Option<u64>,
    compare_before: Option<bool>,
}

impl Settings {
    /// The settings in `path`; a missing file means the defaults.
    pub fn read(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("{} has a problem", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: File = toml::from_str(text)?;
        let timeout = timeout_of(file.timeout_secs, "timeout_secs")?.unwrap_or(DEFAULT_TIMEOUT);
        let checkers = file
            .checkers
            .into_iter()
            .enumerate()
            .map(|(at, entry)| configured(entry, at, timeout))
            .collect::<Result<_>>()?;
        Ok(Self {
            enabled: file.enabled.unwrap_or(true),
            auto: file.auto.unwrap_or(true),
            timeout,
            checkers,
        })
    }
}

fn configured(entry: CheckerEntry, at: usize, timeout: Duration) -> Result<Configured> {
    let command = entry.command.trim().to_string();
    if command.is_empty() {
        bail!("checker {} has an empty command", at + 1);
    }
    let name = entry
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| first_word(&command));
    let folder = entry
        .folder
        .map(|folder| crate::sql::save::expand_home(&folder))
        .map(|folder| folder.canonicalize().unwrap_or(folder));
    if let Some(folder) = &folder {
        if !folder.is_absolute() {
            bail!(
                "checker '{name}': folder must be an absolute path or start with ~/ (got {})",
                folder.display()
            );
        }
    }
    let extensions = entry
        .extensions
        .into_iter()
        .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    let field = format!("checker '{name}' timeout_secs");
    Ok(Configured {
        timeout: timeout_of(entry.timeout_secs, &field)?.unwrap_or(timeout),
        compare_before: entry.compare_before.unwrap_or(true),
        name,
        command,
        extensions,
        folder,
    })
}

fn timeout_of(secs: Option<u64>, field: &str) -> Result<Option<Duration>> {
    match secs {
        None => Ok(None),
        Some(secs) if (1..=MAX_TIMEOUT_SECS).contains(&secs) => Ok(Some(Duration::from_secs(secs))),
        Some(secs) => bail!("{field} must be between 1 and {MAX_TIMEOUT_SECS} (got {secs})"),
    }
}

fn first_word(command: &str) -> String {
    let word = command.split_whitespace().next().unwrap_or(command);
    Path::new(word)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| word.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_turns_on_the_defaults() {
        let dir = crate::assistant::sandbox::tests::temp_dir("diag-settings");
        let settings = Settings::read(&dir.join(FILE_NAME)).unwrap();
        assert_eq!(settings, Settings::default());
        assert!(settings.enabled && settings.auto);
        assert_eq!(settings.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn checkers_take_names_folders_and_timeouts() {
        let settings = Settings::parse(
            r#"
timeout_secs = 5
auto = false

[[checkers]]
command = "npx tsc --noEmit -p . --pretty false"
extensions = [".ts", "TSX"]
folder = "/"

[[checkers]]
name = "cargo"
command = "cargo check --message-format short"
timeout_secs = 60
compare_before = false
"#,
        )
        .unwrap();
        assert!(settings.enabled);
        assert!(!settings.auto);
        assert_eq!(settings.timeout, Duration::from_secs(5));
        let tsc = &settings.checkers[0];
        assert_eq!(tsc.name, "npx");
        assert_eq!(tsc.extensions, ["ts", "tsx"]);
        assert_eq!(tsc.timeout, Duration::from_secs(5), "the file's default");
        assert!(tsc.compare_before);
        assert!(tsc.folder.is_some());
        let cargo = &settings.checkers[1];
        assert_eq!(cargo.name, "cargo");
        assert_eq!(cargo.timeout, Duration::from_secs(60));
        assert!(!cargo.compare_before);
        assert_eq!(cargo.folder, None);
    }

    #[test]
    fn the_off_switch_turns_checking_off() {
        assert!(!Settings::parse("enabled = false").unwrap().enabled);
    }

    #[test]
    fn mistakes_name_the_field_and_the_fix() {
        let unknown = Settings::parse("enable = false").unwrap_err();
        assert!(format!("{unknown:#}").contains("enable"), "{unknown:#}");
        let zero = Settings::parse("timeout_secs = 0").unwrap_err();
        assert!(
            format!("{zero:#}").contains("between 1 and 600"),
            "{zero:#}"
        );
        let empty = Settings::parse("[[checkers]]\ncommand = \" \"").unwrap_err();
        assert!(format!("{empty:#}").contains("empty command"));
        let relative =
            Settings::parse("[[checkers]]\ncommand = \"x\"\nfolder = \"web\"").unwrap_err();
        assert!(
            format!("{relative:#}").contains("absolute path"),
            "{relative:#}"
        );
    }
}
