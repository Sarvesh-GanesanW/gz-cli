//! Running the user's editor (`$VISUAL`, then `$EDITOR`, else `vi`) from
//! inside `gz code`: on a file (`/instructions edit`) or on the message
//! being typed (Ctrl+G). The terminal is handed over for the editor's run
//! and taken back afterwards.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The editor to run: its program and any arguments (`code -w`). The value
/// is split on whitespace and never run through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl EditorCommand {
    /// `$VISUAL`, then `$EDITOR`, ignoring empty values; `vi` (`notepad` on
    /// Windows) when neither is set.
    pub fn from_env() -> Self {
        Self::choose(
            std::env::var("VISUAL").ok().as_deref(),
            std::env::var("EDITOR").ok().as_deref(),
        )
    }

    fn choose(visual: Option<&str>, editor: Option<&str>) -> Self {
        let value = [visual, editor]
            .into_iter()
            .flatten()
            .find(|value| !value.trim().is_empty())
            .unwrap_or(if cfg!(windows) { "notepad" } else { "vi" });
        let mut words = value.split_whitespace().map(str::to_string);
        Self {
            program: words.next().unwrap_or_default(),
            args: words.collect(),
        }
    }

    /// Runs the editor on `path` and waits for it; an editor that can't
    /// start or exits with an error is reported with what to do.
    pub async fn run(&self, path: &Path) -> Result<()> {
        let status = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .arg(path)
            .status()
            .await
            .with_context(|| {
                format!(
                    "cannot start the editor '{}'; set $VISUAL or $EDITOR to an editor on your PATH",
                    self.program
                )
            })?;
        if !status.success() {
            bail!(
                "the editor '{}' exited with {status}; nothing was changed",
                self.program
            );
        }
        Ok(())
    }
}

/// Opens `path` in the user's editor (creating it if missing) with the
/// terminal handed over.
pub async fn edit_file(path: &Path) -> Result<()> {
    if !path.exists() {
        crate::assistant::private_file::write_private(path, "")?;
    }
    with_terminal_handed_over(EditorCommand::from_env().run(path)).await
}

/// Lets the user edit `text` in their editor; returns the edited text.
pub async fn edit_text(text: &str) -> Result<String> {
    edit_text_with(&EditorCommand::from_env(), text, true).await
}

/// [`edit_text`] with a chosen editor; `hand_over` is false in tests, which
/// have no terminal to hand over.
async fn edit_text_with(editor: &EditorCommand, text: &str, hand_over: bool) -> Result<String> {
    let draft = DraftFile::create(text)?;
    let run = editor.run(&draft.path);
    if hand_over {
        with_terminal_handed_over(run).await?;
    } else {
        run.await?;
    }
    let edited = std::fs::read_to_string(&draft.path).with_context(|| {
        format!(
            "cannot read the edited message from {}",
            draft.path.display()
        )
    })?;
    Ok(from_editor(&edited))
}

/// Runs `work` with the terminal in its normal state, then takes it back
/// even when `work` failed.
async fn with_terminal_handed_over(
    work: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    crate::inline_term::hand_over();
    let result = work.await;
    crate::inline_term::take_back()?;
    result
}

/// What the editor saved, as the input's text: Windows line endings
/// become `\n` and the final newline editors add is dropped.
fn from_editor(saved: &str) -> String {
    let text = saved.replace("\r\n", "\n");
    text.strip_suffix('\n').unwrap_or(&text).to_string()
}

/// The message being edited, in an owner-only file removed afterwards.
struct DraftFile {
    path: PathBuf,
}

impl DraftFile {
    /// Writes `text` ending in a newline, as editors save files, so what
    /// the editor adds starts on its own line.
    fn create(text: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("gz-message-{}.md", crate::cmd::new_uuid()));
        let saved = if text.is_empty() || text.ends_with('\n') {
            text.to_string()
        } else {
            format!("{text}\n")
        };
        crate::assistant::private_file::write_private(&path, &saved)
            .context("cannot create a file for the editor; check your temp folder")?;
        Ok(Self { path })
    }
}

impl Drop for DraftFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_wins_then_editor_then_vi() {
        let pick = |v, e| EditorCommand::choose(v, e);
        assert_eq!(pick(Some("code -w"), Some("nano")).program, "code");
        assert_eq!(pick(Some("code -w"), None).args, vec!["-w"]);
        assert_eq!(pick(Some("  "), Some("nano")).program, "nano");
        let fallback = if cfg!(windows) { "notepad" } else { "vi" };
        assert_eq!(pick(None, None).program, fallback);
    }

    #[test]
    fn the_final_newline_and_crlf_are_undone() {
        assert_eq!(from_editor("one\r\ntwo\n"), "one\ntwo");
        assert_eq!(from_editor("keep\n\n"), "keep\n");
        assert_eq!(from_editor(""), "");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_editor_changes_the_message_and_its_file_is_removed() {
        let dir = crate::assistant::sandbox::tests::temp_dir("editor-script");
        let script = dir.join("edit.sh");
        // Appends a line and records which file it was given.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf 'second line\\n' >> \"$1\"\necho \"$1\" > {}\n",
                dir.join("seen").display()
            ),
        )
        .unwrap();
        let editor = EditorCommand {
            program: "sh".into(),
            args: vec![script.display().to_string()],
        };
        let edited = edit_text_with(&editor, "first line", false).await.unwrap();
        assert_eq!(edited, "first line\nsecond line");
        let seen = std::fs::read_to_string(dir.join("seen")).unwrap();
        assert!(!std::path::Path::new(seen.trim()).exists(), "draft removed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_or_missing_editor_says_what_to_do() {
        let failing = EditorCommand {
            program: "false".into(),
            args: vec![],
        };
        let err = edit_text_with(&failing, "x", false).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("nothing was changed"),
            "{err:#}"
        );
        let missing = EditorCommand {
            program: "gz-no-such-editor".into(),
            args: vec![],
        };
        let err = edit_text_with(&missing, "x", false).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("set $VISUAL or $EDITOR"),
            "{err:#}"
        );
    }
}
