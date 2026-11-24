//! Tab completion of plain paths in the input: the path-like word before
//! the cursor, completed against the current folder (or `~`, or an
//! absolute path), folders ending in `/`.

use std::path::{Path, PathBuf};

/// Entries read from one folder at most, so a huge folder never stalls
/// typing.
const MAX_SCANNED: usize = 5_000;

/// Characters that end a folder in a typed path: Windows also takes `\`.
#[cfg(windows)]
const SEPARATORS: &[char] = &['/', '\\'];
#[cfg(not(windows))]
const SEPARATORS: &[char] = &['/'];

/// The word being completed: where it starts in the line (bytes), its text
/// without an opening quote, and whether it had one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub start: usize,
    pub text: String,
    pub quoted: bool,
}

/// The word before the end of `line` that Tab completes: after the last
/// space, or after an unclosed `"`. `@` mentions have their own popup and
/// a leading `/command` its menu, so neither is a path here.
pub fn word_at_end(line: &str) -> Option<Word> {
    let quote = line.rfind('"').filter(|&open| {
        let before = &line[..open];
        before.matches('"').count() % 2 == 0 && (open == 0 || before.ends_with(char::is_whitespace))
    });
    let (start, text, quoted) = match quote {
        Some(open) => (open, &line[open + 1..], true),
        None => {
            let start = line.rfind(char::is_whitespace).map_or(0, |i| i + 1);
            (start, &line[start..], false)
        }
    };
    if text.starts_with('@') || (start == 0 && is_command(text)) {
        return None;
    }
    // `!command` at the start: the path starts after the `!`.
    let (start, text) = match text.strip_prefix('!') {
        Some(rest) if start == 0 && !quoted => (1, rest),
        _ => (start, text),
    };
    Some(Word {
        start,
        text: text.to_string(),
        quoted,
    })
}

/// A `/command` at the start of the input (one segment, no other `/`).
fn is_command(text: &str) -> bool {
    text.strip_prefix('/')
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

/// Where completion looks: the folder relative paths start in, the home
/// folder for `~/`, and which paths must never be listed.
pub struct Lookup<'a> {
    pub base: &'a Path,
    pub home: Option<&'a Path>,
    pub hidden: &'a dyn Fn(&Path) -> bool,
}

/// Every completion of `word`, as the word's new text: the folder part as
/// typed plus a matching name, `/` after folders. Names starting with `.`
/// are offered only when the typed name starts with `.`.
pub fn candidates(word: &str, lookup: &Lookup) -> Vec<String> {
    let (folder_part, prefix) = split_folder(word, SEPARATORS);
    let separator = folder_separator(folder_part, SEPARATORS);
    let Some(folder) = resolve(folder_part, lookup) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&folder) else {
        return Vec::new();
    };
    let mut named: Vec<(String, bool)> = entries
        .take(MAX_SCANNED)
        .filter_map(Result::ok)
        .filter(|entry| !(lookup.hidden)(&entry.path()))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.path().is_dir())
        })
        .filter(|(name, _)| prefix.starts_with('.') || !name.starts_with('.'))
        .collect();
    named.sort();
    let exact: Vec<&(String, bool)> = named
        .iter()
        .filter(|(n, _)| n.starts_with(prefix))
        .collect();
    let chosen = if exact.is_empty() {
        let lower = prefix.to_lowercase();
        named
            .iter()
            .filter(|(n, _)| n.to_lowercase().starts_with(&lower))
            .collect()
    } else {
        exact
    };
    chosen
        .into_iter()
        .map(|(name, is_dir)| {
            let mut text = format!("{folder_part}{name}");
            if *is_dir {
                text.push(separator);
            }
            text
        })
        .collect()
}

/// `word` split after its last folder separator: the folder part as typed
/// and the name being completed.
fn split_folder<'w>(word: &'w str, separators: &[char]) -> (&'w str, &'w str) {
    match word.rfind(separators) {
        Some(at) => word.split_at(at + 1),
        None => ("", word),
    }
}

/// The separator put after a completed folder: the one the user typed
/// last, `/` when they typed none.
fn folder_separator(folder_part: &str, separators: &[char]) -> char {
    folder_part
        .chars()
        .rev()
        .find(|c| separators.contains(c))
        .unwrap_or('/')
}

fn resolve(folder_part: &str, lookup: &Lookup) -> Option<PathBuf> {
    if folder_part.is_empty() {
        return Some(lookup.base.to_path_buf());
    }
    let home_relative = folder_part
        .strip_prefix('~')
        .and_then(|rest| rest.strip_prefix(SEPARATORS));
    if let Some(rest) = home_relative {
        return Some(lookup.home?.join(rest));
    }
    let path = Path::new(folder_part);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        lookup.base.join(path)
    })
}

/// The longest start every candidate shares.
pub fn shared_start(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut end = first.len();
    for other in &candidates[1..] {
        end = first
            .char_indices()
            .zip(other.chars())
            .find(|((_, a), b)| a != b)
            .map_or(end.min(other.len()), |((i, _), _)| i.min(end));
    }
    first[..end].to_string()
}

/// How a chosen completion is written into the input: quoted when it has a
/// space (closed after a file, left open after a folder so typing can go
/// on), and a file gets a space after it.
pub fn written(choice: &str, quoted: bool) -> String {
    let needs_quote = quoted || choice.contains(' ');
    let is_dir = choice.ends_with(SEPARATORS);
    match (needs_quote, is_dir) {
        (true, true) => format!("\"{choice}"),
        (true, false) => format!("\"{choice}\" "),
        (false, true) => choice.to_string(),
        (false, false) => format!("{choice} "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let dir = crate::assistant::sandbox::tests::temp_dir(&format!("complete-{name}"));
        for folder in ["src", "scripts", "my docs", ".github"] {
            std::fs::create_dir_all(dir.join(folder)).unwrap();
        }
        for file in [
            "src/main.rs",
            "src/math.rs",
            "README.md",
            ".env",
            "Setup.py",
        ] {
            std::fs::write(dir.join(file), "x").unwrap();
        }
        dir
    }

    fn lookup<'a>(base: &'a Path, hidden: &'a dyn Fn(&Path) -> bool) -> Lookup<'a> {
        Lookup {
            base,
            home: Some(base),
            hidden,
        }
    }

    #[test]
    fn backslash_folders_split_and_keep_the_typed_separator() {
        let windows = &['/', '\\'][..];
        assert_eq!(split_folder("src\\ma", windows), ("src\\", "ma"));
        assert_eq!(split_folder("C:\\Users\\", windows), ("C:\\Users\\", ""));
        assert_eq!(split_folder("a/b\\c", windows), ("a/b\\", "c"));
        assert_eq!(folder_separator("C:\\Users\\", windows), '\\');
        assert_eq!(folder_separator("src/", windows), '/');
        assert_eq!(folder_separator("", windows), '/');
        let unix = &['/'][..];
        assert_eq!(split_folder("odd\\name", unix), ("", "odd\\name"));
    }

    #[test]
    fn finds_the_word_before_the_cursor() {
        let word = |line: &str| word_at_end(line).map(|w| (w.start, w.text, w.quoted));
        assert_eq!(word("open src/ma"), Some((5, "src/ma".into(), false)));
        assert_eq!(word("see \"my do"), Some((4, "my do".into(), true)));
        assert_eq!(word("\"done\" sr"), Some((7, "sr".into(), false)));
        assert_eq!(word("look @src"), None, "mentions have their own popup");
        assert_eq!(word("/he"), None, "a command at the start");
        assert_eq!(word("/cd sr"), Some((4, "sr".into(), false)));
        assert_eq!(word("/usr/lo"), Some((0, "/usr/lo".into(), false)));
        assert_eq!(word("x "), Some((2, String::new(), false)));
        assert_eq!(word("!cat sr"), Some((5, "sr".into(), false)));
        assert_eq!(word("!sr"), Some((1, "sr".into(), false)));
    }

    #[test]
    fn completes_names_folders_and_nested_paths() {
        let dir = tree("names");
        let never = |_: &Path| false;
        let at = lookup(&dir, &never);
        assert_eq!(candidates("sr", &at), vec!["src/"]);
        assert_eq!(candidates("s", &at), vec!["scripts/", "src/"]);
        assert_eq!(candidates("src/m", &at), vec!["src/main.rs", "src/math.rs"]);
        assert_eq!(
            candidates("setup", &at),
            vec!["Setup.py"],
            "case only if needed"
        );
        assert_eq!(candidates("~/READ", &at), vec!["~/README.md"]);
        let absolute = format!("{}/src/ma", dir.display());
        assert_eq!(candidates(&absolute, &at).len(), 2);
        assert!(candidates("nope/x", &at).is_empty());
    }

    #[test]
    fn hidden_names_only_when_asked_and_secrets_never() {
        let dir = tree("hidden");
        let secret = |p: &Path| p.file_name().is_some_and(|n| n == ".env");
        let at = lookup(&dir, &secret);
        assert!(!candidates("", &at).iter().any(|c| c.starts_with('.')));
        assert_eq!(candidates(".", &at), vec![".github/"], ".env is a secret");
    }

    #[test]
    fn shared_starts_and_written_forms() {
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            shared_start(&list(&["src/main.rs", "src/math.rs"])),
            "src/ma"
        );
        assert_eq!(shared_start(&list(&["a", "b"])), "");
        assert_eq!(shared_start(&list(&["only"])), "only");
        assert_eq!(written("src/", false), "src/");
        assert_eq!(written("a.rs", false), "a.rs ");
        assert_eq!(written("my docs/", false), "\"my docs/");
        assert_eq!(written("my docs/x.md", true), "\"my docs/x.md\" ");
    }
}
