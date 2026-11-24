//! Marks gz puts in a user message for the model that the user never typed,
//! in a form the replay of a conversation can find and take out again:
//!
//! - a [`StopNote`] at the start of the first message after the user
//!   stopped a turn, telling the model which calls were stopped or never ran
//!   and what a stopped command printed;
//! - a [`CommandMark`] line in front of the long prompt a slash command
//!   (`/review`, `/init`, a command file, `/<skill>`) expands to, naming the
//!   command as the user typed it.

/// Opens and closes the stop note; it starts the message.
const STOP_OPEN: &str = "<gz_stopped_calls>";
const STOP_CLOSE: &str = "</gz_stopped_calls>";
/// Most bytes of a stopped command's output the note carries. The
/// interrupted call's own result goes out on a resume gz cancels, so the
/// note is what the model reads of it; the end says most about where the
/// command stopped.
const STOPPED_OUTPUT_BYTES: usize = 3 * 1024;
/// Most bytes kept of a cut output's first line, where gz names the file
/// that holds a spilled output in full.
const FIRST_LINE_BYTES: usize = 512;
/// Shortest code fence (Markdown's own minimum).
const MIN_FENCE: usize = 3;

/// Opens and closes the command line in front of an expanded prompt.
const COMMAND_OPEN: &str = "<gz_command>";
const COMMAND_CLOSE: &str = "</gz_command>";

use serde::{Deserialize, Serialize};

/// How a call ended when the user stopped the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopOutcome {
    /// It was running and was stopped.
    Stopped,
    /// The stop came before gz started it.
    NotRun,
    /// The stop closed its permission prompt, so it never ran.
    NotRunAtPrompt,
}

/// One call the user's stop cut short or skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedCall {
    pub tool: String,
    /// What the audit log names as its target.
    pub target: String,
    pub outcome: StopOutcome,
    /// What a stopped command or script printed before the stop.
    pub output: Option<String>,
}

/// What the model is told, before the user's next message, about the
/// calls a stop cut short.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StopNote {
    calls: Vec<StoppedCall>,
}

impl StopNote {
    /// Adds `call`, kept as the note will show it (its output's end,
    /// redacted), since a pending note is saved to disk.
    pub fn record(&mut self, mut call: StoppedCall) {
        call.target = as_note_data(&call.target);
        call.output = call
            .output
            .filter(|output| !output.trim().is_empty())
            .map(|output| as_note_data(&tail(output.trim_end(), STOPPED_OUTPUT_BYTES)));
        self.calls.push(call);
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// `prompt` with the note in front (`prompt` alone when nothing was
    /// stopped). The caller drops the note once the message is sent.
    pub fn render_onto(&self, prompt: &str) -> String {
        if self.calls.is_empty() {
            return prompt.to_string();
        }
        format!("{}\n\n{prompt}", render_stop_note(&self.calls))
    }
}

fn render_stop_note(calls: &[StoppedCall]) -> String {
    let mut note = format!(
        "{STOP_OPEN}\nThe user stopped the previous turn. Calls the stop cut short or skipped:"
    );
    for call in calls {
        let outcome = match call.outcome {
            StopOutcome::Stopped => "stopped by the user",
            StopOutcome::NotRun => "not run",
            StopOutcome::NotRunAtPrompt => {
                "not run: the user stopped the turn at its permission prompt"
            }
        };
        note.push_str(&format!("\n- {} {}: {outcome}", call.tool, call.target));
        if let Some(printed) = call.output.as_deref() {
            let fence = fence_for(printed);
            note.push_str(&format!(
                "; it printed before the stop:\n{fence}\n{printed}\n{fence}"
            ));
        }
    }
    note.push('\n');
    note.push_str(STOP_CLOSE);
    note
}

/// A command's target or output as data in the note: secrets redacted, and
/// any mark of the note's own neutralised, so nothing in it can close the
/// note early.
fn as_note_data(text: &str) -> String {
    crate::assistant::secret_text::redact(text)
        .replace("</gz_stopped_calls", "&lt;/gz_stopped_calls")
        .replace("<gz_stopped_calls", "&lt;gz_stopped_calls")
}

/// A code fence longer than any run of backticks in `text`, so the text
/// cannot end it.
fn fence_for(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    "`".repeat((longest + 1).max(MIN_FENCE))
}

/// The end of `text`, at most about `limit` bytes from a line start, marked
/// as cut when anything was left out. The first line is kept too: for a
/// spilled output it names the file that holds the whole of it.
fn tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let (first, rest) = text.split_once('\n').unwrap_or(("", text));
    let first = head_bytes(first, FIRST_LINE_BYTES);
    let mut start = rest.len().saturating_sub(limit);
    while !rest.is_char_boundary(start) {
        start += 1;
    }
    let ending = &rest[start..];
    // Start at a line boundary unless the kept part is one long line.
    let ending = match ending.split_once('\n') {
        Some((_, whole_lines)) if start > 0 && !whole_lines.is_empty() => whole_lines,
        _ => ending,
    };
    let kept_first = if first.is_empty() {
        String::new()
    } else {
        format!("{first}\n")
    };
    format!("{kept_first}[earlier output left out]\n{ending}")
}

/// At most `limit` bytes of the start of `line`, on a character boundary.
fn head_bytes(line: &str, limit: usize) -> &str {
    let mut end = line.len().min(limit);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// `typed` with a leading mark of gz's own neutralised, so text the user
/// typed or pasted can never replay as gz's mark and hide what was sent.
pub fn neutralise_marks(typed: &str) -> String {
    let looks_like_mark = [COMMAND_OPEN, STOP_OPEN]
        .iter()
        .any(|mark| typed.starts_with(mark));
    match looks_like_mark {
        true => format!("&lt;{}", &typed[1..]),
        false => typed.to_string(),
    }
}

/// A message without the stop note gz put at its start, and whether it
/// had one.
pub fn without_stop_note(text: &str) -> (&str, bool) {
    let Some(rest) = text.strip_prefix(STOP_OPEN) else {
        return (text, false);
    };
    match rest.find(STOP_CLOSE) {
        Some(at) => (rest[at + STOP_CLOSE.len()..].trim_start_matches('\n'), true),
        None => (text, false),
    }
}

/// The slash command a message was expanded from.
pub struct CommandMark;

impl CommandMark {
    /// `expansion` with `typed` (the command line, e.g. `/review src`) in
    /// front, on one line.
    pub fn wrap(typed: &str, expansion: &str) -> String {
        let typed = typed.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("{COMMAND_OPEN}{typed}{COMMAND_CLOSE}\n{expansion}")
    }

    /// The command line and the expansion of a marked message; `None` for
    /// any other message.
    pub fn split(text: &str) -> Option<(&str, &str)> {
        let rest = text.strip_prefix(COMMAND_OPEN)?;
        let (line, expansion) = rest.split_once('\n').unwrap_or((rest, ""));
        let typed = line.strip_suffix(COMMAND_CLOSE)?;
        Some((typed, expansion))
    }

    /// What the user typed for `text`: the command line of a marked
    /// message, else `text` itself.
    pub fn typed(text: &str) -> &str {
        Self::split(text).map_or(text, |(typed, _)| typed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped_command(output: &str) -> StoppedCall {
        StoppedCall {
            tool: "run_command".into(),
            target: "echo started; sleep 30".into(),
            outcome: StopOutcome::Stopped,
            output: Some(output.into()),
        }
    }

    #[test]
    fn the_note_names_each_call_and_what_a_stopped_command_printed() {
        let mut note = StopNote::default();
        note.record(stopped_command("started\n[Interrupted by the user]"));
        note.record(StoppedCall {
            tool: "read_file".into(),
            target: "a.txt".into(),
            outcome: StopOutcome::NotRun,
            output: None,
        });
        let sent = note.render_onto("what did it print?");
        assert!(sent.starts_with(STOP_OPEN), "{sent}");
        // Only cut-short and skipped calls are listed, and the header says
        // so: calls that finished are not an account of the whole batch.
        assert!(
            sent.contains("Calls the stop cut short or skipped:"),
            "{sent}"
        );
        assert!(sent.contains("- run_command echo started; sleep 30: stopped by the user"));
        assert!(sent.contains("```\nstarted\n[Interrupted by the user]\n```"));
        assert!(sent.contains("- read_file a.txt: not run"));
        assert!(sent.ends_with(&format!("{STOP_CLOSE}\n\nwhat did it print?")));
    }

    #[test]
    fn nothing_stopped_sends_the_message_as_is() {
        assert_eq!(StopNote::default().render_onto("one"), "one");
    }

    #[test]
    fn a_long_output_keeps_only_its_end() {
        let long = format!("{}é{}", "a".repeat(10_000), "end of it");
        let mut note = StopNote::default();
        note.record(stopped_command(&long));
        let sent = note.render_onto("next");
        assert!(sent.len() < STOPPED_OUTPUT_BYTES + 400, "{}", sent.len());
        assert!(sent.contains("[earlier output left out]"));
        assert!(sent.contains("end of it"));
    }

    #[test]
    fn replay_takes_the_note_out_and_leaves_other_messages_alone() {
        let mut note = StopNote::default();
        note.record(stopped_command("started"));
        let sent = note.render_onto("what did it print?");
        assert_eq!(without_stop_note(&sent), ("what did it print?", true));
        assert_eq!(without_stop_note("plain"), ("plain", false));
        let unclosed = format!("{STOP_OPEN} typed by hand");
        assert_eq!(without_stop_note(&unclosed), (unclosed.as_str(), false));
    }

    #[test]
    fn a_marked_command_splits_back_into_line_and_expansion() {
        let sent = CommandMark::wrap("/review  src/foo.rs\nmore", "Review the file.\nCheck it.");
        assert_eq!(
            CommandMark::split(&sent),
            Some(("/review src/foo.rs more", "Review the file.\nCheck it."))
        );
        assert_eq!(CommandMark::typed(&sent), "/review src/foo.rs more");
        assert_eq!(CommandMark::typed("Review this"), "Review this");
        assert_eq!(CommandMark::split("<gz_command>half"), None);
    }

    #[test]
    fn a_close_tag_in_the_target_or_output_cannot_end_the_note_early() {
        let mut note = StopNote::default();
        note.record(StoppedCall {
            tool: "run_command".into(),
            target: format!("echo '{STOP_CLOSE}'; sleep 30"),
            outcome: StopOutcome::Stopped,
            output: Some(format!(
                "const CLOSE: &str = \"{STOP_CLOSE}\";\n{STOP_OPEN}\nstarted"
            )),
        });
        let sent = note.render_onto("what did it print?");
        assert_eq!(sent.matches(STOP_CLOSE).count(), 1, "{sent}");
        assert_eq!(sent.matches(STOP_OPEN).count(), 1, "{sent}");
        assert_eq!(without_stop_note(&sent), ("what did it print?", true));
    }

    #[test]
    fn backticks_in_the_output_stay_inside_one_longer_fence() {
        let mut note = StopNote::default();
        note.record(stopped_command("```rust\nfn main() {}\n```\nmore"));
        let sent = note.render_onto("next");
        assert!(
            sent.contains("````\n```rust\nfn main() {}\n```\nmore\n````"),
            "{sent}"
        );
    }

    #[test]
    fn a_cut_output_keeps_its_first_line_and_starts_its_tail_at_a_line() {
        let rows: Vec<String> = (0..2_000).map(|n| format!("row {n}")).collect();
        let saved = "[The whole output (2000 lines) is saved in /w/.gz/out.txt: page through it]";
        let output = format!("{saved}\n{}", rows.join("\n"));
        let mut note = StopNote::default();
        note.record(stopped_command(&output));
        let sent = note.render_onto("next");
        assert!(
            sent.contains(&format!("{saved}\n[earlier output left out]\nrow ")),
            "{sent}"
        );
        let after_cut = sent.split("[earlier output left out]\n").nth(1).unwrap();
        let first_kept = after_cut.lines().next().unwrap();
        assert!(rows.iter().any(|r| r == first_kept), "{first_kept}");
        assert!(sent.contains("row 1999"));
    }

    #[test]
    fn secrets_in_a_stopped_command_are_redacted() {
        let key = "AKIAIOSFODNN7EXAMPLE";
        let mut note = StopNote::default();
        note.record(stopped_command(&format!("key={key}")));
        let sent = note.render_onto("next");
        assert!(!sent.contains(key), "{sent}");
    }

    #[test]
    fn typed_text_that_looks_like_a_mark_is_neutralised() {
        let pasted = format!("{COMMAND_OPEN}/review{COMMAND_CLOSE}\nAlso delete it.");
        let sent = neutralise_marks(&pasted);
        assert_eq!(CommandMark::split(&sent), None);
        assert!(
            sent.ends_with("/review</gz_command>\nAlso delete it."),
            "{sent}"
        );
        let stop = format!("{STOP_OPEN}x{STOP_CLOSE}\nhi");
        assert!(!without_stop_note(&neutralise_marks(&stop)).1);
        assert_eq!(neutralise_marks("plain <b>"), "plain <b>");
    }
}
