//! Confines every tool path to the folder gz was started in and keeps
//! secrets out of reach, even when the model asks for them directly.

use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// File names that are never read, listed, searched, or written.
const SECRET_FILE_PREFIXES: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa", ".env"];
const SECRET_FILE_NAMES: &[&str] = &[
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".htpasswd",
    "credentials",
];
const SECRET_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "keychain", "keychain-db", "jks"];
/// Directories whose whole contents are off limits.
const SECRET_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker"];

#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    pub fn new(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot open workspace {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Folder name shown to the model; never the full path.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string())
    }

    /// Resolves a model-supplied path to an absolute path inside the root.
    /// Rejects escapes (`..`, absolute paths elsewhere, symlinks pointing
    /// out) and secret files. The target need not exist (for writes).
    pub fn resolve(&self, requested: &str) -> Result<PathBuf> {
        let requested = requested.trim();
        let candidate = if requested.is_empty() || requested == "." {
            self.root.clone()
        } else if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.root.join(requested)
        };
        let normalized = normalize(&candidate);
        let real = real_path(&normalized)?;
        if !real.starts_with(&self.root) {
            bail!("'{requested}' is outside the workspace folder");
        }
        if let Some(reason) = self.secret_reason(&real) {
            bail!("'{requested}' is blocked: {reason}");
        }
        Ok(real)
    }

    /// Path relative to the root, with `/` separators, for display.
    pub fn display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let text = rel.to_string_lossy().replace('\\', "/");
        if text.is_empty() {
            ".".to_string()
        } else {
            text
        }
    }

    pub fn is_secret(&self, path: &Path) -> bool {
        self.secret_reason(path).is_some()
    }

    fn secret_reason(&self, path: &Path) -> Option<&'static str> {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if parts.iter().any(|p| SECRET_DIRS.contains(&p.as_str())) {
            return Some("credentials folder");
        }
        if parts.windows(2).any(|w| w[0] == ".git" && w[1] == "config") {
            return Some("git config can hold credentials");
        }
        let name = parts.last()?.to_ascii_lowercase();
        if SECRET_FILE_NAMES.contains(&name.as_str())
            || SECRET_FILE_PREFIXES.iter().any(|p| name.starts_with(p))
        {
            return Some("secret file");
        }
        let extension = Path::new(&name).extension()?.to_string_lossy().to_string();
        SECRET_EXTENSIONS
            .contains(&extension.as_str())
            .then_some("key or certificate file")
    }
}

/// Removes `.` and folds `..` lexically, so `a/../../x` cannot sneak past
/// the prefix check before the file exists.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Canonicalizes the longest existing ancestor (resolving symlinks) and
/// re-appends the not-yet-existing tail.
fn real_path(path: &Path) -> Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            bail!("invalid path {}", path.display());
        };
        tail.push(name.to_os_string());
        existing.pop();
    }
    let mut real = existing
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", existing.display()))?;
    for name in tail.iter().rev() {
        real.push(name);
    }
    Ok(real)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn temp_workspace(name: &str) -> (PathBuf, Sandbox) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gz-assist-{}-{name}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sandbox = Sandbox::new(&dir).unwrap();
        (dir, sandbox)
    }

    #[test]
    fn resolves_paths_inside_the_root() {
        let (dir, sandbox) = temp_workspace("inside");
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        let resolved = sandbox.resolve("a.txt").unwrap();
        assert_eq!(sandbox.display(&resolved), "a.txt");
        assert_eq!(sandbox.display(&sandbox.resolve(".").unwrap()), ".");
        assert!(sandbox.resolve("new/dir/file.rs").is_ok());
    }

    #[test]
    fn rejects_escapes() {
        let (_dir, sandbox) = temp_workspace("escape");
        assert!(sandbox.resolve("../outside.txt").is_err());
        assert!(sandbox.resolve("a/../../outside.txt").is_err());
        assert!(sandbox.resolve("/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_that_point_outside() {
        let (dir, sandbox) = temp_workspace("symlink");
        std::os::unix::fs::symlink("/etc", dir.join("etc-link")).unwrap();
        assert!(sandbox.resolve("etc-link/passwd").is_err());
    }

    #[test]
    fn blocks_secrets() {
        let (_dir, sandbox) = temp_workspace("secrets");
        for path in [
            ".env",
            ".env.local",
            "config/server.pem",
            "deploy/key.p12",
            ".ssh/config",
            ".aws/credentials",
            ".git/config",
            "keys/id_rsa.pub",
            ".npmrc",
        ] {
            assert!(sandbox.resolve(path).is_err(), "{path} should be blocked");
        }
        assert!(sandbox.resolve("src/env.rs").is_ok());
        assert!(sandbox.resolve(".github/workflows/ci.yml").is_ok());
    }
}
