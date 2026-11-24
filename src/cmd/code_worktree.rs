//! `gz code --worktree [name]`: the session works in a git worktree at
//! `.gz/worktrees/<name>` on branch `gz/<name>`, made from HEAD (or reused
//! when it exists), so its edits stay off the main checkout until merged.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Where worktrees go, under the repository's top folder.
const WORKTREES_DIR: &str = ".gz/worktrees";
const BRANCH_PREFIX: &str = "gz/";
const MAX_NAME_CHARS: usize = 64;
/// The line that keeps worktrees out of the main checkout's `git status`.
const EXCLUDE_LINE: &str = "/.gz/worktrees/";

/// A worktree the session runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub name: String,
    pub branch: String,
    /// The worktree's top folder.
    pub path: PathBuf,
    /// The main checkout's top folder.
    pub repo: PathBuf,
    /// Where the session starts: the same subfolder as the user was in,
    /// when the worktree has it.
    pub start: PathBuf,
    /// The main checkout's HEAD when the session began.
    base: String,
    pub reused: bool,
}

/// Sets up the worktree `--worktree` asks for; `None` without the flag.
/// An empty name gets a made-up one.
pub fn enter(start: &Path, requested: Option<&str>) -> Result<Option<Worktree>> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let name = match requested.trim() {
        "" => made_up_name(),
        name => valid_name(name)?.to_string(),
    };
    let repo = repo_top(start)?;
    let base = git(&repo, &["rev-parse", "--verify", "HEAD"]).map_err(|_| {
        anyhow::anyhow!(
            "--worktree starts from HEAD, but {} has no commits yet; commit once, then try again",
            repo.display()
        )
    })?;
    let branch = format!("{BRANCH_PREFIX}{name}");
    let path = repo.join(WORKTREES_DIR).join(&name);
    let reused = if path.exists() {
        ensure_registered(&repo, &path)?;
        true
    } else {
        add(&repo, &path, &branch)?;
        false
    };
    exclude_from_status(&repo)?;
    let inside = start.strip_prefix(&repo).unwrap_or(Path::new(""));
    let start = Some(path.join(inside))
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(|| path.clone());
    Ok(Some(Worktree {
        name,
        branch,
        path,
        repo,
        start,
        base,
        reused,
    }))
}

/// The main checkout's top folder when `dir` is the top of a worktree
/// `--worktree` made (`<checkout>/.gz/worktrees/<name>`, holding git's
/// `.git` file); `None` for any other folder.
pub fn main_checkout_of(dir: &Path) -> Option<&Path> {
    let worktrees = dir.parent()?;
    let checkout = worktrees.parent()?.parent()?;
    let is_gz_worktree = worktrees == checkout.join(WORKTREES_DIR) && dir.join(".git").is_file();
    is_gz_worktree.then_some(checkout)
}

/// `path` as the same place in the main checkout when it lies inside a
/// `--worktree` worktree, so trust given in the checkout counts in its
/// worktrees (trust is also tied to the file's content, so a file the
/// branch changed is asked about again); any other path as it is.
pub fn in_main_checkout(path: &Path) -> PathBuf {
    for top in path.ancestors() {
        let (Some(checkout), Ok(inside)) = (main_checkout_of(top), path.strip_prefix(top)) else {
            continue;
        };
        return checkout.join(inside);
    }
    path.to_path_buf()
}

/// A personal, uncommitted file (`name`, relative to a checkout's top
/// folder `dir`): a worktree that lacks its own uses the main checkout's,
/// since git never copies untracked files into a new worktree.
pub fn personal_file(dir: &Path, name: &str) -> PathBuf {
    let own = dir.join(name);
    match main_checkout_of(dir) {
        Some(checkout) if !own.exists() => checkout.join(name),
        _ => own,
    }
}

/// `[A-Za-z0-9._-]`, not starting with `.` or `-`, at most 64 characters:
/// safe as a folder name and in a branch name.
fn valid_name(name: &str) -> Result<&str> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let is_valid = name.chars().count() <= MAX_NAME_CHARS
        && name.chars().all(allowed)
        && !name.starts_with(['.', '-'])
        && !name.contains("..")
        && !name.ends_with(".lock")
        && !name.ends_with('.');
    if !is_valid {
        anyhow::bail!(
            "--worktree name '{name}' can't be used; use letters, digits, '.', '_' and '-' \
             (at most {MAX_NAME_CHARS}, not starting with '.' or '-'), e.g. --worktree fix-login"
        );
    }
    Ok(name)
}

fn made_up_name() -> String {
    let id = crate::cmd::new_uuid();
    format!("session-{}", &id[..8])
}

fn repo_top(start: &Path) -> Result<PathBuf> {
    let top = git(start, &["rev-parse", "--show-toplevel"]).map_err(|_| {
        anyhow::anyhow!(
            "--worktree needs a git repository, and {} is not inside one; run gz code from a \
             repository or drop --worktree",
            start.display()
        )
    })?;
    // Compare canonical paths: the user may have started under a symlink.
    std::fs::canonicalize(&top).with_context(|| format!("cannot open {top}"))
}

/// An existing folder at the worktree's place must be that worktree.
fn ensure_registered(repo: &Path, path: &Path) -> Result<()> {
    let listed = git(repo, &["worktree", "list", "--porcelain"])?;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let registered = listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|listed| {
            std::fs::canonicalize(listed).unwrap_or_else(|_| PathBuf::from(listed)) == canonical
        });
    if !registered {
        anyhow::bail!(
            "{} exists but is not a git worktree; pick another --worktree name or remove that \
             folder",
            path.display()
        );
    }
    Ok(())
}

/// Makes the worktree on a new branch from HEAD, or on the branch when an
/// earlier worktree of this name left it behind.
fn add(repo: &Path, path: &Path, branch: &str) -> Result<()> {
    let target = path.to_string_lossy();
    let has_branch = git(
        repo,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    )
    .is_ok();
    let made = if has_branch {
        git(repo, &["worktree", "add", &target, branch])
    } else {
        git(repo, &["worktree", "add", "-b", branch, &target, "HEAD"])
    };
    made.with_context(|| format!("cannot make the worktree {}", path.display()))?;
    Ok(())
}

/// Adds `/.gz/worktrees/` to the repository's own exclude file (never to a
/// tracked `.gitignore`), so the main checkout doesn't list worktrees as
/// untracked.
fn exclude_from_status(repo: &Path) -> Result<()> {
    let common = git(repo, &["rev-parse", "--git-common-dir"])?;
    let common = repo.join(common);
    let exclude = common.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|line| line.trim() == EXCLUDE_LINE) {
        return Ok(());
    }
    std::fs::create_dir_all(common.join("info"))
        .with_context(|| format!("cannot write {}", exclude.display()))?;
    let separator = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let updated = format!("{current}{separator}{EXCLUDE_LINE}\n");
    std::fs::write(&exclude, updated).with_context(|| format!("cannot write {}", exclude.display()))
}

/// Runs git in `dir`; its trimmed stdout, or its stderr as the error.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("cannot run git; install git or drop --worktree")?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Worktree {
    /// One line for the start of the session.
    pub fn opening_note(&self) -> String {
        let verb = if self.reused { "reusing" } else { "made" };
        format!(
            "worktree {} ({verb}, branch {})",
            self.relative_path(),
            self.branch
        )
    }

    /// What to do with the worktree once the session ends: what changed,
    /// and how to merge or remove it.
    pub fn closing_note(&self) -> String {
        let uncommitted = git(&self.path, &["status", "--porcelain"])
            .map(|status| status.lines().count())
            .unwrap_or(0);
        let commits = git(
            &self.repo,
            &[
                "rev-list",
                "--count",
                &format!("{}..{}", self.base, self.branch),
            ],
        )
        .ok()
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
        closing_text(self, uncommitted, commits)
    }

    fn relative_path(&self) -> String {
        format!("{WORKTREES_DIR}/{}", self.name)
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The closing note. Its commands name folders in full, so they work from
/// wherever the user is (the session may have started in a subfolder).
fn closing_text(tree: &Worktree, uncommitted: usize, commits: usize) -> String {
    let path = tree.relative_path();
    let worktree = shell_word(&tree.path.display().to_string());
    let repo = shell_word(&tree.repo.display().to_string());
    let remove = format!(
        "git -C {repo} worktree remove {worktree} && git -C {repo} branch -D {}",
        tree.branch
    );
    if uncommitted == 0 && commits == 0 {
        return format!(
            "Worktree {path} (branch {}) has no changes. Remove it: {remove}",
            tree.branch
        );
    }
    let mut lines = vec![format!(
        "Worktree {path} (branch {}) kept: {} and {} not yet merged.",
        tree.branch,
        plural(commits, "commit"),
        plural(uncommitted, "uncommitted change"),
    )];
    if uncommitted > 0 {
        lines.push(format!(
            "  commit them:  git -C {worktree} add -A && git -C {worktree} commit -m \"…\""
        ));
    }
    lines.push(format!(
        "  merge:        git -C {repo} merge {}",
        tree.branch
    ));
    lines.push(format!("  remove:       {remove}"));
    lines.push(format!("  continue:     gz code --worktree {}", tree.name));
    lines.join("\n")
}

/// `text` as one shell word: as it is when that is safe, else in single
/// quotes.
fn shell_word(text: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%,=~".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<checkout>/.gz/worktrees/w` laid out as `--worktree` makes it.
    fn fake_worktree(name: &str) -> (PathBuf, PathBuf) {
        let checkout =
            std::env::temp_dir().join(format!("gz-worktree-fake-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&checkout).ok();
        let worktree = checkout.join(WORKTREES_DIR).join("w");
        std::fs::create_dir_all(worktree.join(".gz")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../../../.git/worktrees/w\n").unwrap();
        (checkout, worktree)
    }

    #[test]
    fn trust_in_a_worktree_is_keyed_by_the_main_checkout() {
        let (checkout, worktree) = fake_worktree("trust");
        assert_eq!(
            in_main_checkout(&worktree.join(".mcp.json#demo")),
            checkout.join(".mcp.json#demo")
        );
        assert_eq!(
            in_main_checkout(&worktree.join(".gz/hooks.toml")),
            checkout.join(".gz/hooks.toml")
        );
        let elsewhere = checkout.join("src/a.rs");
        assert_eq!(in_main_checkout(&elsewhere), elsewhere);

        let store = crate::assistant::hooks::trust::TrustStore::at(checkout.join("trust.json"));
        store
            .record(&checkout.join(".gz/hooks.toml"), "abc", true)
            .unwrap();
        let verdict = store
            .verdict(&worktree.join(".gz/hooks.toml"), "abc")
            .unwrap();
        assert_eq!(verdict, crate::assistant::hooks::trust::Verdict::Trusted);
        let changed = store
            .verdict(&worktree.join(".gz/hooks.toml"), "def")
            .unwrap();
        assert_eq!(changed, crate::assistant::hooks::trust::Verdict::Unknown);
    }

    #[test]
    fn a_worktree_uses_the_checkouts_personal_files_until_it_has_its_own() {
        let (checkout, worktree) = fake_worktree("personal");
        assert_eq!(
            personal_file(&worktree, "GZ.local.md"),
            checkout.join("GZ.local.md")
        );
        std::fs::write(worktree.join("GZ.local.md"), "mine").unwrap();
        assert_eq!(
            personal_file(&worktree, "GZ.local.md"),
            worktree.join("GZ.local.md")
        );
        assert_eq!(
            personal_file(&checkout, "GZ.local.md"),
            checkout.join("GZ.local.md")
        );
    }

    /// A git repository with one commit and a `sub` folder, in a fresh
    /// folder of its own.
    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gz-worktree-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), "a\n").unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "first",
            ],
        ] {
            git(&dir, &args).unwrap();
        }
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn makes_then_reuses_a_worktree_on_its_own_branch() {
        let repo = repo("make");
        let made = enter(&repo.join("sub"), Some("fix-1")).unwrap().unwrap();
        assert_eq!(made.path, repo.join(".gz/worktrees/fix-1"));
        assert_eq!(made.branch, "gz/fix-1");
        assert_eq!(made.start, made.path.join("sub"), "same subfolder");
        assert!(!made.reused);
        assert_eq!(
            git(&made.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "gz/fix-1"
        );
        assert_eq!(
            git(&repo, &["status", "--porcelain"]).unwrap(),
            "",
            "the main checkout doesn't list the worktree"
        );
        assert!(made.closing_note().contains("has no changes"));

        std::fs::write(made.path.join("sub/b.txt"), "b\n").unwrap();
        let again = enter(&repo, Some("fix-1")).unwrap().unwrap();
        assert!(again.reused);
        let note = again.closing_note();
        assert!(note.contains("1 uncommitted change"), "{note}");
        assert!(note.contains(" merge gz/fix-1"), "{note}");
        let full = repo.join(".gz/worktrees/fix-1");
        assert!(
            note.contains(&format!("worktree remove {}", full.display())),
            "{note}"
        );
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(EXCLUDE_LINE).count(), 1, "added once");
        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn a_branch_left_behind_is_checked_out_again() {
        let repo = repo("branch");
        let made = enter(&repo, Some("keep")).unwrap().unwrap();
        git(&repo, &["worktree", "remove", made.path.to_str().unwrap()]).unwrap();
        let again = enter(&repo, Some("keep")).unwrap().unwrap();
        assert!(!again.reused);
        assert_eq!(
            git(&again.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap(),
            "gz/keep"
        );
        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn refuses_cleanly_outside_a_repository_and_for_bad_names() {
        let outside = std::env::temp_dir().join(format!("gz-worktree-{}-none", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        let err = enter(&outside, Some("x")).unwrap_err().to_string();
        assert!(err.contains("needs a git repository"), "{err}");
        for bad in ["../up", "-x", ".hidden", "a b", "x.lock"] {
            let err = enter(&outside, Some(bad)).unwrap_err().to_string();
            assert!(err.contains("can't be used"), "{bad}: {err}");
        }
        assert_eq!(enter(&outside, None).unwrap(), None);
        std::fs::remove_dir_all(outside).ok();
    }

    #[test]
    fn a_folder_that_is_not_a_worktree_is_left_alone() {
        let repo = repo("taken");
        std::fs::create_dir_all(repo.join(".gz/worktrees/taken")).unwrap();
        let err = enter(&repo, Some("taken")).unwrap_err().to_string();
        assert!(err.contains("is not a git worktree"), "{err}");
        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn a_made_up_name_is_used_when_none_is_given() {
        let repo = repo("auto");
        let made = enter(&repo, Some("")).unwrap().unwrap();
        assert!(made.name.starts_with("session-"), "{}", made.name);
        assert!(made.path.is_dir());
        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn the_closing_note_says_how_to_commit_merge_and_remove() {
        let tree = Worktree {
            name: "w".into(),
            branch: "gz/w".into(),
            path: "/r/.gz/worktrees/w".into(),
            repo: "/r".into(),
            start: "/r/.gz/worktrees/w".into(),
            base: "abc".into(),
            reused: false,
        };
        let note = closing_text(&tree, 0, 2);
        assert!(
            note.contains("2 commits and 0 uncommitted changes"),
            "{note}"
        );
        assert!(!note.contains("commit them"));
        assert!(note.contains("gz code --worktree w"));
        assert!(closing_text(&tree, 3, 0).contains("commit them"));
    }

    #[test]
    fn the_closing_notes_commands_work_from_a_subfolder() {
        let repo = repo("subfolder-note");
        let made = enter(&repo.join("sub"), Some("deep")).unwrap().unwrap();
        std::fs::write(made.path.join("sub/new.txt"), "n\n").unwrap();
        let note = made.closing_note();
        let command = note
            .lines()
            .find_map(|line| line.trim().strip_prefix("commit them:"))
            .expect("a commit line");
        let add = command.split(" && ").next().unwrap().trim();
        let status = Command::new("sh")
            .arg("-c")
            .arg(add)
            .current_dir(repo.join("sub"))
            .status()
            .unwrap();
        assert!(status.success(), "{add}");
        assert!(note.contains(&format!("git -C {} merge gz/deep", repo.display())));
        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn folders_with_spaces_are_quoted_in_commands() {
        assert_eq!(shell_word("/r/.gz/worktrees/w"), "/r/.gz/worktrees/w");
        assert_eq!(shell_word("/my repo"), "'/my repo'");
        assert_eq!(shell_word("/it's"), "'/it'\\''s'");
    }
}
