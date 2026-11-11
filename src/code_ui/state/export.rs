//! `/export`: the conversation on screen as Markdown, written to a new file
//! in the current folder, to a path the user names, or to the clipboard.
//! Writing is the user's own action, so it never asks; the path still has
//! to be inside an open folder and not a secret file.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::assistant::sandbox::Sandbox;
use crate::code_ui::items::{Item, ToolItem, ToolStatus};
use crate::code_ui::labels::{self, Label};
use crate::code_ui::shell_item::ShellOutput;
use crate::subagents::Subagent;

use super::{Effect, UiState};

const CLIPBOARD_ARGUMENT: &str = "clipboard";
const MARKDOWN_EXTENSION: &str = "md";
/// Longest title taken from the first message.
const TITLE_CHARS: usize = 80;
const UNTITLED: &str = "gz code conversation";
/// File names stay short: the first words of the first message.
const SLUG_WORDS: usize = 6;
const SLUG_CHARS: usize = 48;
const UNTITLED_SLUG: &str = "conversation";
/// `!command` output kept in the export; its end matters most.
const COMMAND_OUTPUT_LINES: usize = 40;
/// Numbered names tried (`-2` … `-999`) before giving up on a free one.
const MAX_NAME_TRIES: usize = 999;
/// The sandbox's hint is written for the model, not for the user.
const MODEL_HINT: &str = "; use change_directory";

/// Where `/export` puts the Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportTarget {
    /// A new dated file in the current folder.
    NewFile,
    /// This path, relative to the current folder (`~` works).
    Path(String),
    Clipboard,
}

impl ExportTarget {
    fn parse(command: &str) -> Self {
        let argument = command
            .trim()
            .strip_prefix("/export")
            .unwrap_or_default()
            .trim();
        match argument {
            "" => Self::NewFile,
            CLIPBOARD_ARGUMENT => Self::Clipboard,
            path => Self::Path(path.to_string()),
        }
    }
}

/// What `/export` needs beyond the transcript's items: where the current
/// conversation starts in it (the transcript spans the whole session), its
/// title once a resume names it, and the models its turns went to.
#[derive(Debug, Clone, Default)]
pub struct ConversationLog {
    start: usize,
    title: Option<String>,
    models: Vec<String>,
}

impl ConversationLog {
    pub(super) fn starting_at(start: usize) -> Self {
        Self {
            start,
            ..Self::default()
        }
    }

    pub(super) fn set_title(&mut self, title: &str) {
        self.title = Some(title.to_string());
    }

    pub(super) fn used_model(&mut self, model: Option<String>) {
        if let Some(model) = model.filter(|m| !self.models.contains(m)) {
            self.models.push(model);
        }
    }
}

/// The lines above the conversation.
pub struct Header<'a> {
    /// The conversation's title; the first message stands in without one.
    pub title: Option<&'a str>,
    pub date: &'a str,
    pub models: &'a [String],
    /// The folder, as gz shows it (`~/code/app`).
    pub folder: &'a str,
    pub conversation_id: &'a str,
}

/// The conversation in `items` as Markdown. Panels, notices, errors,
/// turn summaries and the welcome banner are screen furniture and stay out.
pub fn markdown(items: &[Item], header: &Header) -> String {
    let title = header
        .title
        .map(str::to_string)
        .or_else(|| first_message(items).map(title_from))
        .unwrap_or_else(|| UNTITLED.to_string());
    let mut out = format!("# {title}\n\n- Date: {}\n", header.date);
    match header.models {
        [] => {}
        [model] => out.push_str(&format!("- Model: {model}\n")),
        models => out.push_str(&format!("- Models: {}\n", models.join(", "))),
    }
    if !header.folder.is_empty() {
        out.push_str(&format!("- Folder: {}\n", code(header.folder)));
    }
    out.push_str("\n---\n\n");
    let body = body(items);
    if !body.is_empty() {
        out.push_str(&body);
        out.push_str("\n\n---\n\n");
    }
    let id = if header.conversation_id.is_empty() {
        "not started".to_string()
    } else {
        code(header.conversation_id)
    };
    out.push_str(&format!("_Exported from gz code · conversation {id}_\n"));
    out
}

/// `gz-2026-11-11-fix-the-failing-tests`: the date, then the first words
/// of the first message.
pub fn file_stem(items: &[Item], date: &str) -> String {
    let slug = first_message(items).map(slug).unwrap_or_default();
    let slug = if slug.is_empty() {
        UNTITLED_SLUG.to_string()
    } else {
        slug
    };
    format!("gz-{date}-{slug}")
}

fn first_message(items: &[Item]) -> Option<&str> {
    items.iter().find_map(|item| match item {
        Item::User(text) | Item::UserWithContext { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

fn title_from(message: &str) -> String {
    let line = message.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Lowercase ASCII words joined by `-`, so the name works on every system.
fn slug(message: &str) -> String {
    let words = message
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(SLUG_WORDS)
        .map(str::to_ascii_lowercase);
    let mut slug = String::new();
    for word in words {
        let extra = usize::from(!slug.is_empty()) + word.len();
        if slug.len() + extra > SLUG_CHARS {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    slug
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Speaker {
    You,
    Gz,
}

enum Block {
    /// A list line: consecutive ones form one tight list.
    ListLine(String),
    Text(String),
}

#[derive(Default)]
struct Body {
    blocks: Vec<Block>,
    speaker: Option<Speaker>,
}

fn body(items: &[Item]) -> String {
    let mut body = Body::default();
    let mut rest = items;
    while let Some(item) = rest.first() {
        // The screen folds consecutive reads into one line; so does this.
        let reads = leading_reads(rest);
        if reads.len() > 1 {
            body.speak(Speaker::Gz);
            body.list_line(format!("- {}", labels::group_summary(&reads)));
            rest = &rest[reads.len()..];
            continue;
        }
        body.add(item);
        rest = &rest[1..];
    }
    body.finish()
}

fn leading_reads(items: &[Item]) -> Vec<Label> {
    items
        .iter()
        .map_while(|item| match item {
            Item::Tool(tool) if tool.label.read_only && tool.status == ToolStatus::Ok => {
                Some(tool.label.clone())
            }
            _ => None,
        })
        .collect()
}

impl Body {
    fn add(&mut self, item: &Item) {
        match item {
            Item::User(text) => self.user(text, &[]),
            Item::UserWithContext { text, notes } => self.user(text, notes),
            Item::Assistant(text) | Item::AssistantMore(text) => {
                self.speak(Speaker::Gz);
                self.text(text.trim_end().to_string());
            }
            Item::Tool(tool) => {
                self.speak(Speaker::Gz);
                self.list_line(tool_line(tool));
            }
            Item::Group(summary) => {
                self.speak(Speaker::Gz);
                self.list_line(format!("- {summary}"));
            }
            Item::Decision { title, choice, .. } => {
                self.speak(Speaker::Gz);
                self.list_line(format!("- Asked to {} · {choice}", code(title)));
            }
            Item::Subagent(agent) => {
                self.speak(Speaker::Gz);
                self.text(subagent_text(agent));
            }
            Item::UserCommand { command, output } => {
                self.speak(Speaker::You);
                self.text(command_text(command, output));
            }
            // Errors on screen mostly answer slash commands (a refused
            // `/export` among them), not the conversation.
            Item::Banner { .. }
            | Item::Error(_)
            | Item::Panel(_)
            | Item::Summary(_)
            | Item::Notice(_)
            | Item::ShellOutput(_) => {}
        }
    }

    /// Every message gets its own heading, even right after another.
    fn user(&mut self, text: &str, notes: &[String]) {
        self.speaker = None;
        self.speak(Speaker::You);
        self.text(text.trim_end().to_string());
        for note in notes {
            self.list_line(format!("- {note}"));
        }
    }

    fn speak(&mut self, speaker: Speaker) {
        if self.speaker == Some(speaker) {
            return;
        }
        let heading = match speaker {
            Speaker::You => "## You",
            Speaker::Gz => "## gz",
        };
        self.text(heading.to_string());
        self.speaker = Some(speaker);
    }

    fn text(&mut self, text: String) {
        self.blocks.push(Block::Text(text));
    }

    fn list_line(&mut self, line: String) {
        self.blocks.push(Block::ListLine(line));
    }

    fn finish(self) -> String {
        let mut out = String::new();
        let mut after_list_line = false;
        for block in self.blocks {
            let (text, is_list_line) = match block {
                Block::ListLine(line) => (line, true),
                Block::Text(text) => (text, false),
            };
            if !out.is_empty() {
                out.push_str(if after_list_line && is_list_line {
                    "\n"
                } else {
                    "\n\n"
                });
            }
            out.push_str(&text);
            after_list_line = is_list_line;
        }
        out
    }
}

/// `- Update `src/a.rs` · failed: old text not found`, with a script's own
/// calls nested under it.
fn tool_line(tool: &ToolItem) -> String {
    let mut line = format!("- {}", tool.label.verb);
    if !tool.label.target.is_empty() {
        line.push(' ');
        line.push_str(&code(&tool.label.target));
    }
    match tool.status {
        ToolStatus::Ok => {}
        ToolStatus::Failed => {
            line.push_str(" · **failed**");
            if let Some(result) = tool.result.as_deref().filter(|r| !r.trim().is_empty()) {
                line.push_str(&format!(": {}", result.trim()));
            }
        }
        ToolStatus::Running => line.push_str(" · did not finish"),
    }
    for child in &tool.children {
        line.push_str(&format!("\n  - {child}"));
    }
    line
}

fn subagent_text(agent: &Subagent) -> String {
    let profile = agent
        .profile
        .as_deref()
        .map(|p| format!(" ({p})"))
        .unwrap_or_default();
    let mut text = format!(
        "- Subagent **{}**{profile} · {}",
        agent.name,
        agent.status.word()
    );
    let summary = agent.summary.trim();
    if !summary.is_empty() {
        text.push_str("\n\n");
        text.push_str(&indented(summary));
    }
    text
}

/// `! cargo test` with its status, then the end of its output fenced.
fn command_text(command: &str, output: &ShellOutput) -> String {
    let mut text = format!("{} · {}", code(&format!("!{command}")), output.status);
    let lines: Vec<&str> = output.text.trim_end().lines().collect();
    if lines.iter().all(|l| l.trim().is_empty()) {
        return text;
    }
    let kept = &lines[lines.len().saturating_sub(COMMAND_OUTPUT_LINES)..];
    if output.is_cut || kept.len() < lines.len() {
        text.push_str(" · earlier output cut");
    }
    let shown = kept.join("\n");
    let fence = "`".repeat(longest_backtick_run(&shown).max(2) + 1);
    text.push_str(&format!("\n\n{fence}\n{shown}\n{fence}"));
    text
}

/// Inline code that survives backticks in `text`.
fn code(text: &str) -> String {
    match longest_backtick_run(text) {
        0 => format!("`{text}`"),
        run => {
            let fence = "`".repeat(run + 1);
            format!("{fence} {text} {fence}")
        }
    }
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

fn indented(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("  {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Creates `path` with `text`, never replacing a file that is there.
fn write_new(path: &Path, text: &str) -> std::io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// `notes-2.md`, `notes-3.md`, …: the `n`th name for `path`.
fn numbered(path: &Path, n: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}-{n}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{n}"),
    };
    path.with_file_name(name)
}

/// The sandbox's refusal, worded for the user.
fn refusal(requested: &str, err: &anyhow::Error) -> anyhow::Error {
    let reason = format!("{err:#}");
    let reason = reason.split(MODEL_HINT).next().unwrap_or_default();
    anyhow!(
        "Can't export to {requested}: {reason}. Choose a file inside an open folder \
         (/dirs lists them, /add-dir opens another)"
    )
}

impl UiState {
    pub(super) fn export_command(&mut self, command: &str) -> Vec<Effect> {
        vec![Effect::Export(ExportTarget::parse(command))]
    }

    /// Writes or copies the current conversation as Markdown and says where
    /// it went, or why it couldn't.
    pub fn export(&mut self, target: &ExportTarget, conversation_id: &str) {
        let date = crate::spend::day::today();
        let start = self.conversation.start.min(self.transcript.len());
        let items = &self.transcript[start..];
        let models = if self.conversation.models.is_empty() {
            self.model_label().into_iter().collect()
        } else {
            self.conversation.models.clone()
        };
        let folder = self
            .sandbox
            .as_ref()
            .map(|s| s.display_folder(&s.cwd()))
            .unwrap_or_default();
        let header = Header {
            title: self.conversation.title.as_deref(),
            date: &date,
            models: &models,
            folder: &folder,
            conversation_id,
        };
        let text = markdown(items, &header);
        let stem = file_stem(items, &date);
        let outcome = match target {
            ExportTarget::Clipboard => self.copy_export(&text),
            ExportTarget::NewFile => self.export_new_file(&stem, &text),
            ExportTarget::Path(requested) => self.export_to(requested, &text),
        };
        match outcome {
            Ok(done) => self.record(Item::Notice(done)),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn copy_export(&self, text: &str) -> Result<String> {
        self.clipboard.copy_text(text).map_err(|err| {
            anyhow!("Can't copy to the clipboard: {err:#}. /export writes a file instead")
        })?;
        Ok("✓ Copied the conversation to the clipboard as Markdown".into())
    }

    fn export_sandbox(&self) -> Result<&Sandbox> {
        self.sandbox
            .as_ref()
            .context("Can't export: gz has no folder open; start gz code in a folder")
    }

    /// `<stem>.md`, or `<stem>-2.md`, `-3`, … when that name is taken.
    fn export_new_file(&self, stem: &str, text: &str) -> Result<String> {
        let sandbox = self.export_sandbox()?;
        let first = format!("{stem}.{MARKDOWN_EXTENSION}");
        let first = sandbox
            .resolve(&first)
            .map_err(|err| refusal(&first, &err))?;
        for n in 1..=MAX_NAME_TRIES {
            let path = if n == 1 {
                first.clone()
            } else {
                numbered(&first, n)
            };
            match write_new(&path, text) {
                Ok(()) => return Ok(exported_to(sandbox, &path)),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "Can't write {}; check the folder's permissions or /export <path>",
                            sandbox.display(&path)
                        )
                    })
                }
            }
        }
        bail!(
            "Can't export: {stem}.md through -{MAX_NAME_TRIES} all exist; /export <path> names the file"
        )
    }

    fn export_to(&self, requested: &str, text: &str) -> Result<String> {
        let sandbox = self.export_sandbox()?;
        let path = sandbox
            .resolve(requested)
            .map_err(|err| refusal(requested, &err))?;
        let shown = sandbox.display(&path);
        if path.is_dir() {
            bail!("Can't export to {shown}: it is a folder; add a file name, e.g. /export {shown}/chat.md");
        }
        match write_new(&path, text) {
            Ok(()) => Ok(exported_to(sandbox, &path)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let free = (2..=MAX_NAME_TRIES)
                    .map(|n| numbered(&path, n))
                    .find(|p| !p.exists())
                    .map(|p| format!(" (e.g. /export {})", sandbox.display(&p)))
                    .unwrap_or_default();
                bail!("{shown} already exists and gz won't replace it; choose another name{free}")
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                bail!("Can't export to {shown}: its folder doesn't exist; create it first or choose another path")
            }
            Err(err) => Err(err).with_context(|| {
                format!(
                    "Can't write {shown}; check the folder's permissions or choose another path"
                )
            }),
        }
    }
}

fn exported_to(sandbox: &Sandbox, path: &Path) -> String {
    format!("✓ Exported the conversation to {}", sandbox.display(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use crate::assistant::history::{Message, Speaker as HistorySpeaker, Transcript};
    use crate::assistant::sandbox::tests::{temp_dir, temp_workspace};
    use crate::code_ui::clipboard::fake::CopyingClipboard;
    use crate::code_ui::panel::Panel;
    use crate::code_ui::state::test_support::*;
    use crate::subagents::{RunsOn, Status, SubagentUsage};

    fn tool(name: &str, summary: &str, status: ToolStatus, result: Option<&str>) -> Item {
        Item::Tool(ToolItem {
            label: labels::label(name, summary),
            status,
            result: result.map(str::to_string),
            diff: None,
            children: Vec::new(),
            output: None,
        })
    }

    fn subagent(name: &str, status: Status, summary: &str) -> Item {
        Item::Subagent(Box::new(Subagent {
            id: "s1".into(),
            name: name.into(),
            profile: Some("Analyst".into()),
            agent_id: None,
            task: None,
            node_id: None,
            parent: None,
            depth: 0,
            runs_on: RunsOn::default(),
            status,
            tools: Vec::new(),
            text: "progress chatter".into(),
            summary: summary.into(),
            usage: SubagentUsage::default(),
            started: Instant::now(),
            duration: None,
        }))
    }

    fn header<'a>(models: &'a [String]) -> Header<'a> {
        Header {
            title: None,
            date: "2026-11-11",
            models,
            folder: "~/code/app",
            conversation_id: "c-123",
        }
    }

    fn workspace_state(name: &str) -> (PathBuf, UiState) {
        let (dir, sandbox) = temp_workspace(name);
        let mut s = state();
        s.sandbox = Some(sandbox);
        (dir, s)
    }

    fn last_error(s: &UiState) -> String {
        match s.flush.last() {
            Some(Item::Error(e)) => e.clone(),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn every_item_kind_becomes_its_markdown() {
        let items = vec![
            Item::Banner {
                workspace: "app".into(),
                cwd: "/w".into(),
                who: None,
                model: None,
            },
            Item::UserWithContext {
                text: "Fix the failing tests".into(),
                notes: vec!["Read src/a.rs (40 lines)".into()],
            },
            Item::Assistant("I'll look at **the tests**.\n\n- one\n- two".into()),
            tool("read_file", "read_file src/a.rs", ToolStatus::Ok, None),
            tool("read_file", "read_file src/b.rs", ToolStatus::Ok, None),
            tool("list_directory", "list_directory src", ToolStatus::Ok, None),
            tool(
                "edit_file",
                "edit_file src/a.rs",
                ToolStatus::Failed,
                Some("old text not found"),
            ),
            tool(
                "run_command",
                "run_command git push",
                ToolStatus::Failed,
                Some("Blocked by auto mode: Not asked for."),
            ),
            Item::Group("Search 2 patterns".into()),
            Item::Decision {
                title: "run_command rm -rf build".into(),
                choice: "denied: use cargo clean".into(),
                preview: Some("rm -rf build".into()),
            },
            subagent(
                "research",
                Status::Completed,
                "Found 3 callers.\nAll fixed.",
            ),
            Item::Notice("Mode: plan".into()),
            Item::Panel(Panel::new("This session")),
            Item::Summary("✻ Worked for 3s".into()),
            Item::Error("The run failed: HTTP 500".into()),
            Item::User("now run them".into()),
            Item::UserCommand {
                command: "echo `hi`".into(),
                output: ShellOutput {
                    status: "exit code 0".into(),
                    text: "hi\n".into(),
                    is_cut: false,
                    is_error: false,
                },
            },
            Item::Assistant("Done.".into()),
        ];
        let models = vec!["Claude Sonnet 5.5".to_string()];
        let expected = "\
# Fix the failing tests

- Date: 2026-11-11
- Model: Claude Sonnet 5.5
- Folder: `~/code/app`

---

## You

Fix the failing tests

- Read src/a.rs (40 lines)

## gz

I'll look at **the tests**.

- one
- two

- Read 2 files, Listed 1 folder
- Update `src/a.rs` · **failed**: old text not found
- Bash `git push` · **failed**: Blocked by auto mode: Not asked for.
- Search 2 patterns
- Asked to `run_command rm -rf build` · denied: use cargo clean

- Subagent **research** (Analyst) · done

  Found 3 callers.
  All fixed.

## You

now run them

`` !echo `hi` `` · exit code 0

```
hi
```

## gz

Done.

---

_Exported from gz code · conversation `c-123`_
";
        assert_eq!(markdown(&items, &header(&models)), expected);
    }

    #[test]
    fn long_command_output_keeps_its_end_and_says_so() {
        let text: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        let items = vec![Item::UserCommand {
            command: "seq 50".into(),
            output: ShellOutput {
                status: "exit code 1".into(),
                text,
                is_cut: false,
                is_error: true,
            },
        }];
        let out = markdown(&items, &header(&[]));
        assert!(out.contains("`!seq 50` · exit code 1 · earlier output cut"));
        assert!(out.contains("```\nline 11\n") && out.contains("line 50\n```"));
        assert!(!out.contains("line 10\n"));
        assert!(!out.contains("- Model"), "no model line without one");
    }

    #[test]
    fn names_come_from_the_date_and_first_message() {
        let items = [Item::User(
            "Why does `nightly-etl` fail?? Look at jobs/etl.py please, thanks a lot".into(),
        )];
        assert_eq!(
            file_stem(&items, "2026-11-11"),
            "gz-2026-11-11-why-does-nightly-etl-fail-look"
        );
        assert_eq!(
            file_stem(&[Item::User("日本語?".into())], "2026-11-11"),
            "gz-2026-11-11-conversation"
        );
        let long = "word ".repeat(40);
        let out = markdown(&[Item::User(long)], &header(&[]));
        assert!(out.starts_with(&format!("# {}…\n", "word ".repeat(16).trim_end())));
    }

    #[test]
    fn export_command_parses_its_target() {
        let mut s = state();
        for (typed, target) in [
            ("/export", ExportTarget::NewFile),
            ("/export clipboard", ExportTarget::Clipboard),
            (
                "/export ~/notes/my chat.md",
                ExportTarget::Path("~/notes/my chat.md".into()),
            ),
        ] {
            type_text(&mut s, typed);
            assert_eq!(
                s.on_key(key(crossterm::event::KeyCode::Enter)),
                vec![Effect::Export(target)]
            );
        }
    }

    #[test]
    fn writes_a_dated_file_in_the_folder_and_numbers_the_next() {
        let (dir, mut s) = workspace_state("export-new");
        s.start_turn("Fix the failing tests");
        s.export(&ExportTarget::NewFile, "c-1");
        let stem = format!("gz-{}-fix-the-failing-tests", crate::spend::day::today());
        let first = std::fs::read_to_string(dir.join(format!("{stem}.md"))).unwrap();
        assert!(first.starts_with("# Fix the failing tests\n"));
        assert!(first.contains("## You\n\nFix the failing tests"));
        assert!(first.ends_with("conversation `c-1`_\n"));
        assert!(
            matches!(s.flush.last(), Some(Item::Notice(n)) if n.ends_with(&format!("{stem}.md")))
        );

        s.export(&ExportTarget::NewFile, "c-1");
        assert!(dir.join(format!("{stem}-2.md")).is_file());
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.ends_with("-2.md")));
    }

    #[test]
    fn an_explicit_path_is_written_relative_to_the_folder_and_home() {
        let dir = temp_dir("export-home");
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        let mut s = state();
        s.sandbox = Some(Sandbox::with_home(&dir, Some(&dir)).unwrap());
        s.start_turn("hello");
        s.export(&ExportTarget::Path("notes/chat.md".into()), "c-1");
        assert!(std::fs::read_to_string(dir.join("notes/chat.md"))
            .unwrap()
            .contains("hello"));
        s.export(&ExportTarget::Path("~/home.md".into()), "c-1");
        assert!(dir.join("home.md").is_file(), "~ is the home folder");
    }

    #[test]
    fn paths_outside_the_folders_or_to_secrets_are_refused() {
        let (dir, mut s) = workspace_state("export-refuse");
        s.start_turn("hello");
        let outside = dir.parent().unwrap().join(format!(
            "{}-outside.md",
            dir.file_name().unwrap().to_string_lossy()
        ));
        s.export(
            &ExportTarget::Path("../".to_string() + outside.file_name().unwrap().to_str().unwrap()),
            "c-1",
        );
        let error = last_error(&s);
        assert!(error.contains("outside the folders open in gz"), "{error}");
        assert!(error.contains("/add-dir") && !error.contains("change_directory"));
        assert!(!outside.exists());

        s.export(&ExportTarget::Path(".env.md".into()), "c-1");
        assert!(last_error(&s).contains("secret file"));
        assert!(!dir.join(".env.md").exists());
    }

    #[test]
    fn an_existing_file_is_never_replaced() {
        let (dir, mut s) = workspace_state("export-exists");
        std::fs::write(dir.join("notes.md"), "keep me").unwrap();
        s.start_turn("hello");
        s.export(&ExportTarget::Path("notes.md".into()), "c-1");
        let error = last_error(&s);
        assert!(error.contains("notes.md already exists"), "{error}");
        assert!(error.contains("/export notes-2.md"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn a_resumed_conversation_exports_its_history_under_its_title() {
        let (_dir, mut s) = workspace_state("export-resume");
        let copied = Arc::new(Mutex::new(String::new()));
        s.set_clipboard(Box::new(CopyingClipboard(copied.clone())));
        s.record(Item::User("an earlier chat".into()));
        s.show_conversations(Ok(vec![crate::code_ui::state::PickerRow::Conversation {
            id: "c-9".into(),
            title: "Charts".into(),
            detail: String::new(),
        }]));
        let picked = s.on_key(key(crossterm::event::KeyCode::Enter));
        assert!(matches!(&picked[..], [Effect::Resume { id, .. }] if id == "c-9"));
        s.show_history(
            "Charts",
            Ok(Transcript {
                title: Some("Creating charts".into()),
                messages: vec![
                    Message {
                        speaker: HistorySpeaker::You,
                        text: "make a bar chart".into(),
                    },
                    Message {
                        speaker: HistorySpeaker::Gz,
                        text: "Here it is.".into(),
                    },
                ],
            }),
        );
        s.export(&ExportTarget::Clipboard, "c-9");
        let copied = copied.lock().unwrap().clone();
        assert!(copied.starts_with("# Creating charts\n"), "{copied}");
        assert!(copied.contains("## You\n\nmake a bar chart\n\n## gz\n\nHere it is."));
        assert!(
            !copied.contains("an earlier chat"),
            "the conversation left behind stays out"
        );
        assert!(matches!(s.flush.last(), Some(Item::Notice(n)) if n.contains("clipboard")));
    }

    #[test]
    fn the_header_names_every_model_the_turns_used() {
        use crate::assistant::models::{Model, Selection};
        let mut s = with_catalog();
        let copied = Arc::new(Mutex::new(String::new()));
        s.set_clipboard(Box::new(CopyingClipboard(copied.clone())));
        let modes = s.catalog.as_ref().unwrap().modes.clone();
        for (mode, prompt) in modes.into_iter().zip(["one", "two"]) {
            s.model = Some(Selection {
                model: Model::Groundzero(mode),
                effort: None,
            });
            s.start_turn(prompt);
        }
        s.export(&ExportTarget::Clipboard, "c-1");
        let copied = copied.lock().unwrap().clone();
        assert!(
            copied.contains("- Models: Claude Opus 5, Claude Sonnet 5.5\n"),
            "{copied}"
        );
    }
}
