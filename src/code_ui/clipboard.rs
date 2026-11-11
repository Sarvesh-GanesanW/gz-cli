//! Ctrl+V image paste: saves the clipboard's image to a temporary PNG.
//! Text paste arrives separately (bracketed paste), so pasting only handles
//! images. Copying text out (`/export clipboard`) goes through the same
//! system tools.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub trait ImageClipboard: Send {
    /// Saves the clipboard image as a PNG and returns its path; `None` when
    /// the clipboard holds no image.
    fn paste_image(&self) -> Result<Option<PathBuf>>;

    /// Puts `text` on the clipboard.
    fn copy_text(&self, text: &str) -> Result<()>;
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

    fn copy_text(&self, text: &str) -> Result<()> {
        let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
            &[("pbcopy", &[])]
        } else if cfg!(target_os = "linux") {
            &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
        } else {
            bail!("copying isn't supported on this system");
        };
        for (tool, args) in tools {
            if pipe_into(tool, args, text).unwrap_or(false) {
                return Ok(());
            }
        }
        let names: Vec<&str> = tools.iter().map(|(tool, _)| *tool).collect();
        bail!("no clipboard tool worked (tried {})", names.join(", "))
    }
}

/// Runs `tool` with `text` on its stdin; false when it ran and failed.
fn pipe_into(tool: &str, args: &[&str], text: &str) -> Result<bool> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot run {tool}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .with_context(|| format!("cannot write to {tool}"))?;
    }
    Ok(child.wait()?.success())
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

        fn copy_text(&self, _text: &str) -> Result<()> {
            Ok(())
        }
    }

    /// A clipboard that keeps what was copied, and holds no image.
    pub struct CopyingClipboard(pub std::sync::Arc<std::sync::Mutex<String>>);

    impl ImageClipboard for CopyingClipboard {
        fn paste_image(&self) -> Result<Option<PathBuf>> {
            Ok(None)
        }

        fn copy_text(&self, text: &str) -> Result<()> {
            if let Ok(mut copied) = self.0.lock() {
                *copied = text.to_string();
            }
            Ok(())
        }
    }
}
