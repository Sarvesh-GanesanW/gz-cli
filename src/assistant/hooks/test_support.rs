//! Hooks for tests, read from temp folders only.

use std::path::{Path, PathBuf};

use super::trust::TrustStore;
use super::{Hooks, Places};

/// A user config folder and a project folder under `dir`.
pub fn places(dir: &Path) -> Places {
    Places {
        user_file: Some(dir.join("config/hooks.toml")),
        project_file: dir.join("project/.gz/hooks.toml"),
        trust: Some(TrustStore::at(dir.join("config/hooks-trust.json"))),
    }
}

/// A fresh temp folder holding a user `hooks.toml` with `text`, and the
/// hooks loaded from it.
pub fn user_hooks(name: &str, text: &str) -> (PathBuf, Hooks) {
    let dir = crate::assistant::sandbox::tests::temp_dir(name);
    write(&dir.join("config/hooks.toml"), text);
    let hooks = Hooks::load(places(&dir));
    (dir, hooks)
}

pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
