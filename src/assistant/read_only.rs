//! What gz runs without asking, after the user's rules, grants and the
//! mode have had their say ([`automatic`]):
//!
//! - reads and searches inside the open folders (the file tools never
//!   reach outside them or into secret files);
//! - commands from a short read-only list ([`classify_command`]), under
//!   the OS sandbox's read-only profile;
//! - scripts, under that profile too, unless the sandbox already refused
//!   the same script something in this chat;
//! - in accept-edits mode, `mkdir`, `touch`, `cp` and `mv` inside the open
//!   folders that overwrite nothing, under the usual OS sandbox.
//!
//! A deny or ask rule on `read(…)` or `edit(…)` turns the commands and
//! scripts off: they can reach a file without naming it, so they ask.
//!
//! Without an OS sandbox (Windows, or `/sandbox off`) commands and scripts
//! keep asking: the read-only list is a guard against mistakes, the
//! sandbox the guarantee.

mod git_index;

use std::path::{Component, Path, PathBuf};

use crate::assistant::jobs::Jobs;
use crate::assistant::os_sandbox::read_only::{ReadOnlyKind, ReadOnlySupport};
use crate::assistant::os_sandbox::{self, Confinement};
use crate::assistant::permissions::{has_shell_syntax, Policy};
use crate::assistant::rules::FileUse;
use crate::assistant::sandbox::Sandbox;
use crate::assistant::tools::{Access, Planned, ToolOutput};

/// How gz may run a call without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automatic {
    /// A read or search inside the open folders.
    Read,
    /// A command from the read-only list, under the read-only profile.
    ReadOnlyCommand,
    /// A script, under the read-only profile.
    ReadOnlyScript,
    /// A safe file command in accept-edits mode, under the usual sandbox.
    FileCommand,
}

impl Automatic {
    /// The audit log's `decision`.
    pub fn audit_label(self) -> &'static str {
        match self {
            Automatic::Read => "preapproved",
            Automatic::ReadOnlyCommand | Automatic::ReadOnlyScript => "read-only",
            Automatic::FileCommand => "accept-edits",
        }
    }

    /// The sandbox the call runs under: the read-only profile, or the
    /// usual one.
    pub fn confinement(self, base: &Confinement) -> Confinement {
        match self {
            Automatic::ReadOnlyCommand => base.read_only(ReadOnlyKind::Command),
            Automatic::ReadOnlyScript => base.read_only(ReadOnlyKind::Script),
            Automatic::Read | Automatic::FileCommand => base.clone(),
        }
    }
}

/// What [`automatic`] needs to know about the session.
pub struct Session<'a> {
    pub sandbox: &'a Sandbox,
    /// The OS sandbox for the model's commands; `None` when they run as is.
    pub confinement: Option<&'a Confinement>,
    pub policy: &'a Policy,
    /// Whether accept-edits mode is on.
    pub accepts_edits: bool,
    /// Whether plan mode is on: it refuses every command and script, so
    /// none runs here either.
    pub in_plan_mode: bool,
}

/// Whether `planned` runs without asking, and how. Called after rules,
/// grants and the mode, just before the prompt.
pub fn automatic(planned: &Planned, session: &Session) -> Option<Automatic> {
    match planned.access {
        Access::Read => {
            let roots = session.sandbox.roots();
            roots
                .iter()
                .any(|root| planned.scope.starts_with(root))
                .then_some(Automatic::Read)
        }
        Access::Command | Access::Script if session.in_plan_mode => None,
        Access::Command => {
            let support = session.confinement?.read_only_support()?;
            let line = planned.preview.as_deref()?;
            let kind = classify_command(line, session.sandbox, support.hides_secrets)?;
            if kind
                .uses_files()
                .any(|used| session.policy.rules_guard_files(used))
            {
                return None;
            }
            match kind {
                CommandKind::ReadOnly => Some(Automatic::ReadOnlyCommand),
                CommandKind::FileChange => session.accepts_edits.then_some(Automatic::FileCommand),
            }
        }
        Access::Script => {
            if session.policy.rules_guard_files(FileUse::Read) {
                return None;
            }
            let support: ReadOnlySupport = session.confinement?.read_only_support()?;
            let code = planned.preview.as_deref()?;
            let may_be_read_only = !calls_write_helpers(code) && !session.policy.script_wrote(code);
            (support.scripts && may_be_read_only).then_some(Automatic::ReadOnlyScript)
        }
        Access::Write
        | Access::Folder
        | Access::ServerAction
        | Access::Install
        | Access::Trust
        | Access::Fetch
        | Access::LocalTool => None,
    }
}

/// The script helpers that write: a script calling one plainly means to
/// write, so it asks at once instead of failing read-only first.
const WRITE_HELPERS: &[&str] = &["write_file", "edit_file"];

fn calls_write_helpers(code: &str) -> bool {
    WRITE_HELPERS.iter().any(|helper| code.contains(helper))
}

/// The session's jobs, running under `automatic`'s sandbox when given.
pub fn jobs_for(jobs: &Jobs, automatic: Option<Automatic>) -> Jobs {
    match (automatic, jobs.confinement()) {
        (Some(automatic), Some(base)) => jobs.with_confinement(automatic.confinement(base)),
        _ => jobs.clone(),
    }
}

/// The sandbox a script runs under: `automatic`'s, or the session's.
pub fn script_confinement(
    base: Option<&Confinement>,
    automatic: Option<Automatic>,
) -> Option<Confinement> {
    let base = base?;
    Some(match automatic {
        Some(automatic) => automatic.confinement(base),
        None => base.clone(),
    })
}

/// What the model reads when the read-only sandbox refused something a
/// script gz ran without asking tried to do.
pub const SCRIPT_WROTE_NOTE: &str = "[gz ran this script without asking, under a read-only \
sandbox, and the sandbox refused something it tried: a write (to a file, or through a \
helper), a file outside the open folders or a secret file, the network, or another \
program's settings or processes. Nothing was changed. If that step is needed, run the \
script again: gz will show it to the user and ask first.]";

/// After a script ran under the read-only profile: when the sandbox
/// refused something (even if the script caught the error and went on),
/// tells the model and remembers the script for this chat, so the next run
/// of it asks.
pub fn after_read_only_script(
    out: &mut ToolOutput,
    code: &str,
    helper_write_refused: bool,
    policy: &Policy,
) {
    let denied = os_sandbox::shows_denial(&out.output);
    if !(denied || helper_write_refused) {
        return;
    }
    policy.remember_script_wrote(code);
    if !out.output.is_empty() && !out.output.ends_with('\n') {
        out.output.push('\n');
    }
    out.output.push_str(SCRIPT_WROTE_NOTE);
}

/// What a helper write gets inside a script gz runs without asking.
pub const HELPER_WRITE_REFUSED: &str = "this script runs read-only because nobody approved it; \
gz refused the write. Run the script again and gz will ask the user first.";

/// The two kinds of command gz may run without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// Reads only: from the read-only list.
    ReadOnly,
    /// `mkdir`, `touch`, `cp` or `mv` inside the open folders that
    /// overwrites nothing.
    FileChange,
}

impl CommandKind {
    /// How the command uses files, as deny and ask rules see it: a copy
    /// reads its sources, and a move or copy edits where it lands.
    fn uses_files(self) -> impl Iterator<Item = FileUse> {
        let uses: &[FileUse] = match self {
            CommandKind::ReadOnly => &[FileUse::Read],
            CommandKind::FileChange => &[FileUse::Read, FileUse::Edit],
        };
        uses.iter().copied()
    }
}

/// What the classifier asks about the paths in a command line. Paths are
/// as typed, relative to the command's folder.
pub trait Folders {
    /// Whether `path` resolves (symlinks included) inside the open
    /// folders and is not a secret file.
    fn inside(&self, path: &str) -> bool;
    /// Whether something exists at `path`.
    fn exists(&self, path: &str) -> bool;
    /// Whether `path` is a folder.
    fn is_dir(&self, path: &str) -> bool;
    /// The top folder of the git repository the command's folder is in
    /// (the one holding `.git`), as a path `inside` accepts.
    fn git_top(&self) -> Option<String>;
    /// Whether the repository at `top` tracks a secret file (or can't
    /// tell), which git would print from its own objects.
    fn tracks_secrets(&self, top: &str) -> bool;
}

impl Folders for Sandbox {
    fn inside(&self, path: &str) -> bool {
        // `resolve` treats an empty path as the current folder; a command
        // given an empty argument means something else.
        !path.trim().is_empty()
            && path.trim() == path
            && physical(self, path).is_some_and(|path| self.resolve(&path).is_ok())
    }

    fn exists(&self, path: &str) -> bool {
        physical(self, path)
            .and_then(|path| self.resolve(&path).ok())
            .is_some_and(|real| real.exists())
    }

    fn is_dir(&self, path: &str) -> bool {
        physical(self, path)
            .and_then(|path| self.resolve(&path).ok())
            .is_some_and(|real| real.is_dir())
    }

    fn git_top(&self) -> Option<String> {
        // These point git at a repository wherever it is, whatever the
        // folder holds.
        if GIT_LOCATION_VARIABLES
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return None;
        }
        let cwd = self.cwd();
        let top = cwd
            .ancestors()
            .find(|folder| folder.join(".git").exists())?;
        Some(top.to_string_lossy().into_owned())
    }

    fn tracks_secrets(&self, top: &str) -> bool {
        git_index::tracks_secrets(Path::new(top))
    }
}

/// `path` with its `..` steps taken the way the program will take them:
/// on the real folders, so `link/..` is the parent of the link's target,
/// not the folder holding the link (which is how [`Sandbox::resolve`]
/// folds them for the file tools). `None` when the part up to the last
/// `..` doesn't exist.
fn physical(sandbox: &Sandbox, path: &str) -> Option<String> {
    let parts: Vec<Component> = Path::new(path).components().collect();
    let Some(last_up) = parts.iter().rposition(|part| *part == Component::ParentDir) else {
        return Some(path.to_string());
    };
    let head: PathBuf = parts[..=last_up].iter().collect();
    let real_head = std::fs::canonicalize(sandbox.cwd().join(head)).ok()?;
    let rest: PathBuf = parts[last_up + 1..].iter().collect();
    Some(real_head.join(rest).to_string_lossy().into_owned())
}

/// Whether `line` runs without asking: one program from the lists below,
/// with plain arguments (no chaining, pipes, redirection, substitution,
/// variables, globs or `~`), no flag that writes or runs other programs,
/// and every path inside the open folders. `hides_secrets` says the OS
/// sandbox keeps secret files unreadable, which commands that read whole
/// folders (`grep -r`, `rg`) need.
pub fn classify_command(
    line: &str,
    folders: &dyn Folders,
    hides_secrets: bool,
) -> Option<CommandKind> {
    let words = split_words(line)?;
    let (program, args) = words.split_first()?;
    if let Some(rules) = read_only_program(program) {
        return read_only_args_ok(&rules, args, folders, hides_secrets)
            .then_some(CommandKind::ReadOnly);
    }
    if program == "git" {
        return git_is_read_only(args, folders).then_some(CommandKind::ReadOnly);
    }
    file_change_ok(program, args, folders).then_some(CommandKind::FileChange)
}

/// Splits a command line into words the way `sh` would, or `None` when it
/// has anything the shell would expand or interpret: [`has_shell_syntax`]
/// characters, backslashes outside single quotes, unquoted glob characters
/// or a leading `~` or `#`, or an unclosed quote.
fn split_words(line: &str) -> Option<Vec<String>> {
    if has_shell_syntax(line) || cfg!(windows) {
        return None;
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        inner => word.push(inner),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' | '!' => return None,
                        inner => word.push(inner),
                    }
                }
            }
            '\\' | '*' | '?' | '[' | ']' | '!' => return None,
            '~' | '#' if !in_word => return None,
            // Some shells expand `~` after `=` or `:` (`--dir=~/x`).
            '~' if word.ends_with(['=', ':']) => return None,
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// What makes a read-only program write or run something else.
struct ReadOnlyProgram {
    /// Long flags (without `--`) that write or run other programs; their
    /// abbreviations count too, since GNU tools accept them.
    denied_long: &'static [&'static str],
    /// Short flags that do, in any cluster.
    denied_short: &'static str,
    /// Arguments refused wherever they stand, after `--` too: `find`'s
    /// primaries come after its start folders, which may follow `--`.
    denied_anywhere: &'static [&'static str],
    /// The most plain arguments it takes (`uniq IN OUT` writes OUT).
    max_plain: Option<usize>,
    /// When it reads whole folders' contents.
    reads_folders: FolderReads,
}

#[derive(Clone, Copy)]
enum FolderReads {
    Never,
    Always,
    /// With one of these long flags or short letters.
    With(&'static [&'static str], &'static str),
}

const PLAIN: ReadOnlyProgram = ReadOnlyProgram {
    denied_long: &[],
    denied_short: "",
    denied_anywhere: &[],
    max_plain: None,
    reads_folders: FolderReads::Never,
};

/// The read-only list: programs that only read their arguments and print,
/// with the flags that would make them do more.
fn read_only_program(name: &str) -> Option<ReadOnlyProgram> {
    let rules = match name {
        "cat" | "head" | "tail" | "wc" | "pwd" | "echo" | "which" | "stat" | "cut" | "nl"
        | "basename" | "dirname" | "realpath" | "cmp" => PLAIN,
        // Flags that follow symlinks met while walking a folder are refused
        // throughout: such a link may lead out of the open folders, and only
        // the named arguments are checked.
        "ls" | "du" => ReadOnlyProgram {
            denied_long: &["dereference"],
            denied_short: "L",
            ..PLAIN
        },
        "grep" | "egrep" | "fgrep" => ReadOnlyProgram {
            // `-R` follows every symlink (GNU and macOS); macOS `-S`, `-O`
            // and `-p` pick how links are followed.
            denied_long: &["dereference-recursive"],
            denied_short: "RSOp",
            reads_folders: FolderReads::With(&["recursive", "directories"], "rd"),
            ..PLAIN
        },
        "rg" => ReadOnlyProgram {
            denied_long: &["pre", "pre-glob", "hostname-bin", "search-zip", "follow"],
            denied_short: "zL",
            reads_folders: FolderReads::Always,
            ..PLAIN
        },
        // GNU `diff -r` follows symlinks inside the folders it compares.
        "diff" => ReadOnlyProgram {
            denied_long: &["recursive"],
            denied_short: "r",
            ..PLAIN
        },
        "find" => ReadOnlyProgram {
            denied_short: "LH",
            denied_anywhere: FIND_DENIED,
            ..PLAIN
        },
        "sort" => ReadOnlyProgram {
            denied_long: &["output", "compress-program", "temporary-directory"],
            denied_short: "oT",
            ..PLAIN
        },
        "uniq" => ReadOnlyProgram {
            max_plain: Some(1),
            ..PLAIN
        },
        "tree" => ReadOnlyProgram {
            denied_long: &["output"],
            denied_short: "oRl",
            ..PLAIN
        },
        "file" => ReadOnlyProgram {
            denied_long: &["compile"],
            denied_short: "C",
            ..PLAIN
        },
        _ => return None,
    };
    Some(rules)
}

/// `find` primaries that follow symlinks, run programs, delete, or write
/// files.
const FIND_DENIED: &[&str] = &[
    "-follow", "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
    "-fls",
];

/// One argument after the program, as the checks see it.
enum Arg<'a> {
    /// `--name` or `--name=value`.
    Long(&'a str, Option<&'a str>),
    /// `-abc`: one or more short flags (or one with its value attached).
    Short(&'a str),
    /// A plain argument: a path, a pattern, a value.
    Plain(&'a str),
}

/// Reads `args` the way getopt does: `--` ends the flags, `-` alone is
/// standard input (a plain argument).
fn parse_args(args: &[String]) -> Vec<Arg<'_>> {
    let mut parsed = Vec::new();
    let mut flags_done = false;
    for arg in args {
        let arg = arg.as_str();
        if flags_done || arg == "-" || !arg.starts_with('-') {
            parsed.push(Arg::Plain(arg));
        } else if arg == "--" {
            flags_done = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) => parsed.push(Arg::Long(name, Some(value))),
                None => parsed.push(Arg::Long(long, None)),
            }
        } else {
            parsed.push(Arg::Short(&arg[1..]));
        }
    }
    parsed
}

/// Whether the long flag `name` is one of `denied` or an abbreviation of
/// one.
fn long_matches(name: &str, denied: &[&str]) -> bool {
    !name.is_empty() && denied.iter().any(|d| d.starts_with(name))
}

/// Whether a short-flag cluster may carry a path we can't check: a value
/// attached to a flag (`-f.env`, `-O/etc/x`) that looks like one.
fn short_may_hide_a_path(cluster: &str) -> bool {
    cluster.contains(['/', '.', '~'])
}

fn read_only_args_ok(
    rules: &ReadOnlyProgram,
    args: &[String],
    folders: &dyn Folders,
    hides_secrets: bool,
) -> bool {
    if args
        .iter()
        .any(|arg| rules.denied_anywhere.contains(&arg.as_str()))
    {
        return false;
    }
    let mut plain = 0;
    let mut reads_folders = matches!(rules.reads_folders, FolderReads::Always);
    for arg in parse_args(args) {
        match arg {
            Arg::Long(name, value) => {
                if long_matches(name, rules.denied_long) {
                    return false;
                }
                if let FolderReads::With(long, _) = rules.reads_folders {
                    reads_folders |= long_matches(name, long);
                }
                if value.is_some_and(|v| !v.is_empty() && !folders.inside(v)) {
                    return false;
                }
            }
            Arg::Short(cluster) => {
                if cluster.contains(|c| rules.denied_short.contains(c)) {
                    return false;
                }
                if let FolderReads::With(_, short) = rules.reads_folders {
                    reads_folders |= cluster.contains(|c| short.contains(c));
                }
                if short_may_hide_a_path(cluster) {
                    return false;
                }
            }
            Arg::Plain(text) => {
                plain += 1;
                if text != "-" && !folders.inside(text) {
                    return false;
                }
            }
        }
    }
    let within_count = rules.max_plain.is_none_or(|max| plain <= max);
    within_count && (hides_secrets || !reads_folders)
}

/// Environment variables that move where git finds the repository.
const GIT_LOCATION_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
];

/// Git subcommands that only read the repository.
const GIT_READ_ONLY: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "blame",
    "ls-files",
    "branch",
];
/// Git subcommands that may print committed file contents, which the OS
/// sandbox can't hide by name: they ask in a repository tracking secrets.
const GIT_PRINTS_CONTENTS: &[&str] = &["show", "diff", "log"];
/// Git flags that write files or run configured programs.
const GIT_DENIED_LONG: &[&str] = &["output", "ext-diff", "textconv", "open-files-in-pager"];
/// `git branch` flags that only list.
const GIT_BRANCH_LONG: &[&str] = &[
    "list",
    "all",
    "remotes",
    "verbose",
    "show-current",
    "color",
    "no-color",
    "column",
    "no-column",
    "ignore-case",
    "omit-empty",
];
const GIT_BRANCH_SHORT: &str = "arvl";

/// `git <subcommand> …` that only reads: in a repository whose top folder
/// is inside the open folders (git reads the whole repository, not only
/// the command's folder), and for `show`, `diff` and `log` one that tracks
/// no secret file, no options before the subcommand (`-c`, `-C`,
/// `--git-dir`, `--exec-path` and the like), a subcommand from
/// [`GIT_READ_ONLY`] (built-in names win over aliases), no flag that
/// writes a file or runs a configured diff program, and every path inside
/// the open folders, `rev:path` included.
fn git_is_read_only(args: &[String], folders: &dyn Folders) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    if !GIT_READ_ONLY.contains(&subcommand.as_str()) {
        return false;
    }
    let Some(top) = folders.git_top() else {
        return false;
    };
    if !folders.inside(&top) {
        return false;
    }
    if GIT_PRINTS_CONTENTS.contains(&subcommand.as_str()) && folders.tracks_secrets(&top) {
        return false;
    }
    let parsed = parse_args(rest);
    if subcommand == "branch" && !git_branch_lists(&parsed) {
        return false;
    }
    parsed.iter().all(|arg| match arg {
        Arg::Long(name, value) => {
            !long_matches(name, GIT_DENIED_LONG)
                && value.is_none_or(|v| v.is_empty() || folders.inside(v))
        }
        Arg::Short(cluster) => !short_may_hide_a_path(cluster),
        Arg::Plain(text) => git_plain_inside(text, &top, folders),
    })
}

/// A plain git argument: a revision, a path, or `rev:path`, whose path
/// part must be inside the open folders too. Git reads that path from the
/// repository's top folder, unless it starts with `./` or `../`.
fn git_plain_inside(text: &str, top: &str, folders: &dyn Folders) -> bool {
    if !folders.inside(text) {
        return false;
    }
    let Some((_, path)) = text.split_once(':') else {
        return true;
    };
    if path.is_empty() {
        return true;
    }
    if path.starts_with("./") || path.starts_with("../") {
        return folders.inside(path);
    }
    folders.inside(&format!("{}/{path}", top.trim_end_matches('/')))
}

/// Whether a `git branch` call only lists: listing flags, and names only
/// as `--list` patterns.
fn git_branch_lists(args: &[Arg]) -> bool {
    let mut listing = false;
    let mut names = false;
    for arg in args {
        match arg {
            Arg::Long(name, _) => {
                if !GIT_BRANCH_LONG.contains(name) {
                    return false;
                }
                listing |= *name == "list";
            }
            Arg::Short(cluster) => {
                if !cluster.chars().all(|c| GIT_BRANCH_SHORT.contains(c)) {
                    return false;
                }
                listing |= cluster.contains('l');
            }
            Arg::Plain(_) => names = true,
        }
    }
    listing || !names
}

/// Flags each file command may take: ones that never overwrite, follow
/// links out, or make links.
fn file_command_flags(program: &str) -> Option<(&'static [&'static str], &'static str)> {
    Some(match program {
        "mkdir" => (&["parents", "verbose"], "pv"),
        "touch" => (&["no-create"], "acm"),
        "cp" => (&["recursive", "no-clobber", "verbose"], "rRpvn"),
        "mv" => (&["no-clobber", "verbose"], "nv"),
        _ => return None,
    })
}

/// `mkdir`, `touch`, `cp` or `mv` with known flags, every path inside the
/// open folders and outside `.git`, and (for `cp`/`mv`) sources that exist
/// and a destination that overwrites nothing. `rm` and `rmdir` are never
/// here: `/rewind` couldn't bring back what they delete.
fn file_change_ok(program: &str, args: &[String], folders: &dyn Folders) -> bool {
    let Some((long_flags, short_flags)) = file_command_flags(program) else {
        return false;
    };
    let mut paths = Vec::new();
    for arg in parse_args(args) {
        match arg {
            Arg::Long(name, None) if long_flags.contains(&name) => {}
            Arg::Short(cluster) if cluster.chars().all(|c| short_flags.contains(c)) => {}
            Arg::Plain(path) if path != "-" => paths.push(path),
            _ => return false,
        }
    }
    let all_inside = paths
        .iter()
        .all(|path| folders.inside(path) && !in_protected_place(path));
    if paths.is_empty() || !all_inside {
        return false;
    }
    match program {
        "cp" | "mv" => copy_overwrites_nothing(&paths, folders),
        _ => true,
    }
}

/// A `cp`/`mv` source that names a folder's contents rather than the
/// folder: a trailing `/`, or a last step of `.` or `..`.
fn copies_contents(source: &str) -> bool {
    let last = source.rsplit('/').next().unwrap_or(source);
    source.ends_with('/') || last == "." || last == ".."
}

/// Whether `path` is git's own data or gz's project settings (rules,
/// hooks, MCP servers), which a file command never touches unasked.
fn in_protected_place(path: &str) -> bool {
    let path = Path::new(path);
    path.components().any(|part| part.as_os_str() == ".git")
        || crate::assistant::rules::holds_settings(path)
        || path.file_name().is_some_and(|name| name == ".gz")
}

/// `cp`/`mv` SOURCE… DEST: every source exists, and DEST (or, when it is
/// a folder, DEST/name for each source) does not.
fn copy_overwrites_nothing(paths: &[&str], folders: &dyn Folders) -> bool {
    let Some((dest, sources)) = paths.split_last() else {
        return false;
    };
    if sources.is_empty() || !sources.iter().all(|source| folders.exists(source)) {
        return false;
    }
    // `cp -R src/ dst` (BSD) and `cp -R src/. dst` copy src's contents
    // into dst, over files already there: only a plain name says what
    // lands where.
    if sources.iter().any(|source| copies_contents(source)) {
        return false;
    }
    if !folders.exists(dest) {
        return sources.len() == 1;
    }
    if !folders.is_dir(dest) {
        return false;
    }
    sources.iter().all(|source| {
        let name = Path::new(source.trim_end_matches('/'))
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        name.is_some_and(|name| {
            let target = format!("{}/{name}", dest.trim_end_matches('/'));
            folders.inside(&target) && !folders.exists(&target)
        })
    })
}

#[cfg(test)]
#[path = "read_only_tests.rs"]
mod tests;
