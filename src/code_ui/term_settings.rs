//! Terminal settings shared by every profile on this machine: the color
//! theme, how gz code gets your attention, and a custom status line.
//! Kept in `terminal.toml` next to the gz config, owner-only; a missing
//! file means the defaults.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::code_ui::attention::NotifyChoice;
use crate::tui::theme::ThemeName;

const FILE_NAME: &str = "terminal.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalSettings {
    pub theme: ThemeName,
    pub notifications: NotifyChoice,
    /// A command whose first output line shows under the input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_line: Option<String>,
}

impl TerminalSettings {
    /// The saved settings; defaults when there is no file yet.
    pub fn load() -> Result<Self> {
        Self::load_from(&path()?)
    }

    fn load_from(path: &std::path::Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        toml::from_str(&text).with_context(|| {
            format!(
                "{} is not valid; fix it or delete it to start over",
                path.display()
            )
        })
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&path()?)
    }

    fn save_to(&self, path: &std::path::Path) -> Result<()> {
        let text = toml::to_string(self).context("cannot write the terminal settings")?;
        crate::assistant::private_file::write_private(path, &text)
    }
}

/// `terminal.toml` in the gz config folder.
pub fn path() -> Result<PathBuf> {
    let config = crate::config::config_path()?;
    let dir = config
        .parent()
        .context("the gz config file has no folder")?;
    Ok(dir.join(FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_back_as_written() {
        let settings = TerminalSettings {
            theme: ThemeName::LightDaltonized,
            notifications: NotifyChoice::Desktop,
            status_line: Some("~/bin/status --short".into()),
        };
        let text = toml::to_string(&settings).unwrap();
        assert!(text.contains("theme = \"light-daltonized\""), "{text}");
        assert_eq!(toml::from_str::<TerminalSettings>(&text).unwrap(), settings);
    }

    #[test]
    fn saved_settings_load_back_from_the_file_and_a_missing_file_is_the_defaults() {
        let dir = std::env::temp_dir().join(format!("gz-terminal-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(FILE_NAME);
        std::fs::remove_file(&file).ok();
        assert_eq!(
            TerminalSettings::load_from(&file).unwrap(),
            TerminalSettings::default()
        );
        let settings = TerminalSettings {
            theme: ThemeName::Ansi,
            notifications: NotifyChoice::Bell,
            status_line: Some("echo hi".into()),
        };
        settings.save_to(&file).unwrap();
        assert_eq!(TerminalSettings::load_from(&file).unwrap(), settings);
        std::fs::write(&file, "theme = 3").unwrap();
        let err = TerminalSettings::load_from(&file).unwrap_err();
        assert!(
            format!("{err:#}").contains("delete it to start over"),
            "{err:#}"
        );
    }

    #[test]
    fn an_empty_file_is_the_defaults_and_unknown_keys_are_refused() {
        assert_eq!(
            toml::from_str::<TerminalSettings>("").unwrap(),
            TerminalSettings::default()
        );
        assert!(toml::from_str::<TerminalSettings>("colour = \"red\"").is_err());
    }
}
