//! `@path` imports inside instruction files: `@docs/style.md`,
//! `@./notes.md`, `@~/shared/rules.md` or an absolute path pull another
//! file's text into the instruction entry that mentions it. Paths are
//! relative to the importing file; imports nest at most [`MAX_DEPTH`]
//! deep, a file that imports itself (directly or through others) is
//! skipped, and `@…` inside code spans or fenced code blocks is left alone.
//!
//! A project's file may only import files inside the open folders or
//! below the importing file's own folder, so a cloned repository can't
//! pull private files from elsewhere into the chat; the user's own files
//! (`instructions.md`, `GZ.local.md`) may import any non-secret file.
//! Neither may import a file a deny or ask read rule covers: that would
//! send it to the model without the prompt the rule calls for.

use std::path::{Path, PathBuf};

use crate::assistant::sandbox::Sandbox;

/// How deep imports may nest (an import of an import is depth 2).
pub const MAX_DEPTH: usize = 5;
/// Punctuation that ends a sentence rather than a path (`see @a.md.`).
const TRAILING_PUNCTUATION: &[char] = &['.', ',', ';', ':', ')', ']', '}', '!', '?', '"', '\''];

/// Who may be imported from a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The user's own file: any non-secret file.
    Anywhere,
    /// A project's file: open folders and below its own folder only.
    Confined,
}

/// One imported file, in the order it appears (depth first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub path: PathBuf,
    pub text: String,
    pub depth: usize,
}

/// An `@path` that was not imported, and why, for `/instructions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub mention: String,
    pub why: String,
}

/// Everything `file` imports, and every mention that was refused.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Imports {
    pub files: Vec<Imported>,
    pub skipped: Vec<Skipped>,
}

/// Reads a file's text for an import: `None` for a missing, binary or
/// secret file (the caller decides which).
pub type ReadText<'a> = &'a dyn Fn(&Path) -> Option<String>;

/// The deny or ask read rule that covers a file, as written; `None` when
/// no rule stands in the way.
pub type ReadRule<'a> = &'a dyn Fn(&Path) -> Option<String>;

/// Follows the imports of `file` (whose text is `text`).
pub fn collect(
    sandbox: &Sandbox,
    file: &Path,
    text: &str,
    reach: Reach,
    read: ReadText<'_>,
    read_rule: ReadRule<'_>,
) -> Imports {
    let mut found = Imports::default();
    let real = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let fence = real.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut walk = Walk {
        sandbox,
        reach,
        fence,
        read,
        read_rule,
        stack: vec![real.clone()],
        seen: vec![real],
        found: &mut found,
    };
    walk.follow(file, text, 1);
    found
}

struct Walk<'a, 'b> {
    sandbox: &'a Sandbox,
    reach: Reach,
    /// The top file's folder: confined imports stay below it (or in an
    /// open folder).
    fence: PathBuf,
    read: ReadText<'b>,
    read_rule: ReadRule<'b>,
    /// The files being expanded right now, to spot cycles.
    stack: Vec<PathBuf>,
    /// Every file already included, so a file imported twice goes once.
    seen: Vec<PathBuf>,
    found: &'a mut Imports,
}

impl Walk<'_, '_> {
    fn follow(&mut self, file: &Path, text: &str, depth: usize) {
        let folder = file.parent().unwrap_or(Path::new("/")).to_path_buf();
        for mention in mentions(text) {
            self.import(&folder, &mention, depth);
        }
    }

    fn import(&mut self, folder: &Path, mention: &str, depth: usize) {
        let skip = |why: &str| Skipped {
            mention: format!("@{mention}"),
            why: why.to_string(),
        };
        let Some(target) = self.locate(folder, mention) else {
            if looks_like_a_path(mention) {
                self.found.skipped.push(skip("not found"));
            }
            return;
        };
        if self.stack.contains(&target) {
            self.found.skipped.push(skip(
                "it imports itself through other files, so gz skips the loop",
            ));
            return;
        }
        if self.seen.contains(&target) {
            return;
        }
        if depth > MAX_DEPTH {
            self.found.skipped.push(skip(&format!(
                "imports nest more than {MAX_DEPTH} deep; move it up a level"
            )));
            return;
        }
        if self.sandbox.is_secret(&target) {
            self.found
                .skipped
                .push(skip("it is a secret file, which gz never sends"));
            return;
        }
        if !self.may_reach(&target) {
            self.found.skipped.push(skip(
                "a project file may only import files in the open folders or below its own folder",
            ));
            return;
        }
        if let Some(rule) = (self.read_rule)(&target) {
            self.found.skipped.push(skip(&format!(
                "the rule `{rule}` covers it, so gz doesn't send it unasked"
            )));
            return;
        }
        let Some(text) = (self.read)(&target) else {
            self.found.skipped.push(skip("it can't be read as text"));
            return;
        };
        self.seen.push(target.clone());
        self.found.files.push(Imported {
            path: target.clone(),
            text: text.clone(),
            depth,
        });
        self.stack.push(target.clone());
        self.follow(&target, &text, depth + 1);
        self.stack.pop();
    }

    /// The existing file `mention` names, canonical.
    fn locate(&self, folder: &Path, mention: &str) -> Option<PathBuf> {
        let path = if let Some(rest) = mention.strip_prefix("~/") {
            self.sandbox.home()?.join(rest)
        } else if Path::new(mention).is_absolute() {
            PathBuf::from(mention)
        } else {
            folder.join(mention)
        };
        let real = path.canonicalize().ok()?;
        real.is_file().then_some(real)
    }

    fn may_reach(&self, target: &Path) -> bool {
        match self.reach {
            Reach::Anywhere => true,
            Reach::Confined => {
                target.starts_with(&self.fence)
                    || self
                        .sandbox
                        .roots()
                        .iter()
                        .any(|root| target.starts_with(root))
            }
        }
    }
}

/// Whether a mention that resolved to nothing was meant as a path (worth
/// reporting), rather than an `@name` in prose.
fn looks_like_a_path(mention: &str) -> bool {
    mention.starts_with("./")
        || mention.starts_with("../")
        || mention.starts_with("~/")
        || mention.starts_with('/')
        || mention.ends_with(".md")
}

/// The `@path` mentions in `text`, in order, outside code. A mention
/// starts a word (`me@example.com` is not one) and holds a `/` or a `.`.
pub fn mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    for line in text.lines() {
        if let Some(marker) = fence_marker(line) {
            match &fence {
                None => fence = Some(marker),
                Some(open) if marker.starts_with(open.as_str()) => fence = None,
                Some(_) => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        out.extend(outside_code_spans(line).into_iter().flat_map(word_mentions));
    }
    out
}

/// The run of backticks or tildes that opens or closes a fenced block
/// (indented at most three spaces).
fn fence_marker(line: &str) -> Option<String> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let first = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run: String = rest.chars().take_while(|c| *c == first).collect();
    (run.len() >= 3).then_some(run)
}

/// The parts of `line` outside inline code spans (a span opens and closes
/// with the same number of backticks).
fn outside_code_spans(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        parts.push(&rest[..start]);
        let ticks = rest[start..].chars().take_while(|c| *c == '`').count();
        let after = &rest[start + ticks..];
        let closing = "`".repeat(ticks);
        match after.find(&closing) {
            Some(end) => rest = &after[end + ticks..],
            None => {
                // An unclosed run is plain text.
                rest = after;
            }
        }
    }
    parts.push(rest);
    parts
}

fn word_mentions(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|word| {
            word.trim_start_matches(['(', '[', '"', '\''])
                .strip_prefix('@')
        })
        .map(|path| path.trim_end_matches(TRAILING_PUNCTUATION))
        .filter(|path| !path.is_empty() && (path.contains('/') || path.contains('.')))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn no_rule(_: &Path) -> Option<String> {
        None
    }

    fn names(imports: &Imports, home: &Path) -> Vec<String> {
        imports
            .files
            .iter()
            .map(|f| f.path.strip_prefix(home).unwrap().display().to_string())
            .collect()
    }

    #[test]
    fn mentions_skip_code_emails_and_trailing_punctuation() {
        let text = "See @docs/style.md, and @./a.md.\n\
                    mail me@example.com or @someone\n\
                    `@in/span.md` but @after/span.md\n\
                    ```\n@in/fence.md\n```\n\
                    ~~~~\n@tilde/fence.md\n~~~~\n\
                    (@~/shared/x.md)";
        assert_eq!(
            mentions(text),
            ["docs/style.md", "./a.md", "after/span.md", "~/shared/x.md"]
        );
    }

    #[test]
    fn imports_nest_relative_to_each_file_once_each_and_stop_at_cycles() {
        let home = temp_dir("imports-nest");
        std::fs::create_dir_all(home.join("app/docs/more")).unwrap();
        std::fs::write(
            home.join("app/docs/a.md"),
            "A then @more/b.md and @../GZ.md",
        )
        .unwrap();
        std::fs::write(home.join("app/docs/more/b.md"), "B then @../a.md").unwrap();
        std::fs::write(home.join("app/GZ.md"), "top @docs/a.md @docs/a.md").unwrap();
        let sandbox = Sandbox::with_home(&home.join("app"), Some(&home)).unwrap();
        let top = home.join("app/GZ.md");
        let text = read(&top).unwrap();
        let found = collect(&sandbox, &top, &text, Reach::Confined, &read, &no_rule);
        assert_eq!(
            names(&found, &home),
            ["app/docs/a.md", "app/docs/more/b.md"]
        );
        assert_eq!(found.files[1].depth, 2);
        assert_eq!(found.skipped.len(), 2, "{:?}", found.skipped);
        assert!(found
            .skipped
            .iter()
            .all(|s| s.why.contains("imports itself")));
    }

    #[test]
    fn depth_is_limited_and_missing_paths_are_reported() {
        let home = temp_dir("imports-depth");
        for n in 0..=7 {
            std::fs::write(home.join(format!("f{n}.md")), format!("@f{}.md", n + 1)).unwrap();
        }
        let sandbox = Sandbox::with_home(&home, Some(&home)).unwrap();
        let top = home.join("f0.md");
        let found = collect(
            &sandbox,
            &top,
            "@f1.md @./gone.md @types/node",
            Reach::Anywhere,
            &read,
            &no_rule,
        );
        assert_eq!(found.files.len(), MAX_DEPTH);
        let whys: Vec<_> = found.skipped.iter().map(|s| s.mention.as_str()).collect();
        assert_eq!(
            whys,
            ["@f6.md", "@./gone.md"],
            "prose-like @names stay quiet"
        );
    }

    #[test]
    fn project_files_stay_inside_their_folder_and_secrets_never_go() {
        let home = temp_dir("imports-reach");
        std::fs::create_dir_all(home.join("repo")).unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join("notes/private.md"), "private").unwrap();
        std::fs::write(home.join(".ssh/id_rsa"), "KEY").unwrap();
        let sandbox = Sandbox::with_home(&home.join("repo"), Some(&home)).unwrap();
        let top = home.join("repo/GZ.md");
        let text = "@~/notes/private.md @~/.ssh/id_rsa";
        let confined = collect(&sandbox, &top, text, Reach::Confined, &read, &no_rule);
        assert!(confined.files.is_empty());
        assert!(confined.skipped[0].why.contains("open folders"));
        assert!(confined.skipped[1].why.contains("secret"));
        let personal = collect(&sandbox, &top, text, Reach::Anywhere, &read, &no_rule);
        assert_eq!(names(&personal, &home), ["notes/private.md"]);
        assert!(personal.skipped[0].why.contains("secret"));
    }

    #[test]
    fn a_file_a_read_rule_covers_is_never_imported() {
        let home = temp_dir("imports-rule");
        std::fs::create_dir_all(home.join("repo/secrets")).unwrap();
        std::fs::write(home.join("repo/secrets/prod.txt"), "TOPSECRET").unwrap();
        std::fs::write(home.join("repo/style.md"), "tabs").unwrap();
        let sandbox = Sandbox::with_home(&home.join("repo"), Some(&home)).unwrap();
        let top = home.join("repo/GZ.md");
        let text = "@secrets/prod.txt @style.md";
        let denied = |path: &Path| {
            path.to_string_lossy()
                .contains("/secrets/")
                .then(|| "read(secrets/**)".to_string())
        };
        for reach in [Reach::Confined, Reach::Anywhere] {
            let found = collect(&sandbox, &top, text, reach, &read, &denied);
            assert_eq!(names(&found, &home), ["repo/style.md"]);
            assert_eq!(found.skipped.len(), 1, "{:?}", found.skipped);
            assert!(found.skipped[0].why.contains("`read(secrets/**)`"));
        }
    }
}
