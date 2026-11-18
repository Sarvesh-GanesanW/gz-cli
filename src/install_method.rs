//! How this gz binary was installed, told from where it lives. `gz update`
//! leaves package-managed copies to their package manager, and
//! `gz bug-report` names the method.

use std::path::{Path, PathBuf};

/// The winget package identifier, as in `packaging/templates/winget`.
pub const WINGET_PACKAGE_ID: &str = "Groundzero.gz";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Homebrew,
    Scoop,
    Winget,
    Cargo,
    /// `install.sh` / `install.ps1`, which put gz in `~/.local/bin`.
    Script,
    Unknown,
}

impl InstallMethod {
    pub fn label(self) -> &'static str {
        match self {
            InstallMethod::Homebrew => "Homebrew",
            InstallMethod::Scoop => "Scoop",
            InstallMethod::Winget => "winget",
            InstallMethod::Cargo => "cargo",
            InstallMethod::Script => "install script",
            InstallMethod::Unknown => "manual copy or unknown",
        }
    }

    /// The package manager's own update command, for copies `gz update`
    /// must not overwrite (the manager would lose track of its files).
    pub fn package_update_command(self) -> Option<String> {
        match self {
            InstallMethod::Homebrew => Some("brew upgrade gz".to_string()),
            InstallMethod::Scoop => Some("scoop update gz".to_string()),
            InstallMethod::Winget => Some(format!("winget upgrade --id {WINGET_PACKAGE_ID}")),
            InstallMethod::Cargo | InstallMethod::Script | InstallMethod::Unknown => None,
        }
    }
}

/// The running binary with symlinks resolved (Homebrew links `bin/gz` into
/// its Cellar), or as reported when it cannot be resolved.
pub fn current_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// The install method for the running binary.
pub fn current() -> InstallMethod {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    current_exe().map_or(InstallMethod::Unknown, |exe| detect(&exe, home.as_deref()))
}

/// Tells the method from the binary's path. Case and separators are
/// normalised so Windows paths match too.
pub fn detect(exe: &Path, home: Option<&Path>) -> InstallMethod {
    let path = normalise(exe);
    let has = |part: &str| path.contains(part);
    if has("/cellar/") || has("/.linuxbrew/") {
        return InstallMethod::Homebrew;
    }
    if has("/scoop/apps/") || has("/scoop/shims/") {
        return InstallMethod::Scoop;
    }
    if has("/microsoft/winget/packages/") || has("/microsoft/winget/links/") {
        return InstallMethod::Winget;
    }
    if has("/.cargo/bin/") {
        return InstallMethod::Cargo;
    }
    let script_dir = home.map(|home| format!("{}/.local/bin", normalise(home)));
    let folder = path.rsplit_once('/').map(|(folder, _)| folder);
    if script_dir.is_some() && folder == script_dir.as_deref() {
        return InstallMethod::Script;
    }
    InstallMethod::Unknown
}

fn normalise(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(exe: &str) -> InstallMethod {
        detect(Path::new(exe), Some(Path::new("/Users/ana")))
    }

    #[test]
    fn tells_package_managers_apart_by_path() {
        assert_eq!(
            method("/opt/homebrew/Cellar/gz/0.10.0/bin/gz"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            method("/home/linuxbrew/.linuxbrew/Cellar/gz/0.10.0/bin/gz"),
            InstallMethod::Homebrew
        );
        assert_eq!(
            method(r"C:\Users\Ana\scoop\apps\gz\current\gz.exe"),
            InstallMethod::Scoop
        );
        assert_eq!(
            method(
                r"C:\Users\Ana\AppData\Local\Microsoft\WinGet\Packages\Groundzero.gz_Microsoft.Winget.Source_8wekyb3d8bbwe\gz.exe"
            ),
            InstallMethod::Winget
        );
        assert_eq!(method("/Users/ana/.cargo/bin/gz"), InstallMethod::Cargo);
        assert_eq!(method("/Users/ana/.local/bin/gz"), InstallMethod::Script);
        assert_eq!(method("/usr/local/bin/gz"), InstallMethod::Unknown);
        assert_eq!(
            detect(
                Path::new(r"C:\Users\Ana\.local\bin\gz.exe"),
                Some(Path::new(r"C:\Users\Ana"))
            ),
            InstallMethod::Script
        );
    }

    #[test]
    fn only_package_managed_copies_defer_updates() {
        assert_eq!(
            InstallMethod::Homebrew.package_update_command().as_deref(),
            Some("brew upgrade gz")
        );
        assert_eq!(
            InstallMethod::Winget.package_update_command().as_deref(),
            Some("winget upgrade --id Groundzero.gz")
        );
        assert_eq!(
            InstallMethod::Scoop.package_update_command().as_deref(),
            Some("scoop update gz")
        );
        assert_eq!(InstallMethod::Script.package_update_command(), None);
        assert_eq!(InstallMethod::Unknown.package_update_command(), None);
    }
}
