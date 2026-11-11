//! Confines every tool path to the folders the user opened in gz — the
//! folder it started in plus folders added with `/add-dir` or approved
//! through `change_directory` — and keeps secrets out of reach, even when
//! the model asks for them directly. Relative paths resolve against the
//! session's current folder, which moves like a shell's `cd`.
//!
//! A [`Sandbox`] is a handle: its clones share one set of folders, so the
//! UI's `@` completion follows the session's current folder. A background
//! subagent gets its own copy with [`Sandbox::fork`].

use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

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
/// Most folders one session may have open (the contract's `roots` cap).
pub const MAX_ROOTS: usize = 16;

#[derive(Debug, Clone)]
pub struct Sandbox {
    folders: Arc<RwLock<Folders>>,
    /// The user's home folder, for `~` paths and `~/…` display.
    home: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct Folders {
    /// The start folder first, then added folders in the order added.
    roots: Vec<PathBuf>,
    cwd: PathBuf,
}

/// Where a folder the model or the user asked for is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderCheck {
    /// Inside an open folder: gz may switch to it at once.
    Inside(PathBuf),
    /// Outside every open folder, but allowed to become one if the user
    /// agrees.
    Outside(PathBuf),
}

impl Sandbox {
    /// A sandbox opened on `root`, with the user's real home folder.
    pub fn new(root: &Path) -> Result<Self> {
        let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
        Self::with_home(root, home.as_deref())
    }

    /// A sandbox opened on `root` with `home` as `~` (tests pass their own).
    pub fn with_home(root: &Path, home: Option<&Path>) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot open workspace {}", root.display()))?;
        check_root(&root)?;
        let folders = Folders {
            roots: vec![root.clone()],
            cwd: root,
        };
        Ok(Self {
            folders: Arc::new(RwLock::new(folders)),
            home: home.and_then(|h| h.canonicalize().ok()),
        })
    }

    /// An independent copy with the same folders (for a background
    /// subagent, whose `change_directory` must not move the main session).
    pub fn fork(&self) -> Self {
        Self {
            folders: Arc::new(RwLock::new(self.read().clone())),
            home: self.home.clone(),
        }
    }

    // The folder list is plain data, valid even if a holder panicked.
    fn read(&self) -> RwLockReadGuard<'_, Folders> {
        self.folders.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Folders> {
        self.folders.write().unwrap_or_else(|p| p.into_inner())
    }

    /// The folder gz started in.
    pub fn start(&self) -> PathBuf {
        self.read().roots[0].clone()
    }

    /// The current folder: relative paths and commands start here.
    pub fn cwd(&self) -> PathBuf {
        self.read().cwd.clone()
    }

    /// Every open folder, the start folder first.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.read().roots.clone()
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// The current folder's name, shown to the model; never the full path.
    pub fn name(&self) -> String {
        folder_name(&self.cwd())
    }

    /// Resolves a model-supplied path to an absolute path inside an open
    /// folder. Relative paths start at the current folder; `~` is the home
    /// folder. Rejects escapes (`..`, absolute paths elsewhere, symlinks
    /// pointing out) and secret files. The target need not exist (for
    /// writes).
    pub fn resolve(&self, requested: &str) -> Result<PathBuf> {
        let real = self.locate(requested)?;
        if !self.is_open(&real) {
            bail!(
                "'{}' is outside the folders open in gz ({}); use change_directory to ask the user for it",
                requested.trim(),
                self.roots_label()
            );
        }
        if let Some(reason) = secret_reason(&real) {
            bail!("'{}' is blocked: {reason}", requested.trim());
        }
        Ok(real)
    }

    /// Where an existing folder is, for `change_directory` and `/cd`:
    /// inside an open folder, or outside but allowed to become one.
    pub fn check_folder(&self, requested: &str) -> Result<FolderCheck> {
        let real = self.locate(requested)?;
        if !real.is_dir() {
            bail!("{} is not a folder", self.display_folder(&real));
        }
        if let Some(reason) = secret_reason(&real) {
            bail!("{} is blocked: {reason}", self.display_folder(&real));
        }
        if self.is_open(&real) {
            return Ok(FolderCheck::Inside(real));
        }
        check_root(&real)?;
        Ok(FolderCheck::Outside(real))
    }

    /// Makes `folder` (inside an open folder) the current folder.
    pub fn set_cwd(&self, folder: &Path) -> Result<()> {
        if !self.is_open(folder) || !folder.is_dir() {
            bail!("{} is not a folder open in gz", self.display_folder(folder));
        }
        self.write().cwd = folder.to_path_buf();
        Ok(())
    }

    /// Opens `folder` as another root; false when an open folder already
    /// covers it.
    pub fn add_root(&self, folder: &Path) -> Result<bool> {
        check_root(folder)?;
        if !folder.is_dir() {
            bail!("{} is not a folder", self.display_folder(folder));
        }
        if self.is_open(folder) {
            return Ok(false);
        }
        let mut folders = self.write();
        if folders.roots.len() >= MAX_ROOTS {
            bail!("gz can have at most {MAX_ROOTS} folders open; remove one with /dirs remove");
        }
        folders.roots.push(folder.to_path_buf());
        Ok(true)
    }

    /// Closes an added folder. The start folder stays; a current folder
    /// that is no longer open moves back to it.
    pub fn remove_root(&self, folder: &Path) -> Result<()> {
        let mut folders = self.write();
        if folders.roots[0] == folder {
            bail!("the folder gz started in can't be removed");
        }
        let before = folders.roots.len();
        folders.roots.retain(|root| root != folder);
        if folders.roots.len() == before {
            bail!("{} is not an added folder", folder.display());
        }
        if !folders
            .roots
            .iter()
            .any(|root| folders.cwd.starts_with(root))
        {
            folders.cwd = folders.roots[0].clone();
        }
        Ok(())
    }

    /// Path for display: relative to the current folder when inside it,
    /// else `~/…` under home, else absolute; `/` separators.
    pub fn display(&self, path: &Path) -> String {
        let cwd = self.cwd();
        match path.strip_prefix(&cwd) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => slashes(rel),
            Err(_) => self.display_folder(path),
        }
    }

    /// `~`, `~/code/gz-cli`, or the absolute path: how gz names a folder
    /// to the user and to the model.
    pub fn display_folder(&self, path: &Path) -> String {
        match self.home.as_deref().map(|home| path.strip_prefix(home)) {
            Some(Ok(rel)) if rel.as_os_str().is_empty() => "~".to_string(),
            Some(Ok(rel)) => format!("~/{}", slashes(rel)),
            _ => slashes(path),
        }
    }

    pub fn is_secret(&self, path: &Path) -> bool {
        secret_reason(path).is_some()
    }

    fn is_open(&self, path: &Path) -> bool {
        self.read().roots.iter().any(|root| path.starts_with(root))
    }

    fn roots_label(&self) -> String {
        self.roots()
            .iter()
            .map(|root| self.display_folder(root))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Absolute, normalized and symlink-resolved; no open-folder check.
    fn locate(&self, requested: &str) -> Result<PathBuf> {
        let requested = requested.trim();
        let cwd = self.cwd();
        let candidate = if requested.is_empty() || requested == "." {
            cwd
        } else if let Some(rest) = home_relative(requested)? {
            let Some(home) = &self.home else {
                bail!("'{requested}': the home folder is unknown");
            };
            home.join(rest)
        } else if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            cwd.join(requested)
        };
        real_path(&normalize(&candidate))
    }
}

/// `~` → `Some("")`, `~/x` → `Some("x")`, anything else `None`; `~user`
/// is refused rather than read as a folder named `~user`.
pub(crate) fn home_relative(requested: &str) -> Result<Option<&str>> {
    let Some(rest) = requested.strip_prefix('~') else {
        return Ok(None);
    };
    if rest.is_empty() {
        return Ok(Some(""));
    }
    match rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\')) {
        Some(rest) => Ok(Some(rest)),
        None => bail!("'{requested}': only ~ and ~/… are supported"),
    }
}

/// Folders that can never be opened: the filesystem root (everything)
/// and credential folders.
pub(crate) fn check_root(folder: &Path) -> Result<()> {
    if folder.parent().is_none() {
        bail!("gz can't open {}: that is the whole disk", folder.display());
    }
    if let Some(reason) = secret_reason(folder) {
        bail!("gz can't open {}: {reason}", folder.display());
    }
    Ok(())
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string())
}

fn slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Why an absolute path is off limits, if it is.
pub(crate) fn secret_reason(path: &Path) -> Option<&'static str> {
    let parts: Vec<String> = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
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

    pub fn temp_dir(name: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gz-assist-{}-{name}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    pub fn temp_workspace(name: &str) -> (PathBuf, Sandbox) {
        let dir = temp_dir(name);
        let sandbox = Sandbox::new(&dir).unwrap();
        (dir, sandbox)
    }

    /// A fake home with `code/app` and `code/other`, opened at `home`.
    fn home_tree(name: &str) -> (PathBuf, Sandbox) {
        let home = temp_dir(name);
        for dir in ["code/app/src", "code/other", ".ssh", "outside"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::fs::write(home.join("code/app/src/main.rs"), "fn main() {}").unwrap();
        let sandbox = Sandbox::with_home(&home, Some(&home)).unwrap();
        (home, sandbox)
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

    #[test]
    fn started_in_home_the_agent_reaches_repos_and_moves_into_them() {
        let (home, sandbox) = home_tree("home");
        let main = sandbox.resolve("~/code/app/src/main.rs").unwrap();
        assert_eq!(main, home.join("code/app/src/main.rs"));
        assert_eq!(sandbox.display(&main), "code/app/src/main.rs");

        let FolderCheck::Inside(app) = sandbox.check_folder("code/app").unwrap() else {
            panic!("inside the home root");
        };
        sandbox.set_cwd(&app).unwrap();
        assert_eq!(sandbox.name(), "app");
        assert_eq!(
            sandbox.resolve("src/main.rs").unwrap(),
            main,
            "relative to the new cwd"
        );
        assert_eq!(sandbox.display(&main), "src/main.rs");
        assert_eq!(
            sandbox.resolve("../other").unwrap(),
            home.join("code/other"),
            "a sibling inside the same root"
        );
        assert_eq!(
            sandbox.display(&home.join("code/other")),
            "~/code/other",
            "outside the cwd: shown from home"
        );
        assert!(
            sandbox.resolve("~/.ssh/id_ed25519").is_err(),
            "secrets stay blocked"
        );
        assert!(sandbox.check_folder("~/.ssh").is_err());
        assert!(sandbox.resolve("~someone/x").is_err());
    }

    #[test]
    fn paths_resolve_across_roots_and_nowhere_else() {
        let (home, _) = home_tree("roots");
        let sandbox = Sandbox::with_home(&home.join("code/app"), Some(&home)).unwrap();
        let other = home.join("code/other");
        std::fs::write(other.join("notes.md"), "n").unwrap();
        assert!(
            sandbox.resolve("../other/notes.md").is_err(),
            "not open yet"
        );
        assert_eq!(
            sandbox.check_folder("~/code/other").unwrap(),
            FolderCheck::Outside(other.clone())
        );
        assert!(sandbox.add_root(&other).unwrap());
        assert!(!sandbox.add_root(&other).unwrap(), "already open");
        assert!(sandbox.resolve("../other/notes.md").is_ok());
        assert!(sandbox
            .resolve(other.join("notes.md").to_str().unwrap())
            .is_ok());
        assert!(sandbox.resolve("~/outside").is_err());

        sandbox.set_cwd(&other).unwrap();
        sandbox.remove_root(&other).unwrap();
        assert_eq!(
            sandbox.cwd(),
            home.join("code/app"),
            "back to the start folder"
        );
        assert!(sandbox.remove_root(&home.join("code/app")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_root_cannot_reach_another_folder() {
        let (home, _) = home_tree("link-out");
        let sandbox = Sandbox::with_home(&home.join("code/app"), Some(&home)).unwrap();
        std::os::unix::fs::symlink(home.join("code/other"), home.join("code/app/other")).unwrap();
        assert!(sandbox.resolve("other/x.txt").is_err());
        assert!(matches!(
            sandbox.check_folder("other").unwrap(),
            FolderCheck::Outside(_)
        ));
    }

    #[test]
    fn the_disk_root_and_credential_folders_never_open() {
        let (home, sandbox) = home_tree("refuse");
        assert!(Sandbox::new(Path::new("/")).is_err());
        assert!(sandbox.add_root(Path::new("/")).is_err());
        assert!(sandbox.add_root(&home.join(".ssh")).is_err());
        assert!(Sandbox::with_home(&home.join(".ssh"), Some(&home)).is_err());
    }

    #[test]
    fn forks_move_on_their_own_and_clones_follow() {
        let (home, sandbox) = home_tree("fork");
        let shared = sandbox.clone();
        let fork = sandbox.fork();
        sandbox.set_cwd(&home.join("code")).unwrap();
        assert_eq!(shared.cwd(), home.join("code"));
        assert_eq!(fork.cwd(), home);
    }
}
