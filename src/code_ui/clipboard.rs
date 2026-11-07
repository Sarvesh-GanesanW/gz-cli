//! Ctrl+V image paste: saves the clipboard's image to a temporary PNG.
//! Text paste arrives separately (bracketed paste), so this only handles
//! images.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub trait ImageClipboard: Send {
    /// Saves the clipboard image as a PNG and returns its path; `None` when
    /// the clipboard holds no image.
    fn paste_image(&self) -> Result<Option<PathBuf>>;
}

/// The operating system clipboard: `osascript` on macOS, `wl-paste` or
/// `xclip` on Linux.
pub struct SystemClipboard;

fn temp_png() -> PathBuf {
    std::env::temp_dir().join(format!("gz-paste-{}.png", crate::cmd::new_uuid()))
}

impl ImageClipboard for SystemClipboard {
    fn paste_image(&self) -> Result<Option<PathBuf>> {
        let target = temp_png();
        let saved = if cfg!(target_os = "macos") {
            save_with_osascript(&target)?
        } else if cfg!(target_os = "linux") {
            save_with_linux_tool(&target)?
        } else {
            bail!("pasting images isn't supported on this system; drag the file in or use @path");
        };
        Ok(saved.then_some(target))
    }
}

/// AppleScript writes the clipboard's PNG data to `target`; it fails (and
/// we report no image) when the clipboard holds none.
fn save_with_osascript(target: &std::path::Path) -> Result<bool> {
    let file = target.display().to_string();
    let status = Command::new("osascript")
        .args([
            "-e",
            "set png to (the clipboard as «class PNGf»)",
            "-e",
            &format!("set out to open for access POSIX file \"{file}\" with write permission"),
            "-e",
            "write png to out",
            "-e",
            "close access out",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("cannot run osascript to read the clipboard")?;
    Ok(status.success() && target.metadata().is_ok_and(|m| m.len() > 0))
}

fn save_with_linux_tool(target: &std::path::Path) -> Result<bool> {
    let attempts: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    for (tool, args) in attempts {
        let Ok(output) = Command::new(tool).args(args).output() else {
            continue;
        };
        if output.status.success() && !output.stdout.is_empty() {
            std::fs::write(target, &output.stdout)
                .with_context(|| format!("cannot save {}", target.display()))?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
pub mod fake {
    use super::*;

    /// A clipboard that returns a fixed answer.
    pub struct FakeClipboard(pub Option<PathBuf>);

    impl ImageClipboard for FakeClipboard {
        fn paste_image(&self) -> Result<Option<PathBuf>> {
            Ok(self.0.clone())
        }
    }
}
