use super::*;
use std::collections::BTreeSet;

/// Folders for the classifier: relative paths without `..` are inside,
/// absolute ones and escapes are not; `.env` is a secret; `existing`
/// lists what exists, `dirs` the folders among them.
struct Fake {
    existing: BTreeSet<&'static str>,
    dirs: BTreeSet<&'static str>,
    /// The repository's top folder, as `git_top` gives it.
    git_top: Option<&'static str>,
    /// Whether that repository tracks a secret file.
    tracks_secrets: bool,
}

impl Fake {
    fn new() -> Self {
        Self {
            existing: ["a.txt", "b.txt", "src", "out", "out/a.txt", "notes"]
                .into_iter()
                .collect(),
            dirs: ["src", "out", "notes"].into_iter().collect(),
            git_top: Some("."),
            tracks_secrets: false,
        }
    }
}

impl Folders for Fake {
    fn inside(&self, path: &str) -> bool {
        !path.is_empty()
            && !path.starts_with('/')
            && !path.starts_with('~')
            && !path.split('/').any(|part| part == "..")
            && !path.split('/').any(|part| part.starts_with(".env"))
    }

    fn exists(&self, path: &str) -> bool {
        self.existing.contains(path.trim_end_matches('/'))
    }

    fn is_dir(&self, path: &str) -> bool {
        self.dirs.contains(path.trim_end_matches('/'))
    }

    fn git_top(&self) -> Option<String> {
        self.git_top.map(str::to_string)
    }

    fn tracks_secrets(&self, _: &str) -> bool {
        self.tracks_secrets
    }
}

fn kind(line: &str) -> Option<CommandKind> {
    classify_command(line, &Fake::new(), true)
}

fn read_only(line: &str) -> bool {
    kind(line) == Some(CommandKind::ReadOnly)
}

fn file_change(line: &str) -> bool {
    kind(line) == Some(CommandKind::FileChange)
}

#[test]
fn plain_reads_inside_the_folders_run_without_asking() {
    for line in [
        "ls",
        "ls -la src",
        "cat a.txt",
        "head -n 20 a.txt",
        "tail -5 a.txt",
        "wc -l a.txt b.txt",
        "grep -n main src/main.rs",
        "grep -rn 'fn main' src",
        "rg TODO",
        "rg -n 'foo.*bar' src",
        "find . -name '*.rs' -type f",
        "pwd",
        "echo hello world",
        "which cargo",
        "file a.txt",
        "stat a.txt",
        "du -sh src",
        "tree -L 2",
        "sort a.txt",
        "uniq -c a.txt",
        "cut -d , -f 1 a.txt",
        "diff a.txt b.txt",
        "  ls   src  ",
        "cat \"a.txt\"",
    ] {
        assert!(read_only(line), "{line}");
    }
}

#[test]
fn chaining_redirection_and_substitution_always_ask() {
    for line in [
        "ls; rm -rf .",
        "ls && touch x",
        "ls || touch x",
        "cat a.txt | sh",
        "cat a.txt > b.txt",
        "cat < a.txt",
        "echo `id`",
        "echo $(id)",
        "echo $HOME",
        "ls\nrm x",
        "ls &",
        "cat 'a.txt",
        "echo \"a\\\"b\"",
        "cat a\\.txt",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
}

#[test]
fn globs_tilde_and_comments_ask_since_the_shell_would_expand_them() {
    for line in [
        "ls *.rs",
        "cat src/?.rs",
        "ls [ab].txt",
        "ls ~",
        "cat ~/notes",
        "ls # comment",
        "ls !x",
        "ls {a,b}",
        "grep -r x --exclude-dir=~/x .",
        "ls a=~/x",
        "ls a:~/x",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
    assert!(read_only("grep 'a*b' a.txt"), "quoted globs stay literal");
    assert!(read_only("git log HEAD~3"), "~ inside a word is literal");
}

#[test]
fn paths_outside_the_folders_or_secret_files_ask() {
    for line in [
        "cat /etc/passwd",
        "ls ..",
        "cat ../other/a.txt",
        "cat .env",
        "grep -f .env a.txt",
        "head --lines=3 /etc/hosts",
        "grep -f/etc/x a.txt",
        "git show HEAD:.env",
        "git diff --no-index /etc/hosts a.txt",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
}

#[test]
fn programs_off_the_list_or_written_as_paths_ask() {
    for line in [
        "less a.txt",
        "more a.txt",
        "env ls",
        "sh -c ls",
        "xargs cat",
        "/bin/ls",
        "FOO=1 ls",
        "cargo check",
        "python3 x.py",
        "rm a.txt",
        "rmdir src",
        "curl https://example.com",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
}

#[test]
fn flags_that_write_or_run_programs_ask() {
    for line in [
        "find . -exec rm {} ;",
        "find . -execdir sh x",
        "find . -delete",
        "find . -fprint out.txt",
        "find . -fprintf out.txt %p",
        "find . -fls out.txt",
        "find . -ok rm",
        "find . -okdir rm",
        "sort -o b.txt a.txt",
        "sort -uo b.txt a.txt",
        "sort --output=b.txt a.txt",
        "sort --out=b.txt a.txt",
        "sort --compress-program=sh a.txt",
        "sort -T out a.txt",
        "tree -o out.txt",
        "tree -R",
        "tree --output out.txt",
        "file -C",
        "file --compile",
        "rg --pre sh x",
        "rg --pre-glob '*' x",
        "rg --hostname-bin sh x",
        "rg -z x",
        "rg --search-zip x",
        "uniq a.txt b.txt",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
}

#[test]
fn flags_that_follow_symlinks_out_of_a_folder_ask() {
    for line in [
        "grep -R key .",
        "grep -rS key .",
        "grep -rO key .",
        "grep -rp key .",
        "grep --dereference-recursive key .",
        "rg -L key",
        "rg --follow key",
        "find -L . -name a.txt",
        "find -H . -name a.txt",
        "find . -follow -name a.txt",
        "ls -LR .",
        "ls --dereference src",
        "du -L src",
        "tree -l",
        "diff -r src out",
        "diff --recursive src out",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
    for line in [
        "grep -rn key .",
        "rg -n key",
        "find . -name a.txt",
        "ls -lR src",
        "du -sh src",
        "diff a.txt b.txt",
    ] {
        assert!(read_only(line), "{line}");
    }
}

#[test]
fn whole_folder_reads_need_a_sandbox_that_hides_secrets() {
    let fake = Fake::new();
    for line in [
        "grep -r key .",
        "grep --recursive key .",
        "grep -d recurse key .",
        "rg key",
    ] {
        assert_eq!(
            classify_command(line, &fake, false),
            None,
            "{line} without hidden secrets"
        );
        assert_eq!(
            classify_command(line, &fake, true),
            Some(CommandKind::ReadOnly),
            "{line} with hidden secrets"
        );
    }
    assert_eq!(
        classify_command("grep key a.txt", &fake, false),
        Some(CommandKind::ReadOnly)
    );
}

#[test]
fn git_reads_run_and_everything_else_asks() {
    for line in [
        "git status",
        "git status --porcelain",
        "git log --oneline -20",
        "git log -p -- src",
        "git diff",
        "git diff --stat HEAD~1",
        "git show HEAD:src/main.rs",
        "git rev-parse --show-toplevel",
        "git blame a.txt",
        "git ls-files",
        "git branch",
        "git branch -a",
        "git branch -vv",
        "git branch --list 'feat-x'",
        "git log --format=%H",
    ] {
        assert!(read_only(line), "{line}");
    }
    for line in [
        "git",
        "git -c core.pager=sh log",
        "git -C .. status",
        "git --git-dir=x status",
        "git --exec-path=x status",
        "git --paginate log",
        "git commit -m x",
        "git push",
        "git checkout main",
        "git config user.name",
        "git diff --output=x",
        "git log --out=x",
        "git diff --ext-diff",
        "git log --textconv",
        "git log -O/etc/order",
        "git branch new-branch",
        "git branch -d old",
        "git branch -D old",
        "git branch -m a b",
        "git branch --delete old",
        "git branch --set-upstream-to=origin/main",
        "git branch -u origin/main",
        "git my-alias",
        "git grep x",
        "git show HEAD:../secret.txt",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
}

#[test]
fn git_asks_when_the_repository_reaches_beyond_the_open_folders() {
    let outside = Fake {
        git_top: Some("/repo"),
        ..Fake::new()
    };
    let none = Fake {
        git_top: None,
        ..Fake::new()
    };
    for line in [
        "git status",
        "git log -p",
        "git show HEAD:a.txt",
        "git diff",
    ] {
        assert_eq!(classify_command(line, &outside, true), None, "{line}");
        assert_eq!(classify_command(line, &none, true), None, "{line}");
    }
}

#[test]
fn git_commands_that_print_contents_ask_in_a_repository_tracking_secrets() {
    let tracking = Fake {
        tracks_secrets: true,
        ..Fake::new()
    };
    for line in [
        "git show",
        "git log -p",
        "git log --oneline",
        "git diff HEAD~1",
    ] {
        assert_eq!(classify_command(line, &tracking, true), None, "{line}");
    }
    for line in [
        "git status",
        "git ls-files",
        "git blame a.txt",
        "git branch",
    ] {
        assert_eq!(
            classify_command(line, &tracking, true),
            Some(CommandKind::ReadOnly),
            "{line}"
        );
    }
}

#[test]
fn git_rev_paths_are_read_from_the_repository_top() {
    /// The command's folder is `top/sub`, the open one: `top/` itself is
    /// open, but nothing else under it.
    struct Sub;
    impl Folders for Sub {
        fn inside(&self, path: &str) -> bool {
            !path.starts_with("top/") || path.starts_with("top/sub/")
        }
        fn exists(&self, _: &str) -> bool {
            false
        }
        fn is_dir(&self, _: &str) -> bool {
            false
        }
        fn git_top(&self) -> Option<String> {
            Some("top".into())
        }
        fn tracks_secrets(&self, _: &str) -> bool {
            false
        }
    }
    // `HEAD:secret.txt` is top/secret.txt, outside the open `top/sub`.
    assert_eq!(
        classify_command("git show HEAD:secret.txt", &Sub, true),
        None
    );
    assert_eq!(
        classify_command("git show HEAD:sub/a.txt", &Sub, true),
        Some(CommandKind::ReadOnly)
    );
}

#[test]
fn find_primaries_that_write_ask_even_after_a_double_dash() {
    for line in [
        "find -- . -delete",
        "find -- . -fprint out.txt",
        "find -- . -fls out.txt",
        "find -- . -exec rm {} ;",
        "find -- . -name x -execdir sh",
        "find -- src -ok cat",
    ] {
        assert_eq!(kind(line), None, "{line}");
    }
    assert!(read_only("find -- . -name x"));
}

#[test]
fn file_commands_that_overwrite_nothing_are_file_changes() {
    for line in [
        "mkdir build",
        "mkdir -p build/x/y",
        "touch new.txt",
        "touch a.txt",
        "cp a.txt c.txt",
        "cp -r src copy",
        "cp a.txt b.txt notes",
        "mv a.txt c.txt",
        "mv a.txt notes/",
    ] {
        assert!(file_change(line), "{line}");
    }
}

#[test]
fn file_commands_that_could_lose_data_or_leave_the_folders_ask() {
    for line in [
        "rm a.txt",
        "rm -rf src",
        "rmdir notes",
        "cp a.txt b.txt",
        "mv a.txt b.txt",
        "cp a.txt out",
        "mv b.txt a.txt",
        "cp missing.txt c.txt",
        "cp a.txt b.txt c.txt",
        "cp -f a.txt c.txt",
        "cp -l a.txt c.txt",
        "cp -s a.txt c.txt",
        "cp -L a.txt c.txt",
        "mv -f a.txt c.txt",
        "cp a.txt /tmp/c.txt",
        "mv a.txt ../c.txt",
        "touch .git/hooks/pre-commit",
        "mkdir .git/x",
        "mv .gz/permissions.toml x.toml",
        "mv .gz gz-old",
        "touch .gz/hooks.toml",
        "touch ./.gz/permissions.local.toml",
        "mkdir .gz/rules",
        "cp a.txt .mcp.json",
        "mv .mcp.json mcp-old.json",
        "mkdir",
        "mkdir -m 777 x",
        "touch .env",
        "touch -r a.txt b.txt",
        "cp a.txt",
        "cp -R src/ notes",
        "cp -R src/. notes",
        "cp -r . notes",
        "cp -r src/.. notes",
        "mv src/ notes",
        "cp -R src/ copy",
    ] {
        assert!(!file_change(line), "{line}");
    }
}

#[test]
fn audit_labels_and_confinement_follow_the_kind() {
    assert_eq!(Automatic::Read.audit_label(), "preapproved");
    assert_eq!(Automatic::ReadOnlyCommand.audit_label(), "read-only");
    assert_eq!(Automatic::ReadOnlyScript.audit_label(), "read-only");
    assert_eq!(Automatic::FileCommand.audit_label(), "accept-edits");
}

mod session {
    use super::super::*;
    use crate::assistant::os_sandbox::{OsSandbox, SandboxSettings};
    use crate::assistant::permissions::Policy;
    use crate::assistant::sandbox::tests::temp_workspace;
    use crate::assistant::tools::{plan, ToolName};
    use serde_json::json;

    fn confinement(sandbox: &Sandbox, enabled: bool) -> Confinement {
        let settings = SandboxSettings {
            enabled,
            ..SandboxSettings::default()
        };
        Confinement::new(sandbox.clone(), OsSandbox::unsaved(settings))
    }

    fn sandbox_here() -> bool {
        !matches!(
            os_sandbox::mechanism(),
            os_sandbox::Mechanism::Unavailable(_)
        )
    }

    #[test]
    fn reads_inside_the_open_folders_never_ask() {
        let (dir, sandbox) = temp_workspace("ro-reads");
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::write(dir.join("src/deep/a.rs"), "").unwrap();
        let policy = Policy::default();
        let session = Session {
            sandbox: &sandbox,
            confinement: None,
            policy: &policy,
            accepts_edits: false,
            in_plan_mode: false,
        };
        for (tool, input) in [
            (ToolName::ReadFile, json!({"path": "src/deep/a.rs"})),
            (ToolName::ListDirectory, json!({"path": "src"})),
            (ToolName::FindFiles, json!({"pattern": "*.rs"})),
            (ToolName::SearchFiles, json!({"pattern": "x"})),
        ] {
            let planned = plan(&sandbox, tool, &input).unwrap();
            assert_eq!(automatic(&planned, &session), Some(Automatic::Read));
        }
        let write = plan(
            &sandbox,
            ToolName::WriteFile,
            &json!({"path": "b.txt", "content": "x"}),
        )
        .unwrap();
        assert_eq!(automatic(&write, &session), None, "writes still ask");
    }

    #[cfg(unix)]
    #[test]
    fn a_dot_dot_through_a_symlinked_folder_is_checked_where_it_really_leads() {
        let (dir, sandbox) = temp_workspace("ro-dotdot");
        let outside = crate::assistant::sandbox::tests::temp_dir("ro-dotdot-out");
        std::fs::create_dir_all(outside.join("deep")).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("deep"), dir.join("link")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        assert_eq!(
            classify_command("cat link/../secret.txt", &sandbox, true),
            None
        );
        assert_eq!(
            classify_command("cat src/../a.txt", &sandbox, true),
            Some(CommandKind::ReadOnly)
        );
        assert_eq!(
            classify_command("cat missing/../a.txt", &sandbox, true),
            None
        );
    }

    #[test]
    fn git_asks_in_a_repository_opened_below_its_top() {
        let (dir, _) = temp_workspace("ro-git-sub");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let sub = Sandbox::new(&dir.join("sub")).unwrap();
        assert_eq!(classify_command("git status", &sub, true), None);
        let top = Sandbox::new(&dir).unwrap();
        assert_eq!(
            classify_command("git status", &top, true),
            Some(CommandKind::ReadOnly)
        );
    }

    #[test]
    fn plan_mode_runs_reads_but_no_command_or_script() {
        let (_dir, sandbox) = temp_workspace("ro-plan");
        let policy = Policy::default();
        let on = confinement(&sandbox, true);
        let session = Session {
            sandbox: &sandbox,
            confinement: Some(&on),
            policy: &policy,
            accepts_edits: true,
            in_plan_mode: true,
        };
        let read = plan(&sandbox, ToolName::ListDirectory, &json!({"path": "."})).unwrap();
        assert_eq!(automatic(&read, &session), Some(Automatic::Read));
        for (tool, input) in [
            (ToolName::RunCommand, json!({"command": "ls"})),
            (ToolName::RunCommand, json!({"command": "mkdir x"})),
            (
                ToolName::RunScript,
                json!({"language": "python", "code": "print(1)"}),
            ),
        ] {
            let planned = plan(&sandbox, tool, &input).unwrap();
            assert_eq!(automatic(&planned, &session), None, "{input}");
        }
    }

    #[test]
    fn commands_and_scripts_ask_without_an_os_sandbox() {
        let (_dir, sandbox) = temp_workspace("ro-off");
        let policy = Policy::default();
        let off = confinement(&sandbox, false);
        for confinement in [None, Some(&off)] {
            let session = Session {
                sandbox: &sandbox,
                confinement,
                policy: &policy,
                accepts_edits: true,
                in_plan_mode: false,
            };
            let ls = plan(&sandbox, ToolName::RunCommand, &json!({"command": "ls"})).unwrap();
            let mkdir = plan(
                &sandbox,
                ToolName::RunCommand,
                &json!({"command": "mkdir x"}),
            );
            let script = plan(
                &sandbox,
                ToolName::RunScript,
                &json!({"language": "python", "code": "print(1)"}),
            )
            .unwrap();
            assert_eq!(automatic(&ls, &session), None);
            assert_eq!(automatic(&mkdir.unwrap(), &session), None);
            assert_eq!(automatic(&script, &session), None);
        }
    }

    #[test]
    fn under_the_sandbox_read_only_commands_run_and_file_commands_need_accept_edits() {
        if !sandbox_here() {
            eprintln!("skipped: no OS sandbox here");
            return;
        }
        let (dir, sandbox) = temp_workspace("ro-on");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let policy = Policy::default();
        let on = confinement(&sandbox, true);
        let mut session = Session {
            sandbox: &sandbox,
            confinement: Some(&on),
            policy: &policy,
            accepts_edits: false,
            in_plan_mode: false,
        };
        let command =
            |line: &str| plan(&sandbox, ToolName::RunCommand, &json!({"command": line})).unwrap();
        assert_eq!(
            automatic(&command("git status"), &session),
            Some(Automatic::ReadOnlyCommand)
        );
        assert_eq!(automatic(&command("mkdir build"), &session), None);
        assert_eq!(automatic(&command("make"), &session), None);
        session.accepts_edits = true;
        assert_eq!(
            automatic(&command("mkdir build"), &session),
            Some(Automatic::FileCommand)
        );
        assert_eq!(automatic(&command("rm -rf build"), &session), None);
    }

    #[test]
    fn a_script_that_tried_to_write_asks_next_time_in_this_chat() {
        let (_dir, sandbox) = temp_workspace("ro-script");
        let mut policy = Policy::default();
        let on = confinement(&sandbox, true);
        let code = "open('x', 'w').write('y')";
        let script = plan(
            &sandbox,
            ToolName::RunScript,
            &json!({"language": "python", "code": code}),
        )
        .unwrap();
        let scripts_here = on.read_only_support().is_some_and(|s| s.scripts);
        let session = Session {
            sandbox: &sandbox,
            confinement: Some(&on),
            policy: &policy,
            accepts_edits: false,
            in_plan_mode: false,
        };
        let first = automatic(&script, &session);
        assert_eq!(first.is_some(), scripts_here);

        let mut out = ToolOutput::error("PermissionError: [Errno 1] Operation not permitted: 'x'");
        after_read_only_script(&mut out, code, false, &policy);
        assert!(out.output.ends_with(SCRIPT_WROTE_NOTE));
        let session = Session {
            sandbox: &sandbox,
            confinement: Some(&on),
            policy: &policy,
            accepts_edits: false,
            in_plan_mode: false,
        };
        assert_eq!(automatic(&script, &session), None, "asks the second time");
        policy.clear_chat();
        let session = Session {
            sandbox: &sandbox,
            confinement: Some(&on),
            policy: &policy,
            accepts_edits: false,
            in_plan_mode: false,
        };
        assert_eq!(automatic(&script, &session).is_some(), scripts_here);
    }

    #[test]
    fn a_script_calling_write_helpers_asks_at_once() {
        let (_dir, sandbox) = temp_workspace("ro-script-writes");
        let policy = Policy::default();
        let on = confinement(&sandbox, true);
        let session = Session {
            sandbox: &sandbox,
            confinement: Some(&on),
            policy: &policy,
            accepts_edits: false,
            in_plan_mode: false,
        };
        for code in [
            "import gz\ngz.write_file('a', 'b')",
            "gz.edit_file('a', 'b', 'c')",
        ] {
            let script = plan(
                &sandbox,
                ToolName::RunScript,
                &json!({"language": "python", "code": code}),
            )
            .unwrap();
            assert_eq!(automatic(&script, &session), None, "{code}");
        }
    }

    #[test]
    fn a_clean_read_only_script_is_not_remembered() {
        let policy = Policy::default();
        let mut out = ToolOutput::ok("42\n".into());
        after_read_only_script(&mut out, "print(42)", false, &policy);
        assert_eq!(out.output, "42\n");
        assert!(!policy.script_wrote("print(42)"));
        let mut refused = ToolOutput::ok("done".into());
        after_read_only_script(&mut refused, "gz.write_file('a','b')", true, &policy);
        assert!(refused.output.ends_with(SCRIPT_WROTE_NOTE));
        assert!(policy.script_wrote("gz.write_file('a','b')"));
    }
}
