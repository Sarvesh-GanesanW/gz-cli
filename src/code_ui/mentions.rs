//! The `@` file picker: an index of workspace files, fuzzy matching as you
//! type, and recognising dragged-in image paths in pasted text.

use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

use crate::assistant::attachments::is_image;
use crate::assistant::sandbox::Sandbox;

/// Most entries indexed; very large folders still open instantly.
const MAX_INDEXED: usize = 20_000;
/// Rows shown in the popup.
pub const MAX_SHOWN: usize = 8;

/// Workspace paths, relative, with `/` after folders.
#[derive(Debug, Clone, Default)]
pub struct FileIndex {
    entries: Vec<String>,
}

impl FileIndex {
    /// Walks the workspace honoring .gitignore; secrets never appear.
    pub fn scan(sandbox: &Sandbox) -> Self {
        let mut entries: Vec<String> = WalkBuilder::new(sandbox.root())
            .hidden(false)
            .require_git(false)
            .filter_entry(|e| e.file_name() != ".git")
            .build()
            .filter_map(|e| e.ok())
            .filter(|e| e.depth() > 0 && !sandbox.is_secret(e.path()))
            .take(MAX_INDEXED)
            .map(|e| {
                let shown = sandbox.display(e.path());
                if e.file_type().is_some_and(|t| t.is_dir()) {
                    format!("{shown}/")
                } else {
                    shown
                }
            })
            .collect();
        entries.sort();
        Self { entries }
    }

    #[cfg(test)]
    pub fn from_entries(entries: &[&str]) -> Self {
        Self {
            entries: entries.iter().map(|e| e.to_string()).collect(),
        }
    }

    /// Best matches for `query`, best first. An empty query lists the
    /// shallowest entries.
    pub fn matches(&self, query: &str) -> Vec<String> {
        let query = query.to_lowercase();
        let mut scored: Vec<(i64, &String)> = self
            .entries
            .iter()
            .filter_map(|entry| score(entry, &query).map(|s| (s, entry)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored
            .into_iter()
            .take(MAX_SHOWN)
            .map(|(_, e)| e.clone())
            .collect()
    }
}

/// Fuzzy score: every query character must appear in order. Prefix and
/// file-name matches rank highest, then contiguous runs; shorter and
/// shallower paths win ties.
fn score(entry: &str, query: &str) -> Option<i64> {
    let path = entry.to_lowercase();
    let depth = path.trim_end_matches('/').matches('/').count() as i64;
    if query.is_empty() {
        return Some(-depth * 100 - path.len() as i64);
    }
    let mut last = None;
    let mut gaps = 0i64;
    let mut chars = path.char_indices();
    for wanted in query.chars() {
        let (index, _) = chars.find(|(_, c)| *c == wanted)?;
        if let Some(previous) = last {
            gaps += (index - previous - 1) as i64;
        }
        last = Some(index);
    }
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(&path);
    let tail = query.rsplit('/').next().unwrap_or(query);
    let mut points = 1_000 - gaps * 10 - path.len() as i64 - depth * 5;
    if path.starts_with(query) {
        points += 600;
    }
    if !tail.is_empty() && name.starts_with(tail) {
        points += 400;
    }
    if path.contains(query) {
        points += 300;
    }
    Some(points)
}

/// Byte offset of the `@` of the mention being typed at the end of `line`:
/// an unclosed `@"…` (which may contain spaces), else the last word when it
/// starts with `@`.
pub fn mention_start(line: &str) -> Option<usize> {
    if let Some(open) = line.rfind("@\"") {
        let at_word_start = open == 0 || line[..open].ends_with(char::is_whitespace);
        if at_word_start && !line[open + 2..].contains('"') {
            return Some(open);
        }
    }
    let start = line.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    line[start..].starts_with('@').then_some(start)
}

/// The mention being typed at the end of `line`, without the `@` and
/// opening quote.
pub fn active_query(line: &str) -> Option<String> {
    let start = mention_start(line)?;
    Some(line[start + 1..].trim_start_matches('"').to_string())
}

/// `line` with its trailing `@word` replaced by a mention of `path`
/// (quoted when it has spaces). Files get a trailing space; folders don't,
/// so you can keep typing into them.
pub fn complete(line: &str, path: &str) -> String {
    let start = mention_start(line).unwrap_or(line.len());
    let mention = if path.contains(' ') {
        format!("@\"{path}\"")
    } else {
        format!("@{path}")
    };
    let gap = if path.ends_with('/') { "" } else { " " };
    format!("{}{mention}{gap}", &line[..start])
}

/// An existing image file pasted as a path (what terminals send for a
/// dragged-in file): quotes, `file://`, and `\ ` escapes are undone.
pub fn pasted_image(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|t| t.strip_suffix('"')))
        .unwrap_or(trimmed);
    let unprefixed = unquoted.strip_prefix("file://").unwrap_or(unquoted);
    let path = PathBuf::from(unescape(unprefixed));
    (path.is_file() && is_image(&path)).then(|| absolute(&path))
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn absolute(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_rank_prefix_and_names_first() {
        let index = FileIndex::from_entries(&[
            "README.md",
            "docs/manual.md",
            "src/",
            "src/main.rs",
            "src/math.rs",
            "tests/smoke.rs",
        ]);
        let found = index.matches("src/ma");
        assert_eq!(&found[..2], &["src/main.rs", "src/math.rs"]);
        assert_eq!(index.matches("smk"), vec!["tests/smoke.rs"]);
        assert!(index.matches("zzz").is_empty());
        let top = &index.matches("")[..2];
        assert!(
            top.iter().all(|e| !e.trim_end_matches('/').contains('/')),
            "{top:?}"
        );
    }

    #[test]
    fn index_skips_ignored_and_secret_files() {
        let dir = std::env::temp_dir().join(format!("gz-index-{}", crate::cmd::new_uuid()));
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("target/out.bin"), "x").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1").unwrap();
        std::fs::write(dir.join("main.rs"), "fn main(){}").unwrap();
        let index = FileIndex::scan(&Sandbox::new(&dir).unwrap());
        let all = index.matches("");
        assert!(all.contains(&"main.rs".to_string()));
        assert!(!all.iter().any(|e| e.contains("target")), "{all:?}");
        assert!(!all.iter().any(|e| e.contains(".env")), "{all:?}");
    }

    #[test]
    fn finds_the_mention_being_typed() {
        assert_eq!(active_query("look at @src/ma").as_deref(), Some("src/ma"));
        assert_eq!(active_query("@").as_deref(), Some(""));
        assert_eq!(active_query("@\"my no").as_deref(), Some("my no"));
        assert_eq!(active_query("mail me@x.com"), None);
        assert_eq!(active_query("done @a.rs "), None);
    }

    #[test]
    fn completion_quotes_spaces_and_keeps_folders_open() {
        assert_eq!(complete("see @src/ma", "src/main.rs"), "see @src/main.rs ");
        assert_eq!(complete("@sr", "src/"), "@src/");
        assert_eq!(complete("@no", "notes v2.md"), "@\"notes v2.md\" ");
        assert_eq!(
            complete("see @\"notes v", "notes v2.md"),
            "see @\"notes v2.md\" "
        );
    }

    #[test]
    fn recognises_dragged_image_paths() {
        let dir = std::env::temp_dir().join(format!("gz paste {}", crate::cmd::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("shot 1.png");
        std::fs::write(&image, [0x89, b'P']).unwrap();
        let shown = image.display().to_string();
        let escaped = shown.replace(' ', "\\ ");
        assert!(pasted_image(&escaped).is_some(), "backslash-escaped");
        assert!(pasted_image(&format!("'{shown}'")).is_some(), "quoted");
        assert!(pasted_image(&format!("file://{escaped}")).is_some());
        assert!(pasted_image("just some text").is_none());
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        assert!(pasted_image(&dir.join("a.txt").display().to_string()).is_none());
    }
}
