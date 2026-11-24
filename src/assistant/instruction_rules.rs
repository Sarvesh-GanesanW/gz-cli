//! Path-scoped rule files: `.gz/rules/*.md` (subfolders too) in a project
//! folder. A rule whose front matter has `paths:` applies once the model
//! reads or edits a file matching one of its globs (relative to the folder
//! holding `.gz`); a rule without `paths:` applies like the folder's
//! `GZ.md`. The front matter is not sent, only the body.
//!
//! ```markdown
//! ---
//! paths: ["src/api/**/*.rs", "*.sql"]
//! ---
//! Handlers return typed errors.
//! ```
//!
//! A glob without a `/` matches the file name in any folder (`*.sql`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Where rules live inside a project folder.
pub const RULES_DIR: [&str; 2] = [".gz", "rules"];
/// How deep gz looks inside the rules folder.
const MAX_RULE_DEPTH: usize = 4;
/// Rule files read from one folder at most.
const MAX_RULES_PER_FOLDER: usize = 64;

/// One rule file, parsed.
#[derive(Debug, Clone)]
pub struct Rule {
    pub file: PathBuf,
    /// The folder holding `.gz`; globs are relative to it.
    pub project: PathBuf,
    /// The globs as written; empty means "always".
    pub paths: Vec<String>,
    matcher: Option<GlobSet>,
    /// The text after the front matter.
    pub body: String,
}

impl Rule {
    /// Whether this rule applies to every file (no `paths:`).
    pub fn is_always(&self) -> bool {
        self.paths.is_empty()
    }

    /// Whether `file` (absolute) matches one of the rule's globs.
    pub fn matches(&self, file: &Path) -> bool {
        let (Some(matcher), Ok(relative)) = (&self.matcher, file.strip_prefix(&self.project))
        else {
            return false;
        };
        matcher.is_match(relative)
    }
}

/// A rule file that could not be used, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRule {
    pub file: PathBuf,
    pub why: String,
}

/// The rule files under `project/.gz/rules`, in path order: the parsed
/// ones and the ones gz had to skip.
pub fn in_folder(
    project: &Path,
    read: &dyn Fn(&Path) -> Option<String>,
) -> (Vec<Rule>, Vec<BadRule>) {
    let mut dir = project.to_path_buf();
    dir.extend(RULES_DIR);
    let mut files = Vec::new();
    markdown_files(&dir, 0, &mut files);
    files.sort();
    files.truncate(MAX_RULES_PER_FOLDER);
    let mut rules = Vec::new();
    let mut bad = Vec::new();
    for file in files {
        let Some(text) = read(&file) else {
            continue;
        };
        match parse(project, &file, &text) {
            Ok(rule) => rules.push(rule),
            Err(err) => bad.push(BadRule {
                file,
                why: format!("{err:#}"),
            }),
        }
    }
    (rules, bad)
}

fn markdown_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && depth < MAX_RULE_DEPTH {
            markdown_files(&path, depth + 1, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
        {
            out.push(path);
        }
    }
}

/// Parses one rule file's text.
pub fn parse(project: &Path, file: &Path, text: &str) -> Result<Rule> {
    let (front, body) = split_front_matter(text)?;
    let paths = match front {
        Some(front) => paths_of(front)?,
        None => Vec::new(),
    };
    let matcher = if paths.is_empty() {
        None
    } else {
        Some(glob_set(&paths)?)
    };
    Ok(Rule {
        file: file.to_path_buf(),
        project: project.to_path_buf(),
        paths,
        matcher,
        body: body.to_string(),
    })
}

/// `(front matter, body)`; no front matter when the file doesn't start
/// with `---`.
fn split_front_matter(text: &str) -> Result<(Option<&str>, &str)> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok((None, text));
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let body = &rest[offset + line.len()..];
            return Ok((Some(&rest[..offset]), body.trim_start_matches(['\r', '\n'])));
        }
        offset += line.len();
    }
    bail!("the front matter is not closed with a `---` line")
}

/// `paths:` as a list, or one string of comma-separated globs.
fn paths_of(front: &str) -> Result<Vec<String>> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(front).context("the front matter is not valid YAML")?;
    let paths = match value.get("paths") {
        None | Some(serde_yaml::Value::Null) => return Ok(Vec::new()),
        Some(serde_yaml::Value::String(list)) => split_globs(list),
        Some(serde_yaml::Value::Sequence(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|s| s.trim().to_string())
                    .context("every `paths:` entry must be a text glob")
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => bail!("`paths:` must be a glob or a list of globs"),
    };
    Ok(paths.into_iter().filter(|p| !p.is_empty()).collect())
}

/// One `paths:` string split at its top-level commas: a comma inside
/// `{…}` (alternation, `*.{ts,tsx}`) or `[…]` (a class) is part of a glob.
fn split_globs(list: &str) -> Vec<String> {
    let mut globs = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in list.chars() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                globs.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    globs.push(current.trim().to_string());
    globs
}

fn glob_set(paths: &[String]) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for pattern in paths {
        let pattern = pattern.trim_start_matches("./").trim_start_matches('/');
        let anchored = if pattern.contains('/') {
            pattern.to_string()
        } else {
            format!("**/{pattern}")
        };
        let glob = GlobBuilder::new(&anchored)
            .literal_separator(true)
            .build()
            .with_context(|| format!("`{pattern}` is not a valid glob"))?;
        set.add(glob);
    }
    set.build().context("the `paths:` globs can't be combined")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::sandbox::tests::temp_dir;

    #[test]
    fn rules_scope_by_globs_relative_to_the_project() {
        let project = Path::new("/p");
        let rule = parse(
            project,
            Path::new("/p/.gz/rules/api.md"),
            "---\npaths: [\"src/api/**/*.rs\", \"*.sql\"]\n---\n\nTyped errors.",
        )
        .unwrap();
        assert_eq!(rule.body, "Typed errors.");
        assert!(!rule.is_always());
        assert!(rule.matches(Path::new("/p/src/api/v1/users.rs")));
        assert!(
            rule.matches(Path::new("/p/db/migrations/1.sql")),
            "bare names match anywhere"
        );
        assert!(!rule.matches(Path::new("/p/src/main.rs")));
        assert!(!rule.matches(Path::new("/elsewhere/src/api/x.rs")));
        let comma = parse(
            project,
            Path::new("/p/r.md"),
            "---\npaths: src/*.rs, docs/**\n---\nx",
        )
        .unwrap();
        assert!(comma.matches(Path::new("/p/docs/a/b.md")));
        assert!(
            !comma.matches(Path::new("/p/src/a/b.rs")),
            "* stays inside one folder"
        );
    }

    #[test]
    fn a_comma_string_keeps_brace_alternation_and_classes_whole() {
        let rule = parse(
            Path::new("/p"),
            Path::new("/p/r.md"),
            "---\npaths: \"src/**/*.{ts,tsx}, lib/[ab,]*.js\"\n---\nx",
        )
        .unwrap();
        assert_eq!(rule.paths, ["src/**/*.{ts,tsx}", "lib/[ab,]*.js"]);
        assert!(rule.matches(Path::new("/p/src/a.ts")));
        assert!(rule.matches(Path::new("/p/src/ui/b.tsx")));
        assert!(rule.matches(Path::new("/p/lib/a1.js")));
        assert!(!rule.matches(Path::new("/p/src/c.js")));
    }

    #[test]
    fn a_rule_without_paths_always_applies_and_bad_front_matter_is_reported() {
        let project = Path::new("/p");
        let plain = parse(project, Path::new("/p/r.md"), "Be brief.").unwrap();
        assert!(plain.is_always() && plain.body == "Be brief.");
        let other = parse(project, Path::new("/p/r.md"), "---\ntitle: x\n---\nBody").unwrap();
        assert!(other.is_always());
        assert!(parse(project, Path::new("/p/r.md"), "---\npaths: [\n---\n").is_err());
        assert!(parse(project, Path::new("/p/r.md"), "---\npaths: 3\n---\n").is_err());
        assert!(parse(project, Path::new("/p/r.md"), "---\npaths: x\n").is_err());
        assert!(parse(project, Path::new("/p/r.md"), "---\npaths: [\"a[\"]\n---\n").is_err());
    }

    #[test]
    fn rule_files_are_found_in_order_including_subfolders() {
        let dir = temp_dir("rules-find");
        let rules = dir.join(".gz/rules");
        std::fs::create_dir_all(rules.join("lang")).unwrap();
        std::fs::write(rules.join("b.md"), "B").unwrap();
        std::fs::write(rules.join("a.md"), "A").unwrap();
        std::fs::write(rules.join("lang/rust.md"), "---\npaths: \"*.rs\"\n---\nR").unwrap();
        std::fs::write(rules.join("notes.txt"), "not a rule").unwrap();
        std::fs::write(rules.join("bad.md"), "---\npaths: [\n---\n").unwrap();
        let read = |p: &Path| std::fs::read_to_string(p).ok();
        let (found, bad) = in_folder(&dir, &read);
        let bodies: Vec<_> = found.iter().map(|r| r.body.as_str()).collect();
        assert_eq!(bodies, ["A", "B", "R"]);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].why.contains("YAML"), "{}", bad[0].why);
    }
}
