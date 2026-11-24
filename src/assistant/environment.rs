//! The environment snapshot: facts about the machine and the folder gz
//! started in (date, OS, shell, git branch and recent commits), taken once
//! per session and sent as the first `clientContext.instructions` entry.
//! It never changes during the session, so the server's cached
//! `<local_context>` prompt section stays valid from turn to turn; the
//! model is told it is a snapshot and to run git itself for fresh state.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::assistant::sandbox::Sandbox;

/// The entry's `path`: rendered as `<user_instructions path="…">`, so it
/// names what the entry is rather than a file.
pub const ENTRY_PATH: &str = "gz session environment";
/// How many recent commit subjects the snapshot lists.
const RECENT_COMMITS: usize = 5;
/// Longest commit subject kept; longer ones end in `…`.
const MAX_SUBJECT_CHARS: usize = 120;
/// A git call slower than this is abandoned (a huge or networked repo
/// must never hold up the first message).
const GIT_TIMEOUT: Duration = Duration::from_secs(3);
/// Branch names gz checks, in order, when the remote names no default.
const MAIN_BRANCH_GUESSES: [&str; 3] = ["main", "master", "trunk"];
/// `git config --show-scope` names for the repository's own config files.
const REPO_CONFIG_SCOPES: [&str; 2] = ["local", "worktree"];
const WEEKDAYS: [&str; 7] = [
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
];

/// What gz knows about the machine and the start folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The local date, `2026-11-25`.
    pub date: String,
    pub os: &'static str,
    pub arch: &'static str,
    /// The user's shell program name (`zsh`), when known.
    pub shell: Option<String>,
    /// The start folder as gz shows it (`~/code/app`).
    pub folder: String,
    /// `None` when the folder is not in a git repository.
    pub git: Option<GitFacts>,
}

/// The git facts of the start folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitFacts {
    /// The checked-out branch; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// The branch pull requests usually target, when gz can tell.
    pub main_branch: Option<String>,
    /// Up to five commit subjects, newest first.
    pub recent: Vec<String>,
    /// Whether the working tree has uncommitted changes; `None` when git
    /// could not say in time.
    pub dirty: Option<bool>,
}

impl Snapshot {
    /// Takes the snapshot for the sandbox's start folder (runs a few
    /// read-only git commands, each bounded by a timeout).
    pub fn take(sandbox: &Sandbox) -> Self {
        let start = sandbox.start();
        let now_secs = crate::tui::format::system_now_ms() / 1000;
        Self {
            date: crate::spend::day::on(now_secs),
            os: crate::assistant::session::platform(),
            arch: std::env::consts::ARCH,
            shell: user_shell(),
            folder: sandbox.display_folder(&start),
            git: git_facts(&start),
        }
    }

    /// The entry's text. Pure, so the same snapshot is always the same
    /// bytes.
    pub fn text(&self) -> String {
        let mut lines = vec![
            "Facts gz recorded when this session started. They are not updated \
             during the session: run git yourself for the current state."
                .to_string(),
            format!("- Date: {} ({})", self.date, weekday(&self.date)),
            format!("- OS: {} ({})", self.os, self.arch),
        ];
        if let Some(shell) = &self.shell {
            lines.push(format!("- Shell: {shell}"));
        }
        lines.push(format!("- Started in: {}", self.folder));
        match &self.git {
            None => lines.push("- Git: this folder is not in a git repository".into()),
            Some(git) => lines.extend(git.lines()),
        }
        lines.push(memory_scope_note());
        lines.join("\n")
    }
}

/// How the model tells project memories from global ones and files one
/// for this project only (`memory.rs` reads the same tag and prefixes).
fn memory_scope_note() -> String {
    format!(
        "- Memory: entries with ids starting `p_` hold for this project only, `m_` ones \
         everywhere. To save a fact that only holds for this project, start the memory \
         text with `{} ` (gz files it under this project and drops the tag).",
        crate::assistant::memory::PROJECT_TAG
    )
}

impl GitFacts {
    fn lines(&self) -> Vec<String> {
        let branch = self
            .branch
            .as_deref()
            .map_or_else(|| "detached HEAD".to_string(), |b| format!("branch {b}"));
        let mut lines = vec![format!("- Git: repository, on {branch}")];
        if let Some(main) = &self.main_branch {
            lines.push(format!(
                "- Main branch: {main} (the usual base for pull requests)"
            ));
        }
        lines.push(format!(
            "- Working tree: {}",
            match self.dirty {
                Some(true) => "uncommitted changes",
                Some(false) => "clean",
                None => "unknown (run git status to check)",
            }
        ));
        if !self.recent.is_empty() {
            lines.push("- Recent commits, newest first:".into());
            lines.extend(self.recent.iter().map(|subject| format!("  - {subject}")));
        }
        lines
    }
}

/// `Wednesday` for `2026-11-25`; empty for a malformed date.
fn weekday(date: &str) -> &'static str {
    let mut parts = date.split('-').map(str::parse::<i64>);
    let (Some(Ok(year)), Some(Ok(month)), Some(Ok(day))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return "";
    };
    let days = crate::cmd::civil::days_from_civil(year, month as u32, day as u32);
    // 1970-01-01 was a Thursday, the first entry.
    WEEKDAYS[days.rem_euclid(7) as usize]
}

fn user_shell() -> Option<String> {
    let var = if cfg!(windows) { "COMSPEC" } else { "SHELL" };
    let value = std::env::var_os(var)?;
    let name = Path::new(&value)
        .file_stem()?
        .to_string_lossy()
        .into_owned();
    Some(name).filter(|n| !n.is_empty() && !n.chars().any(char::is_control))
}

/// The folder holding a `.git` entry nearest above `start` (or `start`
/// itself); `None` outside every repository. Checked without running git,
/// so a folder outside git never spawns it (on a Mac without developer
/// tools, `git` is a stub that opens an install dialog).
pub fn repo_root(start: &Path) -> Option<&Path> {
    start.ancestors().find(|dir| dir.join(".git").exists())
}

fn git_facts(folder: &Path) -> Option<GitFacts> {
    repo_root(folder)?;
    let inside = git(folder, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() != "true" {
        return None;
    }
    let branch = git(folder, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty());
    let recent = git(
        folder,
        &[
            "log",
            &format!("-{RECENT_COMMITS}"),
            "--no-show-signature",
            "--format=%s",
        ],
    )
    .map(|out| {
        out.lines()
            .map(clean_subject)
            .filter(|s| !s.is_empty())
            .collect()
    })
    .unwrap_or_default();
    let dirty = working_tree_dirty(folder);
    Some(GitFacts {
        branch,
        main_branch: main_branch(folder),
        recent,
        dirty,
    })
}

/// Whether `git status` lists anything; `None` when it can't say in time
/// or when running it could start a program the repository itself names.
fn working_tree_dirty(folder: &Path) -> Option<bool> {
    if repo_sets_filter_programs(folder) {
        return None;
    }
    git(
        folder,
        &[
            "status",
            "--porcelain",
            "--untracked-files=normal",
            // A submodule's own config is not ours to vet.
            "--ignore-submodules=all",
        ],
    )
    .map(|out| !out.trim().is_empty())
}

/// Whether the repository's own config (or a file it includes) defines a
/// clean or process filter: `git status` runs those to compare files, and
/// a cloned or unpacked repo must not get to run a program at gz start.
/// Filters in the user's own global config (git-lfs) are theirs to run.
/// `true` when git can't answer, so the doubt never runs `status`.
fn repo_sets_filter_programs(folder: &Path) -> bool {
    let Some((code, out)) = git_run(
        folder,
        &[
            "config",
            "--show-scope",
            "--get-regexp",
            r"^filter\..*\.(clean|process)$",
        ],
    ) else {
        return true;
    };
    match code {
        // `--get-regexp` exits 1 when nothing matches.
        Some(1) => false,
        Some(0) => out.lines().any(|line| {
            let scope = line.split_whitespace().next().unwrap_or_default();
            REPO_CONFIG_SCOPES.contains(&scope)
        }),
        _ => true,
    }
}

/// The remote's default branch (`origin/HEAD`), else the first of
/// main/master/trunk that exists locally.
fn main_branch(folder: &Path) -> Option<String> {
    let remote_head = git(
        folder,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    );
    if let Some(name) = remote_head
        .as_deref()
        .map(str::trim)
        .and_then(|r| r.strip_prefix("origin/"))
        .filter(|n| !n.is_empty())
    {
        return Some(name.to_string());
    }
    MAIN_BRANCH_GUESSES.into_iter().find_map(|name| {
        git(
            folder,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
        )
        .map(|_| name.to_string())
    })
}

/// One line, no control characters, at most [`MAX_SUBJECT_CHARS`].
fn clean_subject(line: &str) -> String {
    let plain: String = line
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if plain.chars().count() <= MAX_SUBJECT_CHARS {
        return plain;
    }
    let mut cut: String = plain.chars().take(MAX_SUBJECT_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Stdout of a successful, read-only `git` call in `folder`; `None` when
/// git is missing, fails, or runs past [`GIT_TIMEOUT`] (then it is killed).
pub(crate) fn git(folder: &Path, args: &[&str]) -> Option<String> {
    match git_run(folder, args)? {
        (Some(0), out) => Some(out),
        _ => None,
    }
}

/// The exit code (`None` when killed by a signal) and stdout of a `git`
/// call in `folder`; `None` when git is missing or runs past
/// [`GIT_TIMEOUT`] (then it is killed).
pub(crate) fn git_run(folder: &Path, args: &[&str]) -> Option<(Option<i32>, String)> {
    let mut child = Command::new("git")
        // A repository's `core.fsmonitor` names a program that `status`
        // and `check-ignore` would start outside the sandbox; gz runs git
        // on its own here, so nothing the repository configures may run.
        .args(["-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(folder)
        .args(args)
        // Never take the index lock or ask for credentials: the user may be
        // running git in another terminal.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let read = stdout.read_to_end(&mut out).map(|_| out);
        let _ = send.send(read);
    });
    let Ok(Ok(out)) = receive.recv_timeout(GIT_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let status = child.wait().ok()?;
    Some((status.code(), String::from_utf8_lossy(&out).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    fn run_git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn snapshot(git: Option<GitFacts>) -> Snapshot {
        Snapshot {
            date: "2026-11-25".into(),
            os: "macos",
            arch: "aarch64",
            shell: Some("zsh".into()),
            folder: "~/code/app".into(),
            git,
        }
    }

    #[test]
    fn the_text_names_date_weekday_os_shell_folder_and_git_state() {
        let text = snapshot(Some(GitFacts {
            branch: Some("feature/x".into()),
            main_branch: Some("main".into()),
            recent: vec!["Fix login".into(), "Add tests".into()],
            dirty: Some(true),
        }))
        .text();
        for want in [
            "- Date: 2026-11-25 (Wednesday)",
            "- OS: macos (aarch64)",
            "- Shell: zsh",
            "- Started in: ~/code/app",
            "- Git: repository, on branch feature/x",
            "- Main branch: main",
            "- Working tree: uncommitted changes",
            "  - Fix login\n  - Add tests",
            "start the memory text with `[project] `",
        ] {
            assert!(text.contains(want), "{want:?} missing from:\n{text}");
        }
        assert_eq!(
            snapshot(None).text().lines().nth(5),
            Some("- Git: this folder is not in a git repository")
        );
    }

    #[test]
    fn weekdays_and_subjects_are_computed_safely() {
        assert_eq!(weekday("1970-01-01"), "Thursday");
        assert_eq!(weekday("2026-11-23"), "Monday");
        assert_eq!(weekday("garbage"), "");
        assert_eq!(clean_subject("a\u{1b}[31mb\r"), "a[31mb");
        let long = clean_subject(&"x".repeat(300));
        assert_eq!(long.chars().count(), MAX_SUBJECT_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn a_folder_outside_git_has_no_git_facts() {
        let dir = temp_dir("env-no-git");
        assert_eq!(git_facts(&dir), None);
    }

    #[test]
    fn a_folder_outside_git_is_known_without_running_git() {
        let dir = temp_dir("env-no-git-root");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        assert_eq!(repo_root(&dir.join("a/b")), None);
        std::fs::create_dir(dir.join(".git")).unwrap();
        assert_eq!(repo_root(&dir.join("a/b")), Some(dir.as_path()));
    }

    /// A script that leaves `marker` behind when anything runs it.
    fn marker_script(dir: &Path, marker: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("run-me.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn committed_repo(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = temp_dir(name);
        let repo = base.join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("f.txt"), "one").unwrap();
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["commit", "-q", "-m", "first"]);
        (base, repo)
    }

    #[cfg(unix)]
    #[test]
    fn the_snapshot_never_starts_the_repositorys_fsmonitor() {
        let (base, repo) = committed_repo("env-fsmonitor");
        let marker = base.join("RAN");
        let script = marker_script(&base, &marker);
        run_git(
            &repo,
            &["config", "core.fsmonitor", &script.to_string_lossy()],
        );
        std::fs::write(repo.join("new.txt"), "x").unwrap();
        let facts = git_facts(&repo).expect("a repository");
        assert_eq!(facts.dirty, Some(true));
        assert!(!marker.exists(), "core.fsmonitor ran");
    }

    #[cfg(unix)]
    #[test]
    fn a_repository_filter_program_keeps_git_status_from_running() {
        let (base, repo) = committed_repo("env-filter");
        let marker = base.join("RAN");
        let script = marker_script(&base, &marker);
        // Set through an included file, the way a cloned repo might hide it.
        let included = base.join("extra.cfg");
        std::fs::write(
            &included,
            format!("[filter \"x\"]\n\tclean = {}\n", script.display()),
        )
        .unwrap();
        run_git(
            &repo,
            &["config", "include.path", &included.to_string_lossy()],
        );
        std::fs::write(repo.join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(repo.join("f.txt"), "two").unwrap();
        let facts = git_facts(&repo).expect("a repository");
        assert_eq!(facts.dirty, None, "status must not run");
        assert_eq!(facts.recent, ["first"]);
        assert!(!marker.exists(), "the clean filter ran");
    }

    #[test]
    fn a_repository_reports_branch_main_branch_commits_and_dirt() {
        let dir = temp_dir("env-git");
        run_git(&dir, &["init", "-q"]);
        for n in 1..=7 {
            std::fs::write(dir.join("f.txt"), format!("{n}")).unwrap();
            run_git(&dir, &["add", "."]);
            run_git(&dir, &["commit", "-q", "-m", &format!("commit {n}")]);
        }
        run_git(&dir, &["checkout", "-q", "-b", "topic"]);
        let facts = git_facts(&dir).expect("a repository");
        assert_eq!(facts.branch.as_deref(), Some("topic"));
        assert_eq!(facts.main_branch.as_deref(), Some("main"));
        assert_eq!(
            facts.recent,
            ["commit 7", "commit 6", "commit 5", "commit 4", "commit 3"]
        );
        assert_eq!(facts.dirty, Some(false));
        std::fs::write(dir.join("new.txt"), "x").unwrap();
        assert_eq!(git_facts(&dir).unwrap().dirty, Some(true));
    }
}
