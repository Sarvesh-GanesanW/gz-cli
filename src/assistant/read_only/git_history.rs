//! Which read-only git calls could print file contents from history: a
//! commit other than the working tree and the index. A secret committed
//! once and deleted later is still in the repository's objects, where the
//! OS sandbox can't hide it by name and the index check
//! ([`super::git_index`]) no longer sees it, so these calls ask.
//!
//! What stays automatic: `git status` (not `-v`), `git log` without patch
//! or pickaxe options, `git show` of `HEAD` (or `HEAD~n`, `HEAD^`) with
//! the patch turned off, `git diff` of the working tree against the index,
//! `git diff` between revisions with names or counts only (`--stat`,
//! `--name-only`, …), and `git blame` of one file in the working tree.

use super::{long_matches, Folders};

/// `log`, `show`, `diff` and `blame` options that print a patch, a
/// combined diff or a function's lines, or search contents (`-S`/`-G` and
/// friends work as an oracle on a deleted file's text), or run the
/// signature program. Abbreviations count, as git accepts them.
const CONTENT_LONG: &[&str] = &[
    "patch",
    "patch-with-stat",
    "patch-with-raw",
    "unified",
    "binary",
    "cc",
    "remerge-diff",
    "diff-merges",
    "dd",
    "full-diff",
    "word-diff",
    "word-diff-regex",
    "color-words",
    "color-moved",
    "function-context",
    "check",
    "pickaxe-all",
    "pickaxe-regex",
    "find-object",
    "show-signature",
];
/// Short options with the same effect (`-p`, `-u`, `-U<n>`, `-L<range>`,
/// `-G<re>`, `-S<text>`, `-W`, `-c`, `-m`). Any of these characters in a
/// cluster asks, even where it is part of a value: asking is the safe side.
const CONTENT_SHORT: &str = "puULGSWcm";
/// Options that print only names, counts or ids, never contents.
const NAMES_ONLY_LONG: &[&str] = &[
    "stat",
    "numstat",
    "shortstat",
    "name-only",
    "name-status",
    "dirstat",
    "summary",
    "compact-summary",
    "raw",
    "quiet",
    "no-patch",
];
/// `-s` (`--no-patch`) turns the patch off.
const NAMES_ONLY_SHORT: char = 's';
/// `git diff` options that compare against a commit rather than the
/// working tree and the index.
const DIFF_AGAINST_COMMIT_LONG: &[&str] = &["cached", "staged", "merge-base"];
/// `--format`/`--pretty` placeholders that run the signature program.
const SIGNATURE_PLACEHOLDER: &str = "%G";

/// One argument as these checks see it; unlike [`super::Arg`] it keeps
/// whether it came after `--` (then it is always a path).
enum Word<'a> {
    Long(&'a str, Option<&'a str>),
    Short(&'a str),
    Plain { text: &'a str, after_dashes: bool },
}

fn words(args: &[String]) -> Vec<Word<'_>> {
    let mut words = Vec::new();
    let mut after_dashes = false;
    for arg in args {
        let arg = arg.as_str();
        if after_dashes || arg == "-" || !arg.starts_with('-') {
            words.push(Word::Plain {
                text: arg,
                after_dashes,
            });
        } else if arg == "--" {
            after_dashes = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            words.push(Word::Long(name, value));
        } else {
            words.push(Word::Short(&arg[1..]));
        }
    }
    words
}

/// Whether `git <subcommand> <args>` may print contents from a commit
/// other than the working tree and the index (or run the signature
/// program), so it must ask. `subcommand` is one of the read-only ones.
pub(super) fn may_print_history(subcommand: &str, args: &[String], folders: &dyn Folders) -> bool {
    let words = words(args);
    let prints = || words.iter().any(prints_contents);
    match subcommand {
        "status" => words.iter().any(is_verbose),
        "log" => prints(),
        "show" => prints() || !show_stays_on_head(&words),
        "diff" => prints() || diff_reaches_history(&words, folders),
        "blame" => prints() || !blames_one_working_file(&words, folders),
        // `rev-parse`, `ls-files` and `branch` print names and ids only.
        _ => false,
    }
}

fn prints_contents(word: &Word) -> bool {
    match word {
        Word::Long(name, value) => {
            long_matches(name, CONTENT_LONG)
                || (long_matches(name, &["format", "pretty"])
                    && value.is_some_and(|v| v.contains(SIGNATURE_PLACEHOLDER)))
        }
        Word::Short(cluster) => cluster.contains(|c| CONTENT_SHORT.contains(c)),
        Word::Plain { .. } => false,
    }
}

/// `git status -v` prints the staged diff, with lines from `HEAD`.
fn is_verbose(word: &Word) -> bool {
    match word {
        Word::Long(name, _) => long_matches(name, &["verbose"]),
        Word::Short(cluster) => cluster.contains('v'),
        Word::Plain { .. } => false,
    }
}

fn names_only(words: &[Word]) -> bool {
    words.iter().any(|word| match word {
        Word::Long(name, _) => NAMES_ONLY_LONG.contains(name),
        Word::Short(cluster) => cluster.contains(NAMES_ONLY_SHORT),
        Word::Plain { .. } => false,
    })
}

/// `git show` options that print only a commit's header, names or counts
/// and pick no revision. Matched exactly: flags like `--all`, `--tags` or
/// `--glob=` select refs, and a ref may point at a blob, which `git show`
/// prints whatever `-s` or `--stat` say, so anything unlisted asks.
const SHOW_HEADER_LONG: &[&str] = &[
    "oneline",
    "format",
    "pretty",
    "abbrev-commit",
    "no-abbrev-commit",
    "abbrev",
    "no-abbrev",
    "decorate",
    "no-decorate",
    "date",
    "relative-date",
    "no-color",
];

/// `git show` stays automatic only for `HEAD`-style revisions (or none)
/// with the patch off and every flag a header or names-only one.
fn show_stays_on_head(words: &[Word]) -> bool {
    let flags_ok = words.iter().all(|word| match word {
        Word::Long(name, _) => NAMES_ONLY_LONG.contains(name) || SHOW_HEADER_LONG.contains(name),
        Word::Short(cluster) => cluster.chars().all(|c| c == NAMES_ONLY_SHORT),
        Word::Plain { .. } => true,
    });
    flags_ok && names_only(words) && words.iter().all(is_head_revision)
}

/// `git blame` makes no "revision or file?" check, so a folder named like
/// a tag (`git blame v1 a.txt`) would blame that revision. It stays
/// automatic only for exactly one existing regular file: one plain word,
/// or one word after `--` and none before it.
fn blames_one_working_file(words: &[Word], folders: &dyn Folders) -> bool {
    let plain: Vec<_> = words
        .iter()
        .filter_map(|word| match word {
            Word::Plain { text, .. } => Some(*text),
            Word::Long(..) | Word::Short(_) => None,
        })
        .collect();
    let [path] = plain.as_slice() else {
        return false;
    };
    !path.contains(':') && folders.exists(path) && !folders.is_dir(path)
}

/// For `git show`: flags, or a revision that is surely a commit (`HEAD`,
/// `@`, with `~n`/`^n` steps). Any other name could be a blob or a tag
/// pointing at one, which `git show` prints whatever the flags say.
fn is_head_revision(word: &Word) -> bool {
    let Word::Plain { text, after_dashes } = word else {
        return true;
    };
    if *after_dashes {
        return true;
    }
    let steps = text.strip_prefix("HEAD").or_else(|| text.strip_prefix('@'));
    steps.is_some_and(|steps| {
        steps
            .chars()
            .all(|c| c == '~' || c == '^' || c.is_ascii_digit())
    })
}

/// `git diff` compares the working tree with the index unless it names a
/// commit (`--cached`, a revision, `rev:path`); with one it asks unless it
/// prints names or counts only.
fn diff_reaches_history(words: &[Word], folders: &dyn Folders) -> bool {
    let no_index = words
        .iter()
        .any(|w| matches!(w, Word::Long(name, _) if *name == "no-index"));
    if no_index {
        return false;
    }
    let names_commit = words.iter().any(|word| match word {
        Word::Long(name, _) => long_matches(name, DIFF_AGAINST_COMMIT_LONG),
        Word::Plain { .. } => !is_existing_path(word, folders),
        Word::Short(_) => false,
    });
    names_commit && !names_only(words)
}

/// A path git reads from the working tree: after `--`, or an existing
/// file or folder (not `rev:path`). Flags pass.
fn is_existing_path(word: &Word, folders: &dyn Folders) -> bool {
    match word {
        Word::Plain { after_dashes, .. } if *after_dashes => true,
        Word::Plain { text, .. } => !text.contains(':') && folders.exists(text),
        Word::Long(..) | Word::Short(_) => true,
    }
}
