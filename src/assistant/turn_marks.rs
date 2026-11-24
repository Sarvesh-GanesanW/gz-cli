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
/// Most bytes of a stopped command's output the note carries; the model
/// already read the full (capped) result through the interrupted call.
const STOPPED_OUTPUT_BYTES: usize = 3 * 1024;

/// Opens and closes the command line in front of an expanded prompt.
const COMMAND_OPEN: &str = "<gz_command>";
const COMMAND_CLOSE: &str = "</gz_command>";

/// How a call ended when the user stopped the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// It was running (or waiting for permission) and was stopped.
    Stopped,
    /// The stop came before gz started it.
    NotRun,
}

/// One call the user's stop cut short or skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Default)]
pub struct StopNote {
    calls: Vec<StoppedCall>,
}

impl StopNote {
    pub fn record(&mut self, call: StoppedCall) {
        self.calls.push(call);
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
        "{STOP_OPEN}\nThe user stopped the previous turn. What happened to the calls in it:"
    );
    for call in calls {
        let outcome = match call.outcome {
            StopOutcome::Stopped => "stopped by the user",
            StopOutcome::NotRun => "not run",
        };
        note.push_str(&format!("\n- {} {}: {outcome}", call.tool, call.target));
        if let Some(output) = call.output.as_deref().filter(|o| !o.trim().is_empty()) {
            note.push_str(&format!(
                "; it printed before the stop:\n```\n{}\n```",
                tail(output.trim_end(), STOPPED_OUTPUT_BYTES)
            ));
        }
    }
    note.push('\n');
    note.push_str(STOP_CLOSE);
    note
}

/// The last `limit` bytes of `text` (on a character boundary), marked as
/// cut when anything was left out: the end of a stopped command's output
/// says most about where it stopped.
fn tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[earlier output left out]\n{}", &text[start..])
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
}
