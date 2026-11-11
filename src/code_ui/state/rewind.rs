//! `/rewind` and `/diff`: the files gz's own tools changed in this
//! conversation (see [`crate::assistant::checkpoints`]). `/rewind` puts
//! them back as they were before a chosen message and tells the model with
//! the user's next message; `/diff` shows them against how they started.
//! The conversation itself is never rewound, and changes made by commands
//! are not covered.

use std::path::{Path, PathBuf};

use crate::assistant::attachments::PreparedTurn;
use crate::assistant::checkpoints::{Change, FileDiff, Restore, Rewound};
use crate::assistant::tools::unified_diff;
use crate::code_ui::items::{Item, ToolItem, ToolStatus};
use crate::code_ui::labels::{self, Label};
use crate::code_ui::resume;
use crate::code_ui::transcript::Viewer;

use super::{Effect, Picker, PickerRow, UiState};

/// Diff lines one file shows before the rest is cut.
const MAX_FILE_DIFF_LINES: usize = 2_000;
/// Diff lines `/diff` shows in all; later files are only named.
const MAX_TOTAL_DIFF_LINES: usize = 10_000;

const NOT_TRACKED: &str = "Changes made by commands (gz's or your own !commands) are not tracked.";
const CONVERSATION_STAYS: &str = "Only files go back: the conversation itself stays as it is.";
const COMMANDS_NOT_COVERED: &str =
    "Changes made by commands (gz's or your own !commands) are not covered.";
const NO_HISTORY: &str =
    "gz keeps no edit history in this session (it found no config folder); undo changes with git";
const WAIT_FOR_TURN: &str = "Wait for the answer to finish (or esc) before rewinding files";
const WAIT_FOR_SUBAGENTS: &str =
    "Background subagents may still be changing files; wait for them (or /subagents stop), then /rewind";
const DIFF_USAGE: &str = "Use /diff, or /diff <path> for one file";

/// One row of the `/rewind` pickers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindRow {
    /// A message that led to file changes: rewinding to it puts back every
    /// file gz changed since it started.
    Message {
        turn: u64,
        excerpt: String,
        files: usize,
        /// When it was sent, e.g. `5m ago`.
        ago: String,
    },
    /// What to do with files changed since gz wrote them.
    Resolve {
        turn: u64,
        overwrite: bool,
        /// How many files changed since gz wrote them.
        changed: usize,
    },
    Cancel,
}

impl RewindRow {
    pub fn label(&self) -> String {
        match self {
            RewindRow::Message {
                excerpt,
                files,
                ago,
                ..
            } => format!("“{excerpt}” · {} · {ago}", files_word(*files)),
            RewindRow::Resolve {
                overwrite: true,
                changed: 1,
                ..
            } => "Put it back too (what changed in it since is lost)".into(),
            RewindRow::Resolve {
                overwrite: true, ..
            } => "Put them back too (what changed in them since is lost)".into(),
            RewindRow::Resolve {
                overwrite: false,
                changed: 1,
                ..
            } => "Leave it as it is; put back the rest".into(),
            RewindRow::Resolve {
                overwrite: false, ..
            } => "Leave them as they are; put back the rest".into(),
            RewindRow::Cancel => "Cancel".into(),
        }
    }
}

impl UiState {
    /// `/rewind`: the messages that led to file changes, newest first.
    pub(super) fn rewind_command(&mut self) -> Vec<Effect> {
        if self.is_busy() {
            self.record(Item::Error(WAIT_FOR_TURN.into()));
            return Vec::new();
        }
        if !self.background.is_empty() {
            self.record(Item::Error(WAIT_FOR_SUBAGENTS.into()));
            return Vec::new();
        }
        let Some(history) = self.edit_history.clone().filter(|h| h.is_enabled()) else {
            self.record(Item::Error(NO_HISTORY.into()));
            return Vec::new();
        };
        let messages = match history.messages() {
            Ok(messages) => messages,
            Err(err) => {
                self.record(Item::Error(format!("{err:#}")));
                return Vec::new();
            }
        };
        if messages.is_empty() {
            self.record(Item::Notice(format!(
                "Nothing to rewind: gz's tools have changed no files in this conversation \
                 (or they were rewound already). {NOT_TRACKED}"
            )));
            return Vec::new();
        }
        let now = crate::assistant::history::now_millis();
        let mut rows: Vec<PickerRow> = messages
            .into_iter()
            .map(|m| {
                PickerRow::Rewind(RewindRow::Message {
                    turn: m.turn,
                    excerpt: m.excerpt,
                    files: m.files,
                    ago: resume::ago(now.saturating_sub(m.at)),
                })
            })
            .collect();
        rows.push(PickerRow::Header(String::new()));
        rows.push(PickerRow::Header(CONVERSATION_STAYS.into()));
        rows.push(PickerRow::Header(COMMANDS_NOT_COVERED.into()));
        self.picker = Some(Picker {
            title: "Rewind files to before a message".into(),
            rows,
            selected: 0,
            model: None,
        });
        Vec::new()
    }

    /// Enter on a `/rewind` row.
    pub(super) fn choose_rewind(&mut self, row: RewindRow) -> Vec<Effect> {
        self.picker = None;
        match row {
            RewindRow::Message { turn, .. } => self.rewind_or_ask(turn),
            RewindRow::Resolve {
                turn, overwrite, ..
            } => self.rewind_to(turn, overwrite),
            RewindRow::Cancel => self.record(Item::Notice("Nothing was rewound".into())),
        }
        Vec::new()
    }

    /// Rewinds at once, unless some files changed since gz wrote them:
    /// then lists them and asks.
    fn rewind_or_ask(&mut self, turn: u64) {
        let Some(history) = self.edit_history.clone() else {
            return self.record(Item::Error(NO_HISTORY.into()));
        };
        let plan = match history.plan_rewind(turn) {
            Ok(plan) => plan,
            Err(err) => return self.record(Item::Error(format!("{err:#}"))),
        };
        let changed: Vec<PathBuf> = plan
            .into_iter()
            .filter(|f| f.changed_since && !matches!(f.restore, Restore::Impossible(_)))
            .map(|f| f.path)
            .collect();
        if changed.is_empty() {
            return self.rewind_to(turn, false);
        }
        let mut rows: Vec<PickerRow> = changed
            .iter()
            .map(|path| PickerRow::Header(format!("  {}", self.shown(path))))
            .collect();
        rows.push(PickerRow::Header(String::new()));
        let first_choice = rows.len();
        rows.extend([
            PickerRow::Rewind(RewindRow::Resolve {
                turn,
                overwrite: false,
                changed: changed.len(),
            }),
            PickerRow::Rewind(RewindRow::Resolve {
                turn,
                overwrite: true,
                changed: changed.len(),
            }),
            PickerRow::Rewind(RewindRow::Cancel),
        ]);
        self.picker = Some(Picker {
            title: format!(
                "{} changed since gz last wrote {} (by you or a command)",
                files_word(changed.len()),
                if changed.len() == 1 { "it" } else { "them" }
            ),
            rows,
            selected: first_choice,
            model: None,
        });
    }

    fn rewind_to(&mut self, turn: u64, overwrite: bool) {
        let Some(history) = self.edit_history.clone() else {
            return self.record(Item::Error(NO_HISTORY.into()));
        };
        match history.rewind(turn, overwrite) {
            Ok(done) => self.show_rewound(done),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn show_rewound(&mut self, done: Rewound) {
        let mut parts = Vec::new();
        if !done.restored.is_empty() {
            parts.push(format!("restored {}", self.shown_list(&done.restored)));
        }
        if !done.deleted.is_empty() {
            parts.push(format!(
                "deleted {} (gz created {})",
                self.shown_list(&done.deleted),
                if done.deleted.len() == 1 {
                    "it"
                } else {
                    "them"
                }
            ));
        }
        if parts.is_empty() {
            self.record(Item::Notice(format!(
                "No files were put back to before “{}”",
                done.excerpt
            )));
        } else {
            self.record(Item::Notice(format!(
                "✓ Files back to before “{}”: {} · gz hears about it with your next message",
                done.excerpt,
                parts.join(" · ")
            )));
        }
        if !done.skipped.is_empty() {
            self.record(Item::Notice(format!(
                "Left as changed since gz wrote them: {}",
                self.shown_list(&done.skipped)
            )));
        }
        for (path, reason) in &done.not_kept {
            self.record(Item::Error(format!(
                "Could not put back {}: {reason}; restore it with git or by hand",
                self.shown(path)
            )));
        }
        for (path, reason) in &done.failed {
            self.record(Item::Error(format!(
                "Could not put back {}: {reason}; fix that and /rewind to the same message again",
                self.shown(path)
            )));
        }
        if done.changed_files() > 0 {
            self.rewound.push(done);
        }
    }

    /// Adds what `/rewind` changed since the user's last message to `turn`,
    /// for the model only; each rewind is told once.
    pub(super) fn with_rewind_context(&mut self, mut turn: PreparedTurn) -> PreparedTurn {
        if self.rewound.is_empty() {
            return turn;
        }
        let rewinds = std::mem::take(&mut self.rewound);
        let files: usize = rewinds.iter().map(Rewound::changed_files).sum();
        turn.prompt.push_str("\n\n");
        turn.prompt.push_str(&self.rewind_block(&rewinds));
        turn.notes
            .push(format!("Told gz you rewound {}", files_word(files)));
        turn
    }

    fn rewind_block(&self, rewinds: &[Rewound]) -> String {
        let mut block = String::from(
            "<files_rewound>\nThe user ran /rewind, which put files back as they were before \
             one of their earlier messages. The conversation was not rewound: earlier messages \
             still describe changes these files no longer have. Re-read a file before relying \
             on its content or editing it.",
        );
        for done in rewinds {
            block.push_str(&format!(
                "\nBack to before the user's message “{}”:",
                done.excerpt
            ));
            if !done.restored.is_empty() {
                block.push_str(&format!(
                    "\n- restored to their earlier content: {}",
                    self.shown_list(&done.restored)
                ));
            }
            if !done.deleted.is_empty() {
                block.push_str(&format!(
                    "\n- deleted (gz had created them): {}",
                    self.shown_list(&done.deleted)
                ));
            }
            if !done.skipped.is_empty() {
                block.push_str(&format!(
                    "\n- left as the user changed them: {}",
                    self.shown_list(&done.skipped)
                ));
            }
        }
        block.push_str("\n</files_rewound>");
        block
    }

    /// `/diff` and `/diff <path>`.
    pub(super) fn diff_command(&mut self, command: &str) -> Vec<Effect> {
        let Some(history) = self.edit_history.clone().filter(|h| h.is_enabled()) else {
            self.record(Item::Error(NO_HISTORY.into()));
            return Vec::new();
        };
        let mut words = command.split_whitespace().skip(1);
        let only = match (words.next(), words.next()) {
            (None, _) => None,
            (Some(path), None) => match self.resolve(path) {
                Ok(path) => Some(path),
                Err(why) => {
                    self.record(Item::Error(why));
                    return Vec::new();
                }
            },
            _ => {
                self.record(Item::Error(DIFF_USAGE.into()));
                return Vec::new();
            }
        };
        match history.diffs(only.as_deref()) {
            Ok(diffs) if diffs.is_empty() => {
                let notice = match &only {
                    Some(path) => format!(
                        "gz's tools have not changed {} in this conversation. {NOT_TRACKED}",
                        self.shown(path)
                    ),
                    None => {
                        format!(
                            "gz's tools have changed no files in this conversation. {NOT_TRACKED}"
                        )
                    }
                };
                self.record(Item::Notice(notice));
            }
            Ok(diffs) => self.show_diffs(diffs),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
        Vec::new()
    }

    fn show_diffs(&mut self, diffs: Vec<FileDiff>) {
        self.record(Item::Notice(format!(
            "Files gz changed in this conversation, from before gz first changed each \
             to now. {NOT_TRACKED}"
        )));
        let mut budget = MAX_TOTAL_DIFF_LINES;
        let mut items = Vec::new();
        let mut unshown = Vec::new();
        for diff in diffs {
            if budget == 0 {
                unshown.push(diff.path);
                continue;
            }
            let item = self.diff_item(&diff, &mut budget);
            items.push(item);
        }
        let opens_viewer = items.iter().any(ToolItem::hides_detail);
        let tail: usize = items
            .iter()
            .map(|item| {
                Item::Tool(item.clone())
                    .render_expanded(self.transcript_width)
                    .len()
            })
            .sum();
        for item in items {
            self.record(Item::Tool(item));
        }
        if !unshown.is_empty() {
            self.record(Item::Notice(format!(
                "{} more not shown (the diff is over {MAX_TOTAL_DIFF_LINES} lines): {} · /diff <path> shows one",
                files_word(unshown.len()),
                self.shown_list(&unshown)
            )));
        }
        if opens_viewer {
            self.viewer = Some(Viewer::open_on_last(tail));
        }
    }

    /// One file's diff as a tool-style item, cut to the line `budget` left.
    fn diff_item(&self, diff: &FileDiff, budget: &mut usize) -> ToolItem {
        let name = self.shown(&diff.path);
        let (result, text) = match &diff.change {
            Change::Text { before, now } => {
                let full = unified_diff(&name, before.as_deref(), now.as_deref().unwrap_or(""));
                let (added, removed) = labels::diff_counts(&full);
                let state = match (before, now) {
                    (None, Some(_)) => "created by gz",
                    (_, None) => "deleted since",
                    _ => "changed",
                };
                let mut result = format!("{state} · +{added} −{removed}");
                let cap = MAX_FILE_DIFF_LINES.min(*budget);
                let lines = full.lines().count();
                let shown = if lines > cap {
                    result.push_str(&format!(" · diff cut at {cap} of {lines} lines"));
                    full.lines().take(cap).collect::<Vec<_>>().join("\n")
                } else {
                    full
                };
                *budget = budget.saturating_sub(lines.min(cap));
                (result, Some(shown))
            }
            Change::Unchanged => ("back to how it was before gz changed it".to_string(), None),
            Change::Binary => ("changed; not text, so no diff is shown".to_string(), None),
            Change::NotKept(reason) => (format!("no diff: {reason}"), None),
            Change::TooLarge => (
                format!(
                    "now over {} MB, so no diff is shown",
                    crate::assistant::checkpoints::MAX_KEPT_BYTES / (1024 * 1024)
                ),
                None,
            ),
        };
        ToolItem {
            label: Label {
                verb: "Diff".into(),
                target: name,
                read_only: false,
            },
            status: ToolStatus::Ok,
            result: Some(result),
            diff: text,
            children: Vec::new(),
            output: None,
        }
    }

    /// A path the user typed, as gz's tools would resolve it.
    fn resolve(&self, typed: &str) -> Result<PathBuf, String> {
        match &self.sandbox {
            Some(sandbox) => sandbox.resolve(typed).map_err(|err| format!("{err:#}")),
            None => Ok(PathBuf::from(typed)),
        }
    }

    fn shown(&self, path: &Path) -> String {
        match &self.sandbox {
            Some(sandbox) => sandbox.display(path),
            None => path.display().to_string(),
        }
    }

    fn shown_list(&self, paths: &[PathBuf]) -> String {
        paths
            .iter()
            .map(|p| self.shown(p))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn files_word(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::checkpoints::tests::{edit, workspace, write};
    use crate::assistant::checkpoints::EditHistory;
    use crate::code_ui::state::test_support::*;

    use crossterm::event::KeyCode;

    /// A UI over a temp workspace whose history is `history`.
    fn ui(sandbox: &crate::assistant::sandbox::Sandbox, history: &EditHistory) -> UiState {
        let mut s = state();
        s.sandbox = Some(sandbox.clone());
        s.edit_history = Some(history.clone());
        s
    }

    fn run(s: &mut UiState, command: &str) -> Vec<Effect> {
        type_text(s, command);
        s.on_key(key(KeyCode::Enter))
    }

    fn notices(s: &UiState) -> Vec<String> {
        s.flush
            .iter()
            .filter_map(|item| match item {
                Item::Notice(text) | Item::Error(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test]
    async fn rewind_puts_edits_back_and_deletes_created_files() {
        let (dir, sandbox, history) = workspace("ui-rewind");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("tidy a").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        history.begin_turn("add notes").unwrap();
        edit(&history, &sandbox, "a.txt", "two", "three").await;
        write(&history, &sandbox, "notes.md", "# notes\n").await;
        let mut s = ui(&sandbox, &history);

        assert!(run(&mut s, "/rewind").is_empty());
        let picker = s.picker.clone().expect("the message picker");
        let labels: Vec<String> = picker
            .rows
            .iter()
            .filter_map(|row| match row {
                PickerRow::Rewind(row) => Some(row.label()),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 2);
        assert!(labels[0].starts_with("“add notes” · 2 files"), "{labels:?}");
        assert!(labels[1].starts_with("“tidy a” · 1 file"), "{labels:?}");
        assert!(picker
            .rows
            .contains(&PickerRow::Header(CONVERSATION_STAYS.into())));
        assert!(picker
            .rows
            .contains(&PickerRow::Header(COMMANDS_NOT_COVERED.into())));

        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        assert!(s.picker.is_none());
        assert_eq!(read(dir.join("a.txt")), "one\n", "back to before “tidy a”");
        assert!(!dir.join("notes.md").exists(), "gz created it");
        let notice = notices(&s).pop().unwrap();
        assert!(
            notice.contains("“tidy a”")
                && notice.contains("restored a.txt")
                && notice.contains("deleted notes.md"),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn files_changed_since_gz_wrote_them_are_listed_and_kept_unless_chosen() {
        let (dir, sandbox, history) = workspace("ui-rewind-changed");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "bee\n").unwrap();
        history.begin_turn("change both").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        edit(&history, &sandbox, "b.txt", "bee", "wasp").await;
        std::fs::write(dir.join("a.txt"), "my own edit\n").unwrap();
        let mut s = ui(&sandbox, &history);

        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));
        let picker = s.picker.clone().expect("gz asks first");
        assert!(
            picker
                .title
                .starts_with("1 file changed since gz last wrote it"),
            "{}",
            picker.title
        );
        assert!(picker.rows.contains(&PickerRow::Header("  a.txt".into())));
        assert!(
            matches!(&picker.rows[picker.selected], PickerRow::Rewind(row) if row.label() == "Leave it as it is; put back the rest"),
            "{:?}",
            picker.rows
        );
        assert_eq!(
            read(dir.join("a.txt")),
            "my own edit\n",
            "nothing written yet"
        );
        assert_eq!(read(dir.join("b.txt")), "wasp\n");

        // The first choice keeps the changed file and puts back the rest.
        s.on_key(key(KeyCode::Enter));
        assert_eq!(read(dir.join("a.txt")), "my own edit\n");
        assert_eq!(read(dir.join("b.txt")), "bee\n");
        assert!(notices(&s)
            .iter()
            .any(|n| n == "Left as changed since gz wrote them: a.txt"));
    }

    #[tokio::test]
    async fn choosing_to_overwrite_puts_changed_files_back_too() {
        let (dir, sandbox, history) = workspace("ui-rewind-overwrite");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("change a").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        std::fs::write(dir.join("a.txt"), "my own edit\n").unwrap();
        let mut s = ui(&sandbox, &history);
        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        assert_eq!(read(dir.join("a.txt")), "one\n");
    }

    #[tokio::test]
    async fn the_next_message_tells_the_model_once() {
        let (dir, sandbox, history) = workspace("ui-rewind-context");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        history.begin_turn("change a").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        write(&history, &sandbox, "b.txt", "new\n").await;
        let mut s = ui(&sandbox, &history);
        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));

        let turn = s.take_turn("carry on").unwrap();
        assert_eq!(turn.display, "carry on");
        assert_eq!(
            turn.prompt,
            "carry on\n\n<files_rewound>\nThe user ran /rewind, which put files back as they \
             were before one of their earlier messages. The conversation was not rewound: \
             earlier messages still describe changes these files no longer have. Re-read a file \
             before relying on its content or editing it.\nBack to before the user's message \
             “change a”:\n- restored to their earlier content: a.txt\n- deleted (gz had created \
             them): b.txt\n</files_rewound>"
        );
        assert_eq!(turn.notes, vec!["Told gz you rewound 2 files".to_string()]);

        let next = s.take_turn("and then").unwrap();
        assert_eq!(next.prompt, "and then");
        assert!(next.notes.is_empty());
    }

    #[tokio::test]
    async fn diff_shows_each_file_from_before_gz_to_now() {
        let (dir, sandbox, history) = workspace("ui-diff");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("c.txt"), "untouched\n").unwrap();
        history.begin_turn("change").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        write(&history, &sandbox, "b.txt", "new\n").await;
        let mut s = ui(&sandbox, &history);

        assert!(run(&mut s, "/diff").is_empty());
        let tools: Vec<&ToolItem> = s
            .flush
            .iter()
            .filter_map(|item| match item {
                Item::Tool(tool) => Some(tool),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].label.target, "a.txt");
        let diff = tools[0].diff.as_deref().unwrap();
        assert!(diff.contains("-one\n+two"), "{diff}");
        assert_eq!(tools[0].result.as_deref(), Some("changed · +1 −1"));
        assert_eq!(tools[1].result.as_deref(), Some("created by gz · +1 −0"));
        assert!(s.viewer.is_none(), "short diffs stay inline");

        s.flush.clear();
        run(&mut s, "/diff b.txt");
        assert!(
            matches!(&s.flush[..], [Item::Notice(_), Item::Tool(t)] if t.label.target == "b.txt")
        );
        s.flush.clear();
        run(&mut s, "/diff c.txt");
        assert!(
            notices(&s)[0].starts_with("gz's tools have not changed c.txt"),
            "{:?}",
            notices(&s)
        );
    }

    #[tokio::test]
    async fn long_diffs_open_the_full_screen_view() {
        let (dir, sandbox, history) = workspace("ui-diff-long");
        history.begin_turn("write a lot").unwrap();
        let body: String = (0..100).map(|i| format!("line {i}\n")).collect();
        write(&history, &sandbox, "long.txt", &body).await;
        assert!(dir.join("long.txt").exists());
        let mut s = ui(&sandbox, &history);
        run(&mut s, "/diff");
        assert!(s.viewer.is_some());
    }

    #[test]
    fn the_full_screen_view_opens_at_the_start_of_the_diff() {
        let mut viewer = Viewer::open_on_last(50);
        viewer.clamp(200);
        assert_eq!(viewer.scroll, 150);
        let mut short = Viewer::open_on_last(5);
        short.clamp(200);
        assert_eq!(short.scroll, 180, "a short tail just shows the end");
    }

    #[test]
    fn huge_diffs_are_cut_and_say_so() {
        let s = state();
        let before: String = (0..3_000).map(|i| format!("old {i}\n")).collect();
        let diff = FileDiff {
            path: PathBuf::from("/w/big.txt"),
            change: Change::Text {
                before: Some(before),
                now: Some(String::new()),
            },
        };
        let mut budget = MAX_TOTAL_DIFF_LINES;
        let item = s.diff_item(&diff, &mut budget);
        assert!(item
            .result
            .unwrap()
            .ends_with("diff cut at 2000 of 3003 lines"));
        assert_eq!(item.diff.unwrap().lines().count(), MAX_FILE_DIFF_LINES);
        assert_eq!(budget, MAX_TOTAL_DIFF_LINES - MAX_FILE_DIFF_LINES);
    }

    #[test]
    fn rewind_says_why_it_cannot_run() {
        let mut s = state();
        run(&mut s, "/rewind");
        assert_eq!(notices(&s), vec![NO_HISTORY.to_string()]);
        s.flush.clear();
        let (_, _, history) = workspace("ui-rewind-empty");
        s.edit_history = Some(history);
        run(&mut s, "/rewind");
        assert!(
            notices(&s)[0].starts_with("Nothing to rewind"),
            "{:?}",
            notices(&s)
        );
        s.flush.clear();
        s.start_turn("work");
        run(&mut s, "/rewind");
        assert!(notices(&s).contains(&WAIT_FOR_TURN.to_string()));
    }
}
