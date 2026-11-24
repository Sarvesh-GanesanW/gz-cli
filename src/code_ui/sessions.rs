//! Renaming and forking the conversation, run on the session task so a
//! message typed after a fork can only reach the fork. The UI side is
//! `state::sessions`.

use anyhow::{bail, Context, Result};

use crate::assistant::history::{self, ForkOptions, Index};
use crate::assistant::session::Session;
use crate::code_ui::replay;
use crate::code_ui::state::Effect;
use crate::http::ApiClient;

const NOT_SAVED: &str =
    "This conversation has no messages yet, so there is nothing to rename or fork; send a message first";

/// What the UI asks of the session task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `/rename <name>`.
    Rename(String),
    /// `/fork [name]`: copy the whole conversation and continue in the copy.
    Fork { name: Option<String> },
    /// `/rewind` of the conversation: copy it up to just before one of the
    /// user's messages and continue in the copy.
    RewindConversation {
        /// What the user typed in that message.
        prompt: String,
        /// How many later messages of theirs say exactly the same, so a
        /// repeated prompt finds the right one.
        later_repeats: usize,
    },
}

impl Request {
    /// The line the UI shows while the request runs.
    pub fn progress(&self) -> String {
        match self {
            Request::Rename(name) => format!("Renaming the conversation to “{name}”…"),
            Request::Fork { .. } => "Forking the conversation…".into(),
            Request::RewindConversation { .. } => "Rewinding the conversation…".into(),
        }
    }
}

/// A conversation a fork made, now the session's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forked {
    pub id: String,
    pub name: String,
    /// Messages copied into it.
    pub messages: usize,
    /// Why gz's edit history couldn't be copied into it, if it couldn't.
    pub history_error: Option<String>,
}

/// The session task's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Renamed(Result<String, String>),
    Forked(Result<Forked, String>),
}

/// Carries out `request` on `session`; a fork leaves the session in the
/// new conversation.
pub async fn handle(session: &mut Session, http: &ApiClient, request: Request) -> Reply {
    let text = |err: anyhow::Error| format!("{err:#}");
    match request {
        Request::Rename(name) => Reply::Renamed(rename(session, http, &name).await.map_err(text)),
        Request::Fork { name } => {
            let options = ForkOptions {
                name,
                ..ForkOptions::default()
            };
            Reply::Forked(fork_into(session, http, &options).await.map_err(text))
        }
        Request::RewindConversation {
            prompt,
            later_repeats,
        } => Reply::Forked(
            rewind_conversation(session, http, &prompt, later_repeats)
                .await
                .map_err(text),
        ),
    }
}

async fn rename(session: &Session, http: &ApiClient, name: &str) -> Result<String> {
    if !session.is_saved() {
        bail!(NOT_SAVED);
    }
    let id = session.conversation_id().to_string();
    let renamed = history::rename(http, &id, name).await?;
    if let Ok(index) = Index::open() {
        // Best effort: the server has the name; the index only lists it.
        let _ = index.rename(&id, &renamed);
    }
    Ok(renamed)
}

/// After a resumed conversation loads: the index learns its exact message
/// count and the server's title, so `/resume` lists them.
pub fn note_loaded(id: &str, transcript: &Result<history::Transcript, String>) {
    let (Ok(transcript), Ok(index)) = (transcript, Index::open()) else {
        return;
    };
    // Best effort: the index only feeds the `/resume` list.
    let _ = index.refresh(
        id,
        transcript.messages.len() as u64,
        transcript.title.as_deref(),
    );
}

/// `--fork-session`: the conversation `start` resumes is forked first and
/// the fork resumed instead; with `--resume`, the one picked is forked.
/// `picks` says a conversation was asked for at all.
pub async fn fork_at_start(
    session: &mut Session,
    http: &ApiClient,
    start: Vec<Effect>,
    picks: bool,
) -> Result<Vec<Effect>> {
    if !picks {
        bail!("--fork-session forks an earlier conversation; add --continue, --resume or -c <id>");
    }
    let mut forked_start = Vec::with_capacity(start.len() + 1);
    for effect in start {
        match effect {
            Effect::Resume { title, .. } => {
                let forked = fork_into(session, http, &ForkOptions::default())
                    .await
                    .with_context(|| format!("cannot fork “{title}”"))?;
                forked_start.push(Effect::Resume {
                    id: forked.id,
                    title: forked.name,
                });
            }
            Effect::ListConversations => {
                forked_start.push(Effect::ForkNextResume);
                forked_start.push(Effect::ListConversations);
            }
            other => forked_start.push(other),
        }
    }
    Ok(forked_start)
}

/// Forks the session's conversation and continues in the fork.
pub async fn fork_into(
    session: &mut Session,
    http: &ApiClient,
    options: &ForkOptions,
) -> Result<Forked> {
    if !session.is_saved() {
        bail!(NOT_SAVED);
    }
    let made = history::fork(http, session.conversation_id(), options).await?;
    // The count is for the notice and `/resume`; a failed read doesn't undo
    // the fork.
    let messages = history::transcript(http, &made.id)
        .await
        .map(|t| t.messages.len())
        .unwrap_or_default();
    // The fork exists either way; only /rewind of earlier edits is lost.
    let history_error = session
        .edit_history()
        .copy_to(&made.id)
        .err()
        .map(|err| format!("{err:#}; /rewind in the copy can't put back edits made before it"));
    session.continue_conversation(&made.id);
    if let Ok(index) = Index::open() {
        let _ = index.add(&session.workspace(), &made.id, &made.name, messages as u64);
    }
    Ok(Forked {
        id: made.id,
        name: made.name,
        messages,
        history_error,
    })
}

/// Forks the conversation so it ends just before the user's message that
/// said `prompt` (the `later_repeats`-th from the end of those).
async fn rewind_conversation(
    session: &mut Session,
    http: &ApiClient,
    prompt: &str,
    later_repeats: usize,
) -> Result<Forked> {
    if !session.is_saved() {
        bail!(NOT_SAVED);
    }
    let transcript = history::transcript(http, session.conversation_id()).await?;
    let timestamp = message_timestamp(&transcript.messages, prompt, later_repeats)?;
    let options = ForkOptions {
        through_timestamp: Some(timestamp),
        exclude_selected: true,
        name: transcript.title.map(|t| rewound_name(&t)),
    };
    fork_into(session, http, &options).await
}

/// `Fix the parser (rewound)`, within the server's name limit.
fn rewound_name(title: &str) -> String {
    const SUFFIX: &str = " (rewound)";
    let room = history::MAX_NAME_CHARS - SUFFIX.chars().count();
    let base: String = title.chars().take(room).collect();
    format!("{base}{SUFFIX}")
}

/// The saved timestamp of the user's message that said `prompt`.
fn message_timestamp(
    messages: &[history::Message],
    prompt: &str,
    later_repeats: usize,
) -> Result<String> {
    let wanted = prompt.trim();
    let found = messages
        .iter()
        .rev()
        .filter(|m| m.speaker == history::Speaker::You)
        .filter(|m| replay::typed_text(&m.text).trim() == wanted)
        .nth(later_repeats);
    let Some(message) = found else {
        bail!(
            "the chat has no saved message matching that one (the turn may have stopped before \
             the chat saved it), so the conversation can't go back to it; rewind files only, or \
             pick another message"
        );
    };
    match &message.timestamp {
        Some(timestamp) => Ok(timestamp.clone()),
        None => bail!(
            "the chat saved that message without a time, so it can't fork there; pick another message"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::history::{Message, Speaker};

    fn said(text: &str, at: &str) -> Message {
        let mut message = Message::new(Speaker::You, text);
        message.timestamp = Some(at.into());
        message
    }

    #[test]
    fn finds_the_message_by_what_was_typed_counting_repeats_from_the_end() {
        let messages = [
            said("run the tests", "t1"),
            Message::new(Speaker::Gz, "ok"),
            said("fix it\n\n<file path=\"a.py\">\nx\n</file>\n", "t2"),
            said("run the tests", "t3"),
        ];
        assert_eq!(message_timestamp(&messages, "fix it", 0).unwrap(), "t2");
        assert_eq!(
            message_timestamp(&messages, "run the tests", 0).unwrap(),
            "t3"
        );
        assert_eq!(
            message_timestamp(&messages, "run the tests", 1).unwrap(),
            "t1"
        );
        let missing = message_timestamp(&messages, "never said", 0).unwrap_err();
        assert!(missing.to_string().contains("no saved message matching"));
    }

    #[test]
    fn finds_a_message_sent_in_plan_mode() {
        let sent = format!(
            "plan it\n\n{}",
            crate::assistant::session_tools::PLAN_MODE_NOTE
        );
        let messages = [said(&sent, "t1")];
        assert_eq!(message_timestamp(&messages, "plan it", 0).unwrap(), "t1");
    }

    #[test]
    fn rewound_names_fit_the_server_limit() {
        assert_eq!(rewound_name("Parser"), "Parser (rewound)");
        let long = rewound_name(&"x".repeat(600));
        assert_eq!(long.chars().count(), history::MAX_NAME_CHARS);
    }
}
