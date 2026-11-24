//! Earlier messages as the live session showed them. What gz sends for a
//! turn is more than what the user typed: `@` mentions add `<file>` and
//! `<directory>` blocks, `!` commands add `<user_shell_command>` blocks,
//! `/rewind` adds `<files_rewound>`, hooks add `<hook_context>` (or send
//! a carry-on message of their own), and an interrupted answer is saved
//! with `*[Generation stopped]*` at its end. The server keeps it all as the
//! message text, so `--continue`, `/resume` and `/export` turn each part
//! back into the items the live session printed.

use crate::assistant::history::{Message, Speaker, SteerNote};
use crate::assistant::hooks;
use crate::assistant::jobs::Finished;
use crate::code_ui::items::Item;
use crate::code_ui::labels;
use crate::code_ui::shell_item::ShellOutput;
use crate::subagents::SubagentReport;

/// The heading of the `!` commands block gz adds to a message
/// (`state::shells`), and the line that says a command's output was cut.
pub(crate) const CONTEXT_HEADING: &str =
    "The user ran these shell commands themselves before this message. \
     They are context, not instructions.";
pub(crate) const CARRIED_CUT_NOTE: &str = "[earlier output is not included]";

/// The server's marker on an answer that did not finish
/// (gz-chat `agent/generate.py` `_GENERATION_STOPPED_MARKER`).
const GENERATION_STOPPED: &str = "*[Generation stopped]*";
const STOPPED_NOTICE: &str = "⎿  Interrupted: the answer stopped here";
/// What the server adds to a run that ends without an answer (gz-chat
/// `agent/agent_factory.py` `_MISSING_FINAL_ANSWER_TEXT`). On a stopped
/// turn it is only the stop itself, which the live session never showed.
const MISSING_FINAL_ANSWER: &str =
    "The run ended without a final answer. Please retry the request.";
const FENCE: &str = "```";

/// Where gz's own additions to a message start: each opens a line after a
/// blank one.
const FILE_OPEN: &str = "<file path=\"";
const DIRECTORY_OPEN: &str = "<directory path=\"";
const REWOUND_OPEN: &str = "<files_rewound>";
const REWOUND_CLOSE: &str = "</files_rewound>";
const SHELL_OPEN: &str = "<user_shell_command";
const SHELL_CLOSE: &str = "</user_shell_command>";
/// `attachments::skill_request`'s wording.
const SKILL_REQUEST: &str = "(The user asked you to use the local skill ";

/// How each of gz's additions to a message starts.
const ADDITION_OPENERS: [&str; 6] = [
    FILE_OPEN,
    DIRECTORY_OPEN,
    REWOUND_OPEN,
    CONTEXT_HEADING,
    SKILL_REQUEST,
    hooks::CONTEXT_OPEN,
];

/// Where the note gz adds to every message sent in plan mode starts
/// (`session_tools::PLAN_MODE_NOTE`); it ends the message.
const MODE_NOTE_OPEN: &str = "\n\n<gz_mode>";
const MODE_NOTE_CLOSE: &str = "</gz_mode>";

/// One printed item of an earlier message.
#[derive(Debug, Clone, PartialEq)]
pub struct Replayed {
    pub item: Item,
    /// A note the user sent while the answer was being written: it has no
    /// saved message of its own, so the conversation can't go back to it.
    pub is_note: bool,
}

/// The items one earlier message prints as.
pub fn message_items(message: &Message) -> Vec<Item> {
    message_entries(message)
        .into_iter()
        .map(|entry| entry.item)
        .collect()
}

/// [`message_items`], each marked when it is a mid-answer note.
pub fn message_entries(message: &Message) -> Vec<Replayed> {
    match message.speaker {
        Speaker::You => user_items(&message.text)
            .into_iter()
            .map(|item| Replayed {
                item,
                is_note: false,
            })
            .collect(),
        Speaker::Gz => answer_items(message),
    }
}

/// Who a message for the main agent comes from when the user didn't type
/// it: a subagent (its report) or a background command (its end).
pub(crate) fn update_source(text: &str) -> Option<String> {
    SubagentReport::name_in(text)
        .or_else(|| Finished::id_in(text).map(|id| format!("command {id}")))
}

/// The notice for a turn gz started to hand over reports: one or more
/// [`update_source`] messages joined together.
pub(crate) fn reads_updates_notice(message: &str) -> String {
    let sources: Vec<String> = message
        .match_indices('<')
        .filter_map(|(at, _)| update_source(&message[at..]))
        .collect();
    format!("⎿  gz reads the update from {}", sources.join(", "))
}

/// The notice for a report the model read as a note mid-answer.
pub(crate) fn read_update_notice(source: &str) -> String {
    format!("⎿  gz read the update from {source}")
}

/// A user message: the commands they ran first, then what they typed with
/// the same `⎿` notes the live session showed under it. A turn gz started
/// itself to hand over reports shows as the notice it showed live.
pub fn user_items(text: &str) -> Vec<Item> {
    let text = without_mode_note(text);
    if update_source(text).is_some() {
        return vec![Item::Notice(reads_updates_notice(text))];
    }
    if let Some(why) = text.strip_prefix(hooks::CARRY_ON_LEAD) {
        return vec![Item::Notice(carry_on_notice(why))];
    }
    let (text, hook_lines) = without_hook_context(text);
    let mut parsed = UserMessage::parse(text);
    if let Some(lines) = hook_lines {
        parsed.notes.push(hook_context_note(lines));
    }
    let mut items = parsed.commands;
    items.push(if parsed.notes.is_empty() {
        Item::User(parsed.typed)
    } else {
        Item::UserWithContext {
            text: parsed.typed,
            notes: parsed.notes,
        }
    });
    items
}

/// What the user typed, without gz's additions (for auto mode and titles).
pub fn typed_text(text: &str) -> String {
    UserMessage::parse(without_hook_context(without_mode_note(text)).0).typed
}

/// The line the live session showed for a turn a `stop` hook made the
/// agent carry on (the hook's own notice, without its command, which the
/// saved message doesn't keep).
fn carry_on_notice(why: &str) -> String {
    format!("↻ a stop hook asked the agent to carry on: {}", why.trim())
}

/// `text` without the `<hook_context>` block gz put at its end (before any
/// plan-mode note), and how many lines that block held.
fn without_hook_context(text: &str) -> (&str, Option<usize>) {
    let trimmed = text.trim_end();
    let Some(inside) = trimmed.strip_suffix(hooks::CONTEXT_CLOSE) else {
        return (text, None);
    };
    let opening = format!("\n\n{}\n", hooks::CONTEXT_OPEN);
    let Some(at) = inside.rfind(&opening) else {
        return (text, None);
    };
    let lines = inside[at + opening.len()..]
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    (&text[..at], Some(lines))
}

/// The note for what hooks added to a message; the live session showed
/// each hook's addition as its own notice.
fn hook_context_note(lines: usize) -> String {
    let plural = if lines == 1 { "" } else { "s" };
    format!("Hooks added {lines} line{plural} of context for the model")
}

/// `text` without the plan-mode note gz put at its end.
fn without_mode_note(text: &str) -> &str {
    let trimmed = text.trim_end();
    if !trimmed.ends_with(MODE_NOTE_CLOSE) {
        return text;
    }
    // The saved text loses its leading blank lines, so a message that was
    // only the note starts with the tag itself.
    let opening_tag = MODE_NOTE_OPEN.trim_start_matches('\n');
    match trimmed.rfind(MODE_NOTE_OPEN) {
        Some(at) => &text[..at],
        None if trimmed.starts_with(opening_tag) => "",
        None => text,
    }
}

/// An answer, split where the user's notes arrived, then the tools it
/// called and whether it was cut short.
fn answer_items(message: &Message) -> Vec<Replayed> {
    let (text, stopped) = without_stop_marker(&message.text);
    let mut items: Vec<Replayed> = Vec::new();
    let mut at = 0;
    let mut notes: Vec<&SteerNote> = message.steering.iter().collect();
    notes.sort_by_key(|n| n.offset.unwrap_or(usize::MAX));
    for note in notes {
        let cut = note
            .offset
            .map_or(text.len(), |o| floor_boundary(&text, o.min(text.len())));
        push_answer(&mut items, &text[at..cut.max(at)]);
        at = cut.max(at);
        items.extend(note_items(&note.text).into_iter().map(|item| Replayed {
            is_note: matches!(item, Item::User(_) | Item::UserWithContext { .. }),
            item,
        }));
    }
    push_answer(&mut items, &text[at..]);
    let mut notices = Vec::new();
    if !message.tools.is_empty() {
        notices.push(Item::Notice(tools_line(&message.tools)));
    }
    if stopped {
        notices.push(Item::Notice(STOPPED_NOTICE.into()));
    }
    items.extend(notices.into_iter().map(|item| Replayed {
        item,
        is_note: false,
    }));
    items
}

/// A note that arrived mid-answer: the user's words, or a report gz
/// passed on.
fn note_items(text: &str) -> Vec<Item> {
    match update_source(text) {
        Some(source) => vec![Item::Notice(read_update_notice(&source))],
        None => user_items(text),
    }
}

/// Answer text, as a new answer or (after a note) its continuation.
fn push_answer(items: &mut Vec<Replayed>, text: &str) {
    let text = text.trim_matches('\n');
    if text.trim().is_empty() {
        return;
    }
    let continues = items.iter().any(|i| matches!(i.item, Item::Assistant(_)));
    let item = if continues {
        Item::AssistantMore(text.to_string())
    } else {
        Item::Assistant(text.to_string())
    };
    items.push(Replayed {
        item,
        is_note: false,
    });
}

/// The answer without the server's stop marker (and the missing-answer
/// text it put before it), with any code block it left open closed, so the
/// marker's notice prints outside it.
fn without_stop_marker(text: &str) -> (String, bool) {
    let trimmed = text.trim_end();
    let Some(body) = trimmed.strip_suffix(GENERATION_STOPPED) else {
        return (text.to_string(), false);
    };
    let body = body.trim_end();
    let mut body = body
        .strip_suffix(MISSING_FINAL_ANSWER)
        .unwrap_or(body)
        .trim_end()
        .to_string();
    let fences = body
        .lines()
        .filter(|l| l.trim_start().starts_with(FENCE))
        .count();
    if fences % 2 == 1 {
        body.push('\n');
        body.push_str(FENCE);
    }
    (body, true)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// `⎿  Called Read ×2, Bash`: the tools in the order first used.
fn tools_line(names: &[String]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for name in names {
        let verb = labels::label(name, name).verb;
        match counts.iter_mut().find(|(v, _)| *v == verb) {
            Some((_, n)) => *n += 1,
            None => counts.push((verb, 1)),
        }
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(verb, n)| {
            if n == 1 {
                verb
            } else {
                format!("{verb} ×{n}")
            }
        })
        .collect();
    format!("⎿  Called {}", parts.join(", "))
}

/// A user message taken apart.
#[derive(Debug, Default)]
struct UserMessage {
    typed: String,
    notes: Vec<String>,
    commands: Vec<Item>,
}

impl UserMessage {
    fn parse(text: &str) -> Self {
        let start = first_addition(text);
        let mut parsed = Self {
            typed: text[..start].trim_end().to_string(),
            ..Self::default()
        };
        let mut rest = &text[start..];
        while let Some(next) = parsed.take_addition(rest.trim_start_matches('\n')) {
            rest = next;
        }
        let leftover = rest.trim();
        if !leftover.is_empty() {
            // Not gz's: keep it rather than lose what the user wrote.
            parsed.typed = format!("{}\n\n{leftover}", parsed.typed);
        }
        parsed
    }

    /// Reads one addition at the start of `rest` and returns what follows
    /// it; `None` when `rest` doesn't start with one.
    fn take_addition<'a>(&mut self, rest: &'a str) -> Option<&'a str> {
        if rest.starts_with(FILE_OPEN) {
            return self.take_file(rest);
        }
        if rest.starts_with(DIRECTORY_OPEN) {
            return self.take_directory(rest);
        }
        if rest.starts_with(REWOUND_OPEN) {
            return self.take_rewound(rest);
        }
        if let Some(after) = rest.strip_prefix(CONTEXT_HEADING) {
            return Some(self.take_shell_commands(after));
        }
        if let Some(after) = rest.strip_prefix(SKILL_REQUEST) {
            return self.take_skill_request(after);
        }
        if let Some(after) = rest.strip_prefix(hooks::CONTEXT_OPEN) {
            return self.take_hook_context(after);
        }
        None
    }

    /// The hook context block, always the last addition, as one note. Its
    /// last closing tag ends it, so hook output that mentions the tag
    /// doesn't cut it short.
    fn take_hook_context<'a>(&mut self, after: &'a str) -> Option<&'a str> {
        let end = after.rfind(hooks::CONTEXT_CLOSE)?;
        let lines = after[..end]
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        self.notes.push(hook_context_note(lines));
        Some(&after[end + hooks::CONTEXT_CLOSE.len()..])
    }

    /// `<file path="a.py">…</file>` as `Read a.py (12 lines)`.
    fn take_file<'a>(&mut self, rest: &'a str) -> Option<&'a str> {
        let (path, body, after) = element(rest, FILE_OPEN, "</file>")?;
        let lines = body.lines().count();
        let note = match more_count(body, "[… ", " more lines not included]") {
            Some(more) => format!(
                "Read {path} (first {} of {} lines)",
                lines - 1,
                lines - 1 + more
            ),
            None => format!("Read {path} ({lines} lines)"),
        };
        self.notes.push(note);
        Some(after)
    }

    /// `<directory path="src/">…</directory>` as `Listed src/ (4 entries)`.
    fn take_directory<'a>(&mut self, rest: &'a str) -> Option<&'a str> {
        let (path, body, after) = element(rest, DIRECTORY_OPEN, "</directory>")?;
        let listed = body.lines().filter(|l| !l.is_empty()).count();
        let entries = match more_count(body, "[… ", " more]") {
            Some(more) => listed - 1 + more,
            None => listed,
        };
        self.notes
            .push(format!("Listed {path} ({entries} entries)"));
        Some(after)
    }

    /// `<files_rewound>…</files_rewound>` as `Told gz you rewound 2 files`.
    fn take_rewound<'a>(&mut self, rest: &'a str) -> Option<&'a str> {
        let end = rest.find(REWOUND_CLOSE)?;
        let files: usize = rest[..end]
            .lines()
            .filter_map(|l| {
                l.strip_prefix("- restored to their earlier content: ")
                    .or_else(|| l.strip_prefix("- deleted (gz had created them): "))
            })
            .map(|list| list.split(", ").count())
            .sum();
        let word = if files == 1 { "file" } else { "files" };
        self.notes
            .push(format!("Told gz you rewound {files} {word}"));
        Some(&rest[end + REWOUND_CLOSE.len()..])
    }

    /// The `<user_shell_command>` blocks after their heading, each as the
    /// `! command` item the user saw when they ran it.
    fn take_shell_commands<'a>(&mut self, mut rest: &'a str) -> &'a str {
        let mut count = 0;
        loop {
            let trimmed = rest.trim_start_matches('\n');
            let Some((command, after)) = shell_command(trimmed) else {
                break;
            };
            self.commands.push(command);
            count += 1;
            rest = after;
        }
        if count > 0 {
            let plural = if count == 1 { "" } else { "s" };
            self.notes
                .push(format!("Shared {count} shell command{plural} you ran"));
        }
        rest
    }

    /// The skill request line as one `Using skill …` note per skill.
    fn take_skill_request<'a>(&mut self, after: &'a str) -> Option<&'a str> {
        let end = after.find(')')?;
        let (listed, _) = after[..end].split_once(':')?;
        for name in listed.split(", ") {
            self.notes
                .push(format!("Using skill {}", name.trim().trim_matches('`')));
        }
        Some(&after[end + 1..])
    }
}

/// Where gz's additions start: the first one that opens a line after a
/// blank line, or the end.
fn first_addition(text: &str) -> usize {
    ADDITION_OPENERS
        .iter()
        .filter_map(|opener| {
            let at = text.find(&format!("\n\n{opener}"))?;
            Some(at)
        })
        .min()
        .unwrap_or(text.len())
}

/// `<tag path="…">body</tag>`: the path, the body and what follows. The
/// body ends at the first closing tag that ends a line and is followed by
/// another addition or the end, so a closing tag inside a file's own text
/// doesn't cut it short.
fn element<'a>(rest: &'a str, open: &str, close: &str) -> Option<(String, &'a str, &'a str)> {
    let after_open = &rest[open.len()..];
    let path_end = after_open.find("\">\n")?;
    let path = after_open[..path_end].to_string();
    let body_start = path_end + 3;
    let body = &after_open[body_start..];
    let mut from = 0;
    while let Some(found) = body[from..].find(close) {
        let at = from + found;
        let after = &body[at + close.len()..];
        let at_line_start = at == 0 || body[..at].ends_with('\n');
        if at_line_start && ends_an_addition(after) {
            return Some((path, &body[..at], after));
        }
        from = at + close.len();
    }
    None
}

fn ends_an_addition(after: &str) -> bool {
    let next = after.trim_start_matches('\n');
    next.trim().is_empty()
        || ADDITION_OPENERS
            .iter()
            .any(|opener| next.starts_with(opener))
}

/// N from a last line like `[… N more lines not included]`.
fn more_count(body: &str, prefix: &str, suffix: &str) -> Option<usize> {
    body.lines()
        .last()?
        .strip_prefix(prefix)?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

/// One `<user_shell_command id="…" command="…" exit="…">…</…>` block.
fn shell_command(rest: &str) -> Option<(Item, &str)> {
    let after_open = rest.strip_prefix(SHELL_OPEN)?;
    let head_end = after_open.find(">\n")?;
    let head = &after_open[..head_end];
    let command = unescape(&attribute(head, "command")?);
    let exit = attribute(head, "exit").unwrap_or_default();
    let body = &after_open[head_end + 2..];
    let close = body.find(SHELL_CLOSE)?;
    let mut output = body[..close].trim_end_matches('\n');
    let is_cut = output.starts_with(CARRIED_CUT_NOTE);
    if is_cut {
        output = output[CARRIED_CUT_NOTE.len()..].trim_start_matches('\n');
    }
    let item = Item::UserCommand {
        command,
        output: ShellOutput {
            status: exit_status(&exit),
            text: output.replace("&lt;/user_shell_command", "</user_shell_command"),
            is_cut,
            is_error: !matches!(exit.as_str(), "0" | "running"),
        },
    };
    Some((item, &body[close + SHELL_CLOSE.len()..]))
}

/// A double-quoted attribute's raw value.
fn attribute(head: &str, name: &str) -> Option<String> {
    let start = head.find(&format!(" {name}=\""))? + name.len() + 3;
    let end = head[start..].find('"')?;
    Some(head[start..start + end].to_string())
}

/// Undoes `shells::attribute`'s escaping.
fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// The status line the live session showed for an exit label
/// (`assistant::jobs::State::exit_label`).
fn exit_status(exit: &str) -> String {
    match exit {
        "running" => "running".into(),
        "signal" => "ended by a signal".into(),
        "stopped" => "stopped".into(),
        "failed" => "failed".into(),
        code => format!("exit code {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes_of(items: &[Item]) -> (String, Vec<String>) {
        match items.last() {
            Some(Item::UserWithContext { text, notes }) => (text.clone(), notes.clone()),
            Some(Item::User(text)) => (text.clone(), Vec::new()),
            other => panic!("not a user item: {other:?}"),
        }
    }

    #[test]
    fn file_and_folder_blocks_become_the_live_notes() {
        let text = "Look at @invtrack/pricing.py  and answer.\n  What does it return?\n\n\
            <file path=\"invtrack/pricing.py\">\ndef f():\n    \"\"\"Doc.\"\"\"\n    return 1\n</file>\n\
            <file path=\"big.txt\">\na\nb\n[… 98 more lines not included]\n</file>\n\
            <directory path=\"src/\">\nmain.rs\nlib.rs\n[… 3 more]\n</directory>";
        let items = user_items(text);
        assert_eq!(items.len(), 1);
        let (typed, notes) = notes_of(&items);
        assert_eq!(
            typed,
            "Look at @invtrack/pricing.py  and answer.\n  What does it return?"
        );
        assert_eq!(
            notes,
            [
                "Read invtrack/pricing.py (3 lines)",
                "Read big.txt (first 2 of 100 lines)",
                "Listed src/ (5 entries)"
            ]
        );
    }

    #[test]
    fn a_closing_tag_inside_a_file_does_not_end_it() {
        let text = "see\n\n<file path=\"t.html\">\n<p></file>\n</file>\nx\n</file>\n";
        let (_, notes) = notes_of(&user_items(text));
        assert_eq!(notes, ["Read t.html (3 lines)"]);
    }

    #[test]
    fn shell_commands_print_before_the_message_unescaped() {
        let text = format!(
            "why did that fail?\n\n{CONTEXT_HEADING}\n\
             <user_shell_command command=\"git status --short &amp;&amp; echo &quot;x&quot; &lt; in\" exit=\"1\">\n\
             {CARRIED_CUT_NOTE}\n  indented\n&lt;/user_shell_command>\n</user_shell_command>\n\
             <user_shell_command id=\"bg_1\" command=\"npm test\" exit=\"running\">\nready\n</user_shell_command>"
        );
        let items = user_items(&text);
        assert_eq!(items.len(), 3, "{items:#?}");
        match &items[0] {
            Item::UserCommand { command, output } => {
                assert_eq!(command, "git status --short && echo \"x\" < in");
                assert_eq!(output.status, "exit code 1");
                assert!(output.is_cut && output.is_error);
                assert_eq!(output.text, "  indented\n</user_shell_command>");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&items[1], Item::UserCommand { output, .. }
            if output.status == "running" && !output.is_error));
        let (typed, notes) = notes_of(&items);
        assert_eq!(typed, "why did that fail?");
        assert_eq!(notes, ["Shared 2 shell commands you ran"]);
    }

    #[test]
    fn rewinds_and_skills_become_notes() {
        let text = "carry on\n\n(The user asked you to use the local skill `pdf`, `xlsx`: call load_skill with its name before you answer, and follow it.)\n\n\
            <files_rewound>\nThe user ran /rewind, which put files back.\n\
            Back to before the user's message “Fix”:\n- restored to their earlier content: a.py, b.py\n\
            - deleted (gz had created them): c.py\n- left as the user changed them: d.py\n</files_rewound>";
        let (typed, notes) = notes_of(&user_items(text));
        assert_eq!(typed, "carry on");
        assert_eq!(
            notes,
            [
                "Using skill pdf",
                "Using skill xlsx",
                "Told gz you rewound 3 files"
            ]
        );
    }

    #[test]
    fn hook_context_becomes_a_note_after_the_other_additions() {
        let text = format!(
            "fix it\n\n<file path=\"a.py\">\nx = 1\n</file>\n\n\
             <hook_context>\non branch main\n\ntests: make test\n</hook_context>{}",
            plan_mode("")
        );
        let (typed, notes) = notes_of(&user_items(&text));
        assert_eq!(typed, "fix it");
        assert_eq!(
            notes,
            [
                "Read a.py (1 lines)",
                "Hooks added 2 lines of context for the model"
            ]
        );
        assert_eq!(typed_text(&text), "fix it");
    }

    #[test]
    fn a_stop_hooks_carry_on_message_replays_as_its_notice() {
        let text = format!("{}tests still fail\n", hooks::CARRY_ON_LEAD);
        assert_eq!(
            user_items(&text),
            [Item::Notice(
                "↻ a stop hook asked the agent to carry on: tests still fail".into()
            )]
        );
    }

    #[test]
    fn canned_prompts_replay_as_the_live_session_showed_them() {
        // /review, /init and command files send their text as the message
        // and the live session shows that same text, with no markup.
        let review = crate::code_ui::review::prompt(
            crate::code_ui::review::Focus::General,
            &crate::code_ui::review::Target::parse("").unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(user_items(&review), [Item::User(review.clone())]);
        let skill = crate::code_ui::custom_commands::skill_prompt("pdf", "");
        let (typed, notes) = notes_of(&user_items(&skill));
        assert_eq!(
            (typed.as_str(), notes),
            ("Use the pdf skill.", vec!["Using skill pdf".to_string()])
        );
    }

    #[test]
    fn plain_messages_keep_their_markup_and_indentation() {
        let text = "keep <b>this</b> &amp; that\n    indented\n<file path=\"x\"> inline";
        assert_eq!(user_items(text), [Item::User(text.into())]);
    }

    #[test]
    fn a_stopped_answer_closes_its_code_block_before_the_notice() {
        let message = Message::new(
            Speaker::Gz,
            "Here:\n```python\nprint(1)\n\n*[Generation stopped]*",
        );
        let items = message_items(&message);
        assert_eq!(
            items,
            [
                Item::Assistant("Here:\n```python\nprint(1)\n```".into()),
                Item::Notice(STOPPED_NOTICE.into())
            ]
        );
        let bare = Message::new(Speaker::Gz, "*[Generation stopped]*");
        assert_eq!(message_items(&bare), [Item::Notice(STOPPED_NOTICE.into())]);
    }

    #[test]
    fn a_stopped_turn_without_an_answer_shows_only_its_tools_and_the_stop() {
        let mut message = Message::new(
            Speaker::Gz,
            "The run ended without a final answer. Please retry the request.\n\n*[Generation stopped]*",
        );
        message.tools = vec!["run_command".into()];
        let items = message_items(&message);
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(matches!(&items[0], Item::Notice(t) if t.starts_with("⎿  Called ")));
        assert_eq!(items[1], Item::Notice(STOPPED_NOTICE.into()));
    }

    #[test]
    fn a_stopped_turn_keeps_its_partial_answer_without_the_retry_text() {
        let message = Message::new(
            Speaker::Gz,
            "Partial answer\n\nThe run ended without a final answer. Please retry the request.\n\n*[Generation stopped]*",
        );
        assert_eq!(
            message_items(&message),
            [
                Item::Assistant("Partial answer".into()),
                Item::Notice(STOPPED_NOTICE.into())
            ]
        );
    }

    #[test]
    fn the_retry_text_stays_on_a_turn_that_was_not_stopped() {
        let text = "The run ended without a final answer. Please retry the request.";
        let message = Message::new(Speaker::Gz, text);
        assert_eq!(message_items(&message), [Item::Assistant(text.into())]);
        let other = Message::new(
            Speaker::Gz,
            "The run ended without a final answer.\n\n*[Generation stopped]*",
        );
        assert_eq!(
            message_items(&other)[0],
            Item::Assistant("The run ended without a final answer.".into())
        );
    }

    #[test]
    fn steering_notes_split_the_answer_where_they_arrived() {
        let mut message = Message::new(Speaker::Gz, "First part.\nSecond part.");
        message.steering = vec![SteerNote {
            text: "Also add one to main().".into(),
            offset: Some("First part.\n".len()),
        }];
        message.tools = vec!["read_file".into(), "read_file".into(), "run_command".into()];
        let items = message_items(&message);
        assert_eq!(items[0], Item::Assistant("First part.".into()));
        assert_eq!(items[1], Item::User("Also add one to main().".into()));
        assert_eq!(items[2], Item::AssistantMore("Second part.".into()));
        assert!(
            matches!(&items[3], Item::Notice(g) if g.starts_with("⎿  Called ") && g.contains("×2"))
        );
    }

    fn plan_mode(prompt: &str) -> String {
        format!(
            "{prompt}\n\n{}",
            crate::assistant::session_tools::PLAN_MODE_NOTE
        )
    }

    #[test]
    fn the_plan_mode_note_is_not_shown_or_matched() {
        let sent = plan_mode("plan the fix\n\n<file path=\"a\">\nx\n</file>\n");
        let (typed, notes) = notes_of(&user_items(&sent));
        assert_eq!(typed, "plan the fix");
        assert_eq!(notes, ["Read a (1 lines)"]);
        assert_eq!(typed_text(&plan_mode("just this")), "just this");
        assert_eq!(user_items(&plan_mode("hi")), [Item::User("hi".into())]);
    }

    fn subagent_report(name: &str) -> String {
        SubagentReport {
            id: "sa_1".into(),
            name: name.into(),
            status: crate::subagents::Status::Completed,
            text: "Found <three> callers.".into(),
        }
        .message()
    }

    fn command_notice() -> String {
        Finished {
            id: "bg_2".into(),
            command: "npm test".into(),
            exit: "1".into(),
            git_reverts: Default::default(),
        }
        .message()
    }

    #[test]
    fn report_turns_show_as_the_live_notice() {
        let turn = format!("{}\n\n{}", subagent_report("scout"), command_notice());
        let expected = Item::Notice("⎿  gz reads the update from scout, command bg_2".into());
        assert_eq!(user_items(&turn), [expected.clone()]);
        assert_eq!(user_items(&plan_mode(&turn)), [expected]);
    }

    #[test]
    fn reports_read_as_notes_show_as_the_live_notice() {
        let mut message = Message::new(Speaker::Gz, "Before.\nAfter.");
        message.steering = vec![SteerNote {
            text: subagent_report("scout"),
            offset: Some("Before.\n".len()),
        }];
        assert_eq!(
            message_items(&message),
            [
                Item::Assistant("Before.".into()),
                Item::Notice("⎿  gz read the update from scout".into()),
                Item::AssistantMore("After.".into()),
            ]
        );
    }

    #[test]
    fn only_mid_answer_notes_are_marked_as_notes() {
        let mut message = Message::new(Speaker::Gz, "a\nb");
        message.steering = vec![SteerNote {
            text: "note".into(),
            offset: Some(2),
        }];
        let marked: Vec<bool> = message_entries(&message)
            .iter()
            .map(|e| e.is_note)
            .collect();
        assert_eq!(marked, [false, true, false]);
        let asked = Message::new(Speaker::You, "note");
        assert!(message_entries(&asked).iter().all(|e| !e.is_note));
    }

    #[test]
    fn typed_text_drops_the_additions() {
        assert_eq!(typed_text("hi\n\n<file path=\"a\">\nx\n</file>\n"), "hi");
    }

    #[test]
    fn a_stop_hook_carry_on_replays_as_a_notice() {
        let sent = format!("{}keep going", hooks::CARRY_ON_LEAD);
        assert_eq!(
            user_items(&sent),
            [Item::Notice(
                "↻ a stop hook asked the agent to carry on: keep going".into()
            )]
        );
    }

    #[test]
    fn hook_context_replays_as_a_note_not_as_typed_text() {
        let sent = "fix it\n\n<file path=\"a\">\nx\n</file>\n\n<hook_context>\nbranch is main\n\nsays </hook_context> too\n</hook_context>";
        assert_eq!(typed_text(sent), "fix it");
        assert_eq!(
            user_items(sent),
            [Item::UserWithContext {
                text: "fix it".into(),
                notes: vec![
                    "Read a (1 lines)".into(),
                    "Hooks added 2 lines of context for the model".into(),
                ],
            }]
        );
        let alone = "hello\n\n<hook_context>\nbranch-is-main\n</hook_context>";
        assert_eq!(typed_text(alone), "hello");
    }

    #[test]
    fn what_attachments_send_replays_as_the_live_message() {
        let (dir, sandbox) = crate::assistant::sandbox::tests::temp_workspace("replay");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.py"), "def f():\n    return 1\n").unwrap();
        let typed = "explain @src/a.py and @src";
        let turn = crate::assistant::attachments::prepare(&sandbox, typed, &[], &[]).unwrap();
        assert_ne!(turn.prompt, typed, "the prompt carries the blocks");
        assert_eq!(
            user_items(&turn.prompt),
            [Item::UserWithContext {
                text: turn.display.clone(),
                notes: turn.notes.clone(),
            }]
        );
    }
}
