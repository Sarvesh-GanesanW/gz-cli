//! Which checkers look at an edited file: the user's configured ones for
//! that project and extension, plus the built-in fast ones (ruff,
//! shellcheck, go vet) when they are installed. Slow whole-project checkers
//! (tsc, eslint, cargo check) run only when configured.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::settings::{Configured, Settings};

/// The `{files}` placeholder in a configured command.
pub const FILES_PLACEHOLDER: &str = "{files}";

/// One checker run for one edited file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub name: String,
    /// A shell command line, words already quoted.
    pub command: String,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub compare_before: bool,
}

/// A built-in checker: when it applies and how it is run.
struct Builtin {
    name: &'static str,
    program: &'static str,
    applies: fn(&Path, &str) -> bool,
    /// The command after the program, given the file as typed.
    args: fn(&str) -> String,
    /// Runs in the file's own folder (go vet checks a package).
    in_file_folder: bool,
}

const BUILTINS: [Builtin; 3] = [
    Builtin {
        name: "ruff",
        program: "ruff",
        applies: |path, _| has_extension(path, &["py", "pyi", "ipynb"]),
        // --no-fix: a project's `fix = true` must not let a check edit files.
        args: |file| {
            format!("check --output-format concise --no-cache --no-fix --force-exclude --quiet -- {file}")
        },
        in_file_folder: false,
    },
    Builtin {
        name: "shellcheck",
        program: "shellcheck",
        applies: |path, content| {
            has_extension(path, &["sh", "bash", "ksh"])
                || (path.extension().is_none() && has_shell_shebang(content))
        },
        args: |file| format!("-f gcc -- {file}"),
        in_file_folder: false,
    },
    Builtin {
        name: "go vet",
        program: "go",
        applies: |path, _| has_extension(path, &["go"]),
        args: |_| "vet .".to_string(),
        in_file_folder: true,
    },
];

/// Every checker for `file` (absolute, just written with `content`), for a
/// session working in `cwd`. `path_dirs` is where installed programs are
/// looked for (the `PATH`).
pub fn for_file(
    settings: &Settings,
    path_dirs: &[PathBuf],
    file: &Path,
    content: &str,
    cwd: &Path,
) -> Vec<Run> {
    let configured: Vec<Run> = settings
        .checkers
        .iter()
        .filter(|checker| configured_applies(checker, file))
        .map(|checker| configured_run(checker, file, cwd))
        .collect();
    let builtins: Vec<Run> = settings
        .auto
        .then_some(BUILTINS.iter())
        .into_iter()
        .flatten()
        // The user's own checker of the same name replaces the built-in one.
        .filter(|builtin| !configured.iter().any(|run| run.name == builtin.name))
        .filter(|builtin| (builtin.applies)(file, content))
        .filter_map(|builtin| builtin_run(builtin, path_dirs, settings.timeout, file, cwd))
        .collect();
    configured.into_iter().chain(builtins).collect()
}

fn configured_applies(checker: &Configured, file: &Path) -> bool {
    let in_project = checker
        .folder
        .as_ref()
        .is_none_or(|folder| file.starts_with(folder));
    let extensions: Vec<&str> = checker.extensions.iter().map(String::as_str).collect();
    in_project && (extensions.is_empty() || has_extension(file, &extensions))
}

fn configured_run(checker: &Configured, file: &Path, cwd: &Path) -> Run {
    let cwd = checker.folder.clone().unwrap_or_else(|| cwd.to_path_buf());
    let typed = shell_word(&typed_path(file, &cwd));
    Run {
        name: checker.name.clone(),
        command: checker.command.replace(FILES_PLACEHOLDER, &typed),
        cwd,
        timeout: checker.timeout,
        compare_before: checker.compare_before,
    }
}

fn builtin_run(
    builtin: &Builtin,
    path_dirs: &[PathBuf],
    timeout: Duration,
    file: &Path,
    cwd: &Path,
) -> Option<Run> {
    let program = find_program(builtin.program, path_dirs)?;
    let cwd = match (builtin.in_file_folder, file.parent()) {
        (true, Some(folder)) => folder.to_path_buf(),
        _ => cwd.to_path_buf(),
    };
    let typed = shell_word(&typed_path(file, &cwd));
    Some(Run {
        name: builtin.name.to_string(),
        command: format!(
            "{} {}",
            shell_word(&program.to_string_lossy()),
            (builtin.args)(&typed)
        ),
        cwd,
        timeout,
        compare_before: true,
    })
}

/// The installed program called `name`, from the first folder that has it.
/// Relative `PATH` entries (`.`, `bin`) are skipped: they would resolve in
/// the project, and a checked-in program must not run as a checker.
pub fn find_program(name: &str, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_string()]
    };
    path_dirs
        .iter()
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `file` relative to `cwd` when inside it (shorter, and what tools print).
fn typed_path(file: &Path, cwd: &Path) -> String {
    file.strip_prefix(cwd)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

fn shell_word(word: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", word.replace('"', "\"\""))
    } else {
        crate::resolve::rewrite::quote(word)
    }
}

fn has_extension(path: &Path, wanted: &[&str]) -> bool {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|ext| wanted.contains(&ext.as_str()))
}

fn has_shell_shebang(content: &str) -> bool {
    let Some(first) = content.lines().next() else {
        return false;
    };
    let Some(interpreter) = first.strip_prefix("#!") else {
        return false;
    };
    interpreter
        .split(|c: char| c.is_whitespace() || c == '/')
        .any(|word| matches!(word, "sh" | "bash" | "ksh" | "dash"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    /// A folder with fake, executable `programs`.
    pub fn bin_with(programs: &[&str]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = temp_dir("diag-bin");
        for program in programs {
            let path = bin.join(program);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    fn names(runs: &[Run]) -> Vec<&str> {
        runs.iter().map(|run| run.name.as_str()).collect()
    }

    #[test]
    fn builtins_run_only_when_installed_and_for_their_files() {
        let bin = bin_with(&["ruff", "shellcheck"]);
        let settings = Settings::default();
        let cwd = Path::new("/w");
        let dirs = [bin.clone()];
        let py = for_file(&settings, &dirs, Path::new("/w/src/app.py"), "", cwd);
        assert_eq!(names(&py), ["ruff"]);
        assert_eq!(
            py[0].command,
            format!(
                "{}/ruff check --output-format concise --no-cache --no-fix --force-exclude --quiet -- src/app.py",
                bin.display()
            )
        );
        assert_eq!(py[0].cwd, cwd);
        assert!(py[0].compare_before);
        let script = for_file(
            &settings,
            &dirs,
            Path::new("/w/deploy"),
            "#!/usr/bin/env bash\n",
            cwd,
        );
        assert_eq!(names(&script), ["shellcheck"]);
        let plain = for_file(&settings, &dirs, Path::new("/w/notes"), "hello\n", cwd);
        assert!(plain.is_empty());
        let go = for_file(&settings, &dirs, Path::new("/w/main.go"), "", cwd);
        assert!(go.is_empty(), "go is not installed here");
        let rust = for_file(&settings, &dirs, Path::new("/w/src/lib.rs"), "", cwd);
        assert!(rust.is_empty(), "cargo check runs only when configured");
    }

    #[test]
    fn go_vet_checks_the_files_package_in_its_folder() {
        let bin = bin_with(&["go"]);
        let runs = for_file(
            &Settings::default(),
            &[bin],
            Path::new("/w/cmd/api/main.go"),
            "",
            Path::new("/w"),
        );
        assert_eq!(names(&runs), ["go vet"]);
        assert_eq!(runs[0].cwd, Path::new("/w/cmd/api"));
        assert!(
            runs[0].command.ends_with("/go vet ."),
            "{}",
            runs[0].command
        );
    }

    #[test]
    fn auto_off_keeps_only_configured_checkers() {
        let bin = bin_with(&["ruff"]);
        let settings = Settings {
            auto: false,
            ..Settings::default()
        };
        assert!(for_file(&settings, &[bin], Path::new("/w/a.py"), "", Path::new("/w")).is_empty());
    }

    #[test]
    fn configured_checkers_follow_their_project_and_extensions() {
        let bin = bin_with(&["ruff"]);
        let settings = Settings::parse(
            r#"
[[checkers]]
name = "tsc"
command = "npx tsc --noEmit -p . --pretty false"
extensions = ["ts"]
folder = "/web"
timeout_secs = 40

[[checkers]]
name = "ruff"
command = "ruff check --select F {files}"
extensions = ["py"]
"#,
        )
        .unwrap();
        let dirs = [bin];
        let ts = for_file(
            &settings,
            &dirs,
            Path::new("/web/src/a b.ts"),
            "",
            Path::new("/w"),
        );
        assert_eq!(names(&ts), ["tsc"]);
        assert_eq!(ts[0].cwd, Path::new("/web"));
        assert_eq!(ts[0].timeout, Duration::from_secs(40));
        let elsewhere = for_file(
            &settings,
            &dirs,
            Path::new("/api/a.ts"),
            "",
            Path::new("/w"),
        );
        assert!(elsewhere.is_empty(), "tsc belongs to /web only");
        let py = for_file(
            &settings,
            &dirs,
            Path::new("/w/a b.py"),
            "",
            Path::new("/w"),
        );
        assert_eq!(
            names(&py),
            ["ruff"],
            "the user's ruff replaces the built-in one"
        );
        assert_eq!(py[0].command, "ruff check --select F 'a b.py'");
    }

    #[test]
    fn finds_only_executable_programs() {
        let bin = bin_with(&["ruff"]);
        std::fs::write(bin.join("shellcheck"), "not executable").unwrap();
        assert_eq!(find_program("ruff", &[bin.clone()]), Some(bin.join("ruff")));
        assert_eq!(find_program("shellcheck", &[bin]), None);
    }

    #[test]
    fn relative_path_entries_are_never_searched() {
        let bin = bin_with(&["ruff"]);
        let here = std::env::current_dir().unwrap();
        let up: PathBuf = here.components().skip(1).map(|_| "..").collect();
        let relative = up.join(bin.strip_prefix("/").unwrap());
        assert!(
            relative.join("ruff").is_file(),
            "the relative entry does reach it"
        );
        assert_eq!(find_program("ruff", &[relative]), None);
    }
}
