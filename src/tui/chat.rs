//! Chat view: conversation list, transcript, composer, and a streaming run.
//!
//! Network work happens in `fetch.rs`; it reports progress as [`ChatUpdate`]s,
//! which [`ChatView::apply`] folds into the transcript. That enum is the seam
//! for local tools: a client-tool request arrives as `ChatUpdate::Paused`
//! (rendered as a notice today) and is where an approval prompt plugs in;
//! tool activity already renders as `Role::Tool` lines.

use serde_json::{json, Value};
use tui_textarea::TextArea;

use crate::sse::Event;

const TITLE_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
    Notice,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    pub text: String,
}

impl ChatMessage {
    fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFocus {
    List,
    Composer,
}

/// Progress of one chat run, as reported by the stream task.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatUpdate {
    Started {
        run_id: String,
    },
    Text(String),
    Tool(String),
    Notice(String),
    /// The run stopped for a human decision (tool approval, client tool).
    Paused(Value),
    Failed(String),
    Finished {
        status: Option<String>,
    },
}

/// What the stream task should do with one server event.
#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    Update(ChatUpdate),
    Reconnect,
    Done,
    Ignore,
}

/// A run being streamed into the open conversation.
#[derive(Debug, Clone)]
pub struct ActiveRun {
    /// Request tag; answers from older runs are dropped.
    pub tag: u64,
    pub run_id: Option<String>,
}

/// Everything the stream task needs to start a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub tag: u64,
    pub conversation_id: String,
    pub prompt: String,
    /// Set for a new conversation only.
    pub title: Option<String>,
}

impl RunRequest {
    /// `POST /chat/runs` body, matching `gz chat ask` and the web app.
    pub fn body(&self) -> Value {
        let mut body = json!({
            "userInput": self.prompt,
            "conversationId": self.conversation_id,
            "assistantMode": "chat",
            "useRag": false,
            "supportsActionConfirmations": true,
        });
        if let Some(title) = &self.title {
            body["conversationName"] = json!(title);
        }
        body
    }
}

pub struct ChatView {
    pub conversations: Vec<Conversation>,
    pub selected: usize,
    pub open: Option<Conversation>,
    pub messages: Vec<ChatMessage>,
    pub composer: TextArea<'static>,
    pub focus: ChatFocus,
    /// Transcript lines scrolled up from the bottom.
    pub scroll: usize,
    pub run: Option<ActiveRun>,
    pub list_loaded: bool,
    pub error: Option<String>,
}

impl Default for ChatView {
    fn default() -> Self {
        Self {
            conversations: Vec::new(),
            selected: 0,
            open: None,
            messages: Vec::new(),
            composer: new_composer(),
            focus: ChatFocus::List,
            scroll: 0,
            run: None,
            list_loaded: false,
            error: None,
        }
    }
}

fn new_composer() -> TextArea<'static> {
    let mut composer = TextArea::default();
    composer.set_placeholder_text("Ask Groundzero AI…");
    composer.set_cursor_line_style(ratatui::style::Style::default());
    composer
}

impl ChatView {
    pub fn is_streaming(&self) -> bool {
        self.run.is_some()
    }

    pub fn selected_conversation(&self) -> Option<&Conversation> {
        self.conversations.get(self.selected)
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.conversations.is_empty() {
            return;
        }
        let last = self.conversations.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn set_conversations(&mut self, value: &Value) {
        self.conversations = parse_conversations(value);
        self.list_loaded = true;
        if let Some(open) = &self.open {
            if let Some(i) = self.conversations.iter().position(|c| c.id == open.id) {
                self.selected = i;
            }
        }
        self.selected = self
            .selected
            .min(self.conversations.len().saturating_sub(1));
    }

    pub fn open_conversation(&mut self, conversation: Conversation) {
        self.open = Some(conversation);
        self.messages.clear();
        self.scroll = 0;
        self.error = None;
    }

    pub fn new_chat(&mut self) {
        self.open = None;
        self.messages.clear();
        self.scroll = 0;
        self.error = None;
        self.focus = ChatFocus::Composer;
    }

    pub fn set_transcript(&mut self, value: &Value) {
        self.messages = parse_transcript(value);
        self.scroll = 0;
    }

    pub fn composer_text(&self) -> String {
        self.composer.lines().join("\n")
    }

    /// Takes the composer text and shows it as the user's message. Returns
    /// `(conversation id, prompt, title for a new chat)` for the run request.
    pub fn take_prompt(
        &mut self,
        new_id: impl FnOnce() -> String,
    ) -> Option<(String, String, Option<String>)> {
        let prompt = self.composer_text();
        if prompt.trim().is_empty() || self.is_streaming() {
            return None;
        }
        self.composer = new_composer();
        let (id, title) = match &self.open {
            Some(open) => (open.id.clone(), None),
            None => {
                let title = default_title(&prompt);
                let conversation = Conversation {
                    id: new_id(),
                    title: title.clone(),
                };
                let id = conversation.id.clone();
                self.open = Some(conversation);
                (id, Some(title))
            }
        };
        self.messages
            .push(ChatMessage::new(Role::User, prompt.trim()));
        self.messages.push(ChatMessage::new(Role::Assistant, ""));
        self.scroll = 0;
        Some((id, prompt, title))
    }

    pub fn apply(&mut self, update: ChatUpdate) {
        match update {
            ChatUpdate::Started { run_id } => {
                if let Some(run) = self.run.as_mut() {
                    run.run_id = Some(run_id);
                }
            }
            ChatUpdate::Text(text) => self.append_reply(&text),
            ChatUpdate::Tool(name) => self.push_aside(Role::Tool, format!("⚙ {name}")),
            ChatUpdate::Notice(text) => self.push_aside(Role::Notice, text),
            ChatUpdate::Paused(details) => {
                let what = details["toolName"].as_str().unwrap_or("an action");
                self.push_aside(
                    Role::Notice,
                    format!("Paused: approval needed for {what}. Approve it in the web app or with `gz chat approve`."),
                );
                self.run = None;
            }
            ChatUpdate::Failed(message) => {
                self.push_aside(Role::Error, message);
                self.run = None;
            }
            ChatUpdate::Finished { status } => {
                if status.as_deref() == Some("cancelled") {
                    self.push_aside(Role::Notice, "Cancelled.");
                }
                self.drop_empty_reply();
                self.run = None;
            }
        }
    }

    fn append_reply(&mut self, text: &str) {
        match self.messages.last_mut() {
            Some(last) if last.role == Role::Assistant => last.text.push_str(text),
            _ => self.messages.push(ChatMessage::new(Role::Assistant, text)),
        }
    }

    /// Tool and notice lines go before the reply still being written, so
    /// the text keeps flowing at the bottom.
    fn push_aside(&mut self, role: Role, text: impl Into<String>) {
        let line = ChatMessage::new(role, text);
        match self.messages.last() {
            Some(last) if last.role == Role::Assistant && last.text.is_empty() => {
                let at = self.messages.len() - 1;
                self.messages.insert(at, line);
            }
            _ => self.messages.push(line),
        }
    }

    fn drop_empty_reply(&mut self) {
        if self
            .messages
            .last()
            .is_some_and(|m| m.role == Role::Assistant && m.text.is_empty())
        {
            self.messages.pop();
        }
    }
}

pub fn default_title(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or(prompt).trim();
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

pub fn parse_conversations(value: &Value) -> Vec<Conversation> {
    value["conversations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Conversation {
                id: c["conversationId"].as_str()?.to_string(),
                title: c["conversationName"]
                    .as_str()
                    .filter(|t| !t.is_empty())
                    .unwrap_or("Untitled chat")
                    .to_string(),
            })
        })
        .collect()
}

/// Messages carry `speaker` ("Human" / "AI") and `message` text.
pub fn parse_transcript(value: &Value) -> Vec<ChatMessage> {
    value["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let text = m["message"].as_str()?.trim();
            if text.is_empty() {
                return None;
            }
            let speaker = m["speaker"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let role = if speaker == "human" || speaker == "user" {
                Role::User
            } else {
                Role::Assistant
            };
            Some(ChatMessage::new(role, text))
        })
        .collect()
}

/// Maps one SSE event from `/chat/runs/{id}/events` to what the TUI does.
pub fn interpret(event: &Event) -> Signal {
    if event.is_done() {
        return Signal::Done;
    }
    let data = event.json().unwrap_or(Value::Null);
    let kind = data["type"]
        .as_str()
        .or(event.event.as_deref())
        .unwrap_or_default();
    let text_of = |d: &Value| {
        ["message", "detail", "error", "text"]
            .iter()
            .find_map(|k| d[k].as_str())
            .unwrap_or("something went wrong")
            .to_string()
    };
    let update = match kind {
        "reconnect" => return Signal::Reconnect,
        "llm_chunk" => ChatUpdate::Text(data["text"].as_str().unwrap_or_default().to_string()),
        "tool_use" | "tool" => ChatUpdate::Tool(
            data["toolName"]
                .as_str()
                .or(data["name"].as_str())
                .unwrap_or("tool")
                .to_string(),
        ),
        "tool_error" => ChatUpdate::Notice(format!("⚠ tool failed: {}", text_of(&data))),
        "warning" | "system_warning" => ChatUpdate::Notice(format!("⚠ {}", text_of(&data))),
        "interrupt" | "client_tool_request" => ChatUpdate::Paused(data),
        "error" | "failed" => ChatUpdate::Failed(text_of(&data)),
        "stopped" | "cancelled" => ChatUpdate::Finished {
            status: Some("cancelled".to_string()),
        },
        "run_status" | "run_terminal" => match data["status"].as_str() {
            Some("interrupted") => ChatUpdate::Paused(data),
            Some("failed" | "timed_out") => ChatUpdate::Failed(format!(
                "The run ended with status {}",
                data["status"].as_str().unwrap_or_default()
            )),
            _ => return Signal::Ignore,
        },
        _ => return Signal::Ignore,
    };
    Signal::Update(update)
}

/// Wraps `text` to `width` columns on word boundaries, keeping blank lines.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let fits = line.chars().count() + word.chars().count() < width;
            if !line.is_empty() && !fits {
                lines.push(std::mem::take(&mut line));
            }
            let mut word = word.to_string();
            while word.chars().count() > width {
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                lines.push(head);
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(data: &str) -> Event {
        Event {
            id: Some("1".into()),
            event: None,
            data: data.into(),
        }
    }

    fn streaming_view() -> ChatView {
        let mut view = ChatView::default();
        view.composer.insert_str("hello there");
        let (id, prompt, title) = view.take_prompt(|| "c-new".to_string()).unwrap();
        assert_eq!((id.as_str(), prompt.as_str()), ("c-new", "hello there"));
        assert_eq!(title.as_deref(), Some("hello there"));
        view.run = Some(ActiveRun {
            tag: 1,
            run_id: None,
        });
        view
    }

    #[test]
    fn streams_text_into_one_reply() {
        let mut view = streaming_view();
        view.apply(ChatUpdate::Started {
            run_id: "r1".into(),
        });
        view.apply(ChatUpdate::Text("Hel".into()));
        view.apply(ChatUpdate::Tool("search".into()));
        view.apply(ChatUpdate::Text("lo".into()));
        view.apply(ChatUpdate::Finished { status: None });
        let roles: Vec<Role> = view.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(view.messages[1].text, "Hel");
        assert!(!view.is_streaming());
    }

    #[test]
    fn tool_lines_land_before_an_empty_reply() {
        let mut view = streaming_view();
        view.apply(ChatUpdate::Tool("sql".into()));
        view.apply(ChatUpdate::Text("done".into()));
        let roles: Vec<Role> = view.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Tool, Role::Assistant]);
    }

    #[test]
    fn failures_and_pauses_end_the_run() {
        let mut view = streaming_view();
        view.apply(ChatUpdate::Failed("boom".into()));
        assert!(!view.is_streaming());
        assert_eq!(view.messages.last().unwrap().role, Role::Assistant);
        let mut view = streaming_view();
        view.apply(ChatUpdate::Paused(json!({"toolName": "write_file"})));
        assert!(view.messages.iter().any(|m| m.text.contains("write_file")));
        assert!(!view.is_streaming());
    }

    #[test]
    fn empty_or_busy_composer_sends_nothing() {
        let mut view = ChatView::default();
        assert!(view.take_prompt(|| "x".into()).is_none());
        let mut busy = streaming_view();
        busy.composer.insert_str("again");
        assert!(busy.take_prompt(|| "x".into()).is_none());
    }

    #[test]
    fn continuing_a_chat_reuses_its_id() {
        let mut view = ChatView::default();
        view.open_conversation(Conversation {
            id: "c1".into(),
            title: "t".into(),
        });
        view.composer.insert_str("more");
        let (id, _, title) = view.take_prompt(|| "unused".into()).unwrap();
        assert_eq!(id, "c1");
        assert!(title.is_none());
    }

    #[test]
    fn interprets_stream_events() {
        assert_eq!(interpret(&event("[DONE]")), Signal::Done);
        assert_eq!(
            interpret(&event(r#"{"type":"reconnect"}"#)),
            Signal::Reconnect
        );
        assert_eq!(
            interpret(&event(r#"{"type":"llm_chunk","text":"hi"}"#)),
            Signal::Update(ChatUpdate::Text("hi".into()))
        );
        assert!(matches!(
            interpret(&event(r#"{"type":"run_status","status":"interrupted"}"#)),
            Signal::Update(ChatUpdate::Paused(_))
        ));
        assert_eq!(
            interpret(&event(r#"{"type":"run_status","status":"completed"}"#)),
            Signal::Ignore
        );
        assert_eq!(
            interpret(&event(r#"{"type":"error","detail":"nope"}"#)),
            Signal::Update(ChatUpdate::Failed("nope".into()))
        );
    }

    #[test]
    fn parses_list_and_transcript() {
        let list = json!({"conversations": [{"conversationId": "c1", "conversationName": ""}]});
        assert_eq!(parse_conversations(&list)[0].title, "Untitled chat");
        let transcript = json!({"messages": [
            {"speaker": "Human", "message": "q"},
            {"speaker": "AI", "message": "a"},
            {"speaker": "AI", "message": "  "}
        ]});
        let messages = parse_transcript(&transcript);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn run_body_matches_the_cli() {
        let req = RunRequest {
            tag: 1,
            conversation_id: "c1".into(),
            prompt: "hi".into(),
            title: Some("hi".into()),
        };
        let body = req.body();
        assert_eq!(body["conversationId"], "c1");
        assert_eq!(body["conversationName"], "hi");
        assert_eq!(body["assistantMode"], "chat");
    }

    #[test]
    fn wraps_words_and_long_tokens() {
        assert_eq!(wrap("one two three", 9), vec!["one two", "three"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap(&"x".repeat(20), 8).len(), 3);
    }
}
