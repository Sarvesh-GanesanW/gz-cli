//! The `@` file picker: an index of workspace files, fuzzy matching as you
//! type, and recognising dragged-in image paths in pasted text.

use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::assistant::attachments::is_image;
use crate::assistant::sandbox::Sandbox;

/// Most entries indexed; past this the rest of a huge folder is left out.
const MAX_INDEXED: usize = 100_000;
/// Entries a scan hands over at a time, so the popup fills while it runs.
const BATCH: usize = 1_000;
/// An index older than this is rescanned in the background when `@` is
/// typed again, so new and deleted files show up.
const REFRESH_AFTER: Duration = Duration::from_secs(10);
/// Rows shown in the popup.
pub const MAX_SHOWN: usize = 8;

/// Workspace paths, relative, with `/` after folders, and the skills an
/// `@name` can ask for.
///
/// The folder is scanned on a background thread (honoring .gitignore and
/// never listing secrets), so typing never waits: matches come from what
/// has been found so far. A later rescan replaces the entries only once it
/// is complete.
#[derive(Debug, Clone, Default)]
pub struct FileIndex {
    /// The folder scanned; the index is stale once the session moves.
    base: PathBuf,
    skills: Vec<String>,
    scan: Arc<Mutex<Scan>>,
    cache: Arc<Mutex<Option<CachedMatches>>>,
}

/// The last query's matches, kept while the entries are unchanged: the
/// popup asks several times per frame.
#[derive(Debug)]
struct CachedMatches {
    query: String,
    version: u64,
    found: Vec<String>,
}

#[derive(Debug, Default)]
struct Scan {
    entries: Vec<String>,
    /// Bumped whenever `entries` changes.
    version: u64,
    /// True while the first scan is still filling `entries`.
    filling: bool,
    /// True while a background rescan runs.
    rescanning: bool,
    finished: Option<Instant>,
}

impl FileIndex {
    /// Starts scanning the current folder in the background and returns at
    /// once.
    pub fn scan(sandbox: &Sandbox) -> Self {
        let index = Self {
            base: sandbox.cwd(),
            ..Self::default()
        };
        index.lock().filling = true;
        index.spawn_walk(sandbox.clone(), true);
        index
    }

    /// Rescans in the background when the last scan is older than
    /// [`REFRESH_AFTER`]; the current entries serve until it finishes.
    pub fn refresh(&self, sandbox: &Sandbox) {
        {
            let mut scan = self.lock();
            let fresh = scan.finished.is_none_or(|at| at.elapsed() < REFRESH_AFTER);
            if scan.filling || scan.rescanning || fresh {
                return;
            }
            scan.rescanning = true;
        }
        self.spawn_walk(sandbox.clone(), false);
    }

    /// True until the first scan has finished.
    pub fn is_filling(&self) -> bool {
        self.lock().filling
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Scan> {
        // A panicked scan thread leaves whatever it found; that is still a
        // usable index.
        self.scan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Walks `base` on a thread. The first scan (`filling`) hands entries
    /// over in batches; a rescan swaps them in when complete.
    fn spawn_walk(&self, sandbox: Sandbox, filling: bool) {
        let base = self.base.clone();
        let scan = self.scan.clone();
        std::thread::spawn(move || {
            let mut found: Vec<String> = Vec::new();
            for entry in walk(&base, &sandbox) {
                found.push(entry);
                if filling && found.len() == BATCH {
                    add(&scan, std::mem::take(&mut found));
                }
            }
            if filling {
                add(&scan, found);
                finish(&scan, None);
            } else {
                found.sort();
                finish(&scan, Some(found));
            }
        });
    }

    /// Adds skill names, offered first when they match.
    pub fn with_skills(mut self, names: &[String]) -> Self {
        self.skills = names.to_vec();
        // Cached matches were ranked against the old skills.
        self.cache = Arc::default();
        self
    }

    /// The folder this index lists.
    pub fn base(&self) -> &Path {
        &self.base
    }

    #[cfg(test)]
    pub fn from_entries(entries: &[&str]) -> Self {
        let index = Self::default();
        let list: Vec<String> = entries.iter().map(|e| e.to_string()).collect();
        finish(&index.scan, Some(list));
        index
    }

    /// An index whose first scan hasn't found anything yet (tests only).
    #[cfg(test)]
    pub fn still_filling() -> Self {
        let index = Self::default();
        index.lock().filling = true;
        index
    }

    /// Waits for the scan running now (tests only).
    #[cfg(test)]
    pub fn wait(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while {
            let scan = self.lock();
            scan.filling || scan.rescanning
        } {
            assert!(Instant::now() < deadline, "the file index never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Best matches for `query`, best first, from the entries found so
    /// far. An empty query lists the shallowest entries.
    pub fn matches(&self, query: &str) -> Vec<String> {
        let query = query.to_lowercase();
        let scan = self.lock();
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = cache.as_ref() {
            if cached.query == query && cached.version == scan.version {
                return cached.found.clone();
            }
        }
        // A skill outranks a file with the same score, so `@pdf` finds the
        // pdf skill before pdf/ folders.
        let skills = self
            .skills
            .iter()
            .filter_map(|name| score(name, &query).map(|s| (s + 1, name)));
        let mut scored: Vec<(i64, &String)> = scan
            .entries
            .iter()
            .filter_map(|entry| score(entry, &query).map(|s| (s, entry)))
            .chain(skills)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let found: Vec<String> = scored
            .into_iter()
            .take(MAX_SHOWN)
            .map(|(_, e)| e.clone())
            .collect();
        *cache = Some(CachedMatches {
            query,
            version: scan.version,
            found: found.clone(),
        });
        found
    }
}

/// Adds a batch of a first scan's entries.
fn add(scan: &Mutex<Scan>, batch: Vec<String>) {
    let mut scan = scan.lock().unwrap_or_else(|p| p.into_inner());
    scan.entries.extend(batch);
    scan.version += 1;
}

/// Ends a scan: a rescan's complete `entries` replace the old ones; a
/// first scan's entries are sorted in place.
fn finish(scan: &Mutex<Scan>, entries: Option<Vec<String>>) {
    let mut scan = scan.lock().unwrap_or_else(|p| p.into_inner());
    match entries {
        Some(entries) => scan.entries = entries,
        None => scan.entries.sort(),
    }
    scan.version += 1;
    scan.filling = false;
    scan.rescanning = false;
    scan.finished = Some(Instant::now());
}

/// `base`'s entries, relative with `/` separators and `/` after folders,
/// honoring .gitignore and leaving out secrets and `.git`.
fn walk<'a>(base: &'a Path, sandbox: &'a Sandbox) -> impl Iterator<Item = String> + 'a {
    WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .filter_map(|e| e.ok())
        .filter(move |e| e.depth() > 0 && !sandbox.is_secret(e.path()))
        .take(MAX_INDEXED)
        .filter_map(move |e| {
            let relative = e.path().strip_prefix(base).ok()?;
            let shown: Vec<String> = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let shown = shown.join("/");
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            Some(if is_dir { format!("{shown}/") } else { shown })
        })
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
    fn entries_found_later_show_up_in_the_next_match() {
        let index = FileIndex::from_entries(&["src/main.rs"]);
        assert_eq!(index.matches("lib"), Vec::<String>::new());
        add(&index.scan, vec!["src/lib.rs".into()]);
        assert_eq!(
            index.matches("lib"),
            vec!["src/lib.rs"],
            "cache follows the version"
        );
    }

    #[test]
    fn skills_that_arrive_later_join_cached_matches() {
        let index = FileIndex::from_entries(&["src/main.rs"]);
        assert!(index.matches("pd").is_empty());
        let index = index.with_skills(&["pdf".to_string()]);
        assert_eq!(index.matches("pd"), vec!["pdf"]);
    }

    #[test]
    fn an_old_index_is_refreshed_in_the_background() {
        let dir = std::env::temp_dir().join(format!("gz-refresh-{}", crate::cmd::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.rs"), "x").unwrap();
        let sandbox = Sandbox::new(&dir).unwrap();
        let index = FileIndex::scan(&sandbox);
        index.wait();
        std::fs::write(dir.join("new.rs"), "x").unwrap();
        index.refresh(&sandbox);
        index.wait();
        assert!(index.matches("new").is_empty(), "a fresh index is kept");
        index.lock().finished = Some(Instant::now() - REFRESH_AFTER);
        index.refresh(&sandbox);
        assert_eq!(
            index.matches("old"),
            vec!["old.rs"],
            "old entries serve meanwhile"
        );
        index.wait();
        assert_eq!(index.matches("new"), vec!["new.rs"]);
    }

    #[test]
    fn skills_are_offered_alongside_files() {
        let index = FileIndex::from_entries(&["docs/pdf-notes.md", "src/main.rs"])
            .with_skills(&["pdf-tools".to_string()]);
        assert_eq!(index.matches("pdf")[0], "pdf-tools");
        assert!(index
            .matches("pdf")
            .contains(&"docs/pdf-notes.md".to_string()));
        assert!(!index.matches("main").contains(&"pdf-tools".to_string()));
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
        index.wait();
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
