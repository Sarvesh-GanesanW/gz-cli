//! `search_files`, on ripgrep's own engine (the `grep-searcher`,
//! `grep-regex` and `ignore` crates ripgrep is built from): Rust regex
//! syntax, `.gitignore` respected, ripgrep's
//! file types, binary files skipped, and three output modes — the files
//! that match (newest first), the matching lines with context, or counts.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use globset::GlobMatcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::types::{Types, TypesBuilder};
use ignore::WalkBuilder;
use serde_json::Value;

use crate::assistant::sandbox::Sandbox;

/// Entries one result shows unless the model asks for another page size.
const DEFAULT_HEAD_LIMIT: usize = 250;
const MAX_HEAD_LIMIT: usize = 2000;
const MAX_CONTEXT_LINES: usize = 20;
const MAX_LINE_CHARS: usize = 300;
/// Larger files (logs, dumps) are skipped; searching them would stall the turn.
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// Files a files-with-matches search collects before sorting them.
const MAX_MATCHING_FILES: usize = 100_000;

/// What a search returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Paths of matching files, newest first.
    FilesWithMatches,
    /// Matching lines as `path:line: text`, with any context lines.
    Content,
    /// Matching lines per file, then the total.
    Count,
}

impl OutputMode {
    fn parse(name: Option<&str>) -> Result<Self> {
        Ok(match name {
            None | Some("files_with_matches") => Self::FilesWithMatches,
            Some("content") => Self::Content,
            Some("count") => Self::Count,
            Some(other) => {
                bail!("unknown output_mode '{other}' (files_with_matches, content, or count)")
            }
        })
    }
}

/// A validated `search_files` call.
#[derive(Debug, Clone)]
pub struct Search {
    pattern: String,
    matcher: RegexMatcher,
    /// A folder to walk, or one file (searched even when ignored).
    base: PathBuf,
    glob: Option<GlobMatcher>,
    /// Whether the glob has a `/`; without one it also matches file names.
    glob_has_slash: bool,
    types: Option<Types>,
    mode: OutputMode,
    multiline: bool,
    before: usize,
    after: usize,
    head_limit: usize,
    offset: usize,
}

/// One line a search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub path: String,
    pub number: u64,
    pub text: String,
    /// False for a context line around a match.
    pub is_match: bool,
}

fn count(input: &Value, key: &str) -> Option<usize> {
    input[key].as_u64().map(|n| n as usize)
}

impl Search {
    /// Validates the input: the regex compiles, the type is one ripgrep
    /// knows, and `path` is inside the folder.
    pub fn parse(sandbox: &Sandbox, input: &Value) -> Result<Self> {
        let pattern = input["pattern"]
            .as_str()
            .filter(|p| !p.is_empty())
            .context("missing string field 'pattern'")?
            .to_string();
        let multiline = input["multiline"].as_bool().unwrap_or(false);
        let matcher = build_matcher(
            &pattern,
            input["case_insensitive"].as_bool().unwrap_or(false),
            multiline,
        )?;
        let base = sandbox.resolve(input["path"].as_str().unwrap_or("."))?;
        if !base.exists() {
            bail!("{} does not exist", sandbox.display(&base));
        }
        let glob_text = input["glob"].as_str().filter(|g| !g.is_empty());
        let glob = glob_text
            .map(|g| {
                globset::Glob::new(g)
                    .with_context(|| format!("invalid glob '{g}'"))
                    .map(|g| g.compile_matcher())
            })
            .transpose()?;
        let types = input["type"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(file_types)
            .transpose()?;
        let context = count(input, "context").unwrap_or(0);
        let around = |key: &str| count(input, key).unwrap_or(context).min(MAX_CONTEXT_LINES);
        Ok(Self {
            pattern,
            matcher,
            base,
            glob,
            glob_has_slash: glob_text.is_some_and(|g| g.contains('/')),
            types,
            mode: OutputMode::parse(input["output_mode"].as_str())?,
            multiline,
            before: around("before_context"),
            after: around("after_context"),
            head_limit: count(input, "head_limit")
                .unwrap_or(DEFAULT_HEAD_LIMIT)
                .clamp(1, MAX_HEAD_LIMIT),
            offset: count(input, "offset").unwrap_or(0),
        })
    }

    /// The folder (or file) searched, which a read permission covers.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// `/pattern/ in src`, for prompts and the transcript.
    pub fn summary(&self, sandbox: &Sandbox) -> String {
        format!("/{}/ in {}", self.pattern, sandbox.display(&self.base))
    }

    /// The tool result: one page of entries and how to get the next.
    pub fn run(&self, sandbox: &Sandbox) -> String {
        match self.mode {
            OutputMode::FilesWithMatches => self.files_with_matches(sandbox),
            OutputMode::Content => self.content(sandbox),
            OutputMode::Count => self.counts(sandbox),
        }
    }

    /// Matching lines for a script helper, without context: up to `max`,
    /// and whether more were left.
    pub fn matching_lines(&self, sandbox: &Sandbox, max: usize) -> (Vec<Line>, bool) {
        let mut found = Vec::new();
        for path in self.files(sandbox) {
            let Some(lines) = self.search_file(sandbox, &path, 0, 0, None) else {
                continue;
            };
            for line in lines.into_iter().filter(|l| l.is_match) {
                if found.len() == max {
                    return (found, true);
                }
                found.push(line);
            }
        }
        (found, false)
    }

    fn files_with_matches(&self, sandbox: &Sandbox) -> String {
        let mut found: Vec<(SystemTime, String)> = Vec::new();
        for path in self.files(sandbox) {
            if found.len() == MAX_MATCHING_FILES {
                break;
            }
            if self
                .search_file(sandbox, &path, 0, 0, Some(1))
                .is_some_and(|l| !l.is_empty())
            {
                let modified = path
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                found.push((modified, sandbox.display(&path)));
            }
        }
        // Newest first: recent files are usually the ones in play.
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let rows: Vec<String> = found.into_iter().map(|(_, path)| path).collect();
        self.page(rows, "no files match", "files")
    }

    fn content(&self, sandbox: &Sandbox) -> String {
        let wanted = self.offset + self.head_limit;
        let mut rows = Vec::new();
        let mut more = false;
        for path in self.files(sandbox) {
            let Some(lines) = self.search_file(sandbox, &path, self.before, self.after, None)
            else {
                continue;
            };
            if lines.is_empty() {
                continue;
            }
            if self.before + self.after > 0 && !rows.is_empty() {
                rows.push("--".to_string());
            }
            rows.extend(lines.iter().map(content_row));
            if rows.len() > wanted {
                // Enough for this page: stop reading files.
                rows.truncate(wanted);
                more = true;
                break;
            }
        }
        let mut out = self.page(rows, "no matches", "lines");
        if more {
            out.push_str(&format!(
                "\n… more lines; use offset {wanted} for the next page"
            ));
        }
        out
    }

    fn counts(&self, sandbox: &Sandbox) -> String {
        let mut total = 0;
        let mut rows = Vec::new();
        for path in self.files(sandbox) {
            let Some(lines) = self.search_file(sandbox, &path, 0, 0, None) else {
                continue;
            };
            let n = lines.iter().filter(|l| l.is_match).count();
            if n > 0 {
                total += n;
                rows.push(format!("{}: {n}", sandbox.display(&path)));
            }
        }
        if rows.is_empty() {
            return "(no matches)".to_string();
        }
        // The total covers every file, even past the page.
        let files = rows.len();
        let mut out = self.page(rows, "no matches", "files");
        out.push_str(&format!("\ntotal: {total} in {files} files"));
        out
    }

    /// Entries `offset..offset+head_limit` and a note on what's left.
    fn page(&self, rows: Vec<String>, empty: &str, noun: &str) -> String {
        if rows.is_empty() {
            return format!("({empty})");
        }
        if self.offset >= rows.len() {
            return format!(
                "(no entries at offset {}; there are {})",
                self.offset,
                rows.len()
            );
        }
        let total = rows.len();
        let end = (self.offset + self.head_limit).min(total);
        let mut out = rows[self.offset..end].join("\n");
        if end < total {
            out.push_str(&format!(
                "\n… {} more {noun}; use offset {end} for the next page",
                total - end
            ));
        }
        out
    }

    /// The files to search: `base` itself when it is a file, else the
    /// folder's files that pass .gitignore, the type, and the glob.
    fn files<'a>(&'a self, sandbox: &'a Sandbox) -> Box<dyn Iterator<Item = PathBuf> + 'a> {
        if self.base.is_file() {
            return Box::new(std::iter::once(self.base.clone()));
        }
        let mut walk = WalkBuilder::new(&self.base);
        walk.hidden(false)
            .require_git(false)
            .sort_by_file_path(|a, b| a.cmp(b))
            .filter_entry(|entry| entry.file_name() != ".git");
        if let Some(types) = &self.types {
            walk.types(types.clone());
        }
        Box::new(
            walk.build()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
                .map(|entry| entry.into_path())
                .filter(move |path| !sandbox.is_secret(path) && self.glob_allows(sandbox, path)),
        )
    }

    fn glob_allows(&self, sandbox: &Sandbox, path: &Path) -> bool {
        let Some(glob) = &self.glob else {
            return true;
        };
        let by_name = !self.glob_has_slash && path.file_name().is_some_and(|n| glob.is_match(n));
        by_name || glob.is_match(sandbox.display(path))
    }

    /// The lines one file yields, or `None` for a file that can't be
    /// searched (too large, binary, unreadable).
    fn search_file(
        &self,
        sandbox: &Sandbox,
        path: &Path,
        before: usize,
        after: usize,
        max_matches: Option<usize>,
    ) -> Option<Vec<Line>> {
        if path.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            return None;
        }
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .multi_line(self.multiline)
            .before_context(before)
            .after_context(after)
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .build();
        let mut sink = Collect {
            path: sandbox.display(path),
            lines: Vec::new(),
            matches: 0,
            max_matches,
            binary: false,
        };
        searcher.search_path(&self.matcher, path, &mut sink).ok()?;
        (!sink.binary).then_some(sink.lines)
    }
}

fn build_matcher(pattern: &str, case_insensitive: bool, multiline: bool) -> Result<RegexMatcher> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(case_insensitive);
    if multiline {
        builder.multi_line(true).dot_matches_new_line(true);
    } else {
        // Line by line, like ripgrep without -U: a match never spans lines.
        builder.line_terminator(Some(b'\n'));
    }
    builder.build(pattern).with_context(|| {
        format!("invalid regex '{pattern}' (ripgrep syntax: escape {{ }} ( ) etc.)")
    })
}

/// ripgrep's built-in file types (`rust`, `py`, `ts`, …) narrowed to `name`.
fn file_types(name: &str) -> Result<Types> {
    TypesBuilder::new()
        .add_defaults()
        .select(name)
        .build()
        .with_context(|| format!("unknown file type '{name}' (ripgrep types such as rust, py, js)"))
}

fn content_row(line: &Line) -> String {
    let separator = if line.is_match { ':' } else { '-' };
    format!(
        "{}{separator}{}{separator} {}",
        line.path, line.number, line.text
    )
}

/// Collects one file's lines for a search.
struct Collect {
    path: String,
    lines: Vec<Line>,
    matches: usize,
    max_matches: Option<usize>,
    binary: bool,
}

impl Collect {
    /// Adds the lines of `bytes` (one line, or several for a multiline
    /// match), numbered from `first_line`.
    fn push(&mut self, first_line: Option<u64>, bytes: &[u8], is_match: bool) {
        let first = first_line.unwrap_or(0);
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
            let text = String::from_utf8_lossy(line);
            self.lines.push(Line {
                path: self.path.clone(),
                number: first + i as u64,
                text: text.trim_end().chars().take(MAX_LINE_CHARS).collect(),
                is_match,
            });
        }
    }
}

impl Sink for Collect {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, found: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.push(found.line_number(), found.bytes(), true);
        self.matches += 1;
        Ok(self.max_matches.is_none_or(|max| self.matches < max))
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> Result<bool, io::Error> {
        if *context.kind() != SinkContextKind::Other {
            self.push(context.line_number(), context.bytes(), false);
        }
        Ok(true)
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_workspace;
    use serde_json::json;

    fn search(sandbox: &Sandbox, input: Value) -> String {
        Search::parse(sandbox, &input).unwrap().run(sandbox)
    }

    fn workspace(name: &str) -> (PathBuf, Sandbox) {
        let (dir, sandbox) = temp_workspace(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {\n    todo!();\n}\n").unwrap();
        std::fs::write(dir.join("src/b.py"), "# TODO later\nprint('x')\n").unwrap();
        std::fs::write(dir.join("notes.md"), "nothing here\n").unwrap();
        std::fs::write(dir.join("blob.bin"), b"todo\0binary").unwrap();
        (dir, sandbox)
    }

    #[test]
    fn lists_matching_files_by_default_and_skips_binaries() {
        let (_dir, sandbox) = workspace("search-files");
        let out = search(
            &sandbox,
            json!({"pattern": "todo", "case_insensitive": true}),
        );
        let mut files: Vec<&str> = out.lines().collect();
        files.sort();
        assert_eq!(files, vec!["src/a.rs", "src/b.py"]);
    }

    #[test]
    fn content_shows_lines_with_context_and_types_narrow() {
        let (_dir, sandbox) = workspace("search-content");
        let out = search(
            &sandbox,
            json!({"pattern": "todo!", "output_mode": "content", "type": "rust", "context": 1}),
        );
        assert_eq!(
            out,
            "src/a.rs-1- fn main() {\nsrc/a.rs:2:     todo!();\nsrc/a.rs-3- }"
        );
    }

    #[test]
    fn counts_per_file_with_a_total_and_pages_with_offset() {
        let (dir, sandbox) = workspace("search-count");
        std::fs::write(dir.join("src/c.rs"), "x\nx\nx\n").unwrap();
        let out = search(&sandbox, json!({"pattern": "^x$", "output_mode": "count"}));
        assert_eq!(out, "src/c.rs: 3\ntotal: 3 in 1 files");
        let page = search(
            &sandbox,
            json!({"pattern": "^x$", "output_mode": "content", "head_limit": 2}),
        );
        assert!(page.ends_with("use offset 2 for the next page"), "{page}");
        let next = search(
            &sandbox,
            json!({"pattern": "^x$", "output_mode": "content", "head_limit": 2, "offset": 2}),
        );
        assert_eq!(next, "src/c.rs:3: x");
    }

    #[test]
    fn multiline_matches_across_lines_and_globs_filter() {
        let (_dir, sandbox) = workspace("search-multi");
        let out = search(
            &sandbox,
            json!({"pattern": "main\\(\\) \\{\\s+todo", "multiline": true, "glob": "*.rs"}),
        );
        assert_eq!(out, "src/a.rs");
        let none = search(&sandbox, json!({"pattern": "main\\(\\) \\{\\s+todo"}));
        assert_eq!(none, "(no files match)");
    }

    #[test]
    fn bad_input_says_what_to_fix() {
        let (_dir, sandbox) = workspace("search-bad");
        let err = |input: Value| format!("{:#}", Search::parse(&sandbox, &input).unwrap_err());
        assert!(err(json!({"pattern": "("})).contains("invalid regex"));
        assert!(err(json!({"pattern": "x", "type": "klingon"})).contains("unknown file type"));
        assert!(err(json!({"pattern": "x", "output_mode": "all"})).contains("output_mode"));
        assert!(err(json!({"pattern": "x", "path": "missing"})).contains("does not exist"));
    }

    #[test]
    fn script_helpers_get_matching_lines_only() {
        let (_dir, sandbox) = workspace("search-helper");
        let spec = Search::parse(&sandbox, &json!({"pattern": "print", "context": 3})).unwrap();
        let (lines, more) = spec.matching_lines(&sandbox, 10);
        assert!(!more);
        assert_eq!(
            lines,
            vec![Line {
                path: "src/b.py".into(),
                number: 2,
                text: "print('x')".into(),
                is_match: true
            }]
        );
    }
}
