//! The paths a git repository tracks, read from its index file, so the
//! read-only check can tell whether `git show` or `git diff` might print a
//! committed secret file (which the OS sandbox can't hide, since git reads
//! it from its own objects). Read directly instead of running git, which
//! could start programs the repository's config names.

use std::path::{Path, PathBuf};

use crate::assistant::sandbox::secret_reason;

const SIGNATURE: &[u8] = b"DIRC";
/// ctime, mtime, dev, ino, mode, uid, gid, size: ten 32-bit fields.
const STAT_BYTES: usize = 40;
const SHA1_BYTES: usize = 20;
const SHA256_BYTES: usize = 32;
const FLAG_EXTENDED: u16 = 0x4000;

/// Whether the repository at `top` tracks a secret file (`.env`, keys,
/// credential stores). Also true when the index can't be read, so the
/// caller asks rather than guesses; a repository with no index yet tracks
/// nothing.
pub fn tracks_secrets(top: &Path) -> bool {
    let Some(git_dir) = git_dir(top) else {
        return true;
    };
    let index = match std::fs::read(git_dir.join("index")) {
        Ok(index) => index,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    match tracked_paths(&index, hash_bytes(&git_dir)) {
        Some(paths) => paths
            .iter()
            .any(|path| secret_reason(Path::new(path)).is_some()),
        None => true,
    }
}

/// The repository's git folder: `.git` itself, or where a `.git` file
/// (a worktree or submodule) points.
fn git_dir(top: &Path) -> Option<PathBuf> {
    let dot_git = top.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.strip_prefix("gitdir:")?.trim();
    Some(top.join(target))
}

/// The object id length: SHA-256 repositories say so in their config.
fn hash_bytes(git_dir: &Path) -> usize {
    let config = std::fs::read_to_string(git_dir.join("config")).unwrap_or_default();
    let sha256 = config.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        line.starts_with("objectformat") && line.ends_with("sha256")
    });
    if sha256 {
        SHA256_BYTES
    } else {
        SHA1_BYTES
    }
}

/// The paths in a git index (versions 2 to 4), or `None` when it doesn't
/// read as one.
fn tracked_paths(index: &[u8], hash: usize) -> Option<Vec<String>> {
    let mut reader = Reader {
        bytes: index,
        at: 0,
    };
    if reader.take(4)? != SIGNATURE {
        return None;
    }
    let version = reader.u32()?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = reader.u32()?;
    let mut paths = Vec::new();
    let mut previous: Vec<u8> = Vec::new();
    for _ in 0..count {
        let start = reader.at;
        reader.take(STAT_BYTES + hash)?;
        let flags = reader.u16()?;
        if version >= 3 && flags & FLAG_EXTENDED != 0 {
            reader.take(2)?;
        }
        let path = if version == 4 {
            let strip = reader.varint()?;
            let keep = previous.len().checked_sub(strip)?;
            let mut path = previous[..keep].to_vec();
            path.extend_from_slice(reader.until_nul()?);
            path
        } else {
            let path = reader.until_nul()?.to_vec();
            // Entries are padded with NULs to a multiple of eight bytes;
            // the path's own NUL is the first of them.
            let used = reader.at - start;
            reader.take((8 - used % 8) % 8)?;
            path
        };
        paths.push(String::from_utf8_lossy(&path).into_owned());
        previous = path;
    }
    Some(paths)
}

/// A cursor over the index bytes; every read is bounds-checked.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    /// The bytes up to a NUL, skipping the NUL.
    fn until_nul(&mut self) -> Option<&'a [u8]> {
        let rest = self.bytes.get(self.at..)?;
        let len = rest.iter().position(|&b| b == 0)?;
        let text = self.take(len)?;
        self.take(1)?;
        Some(text)
    }

    /// Git's offset varint: seven bits a byte, adding one per extra byte.
    fn varint(&mut self) -> Option<usize> {
        let mut byte = self.take(1)?[0];
        let mut value = usize::from(byte & 0x7f);
        while byte & 0x80 != 0 {
            byte = self.take(1)?[0];
            value = value.checked_add(1)?.checked_mul(128)? | usize::from(byte & 0x7f);
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A repository at `dir` (isolated from the user's git config) with
    /// `files` staged, its index written by real git in `version`.
    fn repo_with(dir: &Path, files: &[&str], version: &str) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        };
        git(&["init", "-q"]);
        for file in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let mut add = vec!["add", "--"];
        add.extend_from_slice(files);
        git(&add);
        git(&["update-index", "--index-version", version]);
    }

    #[test]
    fn reads_every_index_version_git_writes() {
        if !has_git() {
            eprintln!("skipped: git not installed");
            return;
        }
        let files = [
            "README.md",
            "src/a_long_folder_name/main.rs",
            "src/a_long_folder_name/mod.rs",
            "src/b.rs",
        ];
        for version in ["2", "3", "4"] {
            let dir = crate::assistant::sandbox::tests::temp_dir(&format!("git-index-{version}"));
            repo_with(&dir, &files, version);
            let index = std::fs::read(dir.join(".git/index")).unwrap();
            let paths = tracked_paths(&index, SHA1_BYTES).unwrap();
            assert_eq!(paths, files, "version {version}");
            assert!(!tracks_secrets(&dir), "version {version}");
        }
    }

    #[test]
    fn a_tracked_secret_file_is_found() {
        if !has_git() {
            eprintln!("skipped: git not installed");
            return;
        }
        for version in ["2", "4"] {
            let dir = crate::assistant::sandbox::tests::temp_dir(&format!("git-secret-{version}"));
            repo_with(&dir, &["app/.env.production", "app/main.py"], version);
            assert!(tracks_secrets(&dir), "version {version}");
        }
    }

    #[test]
    fn no_index_tracks_nothing_and_a_damaged_one_counts_as_secret() {
        let dir = crate::assistant::sandbox::tests::temp_dir("git-no-index");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(!tracks_secrets(&dir));
        std::fs::write(dir.join(".git/index"), b"DIRC\0\0\0\x02\0\0\0\x05short").unwrap();
        assert!(tracks_secrets(&dir));
        assert!(tracks_secrets(&dir.join("not-a-repo")));
    }

    #[test]
    fn a_git_file_points_at_the_real_git_folder() {
        let dir = crate::assistant::sandbox::tests::temp_dir("git-file");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: real\n").unwrap();
        assert_eq!(git_dir(&dir), Some(dir.join("real")));
    }
}
