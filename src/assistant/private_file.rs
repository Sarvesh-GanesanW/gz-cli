//! Files gz keeps for the user next to its config: owner-only, and written
//! atomically so a crash never leaves one half-written.

use std::path::Path;

use anyhow::{Context, Result};

/// Writes `text` to `path` through a temp file and a rename.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    write_private_bytes(path, text.as_bytes())
}

/// [`write_private`] for content that need not be text.
pub fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("the file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut handle = options
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        handle
            .write_all(bytes)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot save {}", path.display()))
}
