//! The `load_skill` client tool: the chat asks for a skill by name and gets
//! its `SKILL.md` body plus a list of its supporting files, or one of those
//! files by relative path. Read-only and confined to the skill's folder
//! (no `..`, no symlinks out, no secrets, no binaries), so it needs no
//! approval — but it only serves skills that are switched on and verified.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Component, Path};

use crate::assistant::sandbox::secret_reason;
use crate::assistant::tools::{looks_binary, ToolOutput};
use crate::skills::files::{human_size, MAX_FILE_BYTES};
use crate::skills::manifest::SKILL_FILE;
use crate::skills::{Catalog, Home};

/// The chat server's own limit on a `path` argument.
const MAX_PATH_CHARS: usize = 512;
/// Most supporting files listed with a skill.
const MAX_LISTED_FILES: usize = 200;

/// Runs one `load_skill` call against the skills on disk right now.
pub fn run(home: &Home, input: &Value) -> ToolOutput {
    match load(home, input) {
        Ok(text) => ToolOutput::ok(text),
        Err(err) => ToolOutput::error(format!("{err:#}")),
    }
}

fn load(home: &Home, input: &Value) -> Result<String> {
    let name = input["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .context("load_skill needs a skill `name`")?;
    let catalog = Catalog::load(home)?;
    let skill = catalog.usable(name)?;
    match input["path"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        None => Ok(overview(
            &skill.dir,
            skill.manifest.as_ref().map_or("", |m| &m.body),
        )),
        Some(path) => supporting_file(&skill.dir, path),
    }
}

/// The body plus every file the skill brings, for the model to ask for.
fn overview(dir: &Path, body: &str) -> String {
    let mut files = Vec::new();
    list_files(dir, dir, &mut files);
    files.retain(|(rel, _)| rel != SKILL_FILE);
    files.sort();
    let mut out = body.trim_end().to_string();
    if files.is_empty() {
        return out;
    }
    out.push_str("\n\nSupporting files (read one with load_skill and its `path`):\n");
    let total = files.len();
    for (rel, size) in files.into_iter().take(MAX_LISTED_FILES) {
        out.push_str(&format!("- {rel} ({})\n", human_size(size)));
    }
    if total > MAX_LISTED_FILES {
        out.push_str(&format!("… {} more files\n", total - MAX_LISTED_FILES));
    }
    out
}

fn list_files(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let inside = path.strip_prefix(root).unwrap_or(&path);
        if kind.is_symlink() || secret_reason(inside).is_some() {
            continue;
        }
        if kind.is_dir() && entry.file_name() != ".git" {
            list_files(root, &path, out);
        } else if kind.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push((rel.to_string_lossy().replace('\\', "/"), size));
        }
    }
}

/// One text file inside the skill folder.
fn supporting_file(dir: &Path, requested: &str) -> Result<String> {
    let has_parent = requested.split(['/', '\\']).any(|part| part == "..");
    if has_parent || requested.chars().any(char::is_control) {
        bail!("'{requested}' must be a path inside the skill, without '..'");
    }
    if requested.chars().count() > MAX_PATH_CHARS {
        bail!("the path is longer than {MAX_PATH_CHARS} characters");
    }
    let rel = Path::new(requested);
    let relative = rel
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !relative {
        bail!("'{requested}' is outside the skill folder; give a path relative to it");
    }
    if let Some(reason) = secret_reason(rel) {
        bail!("'{requested}' is blocked: {reason}");
    }
    let root = dir.canonicalize().context("cannot open the skill folder")?;
    let path = root.join(rel);
    if !path.exists() {
        bail!("{requested} is not a file in this skill");
    }
    // Resolves symlinks anywhere on the way, so a link out is caught here.
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot open {requested}"))?;
    if !path.starts_with(&root) {
        bail!("'{requested}' is outside the skill folder");
    }
    if !path.is_file() {
        bail!("{requested} is not a file in this skill");
    }
    let size = path.metadata().map(|m| m.len()).unwrap_or(0);
    if size > MAX_FILE_BYTES {
        bail!(
            "{requested} is {}; skills serve files up to {}",
            human_size(size),
            human_size(MAX_FILE_BYTES)
        );
    }
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {requested}"))?;
    if looks_binary(&bytes) {
        bail!("{requested} is a binary file");
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::test_support::{home, write_skill};
    use serde_json::json;

    fn setup() -> Home {
        let home = home("load");
        let dir = write_skill(
            &home.skills_dir(),
            "pdf",
            "PDF forms",
            "# PDF\nRun scripts/fill.py with the form.\n",
        );
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/fill.py"), "print('fill')\n").unwrap();
        std::fs::write(dir.join(".env"), "TOKEN=secret").unwrap();
        std::fs::write(dir.join("logo.png"), [0u8, 1, 2]).unwrap();
        std::fs::write(home.skills_dir().join("outside.txt"), "nope").unwrap();
        home
    }

    #[test]
    fn returns_the_body_without_frontmatter_and_lists_supporting_files() {
        let home = setup();
        let out = run(&home, &json!({"name": "pdf"}));
        assert!(!out.is_error, "{}", out.output);
        assert!(out.output.starts_with("# PDF\nRun scripts/fill.py"));
        assert!(!out.output.contains("description:"));
        assert!(out.output.contains("- scripts/fill.py (14 B)"));
        assert!(!out.output.contains(".env"), "secrets aren't listed");
    }

    #[test]
    fn reads_supporting_text_files_but_nothing_outside_secret_or_binary() {
        let home = setup();
        let read = |path: &str| run(&home, &json!({"name": "pdf", "path": path}));
        assert_eq!(read("scripts/fill.py").output, "print('fill')\n");
        for (path, expected) in [
            ("../outside.txt", "without '..'"),
            ("scripts/../../outside.txt", "without '..'"),
            (".env", "blocked"),
            ("logo.png", "binary"),
            ("missing.md", "not a file"),
            ("a\u{7}b", "without '..'"),
        ] {
            let out = read(path);
            assert!(
                out.is_error && out.output.contains(expected),
                "{path}: {}",
                out.output
            );
        }
        let absolute = home.skills_dir().join("outside.txt");
        let out = read(&absolute.display().to_string());
        assert!(
            out.is_error && out.output.contains("outside the skill folder"),
            "{}",
            out.output
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_skill_are_refused() {
        let home = setup();
        std::os::unix::fs::symlink(
            home.skills_dir().join("outside.txt"),
            home.skills_dir().join("pdf/escape.txt"),
        )
        .unwrap();
        let out = run(&home, &json!({"name": "pdf", "path": "escape.txt"}));
        assert!(
            out.is_error && out.output.contains("outside the skill folder"),
            "{}",
            out.output
        );
    }

    #[test]
    fn unknown_disabled_and_tampered_skills_name_the_available_ones() {
        let home = setup();
        write_skill(&home.skills_dir(), "xlsx", "Sheets", "");
        let out = run(&home, &json!({"name": "nope"}));
        assert!(
            out.is_error && out.output.contains("Available skills: pdf, xlsx"),
            "{}",
            out.output
        );
        crate::skills::set_enabled(&home, "xlsx", false).unwrap();
        let out = run(&home, &json!({"name": "xlsx"}));
        assert!(
            out.is_error && out.output.contains("switched off"),
            "{}",
            out.output
        );
        assert!(run(&home, &json!({})).is_error);
    }
}
