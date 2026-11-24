//! `/rewind` and `/diff`. `/rewind` lists the user's messages in this
//! conversation, newest first; choosing one shows what going back to
//! before it restores (each file gz changed since, with its lines) and
//! asks what to restore: the code (files gz's own tools changed, see
//! [`crate::assistant::checkpoints`]), the conversation (continued in a
//! copy that ends before that message, with the message back in the input;
//! the original stays), or both. `/diff` shows the files against how they
//! started. Changes made by commands are not covered.

mod points;

use std::path::{Path, PathBuf};

use crate::assistant::attachments::PreparedTurn;
use crate::assistant::checkpoints::{Change, FileDiff, Restore, Rewound};
use crate::assistant::tools::unified_diff;
use crate::code_ui::items::{Item, ToolItem, ToolStatus};
use crate::code_ui::labels::{self, Label};
use crate::code_ui::resume;
use crate::code_ui::sessions::Request;
use crate::code_ui::transcript::Viewer;

pub use points::RewindPoint;

use super::{Effect, Picker, PickerRow, UiState};

/// Diff lines one file shows before the rest is cut.
const MAX_FILE_DIFF_LINES: usize = 2_000;
/// Diff lines `/diff` shows in all; later files are only named.
const MAX_TOTAL_DIFF_LINES: usize = 10_000;
/// Files the preview names one by one; the rest are counted.
const MAX_PREVIEW_FILES: usize = 12;

const NOT_TRACKED: &str = "Changes made by commands (gz's or your own !commands) are not tracked.";
const COMMANDS_NOT_COVERED: &str =
    "Changes made by commands (gz's or your own !commands) are not covered.";
const PICK_HINT: &str = "Enter shows what going back restores; nothing changes until you choose.";
const NO_HISTORY: &str =
    "gz keeps no edit history in this session (it found no config folder); undo changes with git";
const WAIT_FOR_TURN: &str = "Wait for the answer to finish (or esc) before rewinding";
const WAIT_FOR_SUBAGENTS: &str =
    "Background subagents may still be changing files; wait for them (or /subagents stop), then /rewind";
const DIFF_USAGE: &str = "Use /diff, or /diff <path> for one file";
const CONVERSATION_PREVIEW: &str = "Conversation: carries on in a copy that ends before this \
     message, with the message back in the input; the original stays (/resume opens it).";
const NO_CONVERSATION: &str =
    "Conversation: this is not one of your messages, so only files can go back to it.";

/// What a `/rewind` choice restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindAction {
    CodeAndConversation,
    Conversation,
    Code,
}

impl RewindAction {
    fn label(self) -> &'static str {
        match self {
            RewindAction::CodeAndConversation => "Restore code and conversation",
            RewindAction::Conversation => "Restore conversation only (files stay as they are)",
            RewindAction::Code => "Restore code only (the conversation stays as it is)",
        }
    }

    fn restores_code(self) -> bool {
        matches!(self, RewindAction::CodeAndConversation | RewindAction::Code)
    }

    fn restores_conversation(self) -> bool {
        matches!(
            self,
            RewindAction::CodeAndConversation | RewindAction::Conversation
        )
    }
}

/// One row of the `/rewind` pickers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindRow {
    /// A point to go back to; enter shows its preview.
    Message {
        point: Box<RewindPoint>,
        /// When it was sent, e.g. `5m ago`, when known.
        ago: Option<String>,
    },
    /// A choice in the preview.
    Action {
        point: Box<RewindPoint>,
        action: RewindAction,
    },
    /// What to do with files changed since gz wrote them.
    Resolve {
        turn: u64,
        overwrite: bool,
        /// How many files changed since gz wrote them.
        changed: usize,
        /// The conversation goes back too once the files have.
        then_conversation: Option<Box<RewindPoint>>,
    },
    Cancel,
}

impl RewindRow {
    pub fn label(&self) -> String {
        match self {
            RewindRow::Message { point, ago } => {
                let mut label = format!("“{}”", point.excerpt);
                if point.files > 0 {
                    label.push_str(&format!(" · {}", files_word(point.files)));
                }
                if let Some(ago) = ago {
                    label.push_str(&format!(" · {ago}"));
                }
                label
            }
            RewindRow::Action { action, .. } => action.label().into(),
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
    /// `/rewind`: the messages to go back to, newest first.
    pub(super) fn rewind_command(&mut self) -> Vec<Effect> {
        if self.refused_while_forking("/rewind") {
            return Vec::new();
        }
        if self.is_busy() {
            self.record(Item::Error(WAIT_FOR_TURN.into()));
            return Vec::new();
        }
        if !self.background.is_empty() {
            self.record(Item::Error(WAIT_FOR_SUBAGENTS.into()));
            return Vec::new();
        }
        let turns = match self.edit_history.clone().filter(|h| h.is_enabled()) {
            Some(history) => match history.messages() {
                Ok(turns) => turns,
                Err(err) => {
                    self.record(Item::Error(format!("{err:#}")));
                    return Vec::new();
                }
            },
            None => Vec::new(),
        };
        let points = points::rewind_points(&self.user_messages(), &turns);
        if points.is_empty() {
            self.record(Item::Notice(format!(
                "Nothing to rewind: this conversation has no messages of yours yet. {NOT_TRACKED}"
            )));
            return Vec::new();
        }
        let now = crate::assistant::history::now_millis();
        let mut rows: Vec<PickerRow> = points
            .into_iter()
            .map(|point| {
                let ago = point.at.map(|at| resume::ago(now.saturating_sub(at)));
                PickerRow::Rewind(RewindRow::Message {
                    point: Box::new(point),
                    ago,
                })
            })
            .collect();
        rows.push(PickerRow::Header(String::new()));
        rows.push(PickerRow::Header(PICK_HINT.into()));
        rows.push(PickerRow::Header(COMMANDS_NOT_COVERED.into()));
        self.picker = Some(Picker {
            title: "Rewind to before a message".into(),
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
            RewindRow::Message { point, .. } => {
                self.open_rewind_preview(*point);
                Vec::new()
            }
            RewindRow::Action { point, action } => self.rewind_with(*point, action),
            RewindRow::Resolve {
                turn,
                overwrite,
                then_conversation,
                ..
            } => {
                let then_conversation = then_conversation.map(|p| *p);
                let rewound_before = self.rewound.len();
                self.rewind_to(turn, overwrite, goes_back(then_conversation.as_ref()));
                self.rewind_conversation_after(then_conversation, rewound_before)
            }
            RewindRow::Cancel => {
                self.record(Item::Notice("Nothing was rewound".into()));
                Vec::new()
            }
        }
    }

    /// The confirmation step: what going back to `point` restores, then
    /// the choices.
    fn open_rewind_preview(&mut self, point: RewindPoint) {
        let mut rows: Vec<PickerRow> = self
            .code_preview(&point)
            .into_iter()
            .map(PickerRow::Header)
            .collect();
        rows.push(PickerRow::Header(
            if point.prompt.is_some() {
                CONVERSATION_PREVIEW
            } else {
                NO_CONVERSATION
            }
            .into(),
        ));
        rows.push(PickerRow::Header(String::new()));
        let first_choice = rows.len();
        let has_code = point.code_turn.is_some() && self.edit_history.is_some();
        let actions = [
            RewindAction::CodeAndConversation,
            RewindAction::Conversation,
            RewindAction::Code,
        ];
        rows.extend(
            actions
                .into_iter()
                .filter(|a| has_code || !a.restores_code())
                .filter(|a| point.prompt.is_some() || !a.restores_conversation())
                .map(|action| {
                    PickerRow::Rewind(RewindRow::Action {
                        point: Box::new(point.clone()),
                        action,
                    })
                }),
        );
        rows.push(PickerRow::Rewind(RewindRow::Cancel));
        self.picker = Some(Picker {
            title: format!("Rewind to before “{}”", point.excerpt),
            rows,
            selected: first_choice,
            model: None,
        });
    }

    /// `Code: 2 files go back` and a line per file with the lines it undoes.
    fn code_preview(&self, point: &RewindPoint) -> Vec<String> {
        let Some(history) = self.edit_history.clone().filter(|h| h.is_enabled()) else {
            return vec![format!("Code: {NO_HISTORY}")];
        };
        let Some(turn) = point.code_turn else {
            return vec!["Code: gz's tools changed no files since this message.".into()];
        };
        let (plan, diffs) = match (history.plan_rewind(turn), history.diffs_since(turn)) {
            (Ok(plan), Ok(diffs)) => (plan, diffs),
            (Err(err), _) | (_, Err(err)) => return vec![format!("Code: {err:#}")],
        };
        let mut lines = vec![format!("Code: {} go back", files_word(plan.len()))];
        for file in plan.iter().take(MAX_PREVIEW_FILES) {
            let name = self.shown(&file.path);
            let what = match &file.restore {
                Restore::Delete => "deleted (gz created it)".to_string(),
                Restore::Impossible(reason) => format!("can't go back: {reason}"),
                Restore::Content => diffs
                    .iter()
                    .find(|d| d.path == file.path)
                    .map_or_else(|| "restored".to_string(), |d| undone_lines(&name, d)),
            };
            let changed = if file.changed_since {
                " · changed since gz wrote it"
            } else {
                ""
            };
            lines.push(format!("  {name} · {what}{changed}"));
        }
        if plan.len() > MAX_PREVIEW_FILES {
            lines.push(format!("  … {} more", plan.len() - MAX_PREVIEW_FILES));
        }
        lines
    }

    /// Carries out a preview choice.
    fn rewind_with(&mut self, point: RewindPoint, action: RewindAction) -> Vec<Effect> {
        let then_conversation = action.restores_conversation().then(|| point.clone());
        let rewound_before = self.rewound.len();
        match point.code_turn.filter(|_| action.restores_code()) {
            Some(turn) => {
                if self.rewind_or_ask(turn, then_conversation.clone()) {
                    return Vec::new();
                }
                self.rewind_conversation_after(then_conversation, rewound_before)
            }
            None => self.rewind_conversation_after(then_conversation, rewound_before),
        }
    }

    /// Asks the session task to rewind the conversation to `point`. Files
    /// rewound in the same step (the rewound notes past `rewound_before`)
    /// need no note if it goes through: the copy never saw those changes.
    fn rewind_conversation_after(
        &mut self,
        point: Option<RewindPoint>,
        rewound_before: usize,
    ) -> Vec<Effect> {
        let Some(RewindPoint {
            prompt: Some(prompt),
            later_repeats,
            ..
        }) = point
        else {
            return Vec::new();
        };
        let effects = self.start_request(Request::RewindConversation {
            prompt,
            later_repeats,
        });
        self.keep_rewound_on_fork(rewound_before);
        effects
    }

    /// Rewinds files at once, unless some changed since gz wrote them:
    /// then lists them and asks (true: the question is open).
    fn rewind_or_ask(&mut self, turn: u64, then_conversation: Option<RewindPoint>) -> bool {
        let Some(history) = self.edit_history.clone() else {
            self.record(Item::Error(NO_HISTORY.into()));
            return false;
        };
        let plan = match history.plan_rewind(turn) {
            Ok(plan) => plan,
            Err(err) => {
                self.record(Item::Error(format!("{err:#}")));
                return false;
            }
        };
        let changed: Vec<PathBuf> = plan
            .into_iter()
            .filter(|f| f.changed_since && !matches!(f.restore, Restore::Impossible(_)))
            .map(|f| f.path)
            .collect();
        if changed.is_empty() {
            self.rewind_to(turn, false, goes_back(then_conversation.as_ref()));
            return false;
        }
        let mut rows: Vec<PickerRow> = changed
            .iter()
            .map(|path| PickerRow::Header(format!("  {}", self.shown(path))))
            .collect();
        rows.push(PickerRow::Header(String::new()));
        let first_choice = rows.len();
        let then = then_conversation.map(Box::new);
        rows.extend([
            PickerRow::Rewind(RewindRow::Resolve {
                turn,
                overwrite: false,
                changed: changed.len(),
                then_conversation: then.clone(),
            }),
            PickerRow::Rewind(RewindRow::Resolve {
                turn,
                overwrite: true,
                changed: changed.len(),
                then_conversation: then,
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
        true
    }

    /// Puts files back to before `turn`; `with_conversation` when the
    /// conversation goes back in the same step.
    fn rewind_to(&mut self, turn: u64, overwrite: bool, with_conversation: bool) {
        let Some(history) = self.edit_history.clone() else {
            return self.record(Item::Error(NO_HISTORY.into()));
        };
        match history.rewind(turn, overwrite) {
            Ok(done) => self.show_rewound(done, with_conversation),
            Err(err) => self.record(Item::Error(format!("{err:#}"))),
        }
    }

    fn show_rewound(&mut self, done: Rewound, with_conversation: bool) {
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
            // With the conversation, the copy never saw these changes, so
            // there is nothing to tell gz (unless the fork fails).
            let told = if with_conversation {
                ""
            } else {
                " · gz hears about it with your next message"
            };
            self.record(Item::Notice(format!(
                "✓ Files back to before “{}”: {}{told}",
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

/// `undoes +3 −1`: the lines gz added and removed since, which go.
fn undone_lines(name: &str, diff: &FileDiff) -> String {
    match &diff.change {
        Change::Text { before, now } => {
            let full = unified_diff(name, before.as_deref(), now.as_deref().unwrap_or(""));
            let (added, removed) = labels::diff_counts(&full);
            format!("undoes +{added} −{removed}")
        }
        Change::Unchanged => "already as it was".into(),
        Change::Binary => "restored (not text, so no line counts)".into(),
        Change::NotKept(reason) => format!("can't go back: {reason}"),
        Change::TooLarge => "restored (too large to count lines)".into(),
    }
}

/// Whether a rewind to `point` also takes the conversation back.
fn goes_back(point: Option<&RewindPoint>) -> bool {
    point.is_some_and(|p| p.prompt.is_some())
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
        assert!(picker.rows.contains(&PickerRow::Header(PICK_HINT.into())));
        assert!(picker
            .rows
            .contains(&PickerRow::Header(COMMANDS_NOT_COVERED.into())));

        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        let preview = s.picker.clone().expect("the preview asks first");
        assert_eq!(preview.title, "Rewind to before “tidy a”");
        let headers: Vec<String> = preview
            .rows
            .iter()
            .filter_map(|row| match row {
                PickerRow::Header(h) => Some(h.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(headers[0], "Code: 2 files go back");
        assert!(
            headers.contains(&"  a.txt · undoes +1 −1".to_string()),
            "{headers:?}"
        );
        assert!(headers.contains(&"  notes.md · deleted (gz created it)".to_string()));
        assert!(headers.contains(&NO_CONVERSATION.to_string()));
        assert_eq!(
            preview.rows[preview.selected],
            PickerRow::Rewind(RewindRow::Action {
                point: match &picker.rows[1] {
                    PickerRow::Rewind(RewindRow::Message { point, .. }) => point.clone(),
                    other => panic!("{other:?}"),
                },
                action: RewindAction::Code
            })
        );
        assert_eq!(
            read(dir.join("a.txt")),
            "three
",
            "nothing changes before a choice"
        );
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

    fn choose(s: &mut UiState, label: &str) -> Vec<Effect> {
        let picker = s.picker.clone().expect("a picker");
        let at = picker
            .rows
            .iter()
            .position(|r| matches!(r, PickerRow::Rewind(row) if row.label() == label))
            .unwrap_or_else(|| panic!("no {label:?} in {:?}", picker.rows));
        s.picker.as_mut().unwrap().selected = at;
        s.on_key(key(KeyCode::Enter))
    }

    #[test]
    fn every_message_can_rewind_the_conversation_into_a_copy() {
        let mut s = state();
        s.record(Item::User("plan the parser".into()));
        s.record(Item::UserWithContext {
            text: "explain @a.py".into(),
            notes: vec!["Read a.py (3 lines)".into()],
        });
        run(&mut s, "/rewind");
        let picker = s.picker.clone().unwrap();
        assert!(
            matches!(&picker.rows[0], PickerRow::Rewind(row) if row.label() == "“explain @a.py”")
        );
        s.on_key(key(KeyCode::Enter));
        let preview = s.picker.clone().unwrap();
        assert!(preview
            .rows
            .contains(&PickerRow::Header(CONVERSATION_PREVIEW.into())));
        assert!(
            !preview.rows.iter().any(|r| matches!(r, PickerRow::Rewind(RewindRow::Action { action, .. }) if action.restores_code())),
            "no edit history, so no code choice"
        );
        let effects = choose(&mut s, RewindAction::Conversation.label());
        assert_eq!(
            effects,
            vec![Effect::Session(Request::RewindConversation {
                prompt: "explain @a.py".into(),
                later_repeats: 0
            })]
        );
        let effects = s.session_reply(crate::code_ui::sessions::Reply::Forked(Ok(
            crate::code_ui::sessions::Forked {
                id: "f1".into(),
                name: "Parser (rewound)".into(),
                messages: 2,
                history_error: None,
            },
        )));
        assert!(effects.contains(&Effect::Resume {
            id: "f1".into(),
            title: "Parser (rewound)".into()
        }));
        assert_eq!(
            s.input_text(),
            "explain @a.py",
            "the message is back to edit"
        );
        assert!(notices(&s)
            .iter()
            .any(|n| n.starts_with("✓ Conversation rewound")));
    }

    #[test]
    fn the_preview_wraps_inside_its_box_at_any_width() {
        use unicode_width::UnicodeWidthStr;
        let mut s = state();
        s.record(Item::User(
            "a message long enough to need cutting in the title".into(),
        ));
        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));
        for width in [40, 60, 100] {
            let picker = s.picker.as_ref().unwrap();
            let lines = crate::code_ui::render::picker_lines(picker, &s, width);
            let text = crate::code_ui::transcript::plain(&lines);
            for line in &text {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
            assert!(
                text.iter().any(|l| l.contains("original stays")),
                "the whole note shows, wrapped: {text:#?}"
            );
            let words: String = text
                .join(" ")
                .replace(['│', '❯'], " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                words.contains("Restore conversation only (files stay as they are)"),
                "{width}: the restore option shows whole: {text:#?}"
            );
        }
    }

    #[tokio::test]
    async fn code_and_conversation_go_back_together() {
        let (dir, sandbox, history) = workspace("ui-rewind-both");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let mut s = ui(&sandbox, &history);
        s.record(Item::User("change a".into()));
        history.begin_turn("change a").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        s.record(Item::User("only talk".into()));

        run(&mut s, "/rewind");
        let labels: Vec<String> = s
            .picker
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .filter_map(|r| match r {
                PickerRow::Rewind(row) => Some(row.label()),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0], "“only talk”");
        assert!(labels[1].starts_with("“change a” · 1 file"), "{labels:?}");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        let effects = choose(&mut s, RewindAction::CodeAndConversation.label());
        assert_eq!(read(dir.join("a.txt")), "one\n");
        assert_eq!(
            effects,
            vec![Effect::Session(Request::RewindConversation {
                prompt: "change a".into(),
                later_repeats: 0
            })]
        );
        s.session_reply(crate::code_ui::sessions::Reply::Forked(Ok(
            crate::code_ui::sessions::Forked {
                id: "f2".into(),
                name: "x".into(),
                messages: 0,
                history_error: None,
            },
        )));
        let turn = s.take_turn("change a differently").unwrap();
        assert!(
            !turn.prompt.contains("<files_rewound>"),
            "the copy never saw the changes: {}",
            turn.prompt
        );
    }

    #[tokio::test]
    async fn an_earlier_code_rewind_is_still_told_after_a_conversation_rewind() {
        let (dir, sandbox, history) = workspace("ui-rewind-earlier");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let mut s = ui(&sandbox, &history);
        s.record(Item::User("change a".into()));
        history.begin_turn("change a").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        s.record(Item::User("only talk".into()));

        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        choose(&mut s, RewindAction::Code.label());
        assert!(notices(&s)
            .iter()
            .any(|n| n.contains("gz hears about it with your next message")));
        assert_eq!(read(dir.join("a.txt")), "one\n");

        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));
        choose(&mut s, RewindAction::Conversation.label());
        s.session_reply(crate::code_ui::sessions::Reply::Forked(Ok(
            crate::code_ui::sessions::Forked {
                id: "f2".into(),
                name: "x".into(),
                messages: 2,
                history_error: None,
            },
        )));
        let turn = s.take_turn("go on").unwrap();
        assert!(
            turn.prompt.contains("<files_rewound>"),
            "the copy still holds the turn whose edits were undone: {}",
            turn.prompt
        );
    }

    #[tokio::test]
    async fn a_rewind_with_the_conversation_does_not_promise_a_note() {
        let (dir, sandbox, history) = workspace("ui-rewind-no-promise");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let mut s = ui(&sandbox, &history);
        s.record(Item::User("change a".into()));
        history.begin_turn("change a").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));
        choose(&mut s, RewindAction::CodeAndConversation.label());
        let said = notices(&s);
        assert!(
            said.iter().any(|n| n.starts_with("✓ Files back")),
            "{said:?}"
        );
        assert!(
            !said.iter().any(|n| n.contains("gz hears about it")),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_conversation_rewind_still_tells_the_model_about_files() {
        let (dir, sandbox, history) = workspace("ui-rewind-fork-fails");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let mut s = ui(&sandbox, &history);
        s.record(Item::User("change a".into()));
        history.begin_turn("change a").unwrap();
        edit(&history, &sandbox, "a.txt", "one", "two").await;
        std::fs::write(dir.join("a.txt"), "mine\n").unwrap();

        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));
        choose(&mut s, RewindAction::CodeAndConversation.label());
        assert!(s.picker.is_some(), "the changed file is asked about first");
        let effects = choose(&mut s, "Put it back too (what changed in it since is lost)");
        assert_eq!(read(dir.join("a.txt")), "one\n");
        assert!(matches!(
            &effects[..],
            [Effect::Session(Request::RewindConversation { .. })]
        ));
        s.session_reply(crate::code_ui::sessions::Reply::Forked(Err(
            "HTTP 404: Conversation message not found".into(),
        )));
        let turn = s.take_turn("go on").unwrap();
        assert!(turn.prompt.contains("<files_rewound>"));
    }

    fn screen(s: &mut UiState, width: u16, height: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| crate::code_ui::render::draw_live(f, s))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn assert_picker_fits(s: &mut UiState, width: u16, height: u16, wanted: &str) {
        let rows = screen(s, width, height);
        let text = rows.join("\n");
        assert!(text.contains(wanted), "{width}x{height}:\n{text}");
        assert!(text.contains('❯'), "the selected row shows:\n{text}");
        assert!(text.contains("esc cancel"), "the key hint shows:\n{text}");
        assert!(
            rows.last().unwrap().contains('╰'),
            "the box closes:\n{text}"
        );
    }

    #[test]
    fn a_long_rewind_list_scrolls_inside_the_screen() {
        let mut s = state();
        for i in 0..30 {
            s.record(Item::User(format!("message {i}")));
        }
        run(&mut s, "/rewind");
        assert_picker_fits(&mut s, 40, 24, "message 29");
        assert_picker_fits(&mut s, 40, 12, "message 29");
        for _ in 0..29 {
            s.on_key(key(KeyCode::Down));
        }
        assert_picker_fits(&mut s, 40, 24, "message 0");
        assert_picker_fits(&mut s, 40, 12, "↑ ");
    }

    #[tokio::test]
    async fn a_tall_preview_keeps_its_choices_on_screen() {
        let (dir, sandbox, history) = workspace("ui-rewind-tall");
        let mut s = ui(&sandbox, &history);
        s.record(Item::User("change them all".into()));
        history.begin_turn("change them all").unwrap();
        for i in 0..14 {
            let name = format!("src/code_ui/state/a_long_file_name_{i}.rs");
            std::fs::create_dir_all(dir.join("src/code_ui/state")).unwrap();
            std::fs::write(dir.join(&name), "one\n").unwrap();
            edit(&history, &sandbox, &name, "one", "two").await;
        }
        run(&mut s, "/rewind");
        s.on_key(key(KeyCode::Enter));
        assert_picker_fits(&mut s, 40, 24, "Restore code and conversation");
        assert_picker_fits(&mut s, 40, 12, "Restore code and conversation");
        for _ in 0..4 {
            s.on_key(key(KeyCode::Down));
        }
        assert_picker_fits(&mut s, 40, 12, "❯ Cancel");
    }

    fn rewind_labels(s: &UiState) -> Vec<String> {
        s.picker
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .filter_map(|r| match r {
                PickerRow::Rewind(row) => Some(row.label()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn notes_sent_mid_answer_are_not_rewind_points() {
        use crate::assistant::history::{Message, Speaker, SteerNote, Transcript};
        let mut s = state();
        let mut answer = Message::new(Speaker::Gz, "one\ntwo");
        answer.steering = vec![SteerNote {
            text: "earlier note".into(),
            offset: Some(4),
        }];
        let transcript = Transcript {
            title: None,
            messages: vec![Message::new(Speaker::You, "earlier ask"), answer],
        };
        s.show_history("t", Ok(transcript));
        s.record(Item::User("ask".into()));
        s.record_note(Item::User("live note".into()));
        run(&mut s, "/rewind");
        assert_eq!(rewind_labels(&s), ["“ask”", "“earlier ask”"]);
    }

    #[test]
    fn rewind_says_why_it_cannot_run() {
        let mut s = state();
        run(&mut s, "/rewind");
        assert!(notices(&s)[0].starts_with("Nothing to rewind"));
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
