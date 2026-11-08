//! The chat page: conversations, sending a prompt, and its streamed run.

use crate::cmd::new_uuid;
use crate::tui::chat::{ActiveRun, ChatFocus, Conversation, RunRequest};
use crate::tui::fetch::{FetchTarget, Request};
use crate::tui::palette::{Action, Entry};

use super::{App, Tone, CHAT};

impl App {
    pub(super) fn load_chat_list(&mut self) {
        let id = self.fetch_get(
            FetchTarget::ChatList,
            "chat",
            "/chat/conversations?assistantMode=chat".to_string(),
        );
        self.awaiting_chat_list = Some(id);
    }

    pub(super) fn load_transcript(&mut self, conversation_id: &str) {
        let path = format!("/chat/conversations/{}", crate::cmd::seg(conversation_id));
        let id = self.fetch_get(
            FetchTarget::ChatTranscript {
                conversation_id: conversation_id.to_string(),
            },
            "chat",
            path,
        );
        self.awaiting_transcript = Some(id);
    }

    pub(super) fn open_conversation(&mut self, conversation: Conversation) {
        if self.chat.is_streaming() {
            self.set_status(Tone::Error, "A reply is still streaming · Esc cancels it");
            return;
        }
        self.remember(Entry {
            label: conversation.title.clone(),
            hint: "recent chat",
            action: Action::OpenConversation {
                id: conversation.id.clone(),
                title: conversation.title.clone(),
            },
        });
        let id = conversation.id.clone();
        self.chat.open_conversation(conversation);
        self.chat.focus = ChatFocus::Composer;
        self.load_transcript(&id);
    }

    pub(super) fn new_chat(&mut self) {
        if self.chat.is_streaming() {
            self.set_status(Tone::Error, "A reply is still streaming · Esc cancels it");
            return;
        }
        self.open_and_focus(CHAT);
        self.chat.new_chat();
        self.set_status(Tone::Info, "New chat · type your question and press Enter");
    }

    pub(super) fn send_prompt(&mut self) {
        let Some((conversation_id, prompt, title)) = self.chat.take_prompt(new_uuid) else {
            return;
        };
        let tag = self.next_id();
        self.chat.run = Some(ActiveRun { tag, run_id: None });
        self.outbox.push(Request::StartChat(RunRequest {
            tag,
            conversation_id,
            prompt,
            title,
        }));
        self.set_status(Tone::Info, "Thinking… · Esc to cancel");
    }

    pub(super) fn cancel_run(&mut self) {
        if let Some(run) = &self.chat.run {
            self.outbox.push(Request::CancelChat {
                tag: run.tag,
                run_id: run.run_id.clone(),
            });
            self.set_status(Tone::Info, "Cancelling…");
        }
    }

    pub fn chat_update(&mut self, tag: u64, update: crate::tui::chat::ChatUpdate) {
        if self.chat.run.as_ref().map(|r| r.tag) != Some(tag) {
            return;
        }
        let ending = matches!(
            update,
            crate::tui::chat::ChatUpdate::Finished { .. }
                | crate::tui::chat::ChatUpdate::Failed(_)
                | crate::tui::chat::ChatUpdate::Paused(_)
        );
        self.chat.apply(update);
        if ending {
            // The list picks up the new chat's title and ordering.
            self.load_chat_list();
            self.set_status(Tone::Ok, "Reply finished");
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::tui::app::test_support::*;
    use crate::tui::app::Focus;

    use crossterm::event::KeyCode;

    use crate::tui::chat::ChatFocus;
    use crate::tui::chat::ChatUpdate;
    use crate::tui::fetch::Request;

    use crate::tui::palette::Action;
    use serde_json::json;

    #[test]
    fn starts_on_chat_and_loads_conversations() {
        let app = app();
        assert!(app.is_chat());
        assert_eq!(app.focus, Focus::Sidebar);
        assert_eq!(
            last_fetch(&app).path,
            "/chat/conversations?assistantMode=chat"
        );
    }

    #[test]
    fn chat_send_streams_and_cancels() {
        let mut app = app();
        answer(
            &mut app,
            json!({"conversations": [{"conversationId": "c1", "conversationName": "First"}]}),
        );
        press(&mut app, &[KeyCode::Enter, KeyCode::Enter]);
        assert_eq!(app.chat.open.as_ref().unwrap().id, "c1");
        assert_eq!(last_fetch(&app).path, "/chat/conversations/c1");
        answer(
            &mut app,
            json!({"messages": [{"speaker": "Human", "message": "hi"}]}),
        );
        assert_eq!(app.chat.focus, ChatFocus::Composer);
        for c in "next q".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        let Some(Request::StartChat(run)) = app.outbox.last().cloned() else {
            panic!("no chat run queued");
        };
        assert_eq!(
            (run.conversation_id.as_str(), run.prompt.as_str()),
            ("c1", "next q")
        );
        app.chat_update(
            run.tag,
            ChatUpdate::Started {
                run_id: "r1".into(),
            },
        );
        app.chat_update(run.tag, ChatUpdate::Text("answer".into()));
        app.chat_update(run.tag + 99, ChatUpdate::Text("stale".into()));
        assert_eq!(app.chat.messages.last().unwrap().text, "answer");
        press(&mut app, &[KeyCode::Esc]);
        assert!(matches!(
            app.outbox.last(),
            Some(Request::CancelChat { run_id: Some(id), .. }) if id == "r1"
        ));
        app.chat_update(
            run.tag,
            ChatUpdate::Finished {
                status: Some("cancelled".into()),
            },
        );
        assert!(!app.chat.is_streaming());
    }

    #[test]
    fn new_chat_gets_a_fresh_conversation_id() {
        let mut app = app();
        app.run_action(Action::NewChat);
        for c in "hello".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        press(&mut app, &[KeyCode::Enter]);
        let Some(Request::StartChat(run)) = app.outbox.last().cloned() else {
            panic!("no chat run queued");
        };
        assert_eq!(run.conversation_id.len(), 36);
        assert_eq!(run.title.as_deref(), Some("hello"));
    }
}
